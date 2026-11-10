/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * Licensed under the BSD-style license in the root LICENSE file.
 */

//! Real policy/handler calls with scripted physical observations. These do not
//! certify ptrace stop provenance, kernel publication, or native cancellation.

use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Pid;
use reverie::Stack;
use reverie::Tool;
use reverie::native_syscall::NativeDescriptorSnapshot;
use reverie::native_syscall::NativeSyscallAbi;
use reverie::native_syscall::NativeSyscallAuthority;
use reverie::native_syscall::NativeSyscallId;
use reverie::native_syscall::NativeSyscallRequest;
use reverie::reserved_input::InputAbi;
use reverie::reserved_input::InputCallInput;
use reverie::reserved_input::InputDescriptorFacts;
use reverie::reserved_input::InputExpectedSharing;
use reverie::reserved_input::InputGrantKind;
use reverie::reserved_input::InputUnavailable;
use reverie::reserved_input::PhysicalInjectionDescription;
use reverie::reserved_input::ReservedInputImage;
use reverie::reserved_input::ReservedInputLayout;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Errno;
use reverie::syscalls::LocalMemory;
use reverie::syscalls::SyscallInfo;

use super::*;
use crate::Config;
use crate::GlobalState;
use crate::ThreadState;
use crate::types::DetPid;

struct NoStack;
struct NoGuard;
impl Drop for NoGuard {
    fn drop(&mut self) {}
}
impl Stack for NoStack {
    type StackGuard = NoGuard;
    fn size(&self) -> usize {
        panic!("not a stack-staging fixture")
    }
    fn capacity(&self) -> usize {
        panic!("not a stack-staging fixture")
    }
    fn push<'a, T>(&mut self, _: T) -> Addr<'a, T> {
        panic!("not a stack-staging fixture")
    }
    fn reserve<'a, T>(&mut self) -> AddrMut<'a, T> {
        panic!("not a stack-staging fixture")
    }
    fn commit(self) -> Result<NoGuard, Errno> {
        panic!("not a stack-staging fixture")
    }
}

struct Fixture {
    config: Config,
    thread: ThreadState<()>,
    control: ReservedInputControl,
    task: InputTaskId,
    sibling: InputTaskId,
    registry: Arc<NativeOperations>,
    injected: Vec<Sysno>,
    producer_result: Result<i64, Errno>,
    bound_requests: Mutex<Vec<NativeSyscallRequest>>,
    resource_requests: std::sync::atomic::AtomicUsize,
    change_request: bool,
    native_abi: Option<NativeSyscallAbi>,
    native_not_run: bool,
    process_memory_result: bool,
    native_tid: i32,
}

impl Fixture {
    fn new(shared: bool) -> Self {
        let config = Config {
            guest_ids: detcore_model::pid::GuestIds::Chosen {
                base: 5000,
                refuse_host_ids: true,
            },
            sequentialize_threads: true,
            ..Config::default()
        };
        let control = ReservedInputControl::new_backend().unwrap();
        persistent::install_native_history(&control).unwrap();
        let registry = NativeOperations::install(&control).unwrap();
        let root = control.backend_register_root(5000, 5000).unwrap();
        persistent::register_initial_history(&control, root.task).unwrap();
        #[cfg(target_arch = "x86_64")]
        let abi = InputAbi::X86_64;
        #[cfg(target_arch = "aarch64")]
        let abi = InputAbi::Aarch64;
        control
            .backend_set_layout(
                root.task,
                ReservedInputLayout {
                    base: reverie::reserved_input::RESERVED_INPUT_BASE,
                    page_bytes: 4096,
                    abi,
                },
            )
            .unwrap();
        let sibling = control
            .backend_register_child(root.task, 5001, 5001, Some(false), Some(shared))
            .unwrap();
        control.grant(root.task, InputGrantKind::Ordinary).unwrap();
        let mut thread = ThreadState::new(DetPid::from_raw(5000), &config, ());
        thread.detpid = Some(DetPid::from_raw(5000));
        Self {
            thread,
            config,
            control,
            task: root.task,
            sibling: sibling.task,
            registry,
            injected: Vec::new(),
            producer_result: Ok(9),
            bound_requests: Mutex::new(Vec::new()),
            resource_requests: std::sync::atomic::AtomicUsize::new(0),
            change_request: false,
            native_abi: None,
            native_not_run: false,
            process_memory_result: false,
            native_tid: 5000,
        }
    }

    fn native(&self, syscall: Syscall, id: u64) -> NativeSyscallCall {
        let (number, args) = syscall.into_parts();
        #[cfg(target_arch = "x86_64")]
        let audit_arch = 0xc000_003e;
        #[cfg(target_arch = "aarch64")]
        let audit_arch = 0xc000_00b7;
        NativeSyscallCall {
            id: NativeSyscallId::from_backend_counter(id),
            abi: self
                .native_abi
                .unwrap_or(NativeSyscallAbi::Linux { audit_arch }),
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
            operation_bound: true,
            input_receipt: None,
            staged_image: None,
        }
    }

    fn gate(&self, task: InputTaskId) -> Result<(), InputUnavailable> {
        self.control.grant(task, InputGrantKind::Ordinary).unwrap();
        let scope = self.control.begin_operation(task, 70).unwrap();
        let receipt = self
            .control
            .prepare_injection(
                &scope,
                PhysicalInjectionDescription {
                    number: Sysno::clone3 as u64,
                    args: [0; 6],
                    input: InputCallInput::Registers,
                    expected_sharing: InputExpectedSharing::default(),
                    transient: Vec::new(),
                    prospective: Vec::new(),
                    unknown_lifecycle: None,
                },
            )
            .unwrap();
        let image = ReservedInputImage::new(self.control.layout(task).unwrap(), &[]).unwrap();
        self.control
            .backend_begin_preparation(&receipt, &scope, image)
            .unwrap();
        self.control
            .backend_prepared_stopped(receipt.id())
            .map(|_| ())
    }
}

impl NativeSyscallFacts for Fixture {
    fn control(&self) -> Option<ReservedInputControl> {
        Some(self.control.clone())
    }
    fn task(&self) -> Option<InputTaskId> {
        Some(self.task)
    }
    fn authority(&self) -> Result<NativeSyscallAuthority, InputUnavailable> {
        self.control.native_syscall_authority(self.task)
    }
    fn descriptor(&self, fd: i32) -> Result<InputDescriptorFacts, InputUnavailable> {
        if self.process_memory_result && matches!(fd, 1 | 9) {
            // Scripted, immutable final-stop facts for dup(1) -> 9. This is
            // deliberately not a native procfd attestation or an FD census
            // offered in place of the actual scalar result.
            return Ok(InputDescriptorFacts {
                fd,
                status_flags: Ok(libc::O_RDWR),
                async_signal: Ok(0),
                async_owner: Ok((crate::host_timed_producers::F_OWNER_PID, 0)),
                identity: Ok((libc::makedev(0, 23), 7, libc::S_IFREG | 0o600)),
                filesystem: Ok(0x9fa0),
                mount_id: Ok(27),
                proc_path: Ok(b"/proc/5000/mem".to_vec()),
                path_context: Ok(reverie::reserved_input::InputPathContext {
                    reader_mount_namespace: (1, 2),
                    target_mount_namespace: (1, 2),
                    reader_root: (3, 4),
                    target_root: (3, 4),
                    reader_cgroup_namespace: (5, 6),
                    target_cgroup_namespace: (5, 6),
                }),
            });
        }
        Ok(InputDescriptorFacts {
            fd,
            status_flags: Ok(libc::O_RDWR),
            async_signal: Ok(0),
            async_owner: Ok((crate::host_timed_producers::F_OWNER_PID, 0)),
            identity: Ok((1, fd as u64, libc::S_IFSOCK | 0o600)),
            filesystem: Ok(0x534f434b),
            mount_id: Ok(1),
            proc_path: Ok(b"socket:[7]".to_vec()),
            path_context: Err(reverie::reserved_input::InputNativeError {
                operation: "scripted non-path socket",
                errno: None,
            }),
        })
    }
    fn descriptors(&self) -> Result<NativeDescriptorSnapshot, InputUnavailable> {
        Ok(NativeDescriptorSnapshot {
            descriptors: if self.process_memory_result {
                vec![
                    self.descriptor(1)?,
                    self.descriptor(2)?,
                    self.descriptor(9)?,
                ]
            } else {
                vec![self.descriptor(9)?]
            },
            mountinfo: Ok(if self.process_memory_result {
                b"27 1 0:23 / /proc rw - proc proc rw\n".to_vec()
            } else {
                Vec::new()
            }),
        })
    }
}

#[reverie::tool]
impl GlobalRPC<GlobalState> for Fixture {
    fn config(&self) -> &Config {
        &self.config
    }
    async fn send_rpc(
        &self,
        message: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        use crate::tool_global::GlobalRequest;
        use crate::tool_global::GlobalResponse;
        use crate::tool_global::ResumeStatus;
        assert!(
            matches!(message.2, GlobalRequest::RequestResources(..)),
            "unexpected fixture RPC: {:?}",
            message.2
        );
        self.resource_requests
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        (None, GlobalResponse::RequestResources(ResumeStatus::Normal))
    }
}

#[reverie::tool]
impl Guest<Detcore> for Fixture {
    type Memory = LocalMemory;
    type Stack = NoStack;
    fn tid(&self) -> Pid {
        Pid::from_raw(self.native_tid)
    }
    fn pid(&self) -> Pid {
        Pid::from_raw(self.native_tid)
    }
    fn ppid(&self) -> Option<Pid> {
        None
    }
    fn memory(&self) -> LocalMemory {
        LocalMemory::new()
    }
    fn thread_state(&self) -> &ThreadState<()> {
        &self.thread
    }
    fn thread_state_mut(&mut self) -> &mut ThreadState<()> {
        &mut self.thread
    }
    fn reserved_input_control(&self) -> Option<ReservedInputControl> {
        Some(self.control.clone())
    }
    fn reserved_input_task(&self) -> Option<InputTaskId> {
        Some(self.task)
    }
    fn supports_native_operations(&self) -> bool {
        true
    }
    fn bind_native_request<S: SyscallInfo>(
        &self,
        operation: &NativeOperation,
        syscall: S,
    ) -> Result<NativeOperation, Error> {
        assert!(
            self.control.current_grant(self.task).is_some(),
            "binding must precede release"
        );
        assert_eq!(
            self.resource_requests
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        let (number, args) = syscall.into_parts();
        let call = self.native(Syscall::from_raw(number, args), 0);
        let request = NativeSyscallRequest {
            abi: call.abi,
            number: call.number,
            args: call.args,
        };
        self.bound_requests.lock().unwrap().push(request);
        operation.bind_request_from_backend(request, ())
    }
    async fn regs(&mut self) -> libc::user_regs_struct {
        panic!("no register rewrite")
    }
    async fn stack(&mut self) -> NoStack {
        panic!("no stack staging")
    }
    async fn daemonize(&mut self) {
        panic!("no daemonization")
    }
    async fn inject<S: SyscallInfo>(&mut self, _: S) -> Result<i64, Errno> {
        panic!("actual handler injection must carry its explicit operation")
    }
    async fn inject_with_native_operation<S: SyscallInfo>(
        &mut self,
        call: S,
        operation: NativeOperation,
    ) -> Result<i64, Errno> {
        let (number, args) = call.into_parts();
        let mut native = self.native(
            Syscall::from_raw(number, args),
            self.injected.len() as u64 + 1,
        );
        if self.change_request {
            native.args[1] = native.args[1].wrapping_add(1);
        }
        if let Some(request) = operation.bound_request() {
            // Explicitly scripted backend ownership check. Native ptrace mode
            // and final-register enforcement require separate backend tests.
            assert!(self.control.current_grant(self.task).is_none());
            assert_eq!(
                self.resource_requests
                    .load(std::sync::atomic::Ordering::SeqCst),
                1
            );
            assert_eq!(
                request,
                NativeSyscallRequest {
                    abi: native.abi,
                    number: native.number,
                    args: native.args
                },
                "native request ownership mismatch before issue"
            );
        }
        let physical = operation.begin_native_attempt().unwrap();
        let observer = operation.observe(self, &native).unwrap().unwrap();
        if self.native_not_run {
            // A backend-authenticated never-issued outcome, not an errno
            // interpreted as proof that the kernel performed no effects.
            observer
                .observe(&native, NativeSyscallEvent::ProvedNotRun, None)
                .unwrap();
            physical.cancelled_before_transition();
            return Err(Errno::EINTR);
        }
        physical.transition_started();
        observer
            .observe(&native, NativeSyscallEvent::ResumeIssued, None)
            .unwrap();
        self.injected.push(number);
        let result = match number {
            Sysno::pidfd_getfd | Sysno::dup => self.producer_result,
            Sysno::write => Ok(args.arg2 as i64),
            Sysno::close | Sysno::ppoll => Ok(0),
            _ => panic!("unexpected actual handler call: {number}"),
        };
        observer
            .observe(
                &native,
                NativeSyscallEvent::NativeResult(result),
                Some(self),
            )
            .unwrap();
        physical.effects_ended();
        result
    }
    async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> reverie::Never {
        panic!("no tail fixture")
    }
    fn set_timer(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
        panic!("no timer")
    }
    fn set_timer_precise(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
        panic!("no timer")
    }
    fn read_clock(&mut self) -> Result<u64, Error> {
        panic!("no clock change")
    }
}

fn call(number: Sysno, args: [usize; 6]) -> Syscall {
    Syscall::from_raw(
        number,
        SyscallArgs::new(args[0], args[1], args[2], args[3], args[4], args[5]),
    )
}

#[test]
fn native_acquisition_emulation_notrun_and_fresh_result_preserve_staging() {
    for variant in 0..4 {
        let guest = Fixture::new(true);
        let syscall = call(
            if variant == 3 {
                Sysno::socket
            } else {
                Sysno::openat
            },
            [0; 6],
        );
        let operation = NativeCallOperation::begin(&guest, syscall)
            .unwrap()
            .unwrap();
        if variant != 0 {
            let native = guest.native(syscall, 1);
            let physical = operation.binding.begin_native_attempt().unwrap();
            let observer = operation.binding.observe(&guest, &native).unwrap().unwrap();
            if variant != 1 {
                physical.transition_started();
                observer
                    .observe(&native, NativeSyscallEvent::ResumeIssued, None)
                    .unwrap();
                observer
                    .observe(&native, NativeSyscallEvent::ResultUnobserved, None)
                    .unwrap();
            }
            observer
                .observe(
                    &native,
                    if variant == 3 {
                        NativeSyscallEvent::NativeResult(Ok(9))
                    } else {
                        NativeSyscallEvent::ProvedNotRun
                    },
                    Some(&guest),
                )
                .unwrap();
            physical.effects_ended();
        }
        operation.returned().unwrap();
        operation.readmitted().unwrap();
        assert!(
            guest.gate(guest.sibling).is_ok(),
            "emulated, proved-not-run, and fresh results must stay eligible: {variant}"
        );
    }
}

#[test]
fn native_receive_error_is_not_erased_by_later_benign_census_or_retry() {
    let guest = Fixture::new(true);
    let syscall = call(Sysno::recvmsg, [7, 0x1000, 0, 0, 0, 0]);
    let operation = NativeCallOperation::begin(&guest, syscall)
        .unwrap()
        .unwrap();
    for (id, result) in [(1, Err(Errno::EFAULT)), (2, Ok(0))] {
        let native = guest.native(syscall, id);
        let physical = operation.binding.begin_native_attempt().unwrap();
        let observer = operation.binding.observe(&guest, &native).unwrap().unwrap();
        physical.transition_started();
        observer
            .observe(&native, NativeSyscallEvent::ResumeIssued, None)
            .unwrap();
        observer
            .observe(
                &native,
                NativeSyscallEvent::NativeResult(result),
                Some(&guest),
            )
            .unwrap();
        physical.effects_ended();
    }
    operation.returned().unwrap();
    operation.readmitted().unwrap();
    assert_eq!(
        guest.gate(guest.sibling).unwrap_err().reason(),
        "native-description-proof-unavailable"
    );
}

#[test]
fn native_operation_drop_retains_unresolved_effects_until_exact_terminal_fence() {
    for result_known in [false, true] {
        let guest = Fixture::new(true);
        let syscall = call(Sysno::openat, [0; 6]);
        let operation = NativeCallOperation::begin(&guest, syscall)
            .unwrap()
            .unwrap();
        let native = guest.native(syscall, 1);
        let physical = operation.binding.begin_native_attempt().unwrap();
        let observer = operation.binding.observe(&guest, &native).unwrap().unwrap();
        physical.transition_started();
        observer
            .observe(&native, NativeSyscallEvent::ResumeIssued, None)
            .unwrap();
        observer
            .observe(&native, NativeSyscallEvent::ResultUnobserved, None)
            .unwrap();
        drop(operation);
        assert!(
            guest.registry.terminal_fenced(&[guest.task]).is_err(),
            "Drop cannot turn native uncertainty into completed cancellation"
        );
        assert_eq!(guest.registry.0.lock().unwrap().len(), 1);
        guest.control.freeze_tasks(&[guest.task]).unwrap();
        if result_known {
            observer
                .observe(
                    &native,
                    NativeSyscallEvent::NativeResult(Err(Errno::ENOENT)),
                    Some(&guest),
                )
                .unwrap();
        } else {
            observer
                .observe(&native, NativeSyscallEvent::EffectsComplete, None)
                .unwrap();
        }
        physical.effects_ended();
        guest.control.backend_terminal(guest.task).unwrap();
        guest.registry.terminal_fenced(&[guest.task]).unwrap();
        guest.control.settle_fenced_tasks(&[guest.task]).unwrap();
        guest.registry.fence_settled(&[guest.task]);
        assert!(guest.registry.0.lock().unwrap().is_empty());
        if result_known {
            assert!(guest.gate(guest.sibling).is_ok());
        } else {
            assert_eq!(
                guest.gate(guest.sibling).unwrap_err().reason(),
                "native-description-proof-unavailable"
            );
        }
    }
}

#[tokio::test]
async fn actual_pidfd_handler_success_then_outer_error_keeps_native_acquisition() {
    let mut guest = Fixture::new(true);
    let tool = <Detcore as Tool>::new(Pid::from_raw(5000), &guest.config);
    let syscall = call(Sysno::pidfd_getfd, [3, 4, 1, 0, 0, 0]);
    let operation = NativeCallOperation::begin(&guest, syscall)
        .unwrap()
        .unwrap();
    let result = {
        let mut wrapped =
            reverie::NativeOperationGuest::<_, Detcore>::new(&mut guest, operation.binding())
                .unwrap();
        tool.handle_pidfd_getfd(&mut wrapped, syscall, 3, 4, 1)
            .await
    };
    let error = result
        .expect_err("scripted impossible kernel success must retain the real handler's Tool error");
    assert!(matches!(error, Error::Tool(_)));
    assert_eq!(guest.injected, [Sysno::pidfd_getfd, Sysno::close]);
    drop(operation);
    guest.control.freeze_tasks(&[guest.task]).unwrap();
    guest.control.backend_terminal(guest.task).unwrap();
    guest.registry.terminal_fenced(&[guest.task]).unwrap();
    guest.control.settle_fenced_tasks(&[guest.task]).unwrap();
    guest.registry.fence_settled(&[guest.task]);
    assert_eq!(
        guest.gate(guest.sibling).unwrap_err().reason(),
        "native-description-proof-unavailable",
        "outer handler error and cleanup close are not evidence that no descriptor was installed"
    );
}

#[tokio::test]
async fn actual_pidfd_handler_native_errno_preserves_no_install_and_original_error() {
    let mut guest = Fixture::new(true);
    guest.producer_result = Err(Errno::EINVAL);
    let tool = <Detcore as Tool>::new(Pid::from_raw(5000), &guest.config);
    let syscall = call(Sysno::pidfd_getfd, [3, 4, 1, 0, 0, 0]);
    let operation = NativeCallOperation::begin(&guest, syscall)
        .unwrap()
        .unwrap();
    let result = {
        let mut wrapped =
            reverie::NativeOperationGuest::<_, Detcore>::new(&mut guest, operation.binding())
                .unwrap();
        tool.handle_pidfd_getfd(&mut wrapped, syscall, 3, 4, 1)
            .await
    };
    assert!(matches!(result, Err(Error::Errno(Errno::EINVAL))));
    assert_eq!(guest.injected, [Sysno::pidfd_getfd]);
    operation.returned().unwrap();
    operation.readmitted().unwrap();
    assert!(guest.gate(guest.sibling).is_ok());
}

#[test]
fn scalar_native_result_is_captured_only_at_its_private_table_stop() {
    for shared in [false, true] {
        let guest = Fixture::new(shared);
        let syscall = call(Sysno::openat, [0; 6]);
        let operation = NativeCallOperation::begin(&guest, syscall)
            .unwrap()
            .unwrap();
        let native = guest.native(syscall, 1);
        let physical = operation.binding.begin_native_attempt().unwrap();
        let observer = operation.binding.observe(&guest, &native).unwrap().unwrap();
        physical.transition_started();
        observer
            .observe(&native, NativeSyscallEvent::ResumeIssued, None)
            .unwrap();
        observer
            .observe(
                &native,
                NativeSyscallEvent::NativeResult(Ok(9)),
                Some(&guest),
            )
            .unwrap();
        physical.effects_ended();
        operation.returned().unwrap();
        operation.readmitted().unwrap();
        if shared {
            assert_eq!(
                guest.gate(guest.sibling).unwrap_err().reason(),
                "native-description-proof-unavailable"
            );
        } else {
            assert!(guest.gate(guest.sibling).is_ok());
        }
    }
}

#[test]
fn wrong_physical_abi_cannot_mutate_either_operation_or_reference_history() {
    let guest = Fixture::new(true);
    let syscall = call(Sysno::openat, [0; 6]);
    let operation = NativeCallOperation::begin(&guest, syscall)
        .unwrap()
        .unwrap();
    let native = guest.native(syscall, 1);
    let physical = operation.binding.begin_native_attempt().unwrap();
    let observer = operation.binding.observe(&guest, &native).unwrap().unwrap();
    let mut wrong = native.clone();
    wrong.abi = NativeSyscallAbi::Unverified;
    assert!(
        observer
            .observe(&wrong, NativeSyscallEvent::ResumeIssued, None)
            .is_err()
    );
    assert!(!operation.owner.state.lock().unwrap().attempts[0].issued);
    // The correct identity must still be accepted once: a rejected callback
    // must not have consumed the subsidiary history's unique issue event.
    physical.transition_started();
    observer
        .observe(&native, NativeSyscallEvent::ResumeIssued, None)
        .unwrap();
    observer
        .observe(&native, NativeSyscallEvent::ProvedNotRun, None)
        .unwrap();
    physical.effects_ended();
    operation.returned().unwrap();
    operation.readmitted().unwrap();
    assert!(guest.gate(guest.sibling).is_ok());
}

#[tokio::test]
async fn actual_background_helper_binds_before_release_and_retires_only_after_readmission() {
    let mut guest = Fixture::new(false);
    let tool = <Detcore as Tool>::new(Pid::from_raw(5000), &guest.config);
    let syscall = call(Sysno::ppoll, [0; 6]);
    let result = tool.record_or_replay_blocking(&mut guest, syscall).await;
    assert_eq!(result.unwrap(), 0);
    assert_eq!(guest.injected, [Sysno::ppoll]);
    assert_eq!(guest.bound_requests.lock().unwrap().len(), 1);
    assert_eq!(
        guest
            .resource_requests
            .load(std::sync::atomic::Ordering::SeqCst),
        2,
        "preserve original release and continuation requests"
    );
    assert!(guest.registry.0.lock().unwrap().is_empty());
    assert!(guest.gate(guest.sibling).is_ok());
}

#[tokio::test]
async fn actual_background_helper_changed_final_request_never_issues_under_old_lease() {
    use futures::FutureExt;
    let mut guest = Fixture::new(false);
    guest.change_request = true;
    let tool = <Detcore as Tool>::new(Pid::from_raw(5000), &guest.config);
    let syscall = call(Sysno::ppoll, [0; 6]);
    let result = std::panic::AssertUnwindSafe(tool.record_or_replay_blocking(&mut guest, syscall))
        .catch_unwind()
        .await;
    assert!(
        result.is_err(),
        "changed request must not become guest errno or native fallback"
    );
    assert!(guest.injected.is_empty());
    assert_eq!(
        guest
            .resource_requests
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(
        guest.registry.0.lock().unwrap().len(),
        1,
        "failed ownership never uses Drop to clear its retained operation"
    );
}

// These controls enter real Detcore syscall/helper bodies. Native stop facts,
// request issue, file-table copy and grant delivery are scripted here; they do
// not claim ptrace execution, a native proc-memory write, or native clone proof.
impl Fixture {
    fn share_files_at_grant(&self, tid: i32) -> InputTaskId {
        let grant = self.control.native_request_grant(self.task).unwrap();
        let child = self
            .control
            .backend_register_child(self.task, tid, tid, Some(false), Some(true))
            .unwrap();
        assert_eq!(self.control.native_request_grant(self.task).unwrap(), grant);
        assert!(!self.control.files_proven_unshared(self.task));
        child.task
    }

    fn copy_files_at_grant(&self, tid: i32) -> Self {
        let grant = self.control.native_request_grant(self.task).unwrap();
        let child = self
            .control
            .backend_register_child(self.task, tid, tid, Some(false), Some(false))
            .unwrap();
        assert_eq!(self.control.native_request_grant(self.task).unwrap(), grant);
        assert!(self.control.files_proven_unshared(child.task));
        let mut thread = ThreadState::new(DetPid::from_raw(tid), &self.config, ());
        thread.detpid = Some(DetPid::from_raw(tid));
        // Only stdio is consumed by these handlers; its kernel facts below are
        // scripted explicitly. This is not a general ThreadState fork fixture.
        Self {
            config: self.config.clone(),
            thread,
            control: self.control.clone(),
            task: child.task,
            sibling: self.sibling,
            registry: self.registry.clone(),
            injected: Vec::new(),
            producer_result: self.producer_result,
            bound_requests: Mutex::new(Vec::new()),
            resource_requests: std::sync::atomic::AtomicUsize::new(0),
            change_request: false,
            native_abi: self.native_abi,
            native_not_run: false,
            process_memory_result: self.process_memory_result,
            native_tid: tid,
        }
    }

    fn probe_staging(&self, task: InputTaskId) -> Result<(), InputUnavailable> {
        // Unlike the terminal assertion helper above, this probe explicitly
        // settles its never-issued preparation so later test operations do not
        // inherit a synthetic busy transaction. No force write or syscall runs.
        self.control.grant(task, InputGrantKind::Ordinary).unwrap();
        let scope = self.control.begin_operation(task, 71).unwrap();
        let receipt = self
            .control
            .prepare_injection(
                &scope,
                PhysicalInjectionDescription {
                    number: Sysno::clone3 as u64,
                    args: [0; 6],
                    input: InputCallInput::Registers,
                    expected_sharing: InputExpectedSharing::default(),
                    transient: Vec::new(),
                    prospective: Vec::new(),
                    unknown_lifecycle: None,
                },
            )
            .unwrap();
        let image = ReservedInputImage::new(self.control.layout(task).unwrap(), &[]).unwrap();
        self.control
            .backend_begin_preparation(&receipt, &scope, image)
            .unwrap();
        let result = self
            .control
            .backend_prepared_stopped(receipt.id())
            .map(|_| ());
        self.control.backend_settled_not_run(receipt.id()).unwrap();
        self.control.finish_operation(scope).unwrap();
        result
    }

    async fn run_dup_handler(&mut self, operation: &NativeCallOperation) {
        let tool = <Detcore as Tool>::new(Pid::from_raw(self.native_tid), &self.config);
        let call = reverie::syscalls::Dup::default().with_oldfd(1);
        let result = {
            let mut wrapped =
                reverie::NativeOperationGuest::<_, Detcore>::new(self, operation.binding())
                    .unwrap();
            tool.handle_dup(&mut wrapped, call).await
        };
        assert_eq!(result.unwrap(), 9);
        assert_eq!(self.injected.last(), Some(&Sysno::dup));
    }

    async fn acquire_through_dup_handler(&mut self) {
        let syscall = call(Sysno::dup, [1, 0, 0, 0, 0, 0]);
        let operation = NativeCallOperation::begin(self, syscall).unwrap().unwrap();
        self.run_dup_handler(&operation).await;
        operation.returned().unwrap();
        operation.readmitted().unwrap();
        assert!(self.registry.0.lock().unwrap().is_empty());
    }

    async fn run_background_write_helper(&mut self) -> Result<i64, Error> {
        let tool = <Detcore as Tool>::new(Pid::from_raw(self.native_tid), &self.config);
        let bytes = *b"x";
        // FD 2 remains an ordinary description even when a distinct FD carries
        // process-memory authority. Thus the shared-table refusal below must
        // come from C/W_seen, not a scripted direct proc-memory write.
        let syscall = call(Sysno::write, [2, bytes.as_ptr() as usize, 1, 0, 0, 0]);
        let result = tool.record_or_replay_blocking(self, syscall).await;
        assert_eq!(self.bound_requests.lock().unwrap().len(), 1);
        assert_eq!(
            self.resource_requests
                .load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        assert!(self.registry.0.lock().unwrap().is_empty());
        result
    }
}

#[tokio::test]
async fn actual_dup_then_background_write_meets_capability_history() {
    let mut guest = Fixture::new(false);
    guest.process_memory_result = true;
    guest.acquire_through_dup_handler().await;
    assert!(
        guest.probe_staging(guest.task).is_ok(),
        "C alone is not C and W_seen"
    );
    guest.share_files_at_grant(5002);
    assert_eq!(guest.run_background_write_helper().await.unwrap(), 1);
    assert_eq!(guest.injected, [Sysno::dup, Sysno::write]);
    assert_eq!(
        guest.probe_staging(guest.task).unwrap_err().reason(),
        "forced-reserved-write"
    );
}

#[tokio::test]
async fn actual_completed_background_write_then_child_dup_keeps_history() {
    let mut parent = Fixture::new(true);
    assert_eq!(parent.run_background_write_helper().await.unwrap(), 1);
    assert!(
        parent.probe_staging(parent.task).is_ok(),
        "W_seen alone is not C and W_seen"
    );
    // A genuine copy-history observation is scripted under the authenticated
    // parent grant. W has fully readmitted before A is even registered.
    let mut child = parent.copy_files_at_grant(5002);
    child.control.invalidate_operation(parent.task);
    child
        .control
        .grant(child.task, InputGrantKind::Ordinary)
        .unwrap();
    child.process_memory_result = true;
    child.acquire_through_dup_handler().await;
    assert_eq!(parent.injected, [Sysno::write]);
    assert_eq!(child.injected, [Sysno::dup]);
    assert_eq!(
        child.probe_staging(child.task).unwrap_err().reason(),
        "forced-reserved-write"
    );
}

#[tokio::test]
async fn actual_handler_history_preserves_no_capability_private_and_notrun_neighbors() {
    for case in ["no-capability", "private-ordinary-write", "proved-not-run"] {
        let mut guest = Fixture::new(false);
        guest.process_memory_result = case != "no-capability";
        guest.acquire_through_dup_handler().await;
        if case != "private-ordinary-write" {
            guest.share_files_at_grant(5002);
        }
        guest.native_not_run = case == "proved-not-run";
        let result = guest.run_background_write_helper().await;
        if guest.native_not_run {
            assert!(matches!(result, Err(Error::Errno(Errno::EINTR))));
            assert_eq!(
                guest.injected,
                [Sysno::dup],
                "ProvedNotRun is not a native issue"
            );
        } else {
            assert_eq!(result.unwrap(), 1);
            assert_eq!(guest.injected, [Sysno::dup, Sysno::write]);
        }
        assert!(
            guest.probe_staging(guest.task).is_ok(),
            "nearby positive: {case}"
        );
    }
}

#[tokio::test]
async fn actual_dup_pending_copy_guards_only_destination_until_owner_readmission() {
    let mut parent = Fixture::new(false);
    parent.process_memory_result = true;
    let syscall = call(Sysno::dup, [1, 0, 0, 0, 0, 0]);
    let acquisition = NativeCallOperation::begin(&parent, syscall)
        .unwrap()
        .unwrap();
    // Script an authenticated nested birth before the acquisition's actual
    // issue, not a claim that a returned foreground handler yields by itself.
    let mut child = parent.copy_files_at_grant(5002);
    assert_eq!(
        child.probe_staging(child.task).unwrap_err().reason(),
        "native-descriptor-acquisition"
    );
    assert!(
        parent.probe_staging(parent.sibling).is_ok(),
        "unrelated earlier copy has no pending A guard"
    );
    parent.run_dup_handler(&acquisition).await;
    acquisition.returned().unwrap();
    // A complete native result alone is not deterministic readmission.
    assert_eq!(
        child.probe_staging(child.task).unwrap_err().reason(),
        "native-descriptor-acquisition"
    );
    acquisition.readmitted().unwrap();
    assert!(
        child.probe_staging(child.task).is_ok(),
        "exact owner readmission retires destination guard"
    );
    child.control.invalidate_operation(parent.task);
    child.share_files_at_grant(5003);
    assert_eq!(child.run_background_write_helper().await.unwrap(), 1);
    assert_eq!(
        child.probe_staging(child.task).unwrap_err().reason(),
        "forced-reserved-write",
        "captured pending A membership survives in the destination history"
    );
}

#[tokio::test]
async fn actual_background_helper_unknown_abi_preserves_native_result_but_withholds_staging() {
    for unknown in [false, true] {
        let mut guest = Fixture::new(false);
        if unknown {
            guest.native_abi = Some(NativeSyscallAbi::Unverified);
        }
        let tool = <Detcore as Tool>::new(Pid::from_raw(5000), &guest.config);
        let syscall = call(Sysno::ppoll, [0; 6]);
        assert_eq!(
            tool.record_or_replay_blocking(&mut guest, syscall)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            guest.injected,
            [Sysno::ppoll],
            "unknown ABI is not native-call refusal"
        );
        assert_eq!(
            guest.bound_requests.lock().unwrap()[0].abi,
            guest.native(syscall, 1).abi
        );
        assert_eq!(
            guest
                .resource_requests
                .load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        assert!(guest.registry.0.lock().unwrap().is_empty());
        let result = guest.probe_staging(guest.task);
        if unknown {
            assert_eq!(
                result.unwrap_err().reason(),
                "native-syscall-abi-unverified"
            );
        } else {
            assert!(result.is_ok(), "native ABI neighbor remains eligible");
        }
    }
}
