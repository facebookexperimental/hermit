// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license found in the LICENSE file.

//! Terminal waitid completion for a backend that emulates child waits
//! (`BackendCapabilities::emulates_child_waits`; today KVM), under Detcore's
//! serial grant.
//!
//! Selection and physical-exit waits precede a private exact-child WNOWAIT
//! proof. From that proof through final injection and logical consumption,
//! nothing requests another scheduler turn. The backend owns both original output
//! pointers, including rusage-first copy faults and the six scalar info stores.

use super::*;

#[derive(Debug)]
struct WaitidFailure(String);

impl std::fmt::Display for WaitidFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "emulated waitid invariant failed: {}", self.0)
    }
}

impl std::error::Error for WaitidFailure {}

fn failure(message: impl Into<String>) -> Error {
    Error::Tool(anyhow::Error::new(WaitidFailure(message.into())))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Selector {
    Exact(DetPid),
    Any,
    Group(libc::pid_t),
}

fn classify(call: syscalls::Waitid) -> Result<Selector, Errno> {
    let options = call.options(); // Linux's int argument: upper register bits are ignored.
    let events = libc::WEXITED | libc::WSTOPPED | libc::WCONTINUED;
    let allowed =
        events | libc::WNOHANG | libc::WNOWAIT | libc::__WNOTHREAD | libc::__WALL | libc::__WCLONE;
    if options & events == 0 || options & !allowed != 0 {
        return Err(Errno::EINVAL);
    }
    // The supported terminal subset is narrower than Linux's valid mask.
    if options & (libc::WSTOPPED | libc::WCONTINUED | libc::__WALL | libc::__WCLONE) != 0 {
        return Err(Errno::EINVAL);
    }
    let selector = match call.which() {
        which if which == libc::P_PID as i32 && call.pid() > 0 => {
            Selector::Exact(DetPid::from_raw(call.pid()))
        }
        which if which == libc::P_ALL as i32 => Selector::Any,
        which if which == libc::P_PGID as i32 && call.pid() >= 0 => Selector::Group(call.pid()),
        which if which == libc::P_PIDFD as i32 && call.pid() >= 0 => {
            // No emulating backend has a pidfd wait implementation. Do not inspect a same-number
            // host fd through /proc. This is an unsupported form, not Linux
            // pidfd parity (including the O_NONBLOCK descriptor case).
            return Err(if options & libc::WNOHANG == 0 {
                Errno::EOPNOTSUPP
            } else {
                Errno::EINVAL
            });
        }
        _ => return Err(Errno::EINVAL),
    };
    // Preserve the backend's terminal SIGCHLD-only boundary. The scheduler
    // handles __WNOTHREAD; that bit must not reach the exact backend syscall.
    Ok(selector)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Decision {
    Ready(DetPid),
    Complete(Result<i64, Errno>),
    Wait,
}

fn decide(ready: Option<DetPid>, has_child: bool, nonblocking: bool, signaled: bool) -> Decision {
    if let Some(child) = ready {
        Decision::Ready(child)
    } else if !has_child {
        Decision::Complete(Err(Errno::ECHILD))
    } else if nonblocking {
        Decision::Complete(Ok(0))
    } else if signaled {
        Decision::Complete(Err(Errno::ERESTARTSYS))
    } else {
        Decision::Wait
    }
}

fn zero_completion(saved: Result<i64, Errno>, actual: Result<i64, Errno>) -> Result<i64, Error> {
    match actual {
        Err(Errno::EINVAL) => saved.map_err(Error::from),
        Err(Errno::EFAULT) => Err(Errno::EFAULT.into()),
        other => Err(failure(format!("zero-field completion returned {other:?}"))),
    }
}

async fn finish_without_event<G, T>(
    guest: &mut G,
    call: syscalls::Waitid,
    saved: Result<i64, Errno>,
) -> Result<i64, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    // Invalid options cannot select a child. The backend still performs Linux's
    // six ordered zero stores, including on ECHILD/EINVAL/ERESTARTSYS; a copy
    // fault overrides the saved result. NULL info and rusage are not accessed.
    let actual = guest.inject(call.with_options(0).with_rusage(None)).await;
    zero_completion(saved, actual)
}

fn checked_final_result(actual: Result<i64, Errno>) -> Result<Result<i64, Errno>, Error> {
    match actual {
        Ok(0) | Err(Errno::EFAULT) => Ok(actual),
        other => Err(failure(format!(
            "proven-child completion returned {other:?}"
        ))),
    }
}

async fn complete_child<G, T>(
    guest: &mut G,
    call: syscalls::Waitid,
    child: DetPid,
) -> Result<i64, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if call.options() & libc::WNOWAIT == 0
        && !guest
            .thread_state()
            .has_exited_child_process_cpu_time(child)
    {
        return Err(failure("proven child has no final CPU snapshot"));
    }
    let exact = call
        .with_which(libc::P_PID as i32)
        .with_pid(child.as_raw())
        .with_options(libc::WEXITED | (call.options() & (libc::WNOHANG | libc::WNOWAIT)));
    // No retry: both zero and EFAULT consumed the proven child unless WNOWAIT.
    // Do not read caller memory for the identity or return via `?` on EFAULT.
    let result = checked_final_result(guest.inject(exact).await)?;
    if call.options() & libc::WNOWAIT == 0 {
        retire_selected_child(guest, child, "waitid").await?;
    }
    result.map_err(Error::from)
}

async fn complete_proven_child<G, T>(
    guest: &mut G,
    call: syscalls::Waitid,
    child: DetPid,
) -> Result<i64, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    // The backend publishes waitability before the owner's exit hook inserts
    // final CPU. Wait only for our registered birth-owned publication; the
    // hook's synchronous prefix does not need another guest scheduler turn.
    if call.options() & libc::WNOWAIT == 0 {
        await_selected_child_cpu_publication(guest, child, "waitid").await?;
    }
    complete_child(guest, call, child).await
}

async fn wait_terminal<G, T>(
    guest: &mut G,
    call: syscalls::Waitid,
    spec: ChildWaitSpec,
    rsrc: Resources,
) -> Result<i64, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let nonblocking = call.options() & libc::WNOHANG != 0;
    let mut request_turn = true;
    let mut pending_signal = None;
    loop {
        if request_turn {
            let status = if nonblocking {
                resource_request(guest, rsrc.clone()).await
            } else {
                // Retain one ordinary WaitChild request for each blocking wait,
                // rather than adding a preliminary polling turn.
                crate::tool_global::child_wait_request(guest, spec).await
            };
            request_turn = false;
            if !nonblocking && matches!(status, ResumeStatus::Signaled(None)) {
                // The scheduler checked ready/no-child precedence before the
                // actual caught selection. Complete that interrupted attempt
                // before its handler; a later child event cannot undo it.
                return finish_without_event(guest, call, Err(Errno::ERESTARTSYS)).await;
            }
            if !nonblocking && pending_signal.is_none() {
                pending_signal = signal_after_wait(guest, status).await?;
            }
        }
        let (ready, has_child) = ready_child_wait(guest, spec).await;
        let child = match decide(ready, has_child, nonblocking, pending_signal.is_some()) {
            Decision::Complete(result) => return finish_without_event(guest, call, result).await,
            Decision::Wait => {
                request_turn = true;
                continue;
            }
            Decision::Ready(child) => child,
        };
        let _ = await_exact_child_physical_exit(guest, child).await;
        // Actual KVM does not request WaitPhysicalChild: its physical-exit
        // reporting flag is false. Keep this full-spec recheck defensive if
        // that helper gains a resource wait; no broad backend selector is used.
        let (current, _) = ready_child_wait(guest, spec).await;
        if current != Some(child) {
            continue;
        }
        // A live KvmStackGuard excludes parked signal observations, including
        // their nested Tool callbacks. Borrow scratch only after the wait has
        // completed, never while its resource request can observe a signal.
        let mut stack = guest.stack().await;
        let info = stack.reserve::<libc::siginfo_t>();
        let _guard = stack
            .commit()
            .map_err(|error| failure(format!("private stack: {error}")))?;
        let probe = call
            .with_which(libc::P_PID as i32)
            .with_pid(child.as_raw())
            .with_info(Some(info))
            .with_rusage(None)
            .with_options(libc::WEXITED | libc::WNOWAIT | libc::WNOHANG);
        match guest.inject(probe).await {
            Ok(0) => {
                let event: libc::siginfo_t = guest
                    .memory()
                    .read_value(info)
                    .map_err(|error| failure(format!("private event read: {error}")))?;
                // The private page is writable. No user pointer has been
                // touched, and no resource request occurs after this proof.
                if unsafe { event.si_pid() } != child.as_raw()
                    || event.si_signo != libc::SIGCHLD
                    || !waitid_code_is_termination(event.si_code)
                {
                    return Err(failure(
                        "private proof did not report the selected terminal child",
                    ));
                }
                return complete_proven_child(guest, call, child).await;
            }
            // Even ECHILD is impossible after logical revalidation under the
            // retained grant. It does not prove that a legacy physical reap
            // already performed logical/CPU accounting. Preserve the mismatch
            // as a run failure instead of dropping that unaccounted identity.
            other => return Err(failure(format!("private WNOWAIT proof returned {other:?}"))),
        }
    }
}

async fn signal_after_wait<G, T>(
    guest: &mut G,
    status: ResumeStatus,
) -> Result<Option<WaitSignalDisposition>, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    match status {
        ResumeStatus::InGuestFatalAlarm => Err(failure(
            "in-guest sleep response reached an emulated waitid",
        )),
        ResumeStatus::Normal => Ok(None),
        // The capable resource protocol has already selected a real caught
        // signal. Its returning gate protects copyout through frame delivery.
        ResumeStatus::Signaled(None) => Ok(Some(WaitSignalDisposition::Interrupt)),
        ResumeStatus::Signaled(Some(_)) => {
            // Legacy scheduler wakes may name blocked signals. Read, but do
            // not replace, the application mask. This scratch guard dies
            // before any subsequent child-wait resource request.
            let mut stack = guest.stack().await;
            let old_mask = stack.reserve::<KernelSigset>();
            let action = stack.reserve::<KernelSigaction>();
            let _guard = stack
                .commit()
                .map_err(|error| failure(format!("mask-query stack: {error}")))?;
            guest
                .inject(
                    syscalls::RtSigprocmask::new()
                        .with_how(libc::SIG_SETMASK)
                        .with_set(None)
                        .with_oldset(Some(old_mask.cast()))
                        .with_sigsetsize(KERNEL_SIGSET_SIZE),
                )
                .await
                .map_err(|error| failure(format!("mask query: {error}")))?;
            let mask: KernelSigset = guest
                .memory()
                .read_value(old_mask)
                .map_err(|error| failure(format!("mask-query read: {error}")))?;
            // A successful sibling send can name a blocked or ignored signal
            // without leaving an interrupting event. Inspect its real action;
            // both caught Restart and Interrupt still require zero copyout and
            // ERESTARTSYS, letting the backend's return boundary decide restart.
            wait_signal_disposition(guest, status, &mask, action, true)
                .await
                .map_err(|error| failure(format!("signal disposition: {error}")))
        }
    }
}

pub(super) async fn handle<G, T>(guest: &mut G, call: syscalls::Waitid) -> Result<i64, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let dettid = guest.thread_state().dettid;
    let mut rsrc = Resources::new(dettid);
    rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
    rsrc.fyi("waitid");
    let selector = match classify(call) {
        Ok(selector) => selector,
        Err(errno) => {
            resource_request(guest, rsrc).await;
            return finish_without_event(guest, call, Err(errno)).await;
        }
    };
    let selector = match selector {
        Selector::Exact(child) => ChildWaitSelector::Exact(child),
        Selector::Any => ChildWaitSelector::Any,
        Selector::Group(0) => {
            let parent = guest.thread_state().detpid.expect("detpid unset");
            let group = process_group(guest, parent)
                .await
                .ok_or_else(|| failure("calling process has no logical process group"))?;
            ChildWaitSelector::ProcessGroup(group)
        }
        Selector::Group(group) => ChildWaitSelector::ProcessGroup(DetPid::from_raw(group)),
    };
    let spec = terminal_child_wait_spec(selector, dettid, call.options());
    // The backend's exact callback boundary supplies disposition exclusion. Preserve
    // the application's mask so a process alarm can interrupt a parked wait.
    wait_terminal(guest, call, spec, rsrc).await
}

#[cfg(test)]
mod tests {
    use reverie::GlobalRPC;
    use reverie::GlobalTool;

    use super::*;
    use crate::config::Config;
    use crate::scheduler::Scheduler;
    use crate::tool_global::GlobalRequest;
    use crate::tool_global::GlobalResponse;
    use crate::tool_global::GlobalState;
    use crate::types::MmId;

    #[test]
    fn classifier_preserves_linux_ids_and_terminal_subset() {
        let call = syscalls::Waitid::new().with_options(libc::WEXITED);
        for id in [-1, 0] {
            assert_eq!(
                classify(call.with_which(libc::P_PID as i32).with_pid(id)),
                Err(Errno::EINVAL)
            );
        }
        assert_eq!(
            classify(call.with_which(libc::P_PGID as i32).with_pid(-1)),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            classify(call.with_which(libc::P_ALL as i32).with_pid(-1)),
            Ok(Selector::Any)
        );
        assert_eq!(
            classify(call.with_which(libc::P_PGID as i32).with_pid(0)),
            Ok(Selector::Group(0))
        );
        let exact = call.with_which(libc::P_PID as i32).with_pid(7);
        assert_eq!(
            classify(exact.with_options(libc::WEXITED | libc::__WNOTHREAD)),
            Ok(Selector::Exact(DetPid::from_raw(7)))
        );
        for bad in [
            0,
            libc::WNOHANG,
            libc::WEXITED | 0x100,
            libc::WSTOPPED,
            libc::WEXITED | libc::WCONTINUED,
            libc::WEXITED | libc::__WCLONE,
            libc::WEXITED | libc::__WALL,
        ] {
            assert_eq!(classify(exact.with_options(bad)), Err(Errno::EINVAL));
        }
        let pidfd = call.with_which(libc::P_PIDFD as i32).with_pid(7);
        assert_eq!(classify(pidfd), Err(Errno::EOPNOTSUPP));
        assert_eq!(classify(pidfd.with_pid(-1)), Err(Errno::EINVAL));
        assert_eq!(classify(pidfd.with_options(0)), Err(Errno::EINVAL));
        for unsupported in [libc::WSTOPPED, libc::WEXITED | libc::__WCLONE] {
            assert_eq!(
                classify(pidfd.with_options(unsupported)),
                Err(Errno::EINVAL)
            );
        }
        assert_eq!(
            classify(pidfd.with_options(libc::WEXITED | libc::WNOHANG)),
            Err(Errno::EINVAL)
        );
    }

    #[test]
    fn snapshot_ready_and_no_child_both_precede_interruption() {
        let child = DetPid::from_raw(7);
        for signaled in [false, true] {
            for nonblocking in [false, true] {
                assert_eq!(
                    decide(Some(child), true, nonblocking, signaled),
                    Decision::Ready(child)
                );
                assert_eq!(
                    decide(None, false, nonblocking, signaled),
                    Decision::Complete(Err(Errno::ECHILD))
                );
            }
        }
        assert_eq!(decide(None, true, true, true), Decision::Complete(Ok(0)));
        assert_eq!(
            decide(None, true, false, true),
            Decision::Complete(Err(Errno::ERESTARTSYS))
        );
        assert_eq!(decide(None, true, false, false), Decision::Wait);
    }

    #[test]
    fn zero_completion_preserves_saved_result_and_fault_precedence() {
        for saved in [
            Ok(0),
            Err(Errno::ECHILD),
            Err(Errno::EINVAL),
            Err(Errno::ERESTARTSYS),
            Err(Errno::EOPNOTSUPP),
        ] {
            assert_eq!(
                zero_completion(saved, Err(Errno::EINVAL)).map_err(|e| e.into_errno().unwrap()),
                saved
            );
            assert!(matches!(
                zero_completion(saved, Err(Errno::EFAULT)),
                Err(Error::Errno(Errno::EFAULT))
            ));
        }
        for unexpected in [
            Ok(0),
            Ok(7),
            Err(Errno::ECHILD),
            Err(Errno::EIO),
            Err(Errno::ERESTARTSYS),
        ] {
            assert!(matches!(
                zero_completion(Ok(0), unexpected),
                Err(Error::Tool(_))
            ));
        }
    }

    #[test]
    fn proven_completion_admits_only_zero_and_copy_fault() {
        for result in [Ok(0), Err(Errno::EFAULT)] {
            assert_eq!(checked_final_result(result).unwrap(), result);
        }
        for unexpected in [
            Ok(7),
            Err(Errno::ECHILD),
            Err(Errno::EINVAL),
            Err(Errno::EINTR),
            Err(Errno::ERESTARTSYS),
        ] {
            assert!(matches!(
                checked_final_result(unexpected),
                Err(Error::Tool(_))
            ));
        }
    }

    // A component transport, not a guest/runtime oracle: execute the production
    // completion with real ThreadState CPU accounting and Scheduler consumption.
    // Only the final backend syscall result is scripted. Any resource request,
    // memory access, restoration or unrelated RPC here fails the test.
    struct CompletionGuest {
        config: Config,
        thread: crate::ThreadState<()>,
        scheduler: Mutex<Scheduler>,
        result: Result<i64, Errno>,
        injected: Vec<syscalls::Waitid>,
        injected_wait4: Vec<syscalls::Wait4>,
        consumed: Mutex<Vec<DetPid>>,
        expect_rollup: bool,
    }

    struct UnusedStack;
    struct UnusedStackGuard;
    impl Drop for UnusedStackGuard {
        fn drop(&mut self) {}
    }
    impl Stack for UnusedStack {
        type StackGuard = UnusedStackGuard;
        fn size(&self) -> usize {
            panic!("unexpected stack")
        }
        fn capacity(&self) -> usize {
            panic!("unexpected stack")
        }
        fn push<'a, T>(&mut self, _: T) -> Addr<'a, T> {
            panic!("unexpected stack")
        }
        fn reserve<'a, T>(&mut self) -> AddrMut<'a, T> {
            panic!("unexpected stack")
        }
        fn commit(self) -> Result<Self::StackGuard, Errno> {
            panic!("unexpected stack")
        }
    }

    #[reverie::tool]
    impl GlobalRPC<GlobalState> for CompletionGuest {
        async fn send_rpc(
            &self,
            message: <GlobalState as GlobalTool>::Request,
        ) -> <GlobalState as GlobalTool>::Response {
            let GlobalRequest::ConsumeChildWait(parent, child) = message.2 else {
                panic!("unexpected RPC after private proof: {:?}", message.2);
            };
            assert_eq!(parent, DetPid::from_raw(3));
            assert_eq!(child, DetPid::from_raw(7));
            assert!(self.expect_rollup);
            assert!(
                !self.thread.has_exited_child_process_cpu_time(child),
                "CPU snapshot must be consumed before the logical RPC"
            );
            self.consumed.lock().unwrap().push(child);
            let consumed = self
                .scheduler
                .lock()
                .unwrap()
                .consume_child_wait(parent, child);
            (None, GlobalResponse::ConsumeChildWait(consumed))
        }
        fn config(&self) -> &Config {
            &self.config
        }
    }

    #[reverie::tool]
    impl Guest<Detcore> for CompletionGuest {
        type Memory = syscalls::LocalMemory;
        type Stack = UnusedStack;
        fn tid(&self) -> Pid {
            Pid::from_raw(3)
        }
        fn pid(&self) -> Pid {
            Pid::from_raw(3)
        }
        fn ppid(&self) -> Option<Pid> {
            None
        }
        fn memory(&self) -> Self::Memory {
            panic!("must not read caller outputs")
        }
        fn thread_state(&self) -> &crate::ThreadState<()> {
            &self.thread
        }
        fn thread_state_mut(&mut self) -> &mut crate::ThreadState<()> {
            &mut self.thread
        }
        async fn regs(&mut self) -> libc::user_regs_struct {
            panic!("unexpected registers")
        }
        async fn stack(&mut self) -> Self::Stack {
            panic!("unexpected stack")
        }
        async fn daemonize(&mut self) {
            panic!("unexpected daemonize")
        }
        async fn inject<S: SyscallInfo>(&mut self, call: S) -> Result<i64, Errno> {
            let (number, args) = call.into_parts();
            match Syscall::from_raw(number, args) {
                Syscall::Waitid(call) => self.injected.push(call),
                Syscall::Wait4(call) => self.injected_wait4.push(call),
                _ => panic!("expected final child wait only"),
            }
            self.result
        }
        async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> reverie::Never {
            panic!("must not tail-inject after proof")
        }
        fn set_timer(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
            panic!("unexpected timer")
        }
        fn set_timer_precise(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
            panic!("unexpected timer")
        }
        fn read_clock(&mut self) -> Result<u64, Error> {
            panic!("unexpected host clock")
        }
    }

    impl CompletionGuest {
        fn new(result: Result<i64, Errno>, nowait: bool) -> Self {
            let config = Config::default();
            let parent = DetPid::from_raw(3);
            let child = DetPid::from_raw(7);
            let mut thread = crate::ThreadState::new(parent, &config, ());
            thread.detpid = Some(parent);
            let mut exited = crate::ThreadState::new(child, &config, ());
            exited.parent_process_cpu_time = Some(Arc::clone(&thread.process_cpu_time));
            // Fix the component's CPU units independently of Config's optional
            // clock/RCB multipliers; this is a real saved 123us child snapshot.
            exited.thread_logical_time = crate::types::DetTime::zero();
            exited.thread_logical_time.add_syscall_with_cost(123_000);
            exited.record_exited_child_process_cpu_time(child);
            let mut scheduler = Scheduler::new(&config);
            scheduler.thread_tree.add_child(parent, parent, true);
            scheduler.thread_tree.add_child(parent, child, true);
            scheduler.logically_kill_thread(&child, &child, MmId::initial(child));
            Self {
                config,
                thread,
                scheduler: Mutex::new(scheduler),
                result,
                injected: Vec::new(),
                injected_wait4: Vec::new(),
                consumed: Mutex::new(Vec::new()),
                expect_rollup: !nowait,
            }
        }
    }

    #[derive(Default)]
    struct CpuPublicationWake(std::sync::atomic::AtomicUsize);
    impl std::task::Wake for CpuPublicationWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    // Use the actual child-state initializer, including its emulated-wait/serial gate,
    // rather than manufacturing a ready notification in the component.
    fn unpublished_child(guest: &mut CompletionGuest) -> crate::ThreadState<()> {
        // The two behaviours the old `backend_is_kvm` identity flag selected
        // that remain capabilities.
        guest.config.backend.provides_process_signal_control = true;
        guest.config.backend.emulates_child_waits = true;
        guest.config.sequentialize_threads = true;
        guest.thread.clone_flags = Some(syscalls::CloneFlags::empty());
        let tool = <Detcore as reverie::Tool>::new(Pid::from_raw(3), &guest.config);
        let mut child = <Detcore as reverie::Tool>::init_thread_state(
            &tool,
            Pid::from_raw(7),
            Some((Pid::from_raw(3), &guest.thread)),
        );
        child.thread_logical_time = crate::types::DetTime::zero();
        child.thread_logical_time.add_syscall_with_cost(123_000);
        child
    }

    #[tokio::test]
    async fn wait4_completion_publishes_then_consumes_exact_child_once() {
        use std::future::Future;
        use std::task::Context;

        use super::super::complete_selected_emulated_wait4;

        let child = DetPid::from_raw(7);
        for options in [
            0,
            libc::WNOHANG,
            libc::WUNTRACED,
            libc::WNOHANG | libc::WUNTRACED,
            libc::__WNOTHREAD,
            libc::WNOHANG | libc::__WNOTHREAD,
            libc::WUNTRACED | libc::__WNOTHREAD,
            libc::WNOHANG | libc::WUNTRACED | libc::__WNOTHREAD,
        ] {
            for result in [Ok(7), Err(Errno::EFAULT)] {
                for prepublished in [false, true] {
                    let mut guest = CompletionGuest::new(result, false);
                    let mut exited = unpublished_child(&mut guest);
                    let call = syscalls::Wait4::new()
                        .with_pid(-1)
                        .with_options(syscalls::WaitPidFlag::from_bits_retain(options));
                    if prepublished {
                        assert!(exited.record_exited_child_process_cpu_time(child));
                    }
                    let mut future =
                        Box::pin(complete_selected_emulated_wait4(&mut guest, call, child));
                    if !prepublished {
                        let wakes = Arc::new(CpuPublicationWake::default());
                        let waker = std::task::Waker::from(Arc::clone(&wakes));
                        let mut cx = Context::from_waker(&waker);
                        assert!(future.as_mut().poll(&mut cx).is_pending());
                        assert_eq!(wakes.0.load(std::sync::atomic::Ordering::SeqCst), 0);
                        assert!(exited.record_exited_child_process_cpu_time(child));
                        assert_eq!(wakes.0.load(std::sync::atomic::Ordering::SeqCst), 1);
                    }
                    let actual = future.await;
                    assert_eq!(actual.map_err(|e| e.into_errno().unwrap()), result);
                    assert!(guest.injected.is_empty());
                    assert_eq!(guest.injected_wait4.len(), 1);
                    assert_eq!(guest.injected_wait4[0].pid(), 7);
                    // Selection already enforced `__WNOTHREAD` through the
                    // owner filter. The backend's raw wait4 accepts only
                    // WNOHANG and WUNTRACED, so completion forwards exactly
                    // those caller bits and nothing else.
                    assert_eq!(
                        guest.injected_wait4[0].options().bits(),
                        options & (libc::WNOHANG | libc::WUNTRACED)
                    );
                    assert_eq!(*guest.consumed.lock().unwrap(), [child]);
                    assert_eq!(
                        guest.thread.process_cpu_time().children_system,
                        LogicalTime::from_nanos(123_000)
                    );
                    assert!(!guest.thread.has_exited_child_process_cpu_time(child));
                    assert!(matches!(
                        complete_selected_emulated_wait4(&mut guest, call, child).await,
                        Err(Error::Tool(_))
                    ));
                    assert_eq!(guest.injected_wait4.len(), 1);
                    assert_eq!(
                        guest.thread.process_cpu_time().children_system,
                        LogicalTime::from_nanos(123_000)
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn wait4_completion_refuses_impossible_selected_results_without_consumption() {
        use super::super::complete_selected_emulated_wait4;
        let parent = DetPid::from_raw(3);
        let child = DetPid::from_raw(7);
        let spec = terminal_child_wait_spec(ChildWaitSelector::Exact(child), parent, libc::WEXITED);
        let call = syscalls::Wait4::new().with_pid(-1);
        for result in [Ok(0), Err(Errno::EINVAL), Err(Errno::ECHILD), Ok(9)] {
            let mut guest = CompletionGuest::new(result, false);
            let mut exited = unpublished_child(&mut guest);
            assert!(exited.record_exited_child_process_cpu_time(child));
            let actual = complete_selected_emulated_wait4(&mut guest, call, child).await;
            assert!(matches!(actual, Err(Error::Tool(_))));
            assert_eq!(guest.injected_wait4.len(), 1);
            assert_eq!(guest.injected_wait4[0].pid(), 7);
            assert!(guest.consumed.lock().unwrap().is_empty());
            assert!(guest.thread.has_exited_child_process_cpu_time(child));
            assert_eq!(
                guest.thread.process_cpu_time().children_system,
                LogicalTime::ZERO
            );
            assert_eq!(
                guest
                    .scheduler
                    .lock()
                    .unwrap()
                    .ready_child_wait(parent, spec),
                Some(child)
            );
        }
    }

    #[tokio::test]
    async fn wait4_completion_unknown_publication_and_cancellation_cannot_inject() {
        use std::future::Future;
        use std::task::Context;

        use super::super::complete_selected_emulated_wait4;
        let parent = DetPid::from_raw(3);
        let child = DetPid::from_raw(7);
        let spec = terminal_child_wait_spec(ChildWaitSelector::Exact(child), parent, libc::WEXITED);
        let call = syscalls::Wait4::new().with_pid(7);
        let mut guest = CompletionGuest::new(Ok(7), false);
        assert!(matches!(
            complete_selected_emulated_wait4(&mut guest, call, child).await,
            Err(Error::Tool(_))
        ));
        assert!(guest.injected_wait4.is_empty());
        let mut exited = unpublished_child(&mut guest);
        let mut future = Box::pin(complete_selected_emulated_wait4(&mut guest, call, child));
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        assert!(future.as_mut().poll(&mut cx).is_pending());
        drop(future);
        assert!(guest.injected_wait4.is_empty());
        assert!(guest.consumed.lock().unwrap().is_empty());
        assert_eq!(
            guest.thread.process_cpu_time().children_system,
            LogicalTime::ZERO
        );
        assert_eq!(
            guest
                .scheduler
                .lock()
                .unwrap()
                .ready_child_wait(parent, spec),
            Some(child)
        );
        assert!(exited.record_exited_child_process_cpu_time(child));
        assert_eq!(
            complete_selected_emulated_wait4(&mut guest, call, child)
                .await
                .unwrap(),
            7
        );
        assert_eq!(guest.injected_wait4.len(), 1);
    }

    #[test]
    fn wait4_terminal_selection_retains_raw_option_validation() {
        use syscalls::WaitPidFlag;

        use super::super::wait4_uses_terminal_selector;
        // Serial KVM selects every terminal wait4 form Linux accepts from a
        // WNOHANG/WUNTRACED/__WNOTHREAD caller: the owner filter applies
        // `__WNOTHREAD`, and completion forwards only WNOHANG/WUNTRACED.
        for bits in [
            0,
            libc::WNOHANG,
            libc::WUNTRACED,
            libc::WNOHANG | libc::WUNTRACED,
            libc::__WNOTHREAD,
            libc::WNOHANG | libc::__WNOTHREAD,
            libc::WUNTRACED | libc::__WNOTHREAD,
            libc::WNOHANG | libc::WUNTRACED | libc::__WNOTHREAD,
        ] {
            assert!(wait4_uses_terminal_selector(
                WaitPidFlag::from_bits_retain(bits),
                true
            ));
        }
        // Every other bit, alone or carried beside `__WNOTHREAD`, stays on the
        // backend's prevalidation path and never selects a logical child.
        for bits in [
            libc::WCONTINUED,
            libc::__WCLONE,
            libc::__WALL,
            0x100,
            -1,
            libc::__WNOTHREAD | libc::WCONTINUED,
            libc::__WNOTHREAD | libc::__WCLONE,
            libc::__WNOTHREAD | libc::__WALL,
            libc::__WNOTHREAD | 0x100,
        ] {
            let options = WaitPidFlag::from_bits_retain(bits);
            assert_eq!(options.bits(), bits);
            assert!(!wait4_uses_terminal_selector(options, true));
        }
        for bits in [0, libc::WNOHANG, libc::__WNOTHREAD, 0x100] {
            assert!(wait4_uses_terminal_selector(
                WaitPidFlag::from_bits_retain(bits),
                false
            ));
        }
        for bits in [
            libc::WUNTRACED,
            libc::WCONTINUED,
            libc::__WCLONE,
            libc::__WALL,
        ] {
            assert!(!wait4_uses_terminal_selector(
                WaitPidFlag::from_bits_retain(bits),
                false
            ));
        }
    }

    #[tokio::test]
    async fn cpu_publication_waits_before_exact_consumption_without_a_new_turn() {
        use std::future::Future;
        use std::task::Context;

        let child = DetPid::from_raw(7);
        let call = syscalls::Waitid::new().with_options(libc::WEXITED);
        for result in [Ok(0), Err(Errno::EFAULT)] {
            for prepublished in [false, true] {
                let mut guest = CompletionGuest::new(result, false);
                let mut exited = unpublished_child(&mut guest);
                // External registration repeats legacy preparation before the
                // child's start hook. It must retain the birth token.
                guest.thread.prepare_child_process_cpu_time(child);
                if prepublished {
                    assert!(exited.record_exited_child_process_cpu_time(child));
                }
                let mut future = Box::pin(complete_proven_child(&mut guest, call, child));
                if !prepublished {
                    let wakes = Arc::new(CpuPublicationWake::default());
                    let waker = std::task::Waker::from(Arc::clone(&wakes));
                    let mut cx = Context::from_waker(&waker);
                    assert!(future.as_mut().poll(&mut cx).is_pending());
                    assert_eq!(wakes.0.load(std::sync::atomic::Ordering::SeqCst), 0);
                    assert!(exited.record_exited_child_process_cpu_time(child));
                    assert_eq!(wakes.0.load(std::sync::atomic::Ordering::SeqCst), 1);
                }
                let mut cx = Context::from_waker(futures::task::noop_waker_ref());
                let std::task::Poll::Ready(actual) = future.as_mut().poll(&mut cx) else {
                    panic!("published CPU must complete without another scheduling turn");
                };
                drop(future);
                assert_eq!(actual.map_err(|e| e.into_errno().unwrap()), result);
                assert_eq!(guest.injected.len(), 1);
                assert_eq!(*guest.consumed.lock().unwrap(), [child]);
                let cpu = guest.thread.process_cpu_time().children_system;
                assert_eq!(cpu, LogicalTime::from_nanos(123_000));
                assert!(!guest.thread.has_exited_child_process_cpu_time(child));
                assert!(matches!(
                    complete_proven_child(&mut guest, call, child).await,
                    Err(Error::Tool(_))
                ));
                assert_eq!(guest.injected.len(), 1);
                assert_eq!(guest.thread.process_cpu_time().children_system, cpu);
            }
        }
    }

    #[tokio::test]
    async fn cpu_publication_cancellation_and_unknown_owner_cannot_consume() {
        use std::future::Future;
        use std::task::Context;

        let parent = DetPid::from_raw(3);
        let child = DetPid::from_raw(7);
        let spec = terminal_child_wait_spec(ChildWaitSelector::Exact(child), parent, libc::WEXITED);
        let call = syscalls::Waitid::new().with_options(libc::WEXITED);
        let mut guest = CompletionGuest::new(Ok(0), false);
        // A snapshot by itself is not a birth-owned publication proof.
        assert!(guest.thread.has_exited_child_process_cpu_time(child));
        assert!(matches!(
            complete_proven_child(&mut guest, call, child).await,
            Err(Error::Tool(_))
        ));
        assert!(guest.injected.is_empty());
        let mut exited = unpublished_child(&mut guest);
        let mut future = Box::pin(complete_proven_child(&mut guest, call, child));
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        assert!(future.as_mut().poll(&mut cx).is_pending());
        // This models callback cancellation, not a guest syscall return. No
        // backend effect or accounting/selection mutation may precede readiness.
        drop(future);
        assert!(guest.injected.is_empty());
        assert!(guest.consumed.lock().unwrap().is_empty());
        assert_eq!(
            guest.thread.process_cpu_time().children_system,
            LogicalTime::ZERO
        );
        assert_eq!(
            guest
                .scheduler
                .lock()
                .unwrap()
                .ready_child_wait(parent, spec),
            Some(child)
        );
        assert!(exited.record_exited_child_process_cpu_time(child));
        assert_eq!(
            complete_proven_child(&mut guest, call, child)
                .await
                .unwrap(),
            0
        );
        assert_eq!(guest.injected.len(), 1);
        assert_eq!(*guest.consumed.lock().unwrap(), [child]);
    }

    #[tokio::test]
    async fn nowait_does_not_wait_for_pending_cpu_publication() {
        let parent = DetPid::from_raw(3);
        let child = DetPid::from_raw(7);
        let spec = terminal_child_wait_spec(ChildWaitSelector::Exact(child), parent, libc::WEXITED);
        for result in [Ok(0), Err(Errno::EFAULT)] {
            let mut guest = CompletionGuest::new(result, true);
            let _exited = unpublished_child(&mut guest);
            let call = syscalls::Waitid::new().with_options(libc::WEXITED | libc::WNOWAIT);
            assert_eq!(
                complete_proven_child(&mut guest, call, child)
                    .await
                    .map_err(|e| e.into_errno().unwrap()),
                result
            );
            assert_eq!(guest.injected.len(), 1);
            assert!(guest.consumed.lock().unwrap().is_empty());
            assert!(!guest.thread.has_exited_child_process_cpu_time(child));
            assert_eq!(
                guest.thread.process_cpu_time().children_system,
                LogicalTime::ZERO
            );
            assert_eq!(
                guest
                    .scheduler
                    .lock()
                    .unwrap()
                    .ready_child_wait(parent, spec),
                Some(child)
            );
        }
    }

    #[tokio::test]
    async fn real_cpu_and_logical_consumption_complete_once_on_success_and_efault() {
        let parent = DetPid::from_raw(3);
        let child = DetPid::from_raw(7);
        let spec = terminal_child_wait_spec(ChildWaitSelector::Exact(child), parent, libc::WEXITED);
        for result in [Ok(0), Err(Errno::EFAULT)] {
            for nowait in [false, true] {
                let mut guest = CompletionGuest::new(result, nowait);
                let info = AddrMut::from_raw(0x1234).unwrap();
                let usage = AddrMut::from_raw(0x5678).unwrap();
                let call = syscalls::Waitid::new()
                    .with_which(libc::P_ALL as i32)
                    .with_info(Some(info))
                    .with_rusage(Some(usage))
                    .with_options(
                        libc::WEXITED | libc::__WNOTHREAD | if nowait { libc::WNOWAIT } else { 0 },
                    );
                assert_eq!(
                    guest.thread.process_cpu_time().children_system,
                    LogicalTime::ZERO
                );
                let actual = complete_child(&mut guest, call, child).await;
                assert_eq!(actual.map_err(|e| e.into_errno().unwrap()), result);
                assert_eq!(guest.injected.len(), 1);
                let injected = guest.injected[0];
                assert_eq!((injected.which(), injected.pid()), (libc::P_PID as i32, 7));
                assert_eq!(
                    (injected.info(), injected.rusage()),
                    (Some(info), Some(usage))
                );
                assert_eq!(
                    injected.options(),
                    libc::WEXITED | if nowait { libc::WNOWAIT } else { 0 }
                );
                assert_eq!(
                    guest.thread.has_exited_child_process_cpu_time(child),
                    nowait
                );
                let ready = guest
                    .scheduler
                    .lock()
                    .unwrap()
                    .ready_child_wait(parent, spec);
                assert_eq!(ready, nowait.then_some(child));
                assert_eq!(guest.consumed.lock().unwrap().len(), usize::from(!nowait));
                let cpu = guest.thread.process_cpu_time().children_system;
                assert_eq!(
                    cpu,
                    if nowait {
                        LogicalTime::ZERO
                    } else {
                        LogicalTime::from_nanos(123_000)
                    }
                );
                if !nowait {
                    // A second completion cannot inject or roll CPU up again.
                    assert!(matches!(
                        complete_child(&mut guest, call, child).await,
                        Err(Error::Tool(_))
                    ));
                    assert_eq!(guest.injected.len(), 1);
                    assert_eq!(guest.thread.process_cpu_time().children_system, cpu);
                }
            }
        }
    }

    #[tokio::test]
    async fn impossible_logical_consume_is_a_tool_failure_after_cpu_rollup() {
        let child = DetPid::from_raw(7);
        let mut guest = CompletionGuest::new(Err(Errno::EFAULT), false);
        // Deliberately contradictory component state: inject reports an effect,
        // but the actual scheduler has no child to consume. This is a refusal
        // test, not a claim that this state is reachable under the serial grant.
        assert!(
            guest
                .scheduler
                .lock()
                .unwrap()
                .consume_child_wait(DetPid::from_raw(3), child)
        );
        let call = syscalls::Waitid::new().with_options(libc::WEXITED);
        assert!(matches!(
            complete_child(&mut guest, call, child).await,
            Err(Error::Tool(_))
        ));
        assert_eq!(
            guest.thread.process_cpu_time().children_system,
            LogicalTime::from_nanos(123_000)
        );
        assert_eq!(*guest.consumed.lock().unwrap(), [child]);
    }

    #[tokio::test]
    async fn unexpected_final_result_cannot_commit_cpu_or_logical_state() {
        let parent = DetPid::from_raw(3);
        let child = DetPid::from_raw(7);
        let spec = terminal_child_wait_spec(ChildWaitSelector::Exact(child), parent, libc::WEXITED);
        let call = syscalls::Waitid::new().with_options(libc::WEXITED);
        for actual in [Ok(7), Err(Errno::ECHILD), Err(Errno::ERESTARTSYS)] {
            let mut guest = CompletionGuest::new(actual, false);
            assert!(matches!(
                complete_child(&mut guest, call, child).await,
                Err(Error::Tool(_))
            ));
            assert_eq!(guest.injected.len(), 1);
            assert!(guest.thread.has_exited_child_process_cpu_time(child));
            assert_eq!(
                guest.thread.process_cpu_time().children_system,
                LogicalTime::ZERO
            );
            assert_eq!(
                guest
                    .scheduler
                    .lock()
                    .unwrap()
                    .ready_child_wait(parent, spec),
                Some(child)
            );
            assert!(guest.consumed.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn scheduler_requery_preserves_group_owner_and_stale_signal_precedence() {
        let config = Config::default();
        let mut scheduler = Scheduler::new(&config);
        let parent = DetPid::from_raw(3);
        let peer = DetTid::from_raw(5);
        let child = DetPid::from_raw(7);
        let sibling = DetPid::from_raw(9);
        scheduler.thread_tree.add_child(parent, parent, true);
        scheduler.thread_tree.add_child(parent, peer, false);
        scheduler.thread_tree.add_child(parent, child, true);
        scheduler.thread_tree.add_child(peer, sibling, true);
        for pid in [child, sibling] {
            scheduler.logically_kill_thread(&pid, &pid, MmId::initial(pid));
        }
        let spec = terminal_child_wait_spec(
            ChildWaitSelector::ProcessGroup(parent),
            parent,
            libc::WEXITED | libc::__WNOTHREAD,
        );
        assert_eq!(scheduler.ready_child_wait(parent, spec), Some(child));
        assert!(
            scheduler
                .thread_tree
                .set_process_group(child, DetPid::from_raw(99))
        );
        let ready = scheduler.ready_child_wait(parent, spec);
        let has_child = scheduler.has_child_wait_target(parent, spec);
        assert_eq!(
            decide(ready, has_child, false, true),
            Decision::Complete(Err(Errno::ECHILD))
        );
        assert!(scheduler.thread_tree.set_process_group(child, parent));
        assert!(scheduler.consume_child_wait(parent, child));
        assert_eq!(
            decide(
                scheduler.ready_child_wait(parent, spec),
                scheduler.has_child_wait_target(parent, spec),
                false,
                true
            ),
            Decision::Complete(Err(Errno::ECHILD))
        );
        let any = terminal_child_wait_spec(ChildWaitSelector::Any, parent, libc::WEXITED);
        assert_eq!(
            decide(
                scheduler.ready_child_wait(parent, any),
                scheduler.has_child_wait_target(parent, any),
                false,
                true
            ),
            Decision::Ready(sibling)
        );
    }
}
