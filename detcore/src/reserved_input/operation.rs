/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * Licensed under the BSD-style license in the root LICENSE file.
 */

//! Call-local native evidence. Neither a Tool return nor a later FD census
//! supplies an installation receipt. Native callbacks retain background facts;
//! the existing logical continuation applies them before retiring its leases.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::Weak;

use reverie::Error;
use reverie::Guest;
use reverie::native_syscall::NativeOperation;
use reverie::native_syscall::NativeOperationCompletion;
use reverie::native_syscall::NativeOperationObserver;
use reverie::native_syscall::NativeSyscallCall;
use reverie::native_syscall::NativeSyscallEvent;
use reverie::native_syscall::NativeSyscallFacts;
use reverie::native_syscall::NativeSyscallObserver;
use reverie::native_syscall::NativeSyscallPurpose;
use reverie::reserved_input::InputAccess;
use reverie::reserved_input::InputAccessDirection;
use reverie::reserved_input::InputAccessLease;
use reverie::reserved_input::InputAccessLifetime;
use reverie::reserved_input::InputExposure;
use reverie::reserved_input::InputRunId;
use reverie::reserved_input::InputSpan;
use reverie::reserved_input::InputTarget;
use reverie::reserved_input::InputTaskId;
use reverie::reserved_input::ReservedInputControl;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::Sysno;

use super::acquisition::AcquisitionClass;
use super::persistent;
use crate::Detcore;
use crate::record_or_replay::RecordOrReplay;

#[derive(Debug, Default)]
struct State {
    attempts: Vec<Attempt>,
    acquisition: Option<InputAccessLease>,
    acquisition_history: Option<persistent::NativeReferenceOperation>,
    write_history: Option<persistent::NativeReferenceOperation>,
    accesses: Vec<InputAccessLease>,
    retired: bool,
}

#[derive(Debug)]
struct Attempt {
    call: NativeSyscallCall,
    class: Option<AcquisitionClass>,
    issued: bool,
    settled: bool,
    never_ran: bool,
    result: Option<Result<i64, reverie::Errno>>,
    // Captured while holding the original stop, without retaining native pins
    // across user continuation. Publication waits for logical authority.
    exposures: Vec<InputExposure>,
    result_exposures: Vec<InputExposure>,
}

/// Strong ownership belongs to this run's scheduler, not a task-global
/// next/last-call slot. A cancelled callback leaves its exact operation here
/// until the existing generation-bound terminal fence can settle it.
#[derive(Debug, Default)]
pub(crate) struct NativeOperations(Mutex<Vec<Arc<Owner>>>);

fn runs() -> &'static Mutex<HashMap<InputRunId, Weak<NativeOperations>>> {
    static RUNS: OnceLock<Mutex<HashMap<InputRunId, Weak<NativeOperations>>>> = OnceLock::new();
    RUNS.get_or_init(|| Mutex::new(HashMap::new()))
}

impl NativeOperations {
    pub(crate) fn install(control: &ReservedInputControl) -> Result<Arc<Self>, Error> {
        let registry = Arc::new(Self::default());
        let mut runs = runs().lock().unwrap();
        runs.retain(|_, run| run.strong_count() != 0);
        if runs
            .insert(control.run_identity(), Arc::downgrade(&registry))
            .is_some()
        {
            return Err(anyhow::anyhow!("native operation registry installed twice").into());
        }
        Ok(registry)
    }

    fn for_control(control: &ReservedInputControl) -> Result<Arc<Self>, Error> {
        runs()
            .lock()
            .unwrap()
            .get(&control.run_identity())
            .and_then(Weak::upgrade)
            .ok_or_else(|| anyhow::anyhow!("native operation registry was not installed").into())
    }

    fn retain(&self, owner: Arc<Owner>) {
        self.0.lock().unwrap().push(owner);
    }

    fn retire(&self, owner: &Owner) {
        self.0
            .lock()
            .unwrap()
            .retain(|retained| !std::ptr::eq(retained.as_ref(), owner));
    }

    /// Called after exact native effects observations and before generic access
    /// settlement at the scheduler's already-established exit/exec fence.
    pub(crate) fn terminal_fenced(&self, tasks: &[InputTaskId]) -> Result<(), Error> {
        let owners: Vec<_> = self
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|owner| tasks.contains(&owner.task))
            .cloned()
            .collect();
        // Validate the entire selected set before publishing or consuming any
        // history. A list of numeric TIDs or a dropped future is insufficient.
        for owner in &owners {
            owner.require_complete()?;
        }
        for owner in &owners {
            owner.terminal_fenced(tasks)?;
        }
        Ok(())
    }

    pub(crate) fn fence_settled(&self, tasks: &[InputTaskId]) {
        self.0
            .lock()
            .unwrap()
            .retain(|owner| !tasks.contains(&owner.task) || !owner.state.lock().unwrap().retired);
    }
}

#[derive(Debug)]
struct Owner {
    control: ReservedInputControl,
    task: InputTaskId,
    state: Arc<Mutex<State>>,
    completion: OnceLock<NativeOperationCompletion>,
}

/// The caller keeps this owner through native attempts, copyout and readmission.
/// The backend's physical cookies separately retain the same state if a future
/// is cancelled. There is deliberately no Drop-based lease retirement.
#[derive(Debug)]
pub(crate) struct NativeCallOperation {
    owner: Arc<Owner>,
    binding: NativeOperation,
}

impl NativeCallOperation {
    pub(crate) fn begin<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        guest: &G,
        call: Syscall,
    ) -> Result<Option<Self>, Error> {
        Self::begin_kind(guest, call, false)
    }

    pub(crate) fn begin_background<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        guest: &G,
        call: Syscall,
    ) -> Result<Option<Self>, Error> {
        Self::begin_kind(guest, call, true)
    }

    fn begin_kind<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        guest: &G,
        call: Syscall,
        background: bool,
    ) -> Result<Option<Self>, Error> {
        let Some((control, task)) = super::capability(guest) else {
            return Ok(None);
        };
        if control.current_grant(task).is_none() {
            return Err(anyhow::anyhow!(
                "native operation registration lacks its pre-release grant"
            )
            .into());
        }
        let owner = Arc::new(Owner {
            control,
            task,
            state: Arc::new(Mutex::new(State::default())),
            completion: OnceLock::new(),
        });
        let binding = NativeOperation::new(owner.clone());
        owner
            .completion
            .set(binding.completion())
            .expect("new operation completion");
        NativeOperations::for_control(&owner.control)?.retain(owner.clone());
        let operation = Self {
            owner: owner.clone(),
            binding,
        };
        if super::acquisition::classify(call).is_some_and(AcquisitionClass::needs_membership) {
            owner.ensure_acquisition_history()?;
            if !owner.control.files_proven_unshared(task) {
                owner.acquire_import()?;
            }
        }
        if background {
            let history =
                persistent::NativeReferenceOperation::background_write(&owner.control, task, call)?;
            if let Some(history) = &history {
                history.bind_operation(&operation.binding)?;
            }
            owner.state.lock().unwrap().write_history = history;
        }
        Ok(Some(operation))
    }

    pub(crate) fn binding(&self) -> NativeOperation {
        self.binding.clone()
    }

    pub(crate) fn close(&self) {
        self.binding.close();
    }

    pub(crate) fn bind_access(&self, lease: InputAccessLease) -> Result<(), Error> {
        self.owner
            .control
            .bind_access_operation(&lease, self.owner.task, &self.binding)
            .map_err(anyhow::Error::new)?;
        self.owner.state.lock().unwrap().accesses.push(lease);
        Ok(())
    }

    pub(crate) fn returned(&self) -> Result<(), Error> {
        self.close();
        self.owner.require_complete()
    }

    /// Call only after the original handler/copyout and its actual continuation
    /// grant. This is not called on an internal Tool error or cancelled future.
    pub(crate) fn readmitted(&self) -> Result<(), Error> {
        self.owner.require_complete()?;
        let grant = self
            .owner
            .control
            .current_grant(self.owner.task)
            .ok_or_else(|| anyhow::anyhow!("native operation lacks its logical readmission"))?;
        let mut state = self.owner.state.lock().unwrap();
        let exposures = take_exposures(&mut state);
        // Never invoke backend/control operations while holding our policy
        // lock: the native observer can independently retain another attempt.
        let acquisition_history = state.acquisition_history.take();
        let write_history = state.write_history.take();
        drop(state);
        if let Some(history) = acquisition_history {
            history.readmitted(&self.binding)?;
        }
        if let Some(history) = write_history {
            history.readmitted(&self.binding)?;
        }
        persistent::publish_native_exposures(&self.owner.control, exposures)?;
        let mut state = self.owner.state.lock().unwrap();
        let mut leases = std::mem::take(&mut state.accesses);
        leases.extend(state.acquisition.take());
        drop(state);
        for lease in leases {
            self.owner
                .control
                .complete_access_after_operation(&lease, self.owner.task, &self.binding)
                .map_err(anyhow::Error::new)?;
            self.owner
                .control
                .readmit_access(lease, self.owner.task, grant)
                .map_err(anyhow::Error::new)?;
        }
        self.owner.state.lock().unwrap().retired = true;
        NativeOperations::for_control(&self.owner.control)?.retire(&self.owner);
        Ok(())
    }
}

impl Drop for NativeCallOperation {
    fn drop(&mut self) {
        // Closing prevents additional physical calls. It does not retire any
        // access/history or infer that a cancelled kernel operation ended.
        self.binding.close();
    }
}

impl Owner {
    fn require_complete(&self) -> Result<(), Error> {
        if !self
            .completion
            .get()
            .expect("bound operation completion")
            .effects_complete()
        {
            return Err(
                anyhow::anyhow!("native operation effects are not closed and complete").into(),
            );
        }
        if self
            .state
            .lock()
            .unwrap()
            .attempts
            .iter()
            .any(|attempt| !attempt.settled)
        {
            return Err(
                anyhow::anyhow!("native operation has an unresolved physical attempt").into(),
            );
        }
        Ok(())
    }

    fn terminal_fenced(&self, tasks: &[InputTaskId]) -> Result<(), Error> {
        if !tasks.contains(&self.task) {
            return Err(anyhow::anyhow!("native operation lacks exact terminal fence").into());
        }
        self.require_complete()?;
        let mut state = self.state.lock().unwrap();
        if state.retired {
            return Ok(());
        }
        let exposures = take_exposures(&mut state);
        let histories = [state.acquisition_history.take(), state.write_history.take()];
        drop(state);
        let completion = self.completion.get().expect("bound operation completion");
        for history in histories.into_iter().flatten() {
            history.terminal_fenced(completion, tasks)?;
        }
        persistent::publish_native_exposures(&self.control, exposures)?;
        let state = self.state.lock().unwrap();
        for lease in state.accesses.iter().chain(state.acquisition.iter()) {
            self.control
                .complete_access_after_completion(lease, self.task, completion)
                .map_err(anyhow::Error::new)?;
        }
        drop(state);
        // Tokens remain retained until the caller's generic fence succeeds.
        self.state.lock().unwrap().retired = true;
        Ok(())
    }

    fn ensure_acquisition_history(&self) -> Result<(), Error> {
        if self.state.lock().unwrap().acquisition_history.is_none() {
            let history =
                persistent::NativeReferenceOperation::acquisition(&self.control, self.task)?;
            history.bind_completion(
                self.completion
                    .get()
                    .expect("operation completion bound before issue"),
            )?;
            self.state.lock().unwrap().acquisition_history = Some(history);
        }
        Ok(())
    }

    fn acquire_import(&self) -> Result<(), Error> {
        if self.state.lock().unwrap().acquisition.is_some() {
            return Ok(());
        }
        let lease = self
            .control
            .acquire_access(
                self.task,
                InputAccess {
                    target: InputTarget::Unknown,
                    span: InputSpan::Unknown,
                    direction: InputAccessDirection::ReadWrite,
                    lifetime: InputAccessLifetime::Transient,
                    cause: "unclassified-descriptor-acquisition".into(),
                },
            )
            .map_err(anyhow::Error::new)?;
        self.control
            .bind_access_completion(
                &lease,
                self.task,
                self.completion
                    .get()
                    .expect("operation completion bound before issue"),
            )
            .map_err(anyhow::Error::new)?;
        let previous = self.state.lock().unwrap().acquisition.replace(lease);
        if previous.is_some() {
            return Err(anyhow::anyhow!("native operation acquired an import lease twice").into());
        }
        Ok(())
    }
}

impl NativeOperationObserver for Owner {
    fn observe(
        &self,
        facts: &dyn NativeSyscallFacts,
        call: &NativeSyscallCall,
    ) -> Result<Option<Arc<dyn NativeSyscallObserver>>, Error> {
        if call.purpose == NativeSyscallPurpose::BackendPrivate {
            return Ok(None);
        }
        if !call.operation_bound {
            return Err(anyhow::anyhow!("native operation cookie was not explicitly bound").into());
        }
        let authority = facts.authority().map_err(anyhow::Error::new)?;
        if facts.task() != Some(self.task)
            || authority.registration.task != self.task
            || facts
                .control()
                .is_none_or(|control| control.run_identity() != self.control.run_identity())
        {
            return Err(
                anyhow::anyhow!("native operation received another task/image or run").into(),
            );
        }
        let class = decode(call).and_then(super::acquisition::classify);
        if class.is_some_and(AcquisitionClass::needs_membership)
            && !authority.files_proven_unshared
            && self.state.lock().unwrap().acquisition.is_none()
        {
            // A helper that actually creates a guest-visible FD must enter
            // this same ledger. After release an absent predeclared lease is
            // a protocol defect, not permission to acquire it in host order.
            if authority.grant.is_none() {
                return Err(
                    anyhow::anyhow!("background acquisition lacks its pre-release lease").into(),
                );
            }
            self.acquire_import()?;
        }
        if class.is_some_and(AcquisitionClass::needs_membership) {
            self.ensure_acquisition_history()?;
        }
        let (acquisition_history, write_history) = {
            let state = self.state.lock().unwrap();
            (
                state
                    .acquisition_history
                    .clone()
                    .filter(|_| class.is_some_and(AcquisitionClass::needs_membership)),
                state.write_history.clone(),
            )
        };
        let mut history_observers = Vec::new();
        for history in [&acquisition_history, &write_history].into_iter().flatten() {
            if let Some(observer) = history.observe_native(facts, call)? {
                history_observers.push(observer);
            }
        }
        let exposures = if authority.grant.is_none() {
            persistent::collect_native_exposures(facts, call)?
        } else {
            // The Tool's final observer publishes ordinary in-grant imports.
            Vec::new()
        };
        let mut state = self.state.lock().unwrap();
        if state
            .attempts
            .iter()
            .any(|attempt| attempt.call.id == call.id)
        {
            return Err(anyhow::anyhow!("native operation repeated a physical identity").into());
        }
        let index = state.attempts.len();
        state.attempts.push(Attempt {
            call: call.clone(),
            class,
            issued: false,
            settled: false,
            never_ran: false,
            result: None,
            exposures,
            result_exposures: Vec::new(),
        });
        Ok(Some(Arc::new(Physical {
            task: self.task,
            state: self.state.clone(),
            index,
            acquisition_history,
            history_observers,
        })))
    }
}

#[derive(Debug)]
struct Physical {
    task: InputTaskId,
    state: Arc<Mutex<State>>,
    index: usize,
    acquisition_history: Option<persistent::NativeReferenceOperation>,
    history_observers: Vec<Arc<dyn NativeSyscallObserver>>,
}

impl NativeSyscallObserver for Physical {
    fn observe(
        &self,
        call: &NativeSyscallCall,
        event: NativeSyscallEvent,
        facts: Option<&dyn NativeSyscallFacts>,
    ) -> Result<(), Error> {
        let mut state = self.state.lock().unwrap();
        let attempt = &mut state.attempts[self.index];
        if call.id != attempt.call.id
            || call.number != attempt.call.number
            || call.args != attempt.call.args
            || call.purpose != attempt.call.purpose
            || call.abi != attempt.call.abi
            || call.operation_bound != attempt.call.operation_bound
            || call.input_receipt != attempt.call.input_receipt
        {
            return Err(
                anyhow::anyhow!("native result does not belong to this physical attempt").into(),
            );
        }
        drop(state);
        // No subsidiary history may consume evidence before the full immutable
        // physical identity has been checked by its exact call-local owner.
        for observer in &self.history_observers {
            observer.observe(call, event, facts)?;
        }
        let mut state = self.state.lock().unwrap();
        let attempt = &mut state.attempts[self.index];
        match event {
            NativeSyscallEvent::ResumeIssued => {
                if attempt.issued || attempt.settled {
                    return Err(anyhow::anyhow!("native attempt issued more than once").into());
                }
                attempt.issued = true;
                return Ok(());
            }
            NativeSyscallEvent::EffectsComplete => {
                if !attempt.issued {
                    return Err(
                        anyhow::anyhow!("effects-ended without an issued native attempt").into(),
                    );
                }
                // Unknown result/membership remains unknown. This notification
                // supplies only the missing native lifetime endpoint.
                attempt.settled = true;
                return Ok(());
            }
            NativeSyscallEvent::ProvedNotRun => {
                attempt.never_ran = true;
                attempt.settled = true;
                attempt.exposures.clear();
                attempt.result_exposures.clear();
                return Ok(());
            }
            NativeSyscallEvent::NativeResult(_) | NativeSyscallEvent::ResultUnobserved => {
                if !attempt.issued || attempt.never_ran || attempt.result.is_some() {
                    return Err(anyhow::anyhow!(
                        "native outcome without its unique issued attempt"
                    )
                    .into());
                }
            }
        }
        let class = attempt.class;
        drop(state);
        let exposures = match event {
            NativeSyscallEvent::NativeResult(result) => membership(
                self.task,
                class,
                result,
                facts,
                self.acquisition_history.as_ref(),
                call,
            )?,
            NativeSyscallEvent::ResultUnobserved => Vec::new(),
            _ => unreachable!(),
        };
        // Pins expire at this result stop. Retain their facts until the
        // existing deterministic readmission/fence. ResultUnobserved is still
        // provisional: a later NotRun or actual result may resolve it.
        if let NativeSyscallEvent::NativeResult(result) = event {
            let mut state = self.state.lock().unwrap();
            let attempt = &mut state.attempts[self.index];
            attempt.result_exposures = exposures;
            attempt.result = Some(result);
            attempt.settled = true;
        }
        Ok(())
    }
}

fn unknown_membership() -> InputExposure {
    InputExposure {
        target: InputTarget::Unknown,
        cause: "native-description-proof-unavailable".into(),
    }
}

fn take_exposures(state: &mut State) -> Vec<InputExposure> {
    state
        .attempts
        .iter_mut()
        .flat_map(|attempt| {
            if attempt.never_ran || !attempt.issued {
                return Vec::new();
            }
            let mut exposures = std::mem::take(&mut attempt.exposures);
            exposures.append(&mut attempt.result_exposures);
            if attempt.result.is_none()
                && attempt
                    .class
                    .is_some_and(AcquisitionClass::needs_membership)
            {
                exposures.push(unknown_membership());
            }
            exposures
        })
        .collect()
}

fn membership(
    task: InputTaskId,
    class: Option<AcquisitionClass>,
    result: Result<i64, reverie::Errno>,
    facts: Option<&dyn NativeSyscallFacts>,
    history: Option<&persistent::NativeReferenceOperation>,
    call: &NativeSyscallCall,
) -> Result<Vec<InputExposure>, Error> {
    let Some(class) = class.filter(|class| class.needs_membership()) else {
        return Ok(Vec::new());
    };
    if class.native_error_proves_no_install(result) {
        return Ok(Vec::new());
    }
    if class == AcquisitionClass::ReturnedFd
        && let Ok(fd) = result
        && let Ok(fd) = i32::try_from(fd)
        && fd >= 0
        && let Some(facts) = facts
    {
        let authority = facts.authority().map_err(anyhow::Error::new)?;
        if facts.task() == Some(task)
            && authority.registration.task == task
            && authority.files_proven_unshared
            && let Ok(descriptor) = facts.descriptor(fd)
            && descriptor.fd == fd
        {
            // This is the exact scalar result slot at its owned native stop,
            // in a table no sibling could replace. The census supplies only
            // mountinfo; it is never used as the installed-membership list.
            let mounts = facts.descriptors().ok();
            let mountinfo = mounts
                .as_ref()
                .and_then(|snapshot| snapshot.mountinfo.as_deref().ok());
            if let Some(cause) = super::descriptor_cause(&descriptor, mountinfo) {
                return Ok(vec![InputExposure {
                    target: InputTarget::Unknown,
                    cause: cause.into(),
                }]);
            }
            if let Some(history) = history {
                if !history.captured_acquisition(
                    call.id,
                    std::slice::from_ref(&descriptor),
                    mountinfo,
                )? {
                    return Ok(vec![unknown_membership()]);
                }
            } else {
                return Err(
                    anyhow::anyhow!("native acquisition result lacks its history owner").into(),
                );
            }
            return Ok(Vec::new());
        }
    }
    Ok(vec![unknown_membership()])
}

fn decode(call: &NativeSyscallCall) -> Option<Syscall> {
    // The separate persistent collector retains the approved facility-only
    // cause. The actual syscall and its operands are left unchanged.
    if !persistent::native_syscall_abi_is_verified(call) {
        return None;
    }
    let number = Sysno::new(usize::try_from(call.number).ok()?)?;
    let [a, b, c, d, e, f] = call.args.map(|arg| usize::try_from(arg).ok());
    Some(Syscall::from_raw(
        number,
        SyscallArgs::new(a?, b?, c?, d?, e?, f?),
    ))
}

#[cfg(test)]
mod tests;
