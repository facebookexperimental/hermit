/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Linux policy for the backend's owned, sealed syscall input page.
//!
//! These facts never enter ThreadState, Config, an RPC or a recording. The
//! backend owns native lifetimes and physical completion; Detcore classifies
//! the operations that can observe a page and supplies logical readmission.
//! A host completion alone is deliberately not a lease-retirement event.

mod acquisition;
mod objects;
mod operation;
pub(crate) use operation::NativeCallOperation;
pub(crate) use operation::NativeOperations;
mod persistent;
pub(crate) use objects::admit_initial;
pub(crate) use objects::descriptor_cause;
pub(crate) use persistent::before_descriptor_export;
pub(crate) use persistent::before_native_call;
pub(crate) use persistent::install_native_history;
pub(crate) use persistent::observe_initial_descriptors;
pub(crate) use persistent::observe_native_syscall;
pub(crate) use persistent::register_initial_history;
use reverie::Guest;
use reverie::reserved_input::InputCallInput;
use reverie::reserved_input::InputExpectedSharing;
use reverie::reserved_input::InputExposure;
use reverie::reserved_input::InputGrantKind;
use reverie::reserved_input::InputInjectionError;
use reverie::reserved_input::InputTarget;
use reverie::reserved_input::InputTaskId;
use reverie::reserved_input::PhysicalInjectionDescription;
use reverie::reserved_input::ReservedInputControl;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;

use crate::Detcore;
use crate::record_or_replay::RecordOrReplay;

pub(crate) fn capability<T: RecordOrReplay, G: Guest<Detcore<T>>>(
    guest: &G,
) -> Option<(ReservedInputControl, InputTaskId)> {
    if !matches!(
        guest.config().guest_ids,
        detcore_model::pid::GuestIds::Chosen { .. }
    ) {
        return None;
    }
    Some((
        guest.reserved_input_control()?,
        guest.reserved_input_task()?,
    ))
}

/// Invalidate before any existing scheduling request can release execution.
/// No clock, request, event, mask or wire field is introduced by this hook.
pub(crate) fn invalidate<T: RecordOrReplay, G: Guest<Detcore<T>>>(guest: &G) {
    if let Some((control, task)) = capability(guest) {
        control.invalidate_operation(task);
    }
}

/// Called only after the existing scheduler reply actually admitted the task.
pub(crate) fn granted<T: RecordOrReplay, G: Guest<Detcore<T>>>(guest: &G, kind: InputGrantKind) {
    if let Some((control, task)) = capability(guest) {
        control.grant(task, kind).unwrap_or_else(|error| {
            crate::scheduler::exit_on_scheduler_refusal(&format!(
                "HERMIT_CHOSEN_INPUT_AUTHORITY: {error}"
            ))
        });
    }
}

/// Publish an identified run-wide producer before its native/model effect.
/// This changes only future staging availability, never the guest operation.
pub(crate) fn expose_global<T: RecordOrReplay, G: Guest<Detcore<T>>>(
    guest: &G,
    cause: &'static str,
) -> Result<(), reverie::Error> {
    if let Some((control, _)) = capability(guest) {
        control
            .add_exposure(InputExposure {
                target: InputTarget::Unknown,
                cause: cause.into(),
            })
            .map_err(anyhow::Error::new)?;
    }
    Ok(())
}

pub(crate) fn expose_local<T: RecordOrReplay, G: Guest<Detcore<T>>>(
    guest: &G,
    cause: &'static str,
) -> Result<(), reverie::Error> {
    if let Some((control, task)) = capability(guest) {
        let registration = control
            .registration(task)
            .ok_or_else(|| anyhow::anyhow!("reserved-input exposure owner is not registered"))?;
        control
            .add_exposure(InputExposure {
                target: InputTarget::Region(registration.region),
                cause: cause.into(),
            })
            .map_err(anyhow::Error::new)?;
    }
    Ok(())
}

/// A known direct registration does not poison ordinary disjoint references.
/// Unknown extent and native address aliases cannot certify disjointness.
pub(crate) fn expose_local_overlap<T: RecordOrReplay, G: Guest<Detcore<T>>>(
    guest: &G,
    address: u64,
    length: u64,
    cause: &'static str,
) -> Result<(), reverie::Error> {
    let Some((control, task)) = capability(guest) else {
        return Ok(());
    };
    let overlaps = control.layout(task).is_none_or(|layout| {
        let reserved = Span::bytes(layout.base, layout.page_bytes as u64);
        effective_spans(address, length)
            .into_iter()
            .any(|span| span.overlaps(reserved))
    });
    if overlaps {
        expose_local(guest, cause)?;
    }
    Ok(())
}

/// Freeze the actual group before an await can publish its retirement. The
/// registry owns the intent, including late retained members, after return.
pub(crate) fn freeze_group<T: RecordOrReplay, G: Guest<Detcore<T>>>(
    guest: &G,
) -> Result<(), reverie::Error> {
    let Some((control, task)) = capability(guest) else {
        return Ok(());
    };
    let cohort = control
        .freeze_group(task, false)
        .map_err(anyhow::Error::new)?;
    if control.group_proven_singleton(task)
        && control
            .cohort_members(&cohort)
            .map_err(anyhow::Error::new)?
            .is_empty()
    {
        // There was never another group owner/possible thread creation. No
        // yield or native effect intervenes in this empty-intent cancellation.
        control
            .cancel_freeze_before_effect(cohort)
            .map_err(anyhow::Error::new)?;
    }
    Ok(())
}

pub(crate) fn before_exit_group<T: RecordOrReplay, G: Guest<Detcore<T>>>(
    guest: &G,
) -> Result<(), reverie::Error> {
    let Some((control, task)) = capability(guest) else {
        return Ok(());
    };
    let registration = control
        .registration(task)
        .ok_or_else(|| anyhow::anyhow!("reserved-input exit owner is not registered"))?;
    if registration.native_tid != registration.native_pid
        && !control.region_proven_single_process(task)
    {
        expose_local(guest, "nonleader-group-exit-reference")?;
    }
    freeze_group(guest)
}

/// Called at the actual native send, after existing no-effect/refusal routing.
/// The boolean is supplied only by that route's existing completion authority
/// or affirmative no-effect proof. An attempted reservation is not authority.
pub(crate) fn before_signal_send<T: RecordOrReplay, G: Guest<Detcore<T>>>(
    guest: &G,
    signal: i32,
    terminal_covered: bool,
) -> Result<(), reverie::Error> {
    if signal == libc::SIGKILL && !terminal_covered {
        expose_global(guest, "unreserved-terminal-send")?;
    }
    Ok(())
}

pub(crate) fn after_fatal_commit<T: RecordOrReplay, G: Guest<Detcore<T>>>(
    guest: &G,
    observation: crate::syscalls::FatalDeliveryObservation,
    retired_others: bool,
) -> Result<(), reverie::Error> {
    use crate::syscalls::FatalDeliveryObservation;
    let Some((control, task)) = capability(guest) else {
        return Ok(());
    };
    if retired_others || observation == FatalDeliveryObservation::Nonfatal {
        return Ok(());
    }
    match observation {
        FatalDeliveryObservation::Fatal { .. } if control.group_proven_singleton(task) => Ok(()),
        FatalDeliveryObservation::Fatal { .. } => expose_local(guest, "terminal-delivery-unproved"),
        FatalDeliveryObservation::Unknown => expose_local(guest, "terminal-disposition-unproved"),
        FatalDeliveryObservation::Nonfatal => unreachable!(),
    }
}

/// The registry retains these obligations even if the callback future drops.
/// Only a returned native operation followed by genuine logical readmission
/// can retire a transient lease; host completion timing is not a decision.
pub(crate) struct BackgroundAccess {
    operation: NativeCallOperation,
    request: reverie::native_syscall::NativeOperation,
}

impl BackgroundAccess {
    pub(crate) fn acquire<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        guest: &G,
        call: Syscall,
    ) -> Result<Option<Self>, reverie::Error> {
        use reverie::reserved_input::InputAccess;
        use reverie::reserved_input::InputAccessDirection;
        use reverie::reserved_input::InputAccessLifetime;
        use reverie::reserved_input::InputSpan;
        let Some((control, task)) = capability(guest) else {
            return Ok(None);
        };
        let registration = control
            .registration(task)
            .ok_or_else(|| anyhow::anyhow!("reserved-input background owner is not registered"))?;
        let operation = NativeCallOperation::begin_background(guest, call)?
            .expect("the same registered Chosen capability was just checked");
        // Bind the actual already-substituted physical request while this
        // task still holds its grant. Final native preparation must preserve
        // these operands/ABI; it may not silently move beyond these leases.
        let request = guest.bind_native_request(&operation.binding(), call)?;
        let intended = request.bound_request().ok_or_else(|| {
            anyhow::anyhow!("background native request has no pre-release binding")
        })?;
        let mut accesses = background_accesses(call);
        if !persistent::native_abi_is_verified(intended.abi, intended.number)
            || persistent::background_descriptor_effect_is_unverified(guest, call)
        {
            // A pinned description observed now does not bind the later
            // native fdget after this task releases its turn. The extra
            // lease covers a possible late import/dup2 destination as well
            // as a proc-memory source; it retires with this operation, not
            // with the separate acquisition that installed that descriptor.
            accesses.extend(unknown(Target::Unknown, Direction::ReadWrite));
        }
        for access in accesses {
            let declaration = InputAccess {
                target: match access.target {
                    Target::Local => InputTarget::Region(registration.region),
                    Target::Unknown => InputTarget::Unknown,
                },
                span: match access.span {
                    Span::Range { start, end } => InputSpan::Range {
                        address: start,
                        length: end - start,
                    },
                    Span::Unknown => InputSpan::Unknown,
                },
                direction: match access.direction {
                    Direction::Read => InputAccessDirection::Read,
                    Direction::Write => InputAccessDirection::Write,
                    Direction::ReadWrite => InputAccessDirection::ReadWrite,
                },
                lifetime: InputAccessLifetime::Transient,
                cause: format!("background-{}-input", call.name()).into(),
            };
            let lease = control
                .acquire_access(task, declaration)
                .map_err(anyhow::Error::new)?;
            operation.bind_access(lease)?;
        }
        Ok(Some(Self { operation, request }))
    }

    pub(crate) fn native_operation(&self) -> reverie::native_syscall::NativeOperation {
        self.request.clone()
    }

    pub(crate) fn returned(&self) -> Result<(), reverie::Error> {
        self.operation.returned()
    }

    pub(crate) fn readmitted(self) -> Result<(), reverie::Error> {
        // Publish actual native facts at the existing continuation, while all
        // buffer/foreign-destination exclusions are still retained.
        self.operation.readmitted()
    }
}

/// Register every physical original/fallback clone before constructing its
/// native future. Original pointer input remains original pointer input: a
/// read made for planning cannot certify the kernel's later mutable read.
pub(crate) async fn inject_original_clone<T: RecordOrReplay, G: Guest<Detcore<T>>>(
    guest: &mut G,
    call: Syscall,
) -> Result<i64, reverie::syscalls::Errno> {
    let Some((control, task)) = capability(guest) else {
        // A backend without the facility never stages private input. Ordinary
        // non-Chosen execution and its injection/event behavior stay intact.
        return guest.inject(call).await;
    };
    if call.name() == "clone" {
        let (_, args) = call.into_parts();
        if args.arg0 & libc::CLONE_CHILD_CLEARTID as usize != 0 {
            // The actual legacy register argument is immutable at issue.
            // A retained copied child inherits this region's exposures;
            // shared-memory children retain the same region. A failed
            // registration does not clear a pre-issuance monotone fact.
            #[cfg(target_arch = "x86_64")]
            let child_tid = args.arg3;
            #[cfg(target_arch = "aarch64")]
            let child_tid = args.arg4;
            expose_local_overlap(guest, child_tid as u64, 4, "clear-tid-reserved-reference")
                .unwrap_or_else(|error| {
                    panic!("reserved-input clone reference publication: {error}")
                });
        }
    }
    let operation = guest.thread_state().stats.syscall_count;
    let description =
        clone_description(call, None, guest.reserved_input_process_exit_signal().ok());
    let receipt = control
        .prepare_observed_injection(task, operation, description)
        .unwrap_or_else(|error| {
            crate::scheduler::exit_on_scheduler_refusal(&format!(
                "HERMIT_CHOSEN_INPUT_AUTHORITY: {error}"
            ))
        });
    let receipt_id = receipt.id();
    match guest.inject_observed(call, receipt).await {
        Ok(native) => {
            if let Some(grant) = control.current_grant(task) {
                control
                    .readmit_injection(receipt_id, task, grant)
                    .unwrap_or_else(|error| {
                        crate::scheduler::exit_on_scheduler_refusal(&format!(
                            "HERMIT_CHOSEN_INPUT_OWNERSHIP: {error}"
                        ))
                    });
            }
            native
        }
        Err(InputInjectionError::Unavailable(error)) => {
            // This was already the original route. Never turn a mechanism
            // refusal into a fake kernel errno or retry without a receipt.
            crate::scheduler::exit_on_scheduler_refusal(&format!(
                "HERMIT_CHOSEN_INPUT_UNAVAILABLE: {error}"
            ))
        }
        Err(InputInjectionError::Fatal(error)) => {
            panic!("reserved-input native ownership failure: {error}")
        }
    }
}

pub(crate) fn clone_description(
    call: Syscall,
    immutable: Option<(u64, &[u8])>,
    inherited_exit_signal: Option<i32>,
) -> PhysicalInjectionDescription {
    let (number, args) = call.into_parts();
    let mut prospective = Vec::new();
    let flags_and_signal = match call.name() {
        "clone" => Some((args.arg0 as u32 as u64 & !0xff, args.arg0 as u64 & 0xff)),
        "fork" => Some((0, libc::SIGCHLD as u64)),
        "vfork" => Some((
            (libc::CLONE_VM | libc::CLONE_VFORK) as u64,
            libc::SIGCHLD as u64,
        )),
        "clone3" => immutable.and_then(|(_, bytes)| {
            (bytes.len() >= 40).then(|| {
                (
                    u64::from_ne_bytes(bytes[..8].try_into().unwrap()),
                    u64::from_ne_bytes(bytes[32..40].try_into().unwrap()),
                )
            })
        }),
        _ => unreachable!("physical clone receipt attached to a non-clone syscall"),
    };
    let expected_sharing = if let Some((flags, signal)) = flags_and_signal {
        if flags & libc::CLONE_NEWPID as u64 != 0 {
            prospective.push(InputExposure {
                target: InputTarget::Unknown,
                cause: "namespace-init-zap".into(),
            });
        }
        if flags & libc::CLONE_THREAD as u64 == 0 {
            // Linux copy_process takes CLONE_PARENT's effective notification
            // from the *native group leader*. Neither clone3's normalized
            // zero nor the requested legacy low byte proves that value.
            let effective = if flags & libc::CLONE_PARENT as u64 != 0 {
                inherited_exit_signal
            } else {
                i32::try_from(signal).ok()
            };
            let cause = match effective {
                Some(libc::SIGKILL) => Some("child-exit-sigkill"),
                Some(_) => None,
                None => Some("clone-lifecycle-unverified"),
            };
            if let Some(cause) = cause {
                prospective.push(InputExposure {
                    target: InputTarget::Unknown,
                    cause: cause.into(),
                });
            }
        }
        InputExpectedSharing {
            may_create: true,
            thread_group: Some(flags & libc::CLONE_THREAD as u64 != 0),
            memory: Some(flags & libc::CLONE_VM as u64 != 0),
            files: Some(flags & libc::CLONE_FILES as u64 != 0),
        }
    } else {
        // A retained child authenticates birth and physical sharing, not the
        // lifecycle bytes imported from an original clone3 pointer. Commit
        // this Tool-owned cause at birth, before its transient access can be
        // readmitted. A proved no-child result cancels prospective facts.
        prospective.push(InputExposure {
            target: InputTarget::Unknown,
            cause: "clone-lifecycle-unverified".into(),
        });
        InputExpectedSharing {
            may_create: true,
            ..InputExpectedSharing::default()
        }
    };
    PhysicalInjectionDescription {
        number: number as u64,
        args: [
            args.arg0 as u64,
            args.arg1 as u64,
            args.arg2 as u64,
            args.arg3 as u64,
            args.arg4 as u64,
            args.arg5 as u64,
        ],
        input: match immutable {
            Some((address, bytes)) => InputCallInput::Immutable {
                address,
                bytes: bytes.into(),
            },
            None if call.name() == "clone3" => InputCallInput::Unknown,
            None => InputCallInput::Registers,
        },
        expected_sharing,
        transient: if immutable.is_none() && call.name() == "clone3" {
            vec![reverie::reserved_input::InputAccess {
                target: InputTarget::Unknown,
                span: reverie::reserved_input::InputSpan::Unknown,
                direction: reverie::reserved_input::InputAccessDirection::Read,
                lifetime: reverie::reserved_input::InputAccessLifetime::Transient,
                cause: "clone-input-unverified".into(),
            }]
        } else {
            Vec::new()
        },
        prospective,
        unknown_lifecycle: Some(InputExposure {
            target: InputTarget::Unknown,
            cause: "clone-lifecycle-unverified".into(),
        }),
    }
}

/// A checked half-open native-memory interval. Unknown is not an empty range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Span {
    Range { start: u64, end: u64 },
    Unknown,
}

impl Span {
    pub(crate) fn bytes(start: u64, bytes: u64) -> Self {
        match start.checked_add(bytes) {
            Some(end) => Self::Range { start, end },
            None => Self::Unknown,
        }
    }

    pub(crate) fn overlaps(self, other: Self) -> bool {
        match (self, other) {
            (Self::Range { start: a, end: b }, Self::Range { start: c, end: d }) => {
                a < b && c < d && a < d && c < b
            }
            _ => true,
        }
    }
}

/// Input normalization alternatives that the admitted native ABI can use.
/// Keeping every possible interpretation may refuse an alias; dropping one
/// would allow a tagged pointer to observe the supposedly private page.
pub(crate) fn effective_spans(address: u64, bytes: u64) -> Vec<Span> {
    let mut starts = vec![address];
    #[cfg(target_arch = "aarch64")]
    starts.push(address & 0x00ff_ffff_ffff_ffff);
    #[cfg(target_arch = "x86_64")]
    {
        // LAM_U48 and LAM_U57 preserve bit 63 and strip their tag bits.
        starts.push(address & 0x8000_ffff_ffff_ffff);
        starts.push(address & 0x81ff_ffff_ffff_ffff);
    }
    starts.sort_unstable();
    starts.dedup();
    starts
        .into_iter()
        .map(|start| Span::bytes(start, bytes))
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Direction {
    Read,
    Write,
    ReadWrite,
}

/// A local unknown footprint differs from an operation whose target address
/// space itself is unknown (for example an opaque device operation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    Local,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Access {
    pub(crate) target: Target,
    pub(crate) span: Span,
    pub(crate) direction: Direction,
}

fn direct(accesses: &mut Vec<Access>, address: usize, bytes: usize, direction: Direction) {
    if bytes == 0 {
        return;
    }
    accesses.extend(
        effective_spans(address as u64, bytes as u64)
            .into_iter()
            .map(|span| Access {
                target: Target::Local,
                span,
                direction,
            }),
    );
}

fn unknown(target: Target, direction: Direction) -> Vec<Access> {
    vec![Access {
        target,
        span: Span::Unknown,
        direction,
    }]
}

/// Classify the actual native call, including a helper's substituted buffers,
/// before it gives up the logical turn. An earlier snapshot of an iovec or
/// msghdr is not an immutable kernel import, so indirect inputs stay unknown.
/// This function does not classify descriptor acquisition or persistent
/// producers; those obligations have different lifetimes.
pub(crate) fn background_accesses(call: Syscall) -> Vec<Access> {
    use Direction::Read;
    use Direction::ReadWrite;
    use Direction::Write;
    let (_, a) = call.into_parts();
    let mut accesses = Vec::new();
    // Names avoid exposing architecture-absent legacy syscall constants in
    // the other native ABI. They are generated by the same Syscall enum as
    // the handler dispatch, not guest-controlled strings.
    match call.name() {
        "write" | "pwrite64" => direct(&mut accesses, a.arg1, a.arg2, Read),
        "read" | "pread64" => direct(&mut accesses, a.arg1, a.arg2, Write),
        "sendto" => {
            direct(&mut accesses, a.arg1, a.arg2, Read);
            direct(&mut accesses, a.arg4, a.arg5, Read);
        }
        "recvfrom" => {
            direct(&mut accesses, a.arg1, a.arg2, Write);
            if a.arg4 != 0 {
                // The socklen word is mutable until the kernel reads it.
                accesses.extend(unknown(Target::Local, ReadWrite));
            }
        }
        "readv" | "writev" | "preadv" | "pwritev" | "preadv2" | "pwritev2" | "sendmsg"
        | "sendmmsg" | "recvmsg" | "recvmmsg" => {
            accesses.extend(unknown(Target::Local, ReadWrite));
        }
        "poll" | "ppoll" => {
            match a.arg1.checked_mul(8) {
                Some(bytes) => direct(&mut accesses, a.arg0, bytes, ReadWrite),
                None => accesses.extend(unknown(Target::Local, ReadWrite)),
            }
            if call.name() == "ppoll" {
                if a.arg2 != 0 {
                    direct(&mut accesses, a.arg2, 16, ReadWrite);
                }
                if a.arg3 != 0 {
                    direct(&mut accesses, a.arg3, a.arg4, Read);
                }
            }
        }
        "select" | "pselect6" => {
            let bytes = a.arg0.checked_add(63).and_then(|n| (n / 64).checked_mul(8));
            if let Some(bytes) = bytes {
                for address in [a.arg1, a.arg2, a.arg3] {
                    if address != 0 {
                        direct(&mut accesses, address, bytes, ReadWrite);
                    }
                }
            } else {
                accesses.extend(unknown(Target::Local, ReadWrite));
            }
            if a.arg4 != 0 {
                direct(&mut accesses, a.arg4, 16, ReadWrite);
            }
            if call.name() == "pselect6" && a.arg5 != 0 {
                accesses.extend(unknown(Target::Local, Read));
            }
        }
        "epoll_wait" | "epoll_pwait" | "epoll_pwait2" => {
            match a.arg2.checked_mul(std::mem::size_of::<libc::epoll_event>()) {
                Some(bytes) => direct(&mut accesses, a.arg1, bytes, Write),
                None => accesses.extend(unknown(Target::Local, Write)),
            }
            if call.name() == "epoll_pwait2" && a.arg3 != 0 {
                direct(&mut accesses, a.arg3, 16, Read);
            }
            if call.name() != "epoll_wait" && a.arg4 != 0 {
                direct(&mut accesses, a.arg4, a.arg5, Read);
            }
        }
        "rt_sigsuspend" => direct(&mut accesses, a.arg0, a.arg1, Read),
        "rt_sigtimedwait" => {
            direct(&mut accesses, a.arg0, a.arg3, Read);
            if a.arg1 != 0 {
                direct(&mut accesses, a.arg1, 128, Write);
            }
            if a.arg2 != 0 {
                direct(&mut accesses, a.arg2, 16, Read);
            }
        }
        "wait4" => {
            if a.arg1 != 0 {
                direct(&mut accesses, a.arg1, 4, Write);
            }
            if a.arg3 != 0 {
                direct(&mut accesses, a.arg3, 144, Write);
            }
        }
        "waitid" => {
            if a.arg2 != 0 {
                direct(&mut accesses, a.arg2, 128, Write);
            }
            if a.arg4 != 0 {
                direct(&mut accesses, a.arg4, 144, Write);
            }
        }
        "connect" => direct(&mut accesses, a.arg1, a.arg2, Read),
        "accept" | "accept4" => {
            if a.arg1 != 0 {
                accesses.extend(unknown(Target::Local, ReadWrite));
            }
        }
        "fsync" | "fdatasync" | "close" | "flock" | "pause" => {}
        // Opaque ioctls can act on another process or on imported device
        // state. Direct-pointer persistent policy remains separate: this
        // transient unknown target is not a claim to inspect nested drivers.
        "ioctl" | "process_vm_readv" | "process_vm_writev" => {
            accesses.extend(unknown(Target::Unknown, ReadWrite));
        }
        _ => accesses.extend(unknown(Target::Local, ReadWrite)),
    }
    accesses
}

#[cfg(test)]
mod tests {
    use reverie::syscalls::SyscallArgs;
    use reverie::syscalls::Sysno;

    use super::*;

    fn call(sysno: Sysno, args: [usize; 6]) -> Syscall {
        Syscall::from_raw(
            sysno,
            SyscallArgs {
                arg0: args[0],
                arg1: args[1],
                arg2: args[2],
                arg3: args[3],
                arg4: args[4],
                arg5: args[5],
            },
        )
    }

    #[test]
    fn scalar_input_is_bounded_but_mutable_indirection_is_not() {
        let page = Span::bytes(0x71010000, 4096);
        let disjoint = background_accesses(call(Sysno::write, [7, 0x10000, 16, 0, 0, 0]));
        assert!(disjoint.iter().all(|access| !access.span.overlaps(page)));
        let overlap = background_accesses(call(Sysno::write, [7, 0x7100ffff, 2, 0, 0, 0]));
        assert!(overlap.iter().any(|access| access.span.overlaps(page)));
        let indirect = background_accesses(call(Sysno::writev, [7, 0x10000, 1, 0, 0, 0]));
        assert_eq!(indirect, unknown(Target::Local, Direction::ReadWrite));
        let zero = background_accesses(call(Sysno::write, [7, 0x71010000, 0, 0, 0, 0]));
        assert!(zero.is_empty());
    }

    #[test]
    fn overflow_and_tagged_aliases_never_become_disjoint_certificates() {
        let page = Span::bytes(0x71010000, 4096);
        assert!(Span::bytes(u64::MAX - 1, 4).overlaps(page));
        assert!(!Span::bytes(0x71010000, 0).overlaps(page));
        assert!(!Span::bytes(0x71011000, 1).overlaps(page));
        #[cfg(target_arch = "x86_64")]
        let tagged = 0x1234_0000_7101_0000;
        #[cfg(target_arch = "aarch64")]
        let tagged = 0x1200_0000_7101_0000;
        assert!(
            effective_spans(tagged, 4)
                .into_iter()
                .any(|span| span.overlaps(page))
        );
        #[cfg(target_arch = "x86_64")]
        {
            // The native masks retain bits47/56 as well as bit63. An
            // interval must not be replaced by a different lower address
            // merely because that retained bit is set.
            let spans = effective_spans(0x9312_8000_7101_0000, 4);
            assert!(spans.contains(&Span::bytes(0x8000_8000_7101_0000, 4)));
            assert!(spans.contains(&Span::bytes(0x8112_8000_7101_0000, 4)));
            assert!(spans.contains(&Span::bytes(0x9312_8000_7101_0000, 4)));
        }
    }

    #[test]
    fn fresh_accept_has_no_foreign_descriptor_acquisition_footprint() {
        // No peer-address copyout: native successful accept itself creates
        // a fresh file. It needs no unknown-target descriptor-use lease.
        assert!(background_accesses(call(Sysno::accept4, [7, 0, 0, 0, 0, 0])).is_empty());
        let output = background_accesses(call(Sysno::accept4, [7, 0x1000, 0x2000, 0, 0, 0]));
        assert!(output.iter().all(|access| access.target == Target::Local));
        assert!(!output.is_empty());
        let opaque = background_accesses(call(Sysno::ioctl, [7, 0, 0x1000, 0, 0, 0]));
        assert_eq!(opaque, unknown(Target::Unknown, Direction::ReadWrite));
    }

    #[test]
    fn clone_parent_uses_effective_native_signal_and_unknown_is_not_sigkill() {
        let mut bytes = [0_u8; 88];
        bytes[..8].copy_from_slice(&(libc::CLONE_PARENT as u64).to_ne_bytes());
        let native = call(Sysno::clone3, [0x1000, bytes.len(), 0, 0, 0, 0]);
        let describe = |signal| clone_description(native, Some((0x1000, &bytes)), signal);
        assert!(describe(Some(libc::SIGCHLD)).prospective.is_empty());
        assert!(describe(Some(0)).prospective.is_empty());
        assert_eq!(
            describe(Some(libc::SIGKILL)).prospective[0].cause.as_ref(),
            "child-exit-sigkill"
        );
        let unknown = describe(None);
        assert_eq!(
            unknown.prospective[0].cause.as_ref(),
            "clone-lifecycle-unverified"
        );
        assert!(
            !unknown
                .prospective
                .iter()
                .any(|fact| fact.cause.as_ref() == "child-exit-sigkill")
        );
        bytes[..8].copy_from_slice(
            &((libc::CLONE_PARENT | libc::CLONE_THREAD | libc::CLONE_VM | libc::CLONE_SIGHAND)
                as u64)
                .to_ne_bytes(),
        );
        assert!(
            clone_description(native, Some((0x1000, &bytes)), Some(libc::SIGKILL))
                .prospective
                .is_empty()
        );
    }

    #[test]
    fn retained_birth_keeps_unknown_clone3_lifecycle_but_not_known_input_neighbors() {
        use reverie::reserved_input::InputAbi;
        use reverie::reserved_input::InputGrantKind;
        use reverie::reserved_input::InputWrite;
        use reverie::reserved_input::ReservedInputControl;
        use reverie::reserved_input::ReservedInputImage;
        use reverie::reserved_input::ReservedInputLayout;

        for input in ["original-clone3", "immutable-clone3", "register-clone"] {
            for birth in [false, true] {
                let control = ReservedInputControl::new_backend().unwrap();
                let task = control.backend_register_root(5000, 5000).unwrap().task;
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
                let grant = control.grant(task, InputGrantKind::Ordinary).unwrap();
                let mut bytes = [0_u8; 88];
                bytes[32..40].copy_from_slice(&(libc::SIGCHLD as u64).to_ne_bytes());
                let (native, immutable) = if input == "register-clone" {
                    (
                        call(Sysno::clone, [libc::SIGCHLD as usize, 0, 0, 0, 0, 0]),
                        None,
                    )
                } else {
                    (
                        call(Sysno::clone3, [0x1000, bytes.len(), 0, 0, 0, 0]),
                        (input == "immutable-clone3").then_some((0x1000, bytes.as_slice())),
                    )
                };
                let description = clone_description(native, immutable, Some(libc::SIGCHLD));
                let number = description.number;
                let args = description.args;
                let receipt = control
                    .prepare_observed_injection(task, 1, description)
                    .unwrap();
                control
                    .backend_bind_injection(&receipt, task, number, args)
                    .unwrap();
                control.backend_issued(receipt.id()).unwrap();
                if birth {
                    // Scripted native retention/sharing only; this unit is not
                    // a ptrace decoder or immutable-image transport witness.
                    let child = control
                        .backend_register_child(task, 5001, 5001, Some(false), Some(false))
                        .unwrap();
                    control
                        .backend_retained_birth(receipt.id(), child.task, Some(false), Some(false))
                        .unwrap();
                } else {
                    control.backend_no_child(receipt.id()).unwrap();
                }
                control
                    .readmit_injection(receipt.id(), task, grant)
                    .unwrap();
                let scope = control.begin_operation(task, 2).unwrap();
                let layout = control.layout(task).unwrap();
                let image = ReservedInputImage::new(
                    layout,
                    &[InputWrite {
                        offset: 0,
                        bytes: &bytes,
                    }],
                )
                .unwrap();
                let next_call = call(
                    Sysno::clone3,
                    [layout.base as usize, bytes.len(), 0, 0, 0, 0],
                );
                let next = control
                    .prepare_injection(
                        &scope,
                        clone_description(
                            next_call,
                            Some((layout.base, &bytes)),
                            Some(libc::SIGCHLD),
                        ),
                    )
                    .unwrap();
                control
                    .backend_begin_preparation(&next, &scope, image.clone())
                    .unwrap();
                match control.backend_prepared_stopped(next.id()) {
                    Err(error) if birth && input == "original-clone3" => {
                        assert_eq!(error.to_string(), "clone-lifecycle-unverified");
                    }
                    Ok(actual) if !birth || input != "original-clone3" => {
                        assert_eq!(actual, image);
                    }
                    other => panic!("input={input}, birth={birth}: {other:?}"),
                }
                // No page write or native issue occurs in this gate probe.
                control.backend_settled_not_run(next.id()).unwrap();
                control.finish_operation(scope).unwrap();
            }
        }
    }
}
