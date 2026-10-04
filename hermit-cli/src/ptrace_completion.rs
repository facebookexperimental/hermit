/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Same-thread ownership of a complete ordinary-ptrace operation.
//!
//! A checkpoint retains the original runtime, future and every guard in that
//! future. It is not a completed cleanup. Deliberate retention on thread exit
//! leaves admission closed; dropping a diagnostic does not abandon the owner.

use std::any::Any;
use std::any::TypeId;
use std::cell::Cell;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::future::Future;
use std::future::poll_fn;
use std::mem::ManuallyDrop;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::thread::ThreadId;
use std::time::Duration;
use std::time::Instant;

use reverie_ptrace::PtraceCallbackDiagnostic;
use reverie_ptrace::PtraceRunFailure;
use reverie_ptrace::PtraceTerminationHandle;
use reverie_ptrace::ToolRunOutcome;
use serde::Deserialize;
use serde::Serialize;

use crate::Error;
use crate::FailureKind;
use crate::GuestTimedOut;

const ATTEMPT_BUDGET: Duration = Duration::from_secs(2);
static NEXT_KEY: AtomicU64 = AtomicU64::new(1);
// Admission and unresolved-owner publication use this SAME lock. Operations
// admitted before publication can finish; no later operation starts effects.
static UNRESOLVED: Mutex<usize> = Mutex::new(0);

type ErasedFuture = Pin<Box<dyn Future<Output = Result<Box<dyn Any>, Error>>>>;

// Keep the pinned operation as a field, rather than an async wrapper's local,
// when its poll unwinds. The poisoned entry is never polled again. This cannot
// undo destructors already run by arbitrary code inside the operation's panic.
struct EraseResult<F>(Pin<Box<F>>);
impl<T: 'static, F: Future<Output = Result<T, Error>>> Future for EraseResult<F> {
    type Output = Result<Box<dyn Any>, Error>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut()
            .0
            .as_mut()
            .poll(cx)
            .map(|result| result.map(|value| Box::new(value) as Box<dyn Any>))
    }
}

thread_local! {
    // TLS destruction must not drop a LocalSet or its backing directories.
    // This deliberately retains resources if their original thread exits.
    static OWNERS: RefCell<ManuallyDrop<BTreeMap<u64, Rc<Entry>>>> =
        const { RefCell::new(ManuallyDrop::new(BTreeMap::new())) };
}

/// The phase whose original owner remains retained.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum HermitCleanupStage {
    /// Spawn/startup has not yet returned a termination handle.
    Startup,
    /// The original ordinary-ptrace task, hooks or output drain are unfinished.
    PtraceCleanup,
    /// Physical ptrace completion returned failed global state; its natural
    /// scheduler join or partial-recording flush remains unfinished.
    GlobalStateCleanup,
    /// An untouched tracer refused the ordinary completion API.
    UnsupportedBackend,
    /// Polling panicked. The retained operation must never be polled again.
    Poisoned,
}

/// A completed failed ptrace run, with its original nonclone primary cause.
/// Secondary backend/cleanup errors never determine its failure classification.
pub struct HermitPtraceFailure {
    failure: PtraceRunFailure,
    cleanup: detcore::BackendFailureCleanup,
    callbacks: Vec<PtraceCallbackDiagnostic>,
    timeout_during_cleanup: bool,
}
impl HermitPtraceFailure {
    /// The original typed failure, origin, secondary errors and captured bytes.
    pub fn failure(&self) -> &PtraceRunFailure {
        &self.failure
    }
    /// Actual failed-global-state cleanup outcomes, separate from the primary.
    pub fn cleanup(&self) -> &detcore::BackendFailureCleanup {
        &self.cleanup
    }
    /// Raw callback observations preserved by the backend.
    pub fn callback_diagnostics(&self) -> &[PtraceCallbackDiagnostic] {
        &self.callbacks
    }
    /// Whether the caller's deadline also expired during failed cleanup.
    pub fn timeout_during_cleanup(&self) -> bool {
        self.timeout_during_cleanup
    }
    pub(crate) fn kind(&self) -> FailureKind {
        crate::error::classify_ptrace_primary(self.failure.primary())
    }
}
impl fmt::Debug for HermitPtraceFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HermitPtraceFailure")
            .field("failure", &self.failure)
            .field("scheduler", &self.cleanup.scheduler)
            .field("preemption_recording", &self.cleanup.preemption_recording)
            .field("callbacks", &self.callbacks)
            .field("timeout_during_cleanup", &self.timeout_during_cleanup)
            .finish()
    }
}
impl fmt::Display for HermitPtraceFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.failure.fmt(f)
    }
}
impl std::error::Error for HermitPtraceFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.failure.primary())
    }
}

#[derive(Clone, Debug)]
struct Snapshot {
    kind: FailureKind,
    primary: String,
    origin: Option<String>,
    stdout: Option<Vec<u8>>,
    stderr: Option<Vec<u8>>,
    callbacks: Vec<PtraceCallbackDiagnostic>,
}
impl Snapshot {
    fn startup() -> Self {
        Self {
            kind: FailureKind::Error,
            primary: "ordinary operation is not yet complete".into(),
            origin: None,
            stdout: None,
            stderr: None,
            callbacks: Vec::new(),
        }
    }
    fn failure(failure: &PtraceRunFailure, callbacks: Vec<PtraceCallbackDiagnostic>) -> Self {
        Self {
            kind: crate::error::classify_ptrace_primary(failure.primary()),
            primary: failure.primary().to_string(),
            origin: Some(format!("{:?}", failure.origin())),
            stdout: failure.captured_prefix().map(|p| p.stdout().to_vec()),
            stderr: failure.captured_prefix().map(|p| p.stderr().to_vec()),
            callbacks,
        }
    }
}

/// Sendable diagnostic identifying an operation retained on its original thread.
///
/// This owns no runtime or tracee capability. `resume::<T>()` must use the exact
/// successful return type of the original public call. Dropping this value does
/// not release the owner or reopen ordinary admission. There is no library alarm
/// or automatic process exit after this value is returned.
///
/// While any operation is retained, later ordinary-ptrace calls in this process
/// refuse admission. Already admitted calls can complete. A poisoned owner or an
/// owner whose original thread exited cannot recover; its resources are retained
/// and admission stays closed for the process lifetime. In-process callers must
/// explicitly supervise this condition rather than assume a failed call cleaned
/// up or that dropping this diagnostic released the resources.
#[derive(Clone, Debug)]
pub struct HermitCleanupUnconfirmed {
    key: u64,
    origin: OriginNumbers,
    stage: HermitCleanupStage,
    snapshot: Snapshot,
}
impl HermitCleanupUnconfirmed {
    /// Opaque recovery key; meaningful only with the original process/thread.
    pub fn recovery_key(&self) -> u64 {
        self.key
    }
    /// The unresolved phase, not a guest exit status.
    pub fn stage(&self) -> HermitCleanupStage {
        self.stage
    }
    /// Classification of the first received failure, never a secondary cause.
    pub fn primary_kind(&self) -> FailureKind {
        self.snapshot.kind
    }
    /// A diagnostic copy. The nonclone typed cause remains with the owner.
    pub fn primary_message(&self) -> &str {
        &self.snapshot.primary
    }
    /// The original backend callback/operation, when one has been received.
    pub fn origin(&self) -> Option<&str> {
        self.snapshot.origin.as_deref()
    }
    /// Captured stdout at this checkpoint; absent for noncapturing operations.
    pub fn stdout_prefix(&self) -> Option<&[u8]> {
        self.snapshot.stdout.as_deref()
    }
    /// Captured stderr at this checkpoint; absent for noncapturing operations.
    pub fn stderr_prefix(&self) -> Option<&[u8]> {
        self.snapshot.stderr.as_deref()
    }
    /// Backend callback observations received before this checkpoint.
    pub fn callback_diagnostics(&self) -> &[PtraceCallbackDiagnostic] {
        &self.snapshot.callbacks
    }
    /// Continue the SAME operation for one cooperative two-second attempt.
    ///
    /// A further checkpoint is returned as another typed
    /// `HermitCleanupUnconfirmed` error. Wrong identity/type, nested runtimes,
    /// reentry and poisoned owners refuse without polling or discarding it.
    /// Synchronous kernel calls and user code cannot be preempted by this budget.
    /// If the original deadline expired before startup produced a termination
    /// handle, recovery continues that same startup and can therefore begin its
    /// effects after the deadline. As soon as the handle becomes available the
    /// already-expired timeout is published; no normal guest completion is
    /// claimed from this recovery. Do not call resume expecting cancellation of
    /// an operation that has not yet established its owned termination handle.
    pub fn resume<T: 'static>(&self) -> Result<T, Error> {
        reject_nested_runtime()?;
        self.origin.check()?;
        let entry = OWNERS
            .try_with(|owners| owners.borrow().get(&self.key).cloned())
            .map_err(|_| RecoveryRefusal::StorageUnavailable)?
            .ok_or(RecoveryRefusal::UnknownKey)?;
        if entry.result_type != TypeId::of::<T>() {
            return Err(RecoveryRefusal::WrongResultType.into());
        }
        drive(entry, true)
    }
}
impl fmt::Display for HermitCleanupUnconfirmed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Hermit cleanup remains unconfirmed ({:?}, recovery {}): {}",
            self.stage, self.key, self.snapshot.primary
        )
    }
}
impl std::error::Error for HermitCleanupUnconfirmed {}

/// A refusal that leaves any existing operation owner untouched.
#[derive(Debug)]
pub enum RecoveryRefusal {
    /// Public synchronous entry cannot nest a Tokio runtime.
    NestedRuntime,
    /// Recovery must run on the original thread and process/PID namespace.
    WrongIdentity,
    /// Current process identity could not be established.
    Identity(std::io::Error),
    /// The ORIGINAL process pidfd is ready or returned unexpected poll flags.
    /// This refuses recovery; it is not proof of guest cleanup.
    OriginalProcessUnavailable { ready: i32, events: i16 },
    /// Thread-local storage is unavailable during thread destruction.
    StorageUnavailable,
    /// The key was already completed or never belonged to this thread.
    UnknownKey,
    /// Recovery used a different successful return type.
    WrongResultType,
    /// This operation is already being polled.
    Reentrant,
    /// A previous poll panicked; it must not be resumed.
    Poisoned,
    /// An unresolved owner prevents admitting another ordinary operation.
    AdmissionClosed,
    /// The admission mutex was poisoned; admission is conservatively refused.
    AdmissionPoisoned,
    /// The monotonic recovery key space is exhausted.
    KeyExhausted,
}
impl fmt::Display for RecoveryRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hermit owner recovery refused: {self:?}")
    }
}
impl std::error::Error for RecoveryRefusal {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OriginNumbers {
    thread: ThreadId,
    pid: u32,
    namespace_dev: u64,
    namespace_ino: u64,
}
impl OriginNumbers {
    fn current() -> Result<(Self, File), RecoveryRefusal> {
        let file = File::open("/proc/self/ns/pid").map_err(RecoveryRefusal::Identity)?;
        let meta = file.metadata().map_err(RecoveryRefusal::Identity)?;
        Ok((
            Self {
                thread: std::thread::current().id(),
                pid: std::process::id(),
                namespace_dev: meta.dev(),
                namespace_ino: meta.ino(),
            },
            file,
        ))
    }
    fn check(&self) -> Result<(), RecoveryRefusal> {
        let (current, _namespace) = Self::current()?;
        if *self == current {
            Ok(())
        } else {
            Err(RecoveryRefusal::WrongIdentity)
        }
    }
}

// A numeric PID, Rust ThreadId and namespace can all be copied across fork.
// After the origin dies, even its PID can be reused. This descriptor keeps the
// original kernel process identity; it is never reopened on recovery. Flags 0
// deliberately track the process, including a surviving nonleader owner thread.
struct OriginalProcess(OwnedFd);
impl OriginalProcess {
    fn capture(pid: u32) -> Result<Self, RecoveryRefusal> {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
        if fd < 0 {
            return Err(RecoveryRefusal::Identity(std::io::Error::last_os_error()));
        }
        Ok(Self(unsafe { OwnedFd::from_raw_fd(fd as i32) }))
    }
    fn check(&self) -> Result<(), RecoveryRefusal> {
        Self::check_fd(self.0.as_raw_fd())
    }
    fn check_fd(fd: i32) -> Result<(), RecoveryRefusal> {
        // poll deliberately ignores negative descriptors, so reject first.
        if fd < 0 {
            return Err(RecoveryRefusal::Identity(
                std::io::Error::from_raw_os_error(libc::EBADF),
            ));
        }
        let mut descriptor = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if ready < 0 {
            return Err(RecoveryRefusal::Identity(std::io::Error::last_os_error()));
        }
        if ready != 0 || descriptor.revents != 0 {
            return Err(RecoveryRefusal::OriginalProcessUnavailable {
                ready,
                events: descriptor.revents,
            });
        }
        Ok(())
    }
}

pub(crate) struct Control {
    termination: RefCell<Option<PtraceTerminationHandle>>,
    timeout: Option<(Instant, Duration)>,
    expired: Cell<bool>,
    received_failure: Cell<bool>,
    cancellation_deadline: Cell<Option<Instant>>,
    global_cleanup_deadline: Cell<Option<Instant>>,
    stage: Cell<HermitCleanupStage>,
    snapshot: RefCell<Snapshot>,
    checkpoint: Cell<bool>,
    epoch: Cell<u64>,
}
impl Control {
    fn new(timeout: Option<Duration>) -> Self {
        Self {
            termination: RefCell::new(None),
            timeout: timeout.map(|d| (Instant::now() + d, d)),
            expired: Cell::new(false),
            received_failure: Cell::new(false),
            cancellation_deadline: Cell::new(None),
            global_cleanup_deadline: Cell::new(None),
            stage: Cell::new(HermitCleanupStage::Startup),
            snapshot: RefCell::new(Snapshot::startup()),
            checkpoint: Cell::new(false),
            epoch: Cell::new(0),
        }
    }
    fn register(&self, handle: Option<PtraceTerminationHandle>) {
        *self.termination.borrow_mut() = handle;
        self.stage.set(HermitCleanupStage::PtraceCleanup);
        if self.expired.get() {
            self.publish_timeout();
        }
    }
    fn publish_timeout(&self) {
        if let (Some(handle), Some((_, limit))) = (self.termination.borrow().as_ref(), self.timeout)
        {
            // Reverie preserves an earlier Tool failure as primary.
            handle.terminate(reverie::Error::Tool(Error::new(GuestTimedOut { limit })));
        }
    }
    fn expire_if_due(&self) {
        if !self.expired.get() && self.timeout.is_some_and(|(at, _)| Instant::now() >= at) {
            self.expired.set(true);
            if !self.received_failure.get() {
                let mut snapshot = self.snapshot.borrow_mut();
                snapshot.kind = FailureKind::RunTimeout;
                snapshot.primary = GuestTimedOut {
                    limit: self.timeout.unwrap().1,
                }
                .to_string();
            }
            self.publish_timeout();
            self.cancellation_deadline
                .set(Some(Instant::now() + ATTEMPT_BUDGET));
        }
    }
    async fn yield_checkpoint(&self) {
        let epoch = self.epoch.get();
        self.checkpoint.set(true);
        poll_fn(|_| {
            if self.epoch.get() != epoch {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }
    fn global_failure(&self, failure: &PtraceRunFailure, callbacks: Vec<PtraceCallbackDiagnostic>) {
        self.termination.borrow_mut().take();
        self.received_failure.set(true);
        *self.snapshot.borrow_mut() = Snapshot::failure(failure, callbacks);
        self.stage.set(HermitCleanupStage::GlobalStateCleanup);
        self.global_cleanup_deadline
            .set(Some(Instant::now() + ATTEMPT_BUDGET));
    }
}

struct Driver {
    runtime: tokio::runtime::Runtime,
    future: ErasedFuture,
    control: Rc<Control>,
}
struct Entry {
    key: u64,
    origin: OriginNumbers,
    _namespace: File,
    original_process: OriginalProcess,
    result_type: TypeId,
    driver: RefCell<ManuallyDrop<Driver>>,
    in_flight: Cell<bool>,
    poisoned: Cell<bool>,
    quarantined: Cell<bool>,
}
impl Entry {
    fn check_origin(&self) -> Result<(), RecoveryRefusal> {
        self.origin.check()?;
        self.original_process.check()
    }
}

struct PollGuard<'a>(&'a Cell<bool>);
impl Drop for PollGuard<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}
fn reject_nested_runtime() -> Result<(), RecoveryRefusal> {
    if tokio::runtime::Handle::try_current().is_ok() {
        Err(RecoveryRefusal::NestedRuntime)
    } else {
        Ok(())
    }
}

/// Installs the whole operation before its first poll. `make` must only construct
/// a future; every ordinary effect and all owned guards belong inside that future.
pub(crate) fn run<T, F, Make>(timeout: Option<Duration>, make: Make) -> Result<T, Error>
where
    T: 'static,
    F: Future<Output = Result<T, Error>> + 'static,
    Make: FnOnce(Rc<Control>) -> F,
{
    reject_nested_runtime()?;
    let (origin, namespace) = OriginNumbers::current()?;
    let original_process = OriginalProcess::capture(origin.pid)?;
    let key = NEXT_KEY
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
        .map_err(|_| RecoveryRefusal::KeyExhausted)?;
    let control = Rc::new(Control::new(timeout));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let admission = UNRESOLVED
        .lock()
        .map_err(|_| RecoveryRefusal::AdmissionPoisoned)?;
    if *admission != 0 {
        return Err(RecoveryRefusal::AdmissionClosed.into());
    }
    // This is the admission linearization, before make or any first poll.
    drop(admission);
    let future = make(control.clone());
    let future: ErasedFuture = Box::pin(EraseResult(Box::pin(future)));
    let entry = Rc::new(Entry {
        key,
        origin,
        _namespace: namespace,
        original_process,
        result_type: TypeId::of::<T>(),
        driver: RefCell::new(ManuallyDrop::new(Driver {
            runtime,
            future,
            control,
        })),
        in_flight: Cell::new(false),
        poisoned: Cell::new(false),
        quarantined: Cell::new(false),
    });
    OWNERS
        .try_with(|owners| {
            owners.borrow_mut().insert(key, entry.clone());
        })
        .map_err(|_| RecoveryRefusal::StorageUnavailable)?;
    drive(entry, false)
}

fn drive<T: 'static>(entry: Rc<Entry>, recovery: bool) -> Result<T, Error> {
    entry.check_origin()?;
    if entry.poisoned.get() {
        return Err(RecoveryRefusal::Poisoned.into());
    }
    if entry.in_flight.replace(true) {
        return Err(RecoveryRefusal::Reentrant.into());
    }
    let _poll_guard = PollGuard(&entry.in_flight);
    let attempt_deadline = recovery.then(|| Instant::now() + ATTEMPT_BUDGET);
    let polled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut stored = entry.driver.borrow_mut();
        let Driver {
            runtime,
            future,
            control,
        } = &mut **stored;
        if recovery {
            control.checkpoint.set(false);
            control.epoch.set(control.epoch.get().wrapping_add(1));
        }
        runtime.block_on(async {
            let mut timer: Option<Pin<Box<tokio::time::Sleep>>> = None;
            poll_fn(|cx| {
                control.expire_if_due();
                if !recovery
                    && control.expired.get()
                    && control.stage.get() == HermitCleanupStage::Startup
                {
                    return Poll::Ready(None);
                }
                let polled = future.as_mut().poll(cx);
                // A synchronous poll can cross the absolute deadline. Observe
                // that fact on its return without claiming preemption.
                control.expire_if_due();
                if let Poll::Ready(result) = polled {
                    let result = match (result, control.timeout) {
                        (Ok(_), Some((_, limit))) if control.expired.get() => {
                            Err(Error::new(GuestTimedOut { limit }))
                        }
                        (result, _) => result,
                    };
                    return Poll::Ready(Some(result));
                }
                if control.checkpoint.get() {
                    return Poll::Ready(None);
                }
                let deadline = if recovery {
                    attempt_deadline
                } else {
                    [
                        control
                            .timeout
                            .filter(|_| !control.expired.get())
                            .map(|(at, _)| at),
                        control.cancellation_deadline.get(),
                        control.global_cleanup_deadline.get(),
                    ]
                    .into_iter()
                    .flatten()
                    .min()
                };
                if let Some(deadline) = deadline {
                    if Instant::now() >= deadline {
                        return Poll::Ready(None);
                    }
                    let at = tokio::time::Instant::from_std(deadline);
                    match timer.as_mut() {
                        Some(timer) => timer.as_mut().reset(at),
                        None => timer = Some(Box::pin(tokio::time::sleep_until(at))),
                    }
                    if timer.as_mut().unwrap().as_mut().poll(cx).is_ready() {
                        cx.waker().wake_by_ref();
                    }
                }
                Poll::Pending
            })
            .await
        })
    }));
    match polled {
        Ok(Some(result)) => {
            OWNERS.with(|owners| {
                owners.borrow_mut().remove(&entry.key);
            });
            if entry.quarantined.replace(false) {
                let mut unresolved = UNRESOLVED
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                *unresolved -= 1;
            }
            // Runtime teardown happens only after the whole operation returned,
            // outside block_on. No unresolved future is dropped here.
            let driver = unsafe { ManuallyDrop::take(&mut *entry.driver.borrow_mut()) };
            drop(driver);
            result?
                .downcast::<T>()
                .map(|value| *value)
                .map_err(|_| RecoveryRefusal::WrongResultType.into())
        }
        outcome => {
            if !entry.quarantined.replace(true) {
                let mut unresolved = UNRESOLVED
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                *unresolved = unresolved.saturating_add(1);
            }
            if outcome.is_err() {
                entry.poisoned.set(true);
            }
            let stored = entry.driver.borrow();
            let control = &stored.control;
            if entry.poisoned.get() {
                control.stage.set(HermitCleanupStage::Poisoned);
                if !control.received_failure.get() && !control.expired.get() {
                    let mut snapshot = control.snapshot.borrow_mut();
                    snapshot.kind = FailureKind::Panic;
                    snapshot.primary =
                        "ordinary operation panicked while polling; owner is poisoned".into();
                }
            }
            Err(Error::new(HermitCleanupUnconfirmed {
                key: entry.key,
                origin: entry.origin,
                stage: control.stage.get(),
                snapshot: control.snapshot.borrow().clone(),
            }))
        }
    }
}

pub(crate) async fn wait(
    tracer: reverie_ptrace::Tracer<detcore::GlobalState>,
    control: Rc<Control>,
) -> Result<(reverie::ExitStatus, detcore::GlobalState), Error> {
    control.register(tracer.termination_handle());
    consume(tracer.wait_completion().await, control).await
}
pub(crate) async fn wait_with_output(
    tracer: reverie_ptrace::Tracer<detcore::GlobalState>,
    control: Rc<Control>,
) -> Result<(reverie::process::Output, detcore::GlobalState), Error> {
    control.register(tracer.termination_handle());
    consume(tracer.wait_with_output_completion().await, control).await
}
async fn consume<R>(
    mut outcome: ToolRunOutcome<detcore::GlobalState, R>,
    control: Rc<Control>,
) -> Result<(R, detcore::GlobalState), Error> {
    loop {
        match outcome {
            ToolRunOutcome::Complete(completion) => {
                let callbacks = completion.callback_diagnostics().to_vec();
                control.termination.borrow_mut().take();
                return match completion.result {
                    Ok(result) => Ok((result, completion.global_state)),
                    Err(failure) => {
                        control.global_failure(&failure, callbacks.clone());
                        let cleanup = completion
                            .global_state
                            .clean_up_after_backend_failure()
                            .await;
                        Err(HermitPtraceFailure {
                            failure,
                            cleanup,
                            callbacks,
                            timeout_during_cleanup: control.expired.get(),
                        }
                        .into())
                    }
                };
            }
            ToolRunOutcome::CleanupPending(pending) => {
                control.received_failure.set(true);
                *control.snapshot.borrow_mut() =
                    Snapshot::failure(pending.failure(), pending.callback_diagnostics());
                control.stage.set(HermitCleanupStage::PtraceCleanup);
                control.yield_checkpoint().await;
                outcome = pending.resume_cleanup().await;
            }
            ToolRunOutcome::UnsupportedBackend(tracer) => {
                control.stage.set(HermitCleanupStage::UnsupportedBackend);
                control.snapshot.borrow_mut().primary =
                    "configured tracer does not support ordinary owned completion".into();
                // The untouched tracer remains in this async frame forever;
                // repeated recovery refuses at a checkpoint, never legacy wait.
                loop {
                    std::hint::black_box(&tracer);
                    control.yield_checkpoint().await;
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "ptrace_completion_tests.rs"]
mod tests;
