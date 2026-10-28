/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Process and thread IDs that Detcore chooses (`GuestIds::Chosen`).
//!
//! Without a PID namespace of its own (`--no-namespace`), a guest would get
//! the host's next free ID for every task, a different one in every run. With
//! the capability to choose IDs (`CAP_SYS_ADMIN` or `CAP_CHECKPOINT_RESTORE`
//! over the host's PID namespace), Detcore gives the guest's first process
//! the run's base ID and issues every later clone-family call as `clone3`
//! with a `set_tid` array whose entry for Hermit's PID namespace is the next
//! ID (after 1 for a new PID namespace's first task, or after the guest's own
//! entries), so the kernel itself hands out IDs that depend only on the base
//! and the guest's own order of clone calls. A call Detcore takes an ID
//! for keeps it whether or not it creates a task, so a failed one leaves a
//! gap. The kernel
//! then writes those IDs wherever it writes a task's ID: clone's return value
//! and `CLONE_PARENT_SETTID`/`CLONE_CHILD_SETTID` words, `getpid` and
//! `gettid`, `/proc`, wait results and `si_pid`, PI-futex owner words and
//! robust-list cleanup. Nothing needs translating.
//!
//! `clone3` is given exactly the arguments Linux's own `clone`, `fork` and
//! `vfork` pass to `kernel_clone` (kernel/fork.c), so the kernel validates
//! the call as it would the guest's, and the task created is the one the
//! guest's call would have created, with the chosen ID. A call `clone3`
//! cannot express, one whose `clone3` could fare differently (under a
//! seccomp filter of the guest's own), or one by a task inside a PID
//! namespace the guest made, goes to the kernel as the guest made it, and its
//! task gets a host ID: a run that needs every ID chosen (`--strict`, an
//! explicit `--pid-base`) then ends before the task exists, and any other run
//! says its IDs are not deterministic (`GlobalRequest::GaveHostId`).

use reverie::Error;
use reverie::Guest;
use reverie::reserved_input::InputInjectionError;
use reverie::reserved_input::InputUnavailable;
use reverie::reserved_input::InputWrite;
use reverie::reserved_input::ReservedInputImage;
use reverie::syscalls;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Errno;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;
use serde::Deserialize;
use serde::Serialize;
use tracing::debug;
use tracing::warn;

use crate::Detcore;
use crate::record_or_replay::RecordOrReplay;
use crate::types::DetTid;

#[cfg(test)]
mod injection_tests;

/// `struct clone_args` through `cgroup`: `CLONE_ARGS_SIZE_VER2`
/// (include/uapi/linux/sched.h), the largest size Detcore rewrites. A
/// longer struct with nonzero bytes past it is `E2BIG` only on kernels that
/// know no later version, so it takes the host-ID path.
pub(crate) const CLONE_ARGS_SIZE_VER2: usize = 88;
/// Through `set_tid_size`, the first size that carries `set_tid`.
const CLONE_ARGS_SIZE_VER1: usize = 80;
/// The first version, through `tls`: the smallest size the kernel accepts.
const CLONE_ARGS_SIZE_VER0: usize = 64;
const SET_TID_OFFSET: usize = 64;

/// The end of user memory as `clone3_stack_valid`'s `access_ok` sees it with
/// 4-level page tables (`TASK_SIZE_MAX`). With 5-level page tables a stack
/// above it is in user memory too; such a stack is then taken as
/// inexpressible, the conservative side.
#[cfg(target_arch = "x86_64")]
const USER_ADDRESS_LIMIT: u64 = (1 << 47) - 4096;
/// The end of user memory with 48-bit virtual addresses (`TASK_SIZE_64`);
/// with 52-bit ones a stack above it is taken as inexpressible.
#[cfg(not(target_arch = "x86_64"))]
const USER_ADDRESS_LIMIT: u64 = 1 << 48;
const SET_TID_SIZE_OFFSET: usize = 72;

/// What becomes of a clone-family call when Detcore chooses the IDs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ChosenIdCall {
    /// Issue `clone3` with these `struct clone_args` bytes, of which the
    /// first `size` are passed, once `set_tid` is filled in
    /// ([`ChosenIdCall::with_set_tid`]).
    Clone3 {
        args: [u8; CLONE_ARGS_SIZE_VER2],
        size: usize,
    },
    /// Issue the call as the guest made it: every kernel that can choose IDs
    /// (5.5 and later, for `set_tid`) refuses it before it creates a task, by
    /// the ABI (`CLONE_PIDFD` beside `CLONE_DETACHED`, incompatible legacy
    /// thread/signal-sharing flags, a `clone3` struct shorter than its first
    /// version), so no ID is needed, and `clone3`
    /// could refuse it with another error.
    Unchanged,
    /// The original legacy call fails validation before conversion. Return
    /// this errno without allocating an ID or issuing a rewritten syscall.
    Invalid(Errno),
    /// A guest `clone3` that passes `set_tid` itself, with its `struct
    /// clone_args` (the first 88 bytes, zero extended) and the size to pass:
    /// its ID is the guest's choice if the array reaches Hermit's PID
    /// namespace, else Detcore adds that entry (`Detcore::plan_clone_id`).
    GuestChosen {
        args: [u8; CLONE_ARGS_SIZE_VER2],
        size: usize,
    },
    /// The call creates a task, and `clone3` cannot express it; the reason.
    Inexpressible(String),
}

impl ChosenIdCall {
    /// The arguments with `set_tid` pointing at `set_tid_address`, where the
    /// array of `set_tid_size` IDs is.
    pub(crate) fn with_set_tid(
        mut args: [u8; CLONE_ARGS_SIZE_VER2],
        set_tid_address: u64,
        set_tid_size: usize,
    ) -> [u8; CLONE_ARGS_SIZE_VER2] {
        args[SET_TID_OFFSET..SET_TID_OFFSET + 8].copy_from_slice(&set_tid_address.to_ne_bytes());
        args[SET_TID_SIZE_OFFSET..SET_TID_SIZE_OFFSET + 8]
            .copy_from_slice(&(set_tid_size as u64).to_ne_bytes());
        args
    }
}

/// The `clone3` form of the clone-family call `(sysno, args)`. A `clone3`
/// call's own `struct clone_args` is `clone3_args`, as many bytes as the call
/// passes, or `None` if they could not be read.
///
/// For `clone`, `fork` and `vfork` the fields are the `kernel_clone_args`
/// those calls build (kernel/fork.c, `SYSCALL_DEFINE5(clone)`, `fork`,
/// `vfork`), adjusted where `clone3`'s own checks (`clone3_args_valid`,
/// `copy_clone_args_from_user`) are stricter than what the kernel does with
/// the field anyway:
///
/// - `CLONE_DETACHED`, which `clone3` refuses, is dropped: `copy_process`
///   ignores it except beside `CLONE_PIDFD`, where it fails the call, which
///   is then left unchanged.
/// - The exit signal is 0 beside `CLONE_THREAD` or `CLONE_PARENT`, which
///   `clone3` requires: `copy_process` then takes -1 or the parent's exit
///   signal whatever was passed.
/// - A new stack pointer `sp` becomes `stack = sp - 1, stack_size = 1`:
///   `clone3_stack_valid` starts the child at `stack + stack_size`.
///
/// An exit signal above 64 is inexpressible on kernels that accept it in
/// legacy `clone`. Beside `CLONE_PARENT`/`CLONE_THREAD`, return EINVAL before
/// discarding the original signal, matching Linux v7.1.3 `kernel_clone`.
/// The CLI admits Chosen only after probing this early signal validation;
/// hosts without it are refused or use the explicit Host-ID fallback.
/// A `clone3` call is rewritten only when it passes no `set_tid` of
/// its own and its size is one of the three versions; Detcore then fills in
/// `set_tid`, extending a first-version struct to the second.
pub(crate) fn clone3_form(
    sysno: Sysno,
    args: &SyscallArgs,
    clone3_args: Option<&[u8]>,
) -> ChosenIdCall {
    let fields = match sysno {
        #[cfg(not(target_arch = "aarch64"))]
        Sysno::fork => LegacyClone {
            flags: 0,
            exit_signal: libc::SIGCHLD as u64,
            ..LegacyClone::default()
        },
        #[cfg(not(target_arch = "aarch64"))]
        Sysno::vfork => LegacyClone {
            flags: (libc::CLONE_VFORK | libc::CLONE_VM) as u64,
            exit_signal: libc::SIGCHLD as u64,
            ..LegacyClone::default()
        },
        Sysno::clone => LegacyClone::of_clone(args, CloneArgOrder::NATIVE),
        Sysno::clone3 => return clone3_with_room_for_set_tid(clone3_args),
        _ => {
            return ChosenIdCall::Inexpressible(format!(
                "{sysno} is not a clone-family call Detcore knows"
            ));
        }
    };
    fields.as_clone3()
}

/// The `kernel_clone_args` fields a `clone`, `fork` or `vfork` builds.
#[derive(Clone, Copy, Debug, Default)]
struct LegacyClone {
    flags: u64,
    pidfd: u64,
    child_tid: u64,
    parent_tid: u64,
    exit_signal: u64,
    stack: u64,
    tls: u64,
}

/// The order of `clone`'s last two arguments (kernel/fork.c,
/// `SYSCALL_DEFINE5(clone)`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CloneArgOrder {
    /// `clone(flags, newsp, parent_tidptr, child_tidptr, tls)`: x86-64.
    #[cfg_attr(
        target_arch = "aarch64",
        allow(
            dead_code,
            reason = "the other architecture's order, built for its test"
        )
    )]
    ChildTidThenTls,
    /// `clone(flags, newsp, parent_tidptr, tls, child_tidptr)`: arm64
    /// (`CONFIG_CLONE_BACKWARDS`).
    #[cfg_attr(
        not(target_arch = "aarch64"),
        allow(
            dead_code,
            reason = "the other architecture's order, built for its test"
        )
    )]
    TlsThenChildTid,
}

impl CloneArgOrder {
    #[cfg(target_arch = "aarch64")]
    const NATIVE: Self = CloneArgOrder::TlsThenChildTid;
    #[cfg(not(target_arch = "aarch64"))]
    const NATIVE: Self = CloneArgOrder::ChildTidThenTls;
}

impl LegacyClone {
    /// `SYSCALL_DEFINE5(clone)`: the low 32 bits of the flags, less
    /// `CSIGNAL`, which is the exit signal; the parent TID address serves as
    /// the pidfd address too; the last two arguments in `order`.
    fn of_clone(args: &SyscallArgs, order: CloneArgOrder) -> Self {
        let low = args.arg0 as u32 as u64;
        let csignal = libc::CSIGNAL as u64;
        let (child_tid, tls) = match order {
            CloneArgOrder::ChildTidThenTls => (args.arg3, args.arg4),
            CloneArgOrder::TlsThenChildTid => (args.arg4, args.arg3),
        };
        LegacyClone {
            flags: low & !csignal,
            pidfd: args.arg2 as u64,
            child_tid: child_tid as u64,
            parent_tid: args.arg2 as u64,
            exit_signal: low & csignal,
            stack: args.arg1 as u64,
            tls: tls as u64,
        }
    }

    fn as_clone3(self) -> ChosenIdCall {
        let shares_parent = (libc::CLONE_THREAD | libc::CLONE_PARENT) as u64;
        // Linux v7.1.3 kernel/fork.c:2702, kernel_clone, validates the
        // original signal before copy_process replaces it for these flags:
        // https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/fork.c?id=199c9959d3a9b53f346c221757fc7ac507fbac50#n2702
        // v6.16 validated only clone3's signal. Chosen-ID admission probes
        // the required early validation before a guest starts; Namespace/Host
        // calls bypass this conversion and retain native results.
        if self.flags & shares_parent != 0 && self.exit_signal > 64 {
            return ChosenIdCall::Invalid(Errno::EINVAL);
        }
        // These legacy-clone combinations cannot create a child. Let the
        // original kernel call preserve its error precedence instead of
        // confusing its EINVAL with the added set_tid entry. Linux v6.17
        // copy_process rejects both before alloc_pid:
        // https://github.com/torvalds/linux/blob/v6.17/kernel/fork.c#L1945
        let thread = self.flags & libc::CLONE_THREAD as u64 != 0;
        let sighand = self.flags & libc::CLONE_SIGHAND as u64 != 0;
        let vm = self.flags & libc::CLONE_VM as u64 != 0;
        if (thread && !sighand) || (sighand && !vm) {
            return ChosenIdCall::Unchanged;
        }
        let detached = libc::CLONE_DETACHED as u64;
        let pidfd = libc::CLONE_PIDFD as u64;
        // copy_process: CLONE_PIDFD beside CLONE_DETACHED is EINVAL, before
        // any task exists. (Beside CLONE_THREAD it is EINVAL only before
        // Linux 6.9; the rewritten call then fails the same way, and the
        // guest's own call decides.)
        if self.flags & pidfd != 0 && self.flags & detached != 0 {
            return ChosenIdCall::Unchanged;
        }
        let exit_signal = if self.flags & shares_parent != 0 {
            0
        } else {
            self.exit_signal
        };
        if exit_signal > 64 {
            return ChosenIdCall::Inexpressible(format!(
                "its exit signal {exit_signal} is not a signal number, which clone3 refuses"
            ));
        }
        let (stack, stack_size) = match self.stack {
            0 => (0, 0),
            // clone3 starts the child at `stack + stack_size`, both nonzero,
            // and refuses a range outside user memory (clone3_stack_valid).
            sp if sp == 1 || sp > USER_ADDRESS_LIMIT => {
                return ChosenIdCall::Inexpressible(format!(
                    "its new stack pointer {sp:#x} has no clone3 form, which needs a nonzero \
                     stack in user memory below it"
                ));
            }
            sp => (sp - 1, 1),
        };
        let fields: [u64; 11] = [
            self.flags & !detached,
            self.pidfd,
            self.child_tid,
            self.parent_tid,
            exit_signal,
            stack,
            stack_size,
            self.tls,
            0, // set_tid, filled in later
            0, // set_tid_size
            0, // cgroup
        ];
        let mut args = [0u8; CLONE_ARGS_SIZE_VER2];
        for (chunk, field) in args.as_chunks_mut::<8>().0.iter_mut().zip(fields) {
            *chunk = field.to_ne_bytes();
        }
        ChosenIdCall::Clone3 {
            args,
            size: CLONE_ARGS_SIZE_VER1,
        }
    }
}

/// A guest's own `clone3` arguments, extended to carry `set_tid`.
///
/// `copy_clone_args_from_user` reads the first `min(size, 88)` bytes, zero
/// extends a shorter struct, and refuses a longer one whose extra bytes are
/// not all zero (`E2BIG`); a struct shorter than the first version is
/// `EINVAL`. So a struct of any size that the kernel accepts is the
/// second-version struct of its first 88 bytes, zero extended, and `set_tid`
/// is unset exactly when bytes 64 to 79 are zero: then Detcore fills it in.
fn clone3_with_room_for_set_tid(bytes: Option<&[u8]>) -> ChosenIdCall {
    let Some(bytes) = bytes else {
        // Mapped, but not readable with the guest's access: the kernel may
        // read it (`Clone3Read::Unreadable`).
        return ChosenIdCall::Inexpressible(
            "Detcore could not read its struct clone_args with the guest's own access, though \
             it is mapped"
                .to_owned(),
        );
    };
    let size = bytes.len();
    if size < CLONE_ARGS_SIZE_VER0 {
        // copy_clone_args_from_user: EINVAL.
        return ChosenIdCall::Unchanged;
    }
    if bytes
        .iter()
        .skip(CLONE_ARGS_SIZE_VER2)
        .any(|byte| *byte != 0)
    {
        // copy_clone_args_from_user refuses it with E2BIG only while the
        // kernel knows no later struct version; a newer one may read it.
        return ChosenIdCall::Inexpressible(format!(
            "its struct clone_args has nonzero bytes past the {CLONE_ARGS_SIZE_VER2} Detcore \
             knows, which a newer kernel may read"
        ));
    }
    let mut args = [0u8; CLONE_ARGS_SIZE_VER2];
    let known = size.min(CLONE_ARGS_SIZE_VER2);
    args[..known].copy_from_slice(&bytes[..known]);
    if args[SET_TID_OFFSET..CLONE_ARGS_SIZE_VER1]
        .iter()
        .any(|byte| *byte != 0)
    {
        // The guest names its task's IDs itself, or passes `set_tid` and
        // `set_tid_size` the kernel refuses (`Detcore::plan_clone_id` tells
        // which).
        return ChosenIdCall::GuestChosen {
            args,
            size: known.max(CLONE_ARGS_SIZE_VER1),
        };
    }
    ChosenIdCall::Clone3 {
        args,
        size: known.max(CLONE_ARGS_SIZE_VER1),
    }
}

/// A clone-family call that is to create its task as `clone3` with a
/// `set_tid` array that reaches Hermit's PID namespace.
#[derive(Debug)]
pub(crate) struct ChosenId {
    /// The array, from the new task's namespace outwards.
    set_tid: Vec<libc::pid_t>,
    /// The ID Detcore took for the task in Hermit's namespace, the array's
    /// last entry; `None` where the guest named every entry itself.
    id: Option<i32>,
    /// This attempt took a fresh allocator value while holding its turn.
    /// A saved restart ID has no such attempt-local registration proof.
    fresh_allocation: bool,
    /// The call as a restart keeps its ID under (`RestartedCloneId::call`).
    restart: Option<Vec<u8>>,
    /// `struct clone_args`, `set_tid` still unset.
    args: [u8; CLONE_ARGS_SIZE_VER2],
    /// How many of those bytes `clone3` is given.
    size: usize,
    /// The guest's call passed `CLONE_UNTRACED`, which `args` no longer do.
    untraced: bool,
    /// The array begins with entries of the guest's own.
    guest_entries: bool,
}

/// Evidence that adding the outer entry did not introduce an EINVAL cause.
///
/// This is conditional on pid_max staying stable across this attempt. The
/// before/after samples detect a visible change, not an ABA shrink and restore.
/// External host-root sysctl mutation in an init/no-namespace run is outside
/// this admitted environment; these samples are not an atomic stability proof.
/// Linux alloc_pid checks the array length, each entry's range, and whether a
/// namespace without a reaper is being given its first ID. The new last entry
/// is in Hermit's already-active namespace; inner guest entries are unchanged.
/// https://github.com/torvalds/linux/blob/v6.17/kernel/pid.c#L178
///
/// The fresh allocator value was not registered or clocked at allocation.
/// The caller retains its serial turn, or installs the pending-vfork barrier
/// before releasing it; no other guest can register that value before this
/// failed birth. Thus ThreadIdRefused's zombie exception cannot hide a use of
/// this fresh value. It is only an additional check, not a universal registry
/// query. Saved restart IDs lack this attempt-local proof. Registration stays
/// recorded after exit; clock components themselves can move during exec.
#[derive(Debug)]
struct AddedIdEvidence {
    id: i32,
    maximum: i32,
    args: [u8; CLONE_ARGS_SIZE_VER2],
    size: usize,
    set_tid: Vec<libc::pid_t>,
}

impl AddedIdEvidence {
    fn capture<M: MemoryAccess>(
        memory: &M,
        tid: i32,
        call: &Syscall,
        chosen: &ChosenId,
        maximum: Option<i32>,
    ) -> Option<Self> {
        let id = chosen.id?;
        let maximum = maximum?;
        if !chosen.fresh_allocation || id <= 0 || id >= maximum {
            return None;
        }
        // This proof preserves the original clone3 size and every original
        // field. Legacy conversions and unknown later struct bytes need
        // their own equivalence proof, so they retain the refusal policy.
        let Syscall::Clone3(original) = call else {
            return None;
        };
        if original.size() != chosen.size
            || !(CLONE_ARGS_SIZE_VER1..=CLONE_ARGS_SIZE_VER2).contains(&chosen.size)
        {
            return None;
        }
        let Clone3Read::Bytes(bytes) = read_clone_args(memory, tid, original) else {
            return None;
        };
        let mut original_args = [0; CLONE_ARGS_SIZE_VER2];
        original_args[..bytes.len()].copy_from_slice(&bytes);
        if original_args != chosen.args {
            return None;
        }
        let flags = u64::from_ne_bytes(original_args[..8].try_into().unwrap());
        let depth = pid_namespace_depth(tid, flags)?;
        if chosen.set_tid.len() != depth + 1
            || chosen.set_tid.len() > MAX_SET_TID
            || chosen.set_tid.last() != Some(&id)
        {
            return None;
        }
        if chosen.guest_entries {
            let original_entries = read_guest_set_tid(memory, tid, &original_args).ok()?;
            if original_entries != chosen.set_tid[..depth] {
                return None;
            }
        } else if original_args[SET_TID_OFFSET..SET_TID_SIZE_OFFSET + 8] != [0; 16]
            || !(depth == 0
                || (depth == 1 && flags & libc::CLONE_NEWPID as u64 != 0 && chosen.set_tid[0] == 1))
        {
            return None;
        }
        Some(Self {
            id,
            maximum,
            args: original_args,
            size: chosen.size,
            set_tid: chosen.set_tid.clone(),
        })
    }

    fn staged_matches(
        &self,
        image: &ReservedInputImage,
        rewritten: &syscalls::Clone3,
        array: u64,
    ) -> bool {
        if rewritten.size() != self.size {
            return false;
        }
        let Some(address) = rewritten.args() else {
            return false;
        };
        let mut observed = [0; CLONE_ARGS_SIZE_VER2];
        let expected = ChosenIdCall::with_set_tid(self.args, array, self.set_tid.len());
        let mut entries = vec![0; self.set_tid.len() * std::mem::size_of::<libc::pid_t>()];
        image.read(address.as_raw() as u64, &mut observed)
            && observed == expected
            && image.read(array, &mut entries)
            && entries
                .as_chunks::<4>()
                .0
                .iter()
                .map(|entry| i32::from_ne_bytes(*entry))
                .eq(self.set_tid.iter().copied())
    }
}

/// Construct a complete future image without touching application memory.
/// Native inputs/outputs, including effective tagged aliases, may not name
/// this page. Only fields whose native flags make them active are excluded.
fn reserved_clone_image(
    layout: reverie::reserved_input::ReservedInputLayout,
    original: Syscall,
    args: [u8; CLONE_ARGS_SIZE_VER2],
    entries: &[libc::pid_t],
    size: usize,
) -> Result<(ReservedInputImage, syscalls::Clone3, u64), InputUnavailable> {
    use crate::reserved_input::Span;
    use crate::reserved_input::effective_spans;
    let page = Span::bytes(layout.base, layout.page_bytes as u64);
    let excludes = |address: u64, bytes: u64| {
        effective_spans(address, bytes)
            .into_iter()
            .any(|span| span.overlaps(page))
    };
    let field = |offset: usize| u64::from_ne_bytes(args[offset..offset + 8].try_into().unwrap());
    let flags = field(0);
    if let Syscall::Clone3(call) = original {
        let address = call.args().map_or(0, |address| address.as_raw() as u64);
        if excludes(address, call.size() as u64) {
            return Err(InputUnavailable::new(
                "clone-original-input-overlaps-reserved-page",
            ));
        }
    }
    if field(72) != 0
        && field(72)
            .checked_mul(4)
            .is_none_or(|bytes| excludes(field(64), bytes))
    {
        return Err(InputUnavailable::new(
            "clone-set-tid-input-overlaps-reserved-page",
        ));
    }
    for (active, offset) in [
        (libc::CLONE_PIDFD as u64, 8),
        (
            (libc::CLONE_CHILD_SETTID | libc::CLONE_CHILD_CLEARTID) as u64,
            16,
        ),
        (libc::CLONE_PARENT_SETTID as u64, 24),
    ] {
        if flags & active != 0 && excludes(field(offset), 4) {
            return Err(InputUnavailable::new(
                "clone-native-output-overlaps-reserved-page",
            ));
        }
    }
    if field(48) != 0 && excludes(field(40), field(48)) {
        return Err(InputUnavailable::new(
            "clone-child-stack-overlaps-reserved-page",
        ));
    }
    if flags & libc::CLONE_SETTLS as u64 != 0 && excludes(field(56), 1) {
        return Err(InputUnavailable::new("clone-tls-overlaps-reserved-page"));
    }
    if entries.len() > MAX_SET_TID {
        return Err(InputUnavailable::new("clone-set-tid-image-too-large"));
    }
    let mut array = [0; MAX_SET_TID * std::mem::size_of::<libc::pid_t>()];
    for (entry, bytes) in entries.iter().zip(array.as_chunks_mut::<4>().0.iter_mut()) {
        bytes.copy_from_slice(&entry.to_ne_bytes());
    }
    let array_address = layout.base;
    let args_address = layout.base + array.len() as u64;
    let staged_args = ChosenIdCall::with_set_tid(args, array_address, entries.len());
    let image = ReservedInputImage::new(
        layout,
        &[
            InputWrite {
                offset: 0,
                bytes: &array,
            },
            InputWrite {
                offset: array.len(),
                bytes: &staged_args,
            },
        ],
    )?;
    let rewritten = syscalls::Clone3::new()
        .with_args(AddrMut::from_raw(args_address as usize))
        .with_size(size);
    Ok((image, rewritten, array_address))
}

/// How a clone-family call is issued, as [`Detcore::plan_clone_id`] decides.
#[derive(Debug)]
pub(crate) enum ClonePlan {
    /// As the guest made it, the kernel numbering its task: the run's PID
    /// namespace does, or the host does (`GuestIds::Namespace`, `Host`).
    Kernel,
    /// As `clone3` with `set_tid`.
    Chosen(ChosenId),
    /// As the guest made it, though Detcore chooses the IDs: its task gets a
    /// host ID, for `reason`, which is reported ([`Detcore::gave_host_id`], or
    /// for a vfork [`Detcore::vfork_gets_host_id`] before the call).
    AsMade { reason: String },
    /// As the guest made it, though Detcore chooses the IDs: every kernel
    /// refuses it before creating a task (`ChosenIdCall::Unchanged`), so it
    /// is reported only if it creates one after all, and a run that needs
    /// every ID chosen lets the guest have the kernel's error.
    KernelRefuses,
    /// [`ClonePlan::KernelRefuses`] for a legacy `clone` with
    /// `CLONE_UNTRACED`, issued without the flag: it is one of
    /// `CLONE_LEGACY_FLAGS`, which no validity check rejects, so the
    /// kernel's error is the same, and a task created after all is traced.
    KernelRefusesUntraced,
    /// As the guest made it: a guest `clone3` that names its task's ID in
    /// Hermit's PID namespace itself, or passes a `set_tid` array the kernel
    /// refuses before creating a task.
    GuestChosen,
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Decide how the guest's clone-family call `call` is issued, and, when
    /// Detcore chooses the IDs (`GuestIds::Chosen`), take the ID of the task
    /// it is to create: in the caller's turn, before anything the call does
    /// could let another thread run, so with sequentialized threads the order
    /// of the IDs is the guest's own order of calls. A call that was
    /// restarted keeps the ID it took (`ThreadState::restarted_clone_id`), if
    /// it comes back the same.
    ///
    /// The kernel validates the call: Detcore issues it as `clone3` with a
    /// `set_tid` array that reaches Hermit's PID namespace (the entries the
    /// guest gave, or 1 for a new namespace's first task, then the chosen
    /// ID), and the call's own errors come back as they are. Only a call that
    /// cannot carry a chosen ID takes the host-ID path, which ends a run that
    /// needs every ID chosen here, before any task exists.
    pub(crate) async fn plan_clone_id<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &Syscall,
    ) -> Result<ClonePlan, Error> {
        let detcore_model::pid::GuestIds::Chosen {
            refuse_host_ids, ..
        } = self.cfg.guest_ids
        else {
            return Ok(ClonePlan::Kernel);
        };
        let (sysno, args) = call.into_parts();
        // The calling thread's own maps: the leader's are empty once it has
        // exited, while the others still run on the same memory.
        let tid = guest.tid().as_raw();
        let witness = FaultWitness::for_backend(&guest.config().backend, tid);
        let clone3_read = match call {
            Syscall::Clone3(clone) => Some(read_clone_args(&guest.memory(), witness, clone)),
            _ => None,
        };
        if clone3_read == Some(Clone3Read::Refused) {
            return Ok(ClonePlan::KernelRefuses);
        }
        // A task the tracer does not follow would run outside Detcore's
        // scheduler, so a call with CLONE_UNTRACED is issued only rewritten,
        // without the flag; where it cannot be, the run ends. A struct whose
        // flags Detcore could not read at all counts as untraced.
        let flags = match (&clone3_read, sysno) {
            (_, Sysno::clone) => args.arg0 as u64,
            (Some(Clone3Read::Bytes(bytes)), _) => bytes
                .get(..8)
                .map_or(0, |flags| u64::from_ne_bytes(flags.try_into().unwrap())),
            (Some(Clone3Read::Unreadable { flags }), _) => {
                flags.unwrap_or(libc::CLONE_UNTRACED as u64)
            }
            _ => 0,
        };
        let untraced = flags & libc::CLONE_UNTRACED as u64 != 0;
        let clone3_args = match clone3_read {
            Some(Clone3Read::Bytes(bytes)) => Some(bytes),
            _ => None,
        };
        let form = clone3_form(sysno, &args, clone3_args.as_deref());
        // A restarted call's ID is for that call only: taken here whatever
        // the plan, so that no other call inherits it.
        let restarted = guest.thread_state_mut().restarted_clone_id.take();
        let tid = guest.tid().as_raw();
        let (args, size, set_tid, guest_entries) = match form {
            ChosenIdCall::Invalid(errno) => return Err(errno.into()),
            ChosenIdCall::Clone3 { args, size } => {
                let flags = u64::from_ne_bytes(args[..8].try_into().unwrap());
                // The new task's ID in each namespace inside Hermit's: none,
                // or 1, a new namespace's first task, with CLONE_NEWPID from
                // Hermit's own. A namespace the guest made for its children
                // already has tasks numbered by the kernel.
                let inner = match pid_namespace_depth(tid, flags) {
                    Some(0) => Vec::new(),
                    Some(1) if flags & libc::CLONE_NEWPID as u64 != 0 => vec![1],
                    Some(depth) => {
                        let why = format!(
                            "its task is created {depth} PID namespace(s) below Hermit's, in one \
                             the guest made, whose IDs the kernel picks"
                        );
                        return self
                            .host_id_planned(guest, sysno, why, refuse_host_ids, untraced)
                            .await;
                    }
                    None => {
                        let why =
                            "the PID namespace its task is created in could not be read".to_owned();
                        return self
                            .host_id_planned(guest, sysno, why, refuse_host_ids, untraced)
                            .await;
                    }
                };
                if let Some(why) = self.clone3_could_fare_otherwise(guest) {
                    return self
                        .host_id_planned(guest, sysno, why, refuse_host_ids, untraced)
                        .await;
                }
                (args, size, inner, false)
            }
            ChosenIdCall::GuestChosen { args, size } => {
                // An array every kernel refuses goes to the kernel, whose
                // EINVAL or EFAULT is the guest's. One Detcore cannot read
                // though it is mapped could still be read by the kernel, so
                // it may create a task whose ID in Hermit's namespace the
                // kernel picks.
                let tids = match read_guest_set_tid(&guest.memory(), witness, &args) {
                    Ok(tids) => tids,
                    Err(SetTidUnread::Refused) => return Ok(ClonePlan::GuestChosen),
                    Err(SetTidUnread::Unreadable) => {
                        let why = "Detcore could not read its set_tid array with the guest's own \
                                   access"
                            .to_owned();
                        return self
                            .host_id_planned(guest, sysno, why, refuse_host_ids, untraced)
                            .await;
                    }
                };
                let flags = u64::from_ne_bytes(args[..8].try_into().unwrap());
                // The array runs from the new task's PID namespace outwards.
                match pid_namespace_depth(tid, flags) {
                    // It names the task's ID in Hermit's namespace itself.
                    Some(depth) if tids.len() > depth => {
                        if !untraced {
                            return Ok(ClonePlan::GuestChosen);
                        }
                        // Issued rewritten only to drop CLONE_UNTRACED.
                        let args = without_untraced(args);
                        return Ok(ClonePlan::Chosen(ChosenId {
                            set_tid: tids,
                            id: None,
                            fresh_allocation: false,
                            restart: None,
                            args,
                            size,
                            untraced,
                            guest_entries: true,
                        }));
                    }
                    // One short: Detcore adds the ID in Hermit's namespace.
                    Some(depth) if tids.len() == depth && depth < MAX_SET_TID => {
                        (args, size, tids, true)
                    }
                    Some(depth) => {
                        let why = format!(
                            "its set_tid names {} ID(s), but its task is created {depth} PID \
                             namespace(s) below Hermit's, so the IDs in between are the kernel's",
                            tids.len()
                        );
                        return self
                            .host_id_planned(guest, sysno, why, refuse_host_ids, untraced)
                            .await;
                    }
                    None => {
                        let why =
                            "the PID namespace its task is created in could not be read".to_owned();
                        return self
                            .host_id_planned(guest, sysno, why, refuse_host_ids, untraced)
                            .await;
                    }
                }
            }
            // CLONE_UNTRACED is dropped from such a call (a legacy clone's:
            // a clone3 struct is never Unchanged), keeping the kernel's error,
            // so that a task it creates after all is traced.
            ChosenIdCall::Unchanged if untraced => return Ok(ClonePlan::KernelRefusesUntraced),
            ChosenIdCall::Unchanged => return Ok(ClonePlan::KernelRefuses),
            ChosenIdCall::Inexpressible(why) => {
                return self
                    .host_id_planned(guest, sysno, why, refuse_host_ids, untraced)
                    .await;
            }
        };
        // A task Detcore numbers must be traced: its own clones reach Detcore
        // only then. CLONE_UNTRACED only keeps a tracer from following the
        // task, and the guest's tasks are Hermit's to trace, so the rewrite
        // drops it (as https://github.com/rrnewton/hermit/pull/4660 does for
        // every clone). Nothing else in the call depends on it.
        let args = without_untraced(args);
        let call = RestartedCloneId::call(sysno, &args, size);
        let (id, fresh_allocation) = match restarted {
            Some(restarted) if restarted.call == call => (restarted.id, false),
            _ => match crate::tool_global::next_guest_id(guest).await {
                Some(id) => (id, true),
                // Exhaustion does not wrap to a nonpositive or reused ID.
                // Keep the existing host-ID fallback or strict refusal.
                None => {
                    return self
                        .host_id_planned(
                            guest,
                            sysno,
                            "Detcore has no unused positive process ID left".to_owned(),
                            refuse_host_ids,
                            untraced,
                        )
                        .await;
                }
            },
        };
        let mut set_tid = set_tid;
        set_tid.push(id);
        Ok(ClonePlan::Chosen(ChosenId {
            set_tid,
            id: Some(id),
            fresh_allocation,
            restart: Some(call),
            args,
            size,
            untraced,
            guest_entries,
        }))
    }

    /// Why the caller's `clone3` could fare otherwise than its own call, if
    /// it could: a seccomp filter the guest installed (only Reverie's and
    /// those Hermit inherited are known) could trap, kill or refuse `clone3`
    /// where it allows the guest's call.
    fn clone3_could_fare_otherwise<G: Guest<Self>>(&self, guest: &G) -> Option<String> {
        let tid = guest.tid().as_raw();
        let expected = match self.cfg.inherited_seccomp {
            // Reverie's own filter, and those Hermit runs under.
            detcore_model::config::InheritedSeccomp::Disabled => 1,
            detcore_model::config::InheritedSeccomp::Filter(Some(filters)) => filters + 1,
            _ => {
                return Some(
                    "Hermit could not count the seccomp filters it runs under, so a filter of \
                     the guest's own could not be told from them"
                        .to_owned(),
                );
            }
        };
        let status = std::fs::read_to_string(format!("/proc/{tid}/status")).unwrap_or_default();
        let filters = status
            .lines()
            .find_map(|line| line.strip_prefix("Seccomp_filters:"))
            .and_then(|count| count.trim().parse::<u32>().ok());
        match filters {
            Some(filters) if filters <= expected => None,
            Some(_) => Some(
                "the guest installed a seccomp filter, which could trap, kill or refuse clone3 \
                 where it allows this call"
                    .to_owned(),
            ),
            None => Some(
                "this kernel does not report the task's seccomp filters (Linux 5.9 and later \
                 do), so one of the guest's own could not be ruled out"
                    .to_owned(),
            ),
        }
    }

    /// A call whose task will get a host ID, for `why`: the end of a run that
    /// needs every ID chosen, or of any run if the call is `untraced` (its
    /// task would run outside Detcore's scheduler), else a plan to issue it
    /// as made. A run that needs every ID chosen ends before the kernel sees
    /// the call, so an error the kernel would also have given it is not the
    /// guest's to see: the run fails closed rather than refusing only after a
    /// task with a host ID exists.
    async fn host_id_planned<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
        why: String,
        refuse_host_ids: bool,
        untraced: bool,
    ) -> Result<ClonePlan, Error> {
        let reason = format!("this {sysno} cannot be given a chosen process ID: {why}");
        if untraced {
            refuse_untraced(guest, reason).await
        }
        if refuse_host_ids {
            refuse_host_id(guest, reason).await
        }
        Ok(ClonePlan::AsMade { reason })
    }

    /// What a refusal for a held chosen ID asks for: a replay takes its
    /// record's base, so it names the record; any other run can pass another
    /// base.
    fn free_base_hint(&self) -> String {
        let base = self
            .cfg
            .guest_ids
            .base()
            .map_or_else(|| "?".to_owned(), |base| base.to_string());
        match self
            .cfg
            .replay_preemptions_from
            .as_ref()
            .or(self.cfg.replay_schedule_from.as_ref())
        {
            Some(record) => format!(
                "The replayed record {} takes base {base}; replay it where no host task holds \
                 the IDs from that base",
                record.display()
            ),
            None => "Pass a --pid-base whose IDs no host task holds".to_owned(),
        }
    }

    /// Report that the guest's clone-family call created task `child` with a
    /// host ID, for `reason`: the end of a run that needs every ID chosen,
    /// else a warning and a count, which the summary shows and which marks
    /// the preemption record `GuestIds::Host`, so that a replay refuses it.
    pub(crate) async fn gave_host_id<G: Guest<Self>>(
        &self,
        guest: &mut G,
        child: i64,
        reason: &str,
    ) {
        let reason = format!("task {child} got a host ID, not a chosen one: {reason}");
        if matches!(
            self.cfg.guest_ids,
            detcore_model::pid::GuestIds::Chosen {
                refuse_host_ids: true,
                ..
            }
        ) {
            refuse_host_id(guest, reason).await
        }
        warn!(
            "[detcore, dtid {}] {reason}; that ID differs from run to run",
            guest.thread_state().dettid
        );
        crate::tool_global::note_host_id_task(guest).await;
    }

    /// Report, before a `CLONE_VFORK` call issued as made, that its task, if
    /// it creates one, has a host ID for `reason`. Its parent returns only
    /// once the task has exec'd or exited, and the run can end while it runs,
    /// so a report after the call would come too late for the preemption
    /// record. A call that then fails was reported all the same, which errs
    /// on the side of a record a replay refuses.
    async fn vfork_gets_host_id<G: Guest<Self>>(&self, guest: &mut G, reason: &str) {
        let reason = format!(
            "this vfork's task gets a host ID, not a chosen one, reported before the call: \
             {reason}"
        );
        if matches!(
            self.cfg.guest_ids,
            detcore_model::pid::GuestIds::Chosen {
                refuse_host_ids: true,
                ..
            }
        ) {
            refuse_host_id(guest, reason).await
        }
        warn!(
            "[detcore, dtid {}] {reason}; that ID differs from run to run",
            guest.thread_state().dettid
        );
        crate::tool_global::note_host_id_task(guest).await;
    }

    /// Issue the guest's `CLONE_VFORK` call `call` as made, its task's host
    /// ID reported first, for `reason` ([`Self::vfork_gets_host_id`]). A
    /// call that then fails, other than for a restart, created no task,
    /// which a warning says, since the count already has it.
    async fn issue_reported_vfork<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
        reason: &str,
    ) -> Result<i64, Errno> {
        self.vfork_gets_host_id(guest, reason).await;
        let result = crate::reserved_input::inject_original_clone(guest, call).await;
        if let Err(errno) = result
            && !is_restart(errno)
        {
            warn!(
                "[detcore, dtid {}] the vfork reported above failed with {errno} and created no \
                 task; the run's count of accepted host-ID reports includes it",
                guest.thread_state().dettid
            );
        }
        result
    }

    /// Issue `call`, which every kernel is expected to refuse before creating
    /// a task, as made; a task it creates after all has a host ID, reported
    /// after the call.
    async fn issue_kernel_refused<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Errno> {
        let result = crate::reserved_input::inject_original_clone(guest, call).await;
        if let Ok(child) = result {
            self.gave_host_id(
                guest,
                child,
                "unexpectedly, as Detcore expected every kernel to refuse it",
            )
            .await;
        }
        result
    }

    /// Issue the guest's clone-family call `call` as `plan` says; `vfork`
    /// says whether it passes `CLONE_VFORK`.
    pub(crate) async fn inject_clone_planned<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
        plan: ClonePlan,
        vfork: bool,
    ) -> Result<i64, Errno> {
        match plan {
            ClonePlan::Kernel | ClonePlan::GuestChosen => {
                crate::reserved_input::inject_original_clone(guest, call).await
            }
            ClonePlan::AsMade { reason } => {
                if vfork {
                    return self.issue_reported_vfork(guest, call, &reason).await;
                }
                let result = crate::reserved_input::inject_original_clone(guest, call).await;
                if let Ok(child) = result {
                    self.gave_host_id(guest, child, &reason).await;
                }
                result
            }
            ClonePlan::KernelRefuses => self.issue_kernel_refused(guest, call).await,
            ClonePlan::KernelRefusesUntraced => {
                let (sysno, args) = call.into_parts();
                self.issue_kernel_refused(
                    guest,
                    Syscall::from_raw(sysno, traced_legacy_clone(args)),
                )
                .await
            }
            ClonePlan::Chosen(chosen) => {
                self.inject_with_chosen_id(guest, call, chosen, vfork).await
            }
        }
    }

    /// Prepare and issue one native attempt with an owned immutable image.
    /// A successful mechanism result certifies full-page write/readback and
    /// settled ownership even when the native syscall returned an errno.
    async fn stage_chosen_clone<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
        args: [u8; CLONE_ARGS_SIZE_VER2],
        set_tid: &[libc::pid_t],
        size: usize,
        attribution: Option<&AddedIdEvidence>,
    ) -> Result<(Result<i64, Errno>, bool), InputInjectionError> {
        let (control, task) = crate::reserved_input::capability(guest)
            .ok_or_else(|| InputUnavailable::new("reserved-input-facility-unavailable"))?;
        let layout = control
            .layout(task)
            .ok_or_else(|| InputUnavailable::new("reserved-input-layout-unqualified"))?;
        let (image, rewritten, array) = reserved_clone_image(layout, call, args, set_tid, size)?;
        // Compare F2's complete expected struct/array against the future view.
        // The backend uses that same immutable image for preparation and
        // native readback; there is no Tool callback after page publication.
        let input_matches =
            attribution.is_some_and(|proof| proof.staged_matches(&image, &rewritten, array));
        let scope = control.begin_operation(task, guest.thread_state().stats.syscall_count)?;
        let args_address = rewritten.args().expect("fixed reserved address").as_raw();
        let args_offset = args_address - layout.base as usize;
        let description = crate::reserved_input::clone_description(
            rewritten.into(),
            Some((
                args_address as u64,
                &image.bytes()[args_offset..args_offset + CLONE_ARGS_SIZE_VER2],
            )),
            guest.reserved_input_process_exit_signal().ok(),
        );
        let receipt = match control.prepare_injection(&scope, description) {
            Ok(receipt) => receipt,
            Err(error) => {
                control.finish_operation(scope)?;
                return Err(error.into());
            }
        };
        let result = guest
            .inject_staged_input(rewritten, &scope, image, receipt)
            .await;
        // No synthetic grant: finish only releases this operation's consumed
        // capability after the native ledger proves its physical obligations.
        control
            .finish_operation(scope)
            .map_err(|error| InputInjectionError::Fatal(error.to_string()))?;
        result.map(|native| (native, input_matches))
    }

    /// Issue the guest's clone-family call `call` as `clone3` creating its
    /// task with the `set_tid` array `chosen` names, through backend-owned
    /// sealed input, never through a borrowed application stack.
    ///
    /// The added ID can introduce `EEXIST` when held, `EPERM` without the
    /// capability to choose it, or `EINVAL` when out of range. EINVAL may be
    /// attributed to the original call only with [`AddedIdEvidence`], under
    /// its explicit stable-pid_max assumption. Missing evidence keeps the
    /// error ambiguous. For ambiguous errors, and when scratch memory cannot be
    /// written, a run that needs every ID chosen ends, before any task gets a
    /// host ID; any other run issues the guest's own call, whose task, if it
    /// creates one, has a host ID ([`Self::gave_host_id`]). A restart keeps
    /// the ID for the restarted call.
    async fn inject_with_chosen_id<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
        chosen: ChosenId,
        vfork: bool,
    ) -> Result<i64, Errno> {
        let refuse_host_ids = matches!(
            self.cfg.guest_ids,
            detcore_model::pid::GuestIds::Chosen {
                refuse_host_ids: true,
                ..
            }
        );
        let maximum_before = pid_max();
        // A filter can distinguish the staged struct pointer or injection
        // instruction pointer even when every original field is preserved.
        // Only a launch with no inherited filter, and no additional guest
        // filter beyond Reverie's known Allow/Trace filter, can certify this
        // errno. Counting arbitrary inherited filters is not equivalence.
        let mut attribution = (self.cfg.sequentialize_threads
            && self.cfg.inherited_seccomp == detcore_model::config::InheritedSeccomp::Disabled
            && self.clone3_could_fare_otherwise(guest).is_none())
        .then(|| {
            AddedIdEvidence::capture(
                &guest.memory(),
                guest.tid().as_raw(),
                &call,
                &chosen,
                maximum_before,
            )
        })
        .flatten();
        if let Some(evidence) = &attribution
            && crate::tool_global::thread_id_refused(guest, DetTid::from_raw(evidence.id)).await
        {
            attribution = None;
        }
        let ChosenId {
            set_tid,
            id,
            fresh_allocation: _,
            restart,
            args,
            size,
            untraced,
            guest_entries,
        } = chosen;
        // Reject a known bad added ID before rewriting. The sample alone
        // does not establish attribution of a later EINVAL.
        if let Some(cause) = id.and_then(|id| chosen_id_range_refusal(id, maximum_before)) {
            if refuse_host_ids {
                refuse_host_id(
                    guest,
                    format!(
                        "this {} cannot create its task with its chosen ID: {cause}. The guest \
                         has created more tasks than the IDs from its base to pid_max hold; the \
                         run needs a lower base",
                        call.number()
                    ),
                )
                .await
            }
            return self
                .issue_as_made(guest, call, &cause, untraced, vfork)
                .await;
        }
        let cause = match self
            .stage_chosen_clone(guest, call, args, &set_tid, size, attribution.as_ref())
            .await
        {
            Ok((result, staged_matches)) => {
                if !staged_matches {
                    attribution = None;
                }
                // No child exists after EINVAL. Recheck the same allocated
                // value, not a PID obtained from the failed syscall result.
                // Matching samples do not exclude pid_max ABA; the admitted
                // environment must keep that sysctl stable during the attempt.
                if result == Err(Errno::EINVAL)
                    && let Some(evidence) = &attribution
                    && pid_max() == Some(evidence.maximum)
                    && !crate::tool_global::thread_id_refused(guest, DetTid::from_raw(evidence.id))
                        .await
                {
                    return Err(Errno::EINVAL);
                }
                match result {
                    Ok(child) => {
                        debug!(
                            "[detcore, dtid {}] {} created task {child} with set_tid {set_tid:?}",
                            guest.thread_state().dettid,
                            call.number()
                        );
                        return Ok(child);
                    }
                    // Linux restarts the guest's call whole; it keeps its ID.
                    Err(errno) if is_restart(errno) => {
                        if let (Some(id), Some(call)) = (id, restart) {
                            guest.thread_state_mut().restarted_clone_id =
                                Some(RestartedCloneId { id, call });
                        }
                        return Err(errno);
                    }
                    Err(errno) => match id.and_then(|_| set_tid_failure(errno)) {
                        None => return Err(errno),
                        Some(SetTidFailure::Taken) => {
                            let id = id.unwrap();
                            let cause = if guest_entries {
                                format!(
                                    "ID {id}, or one of the guest's own set_tid entries, is \
                                     taken (EEXIST); ID {id}: {}",
                                    holder_of(id)
                                )
                            } else {
                                format!("ID {id} is taken: {}", holder_of(id))
                            };
                            if refuse_host_ids {
                                refuse_host_id(
                                    guest,
                                    format!(
                                        "this {} cannot create its task with its chosen ID: \
                                         {cause}. {}",
                                        call.number(),
                                        self.free_base_hint()
                                    ),
                                )
                                .await
                            }
                            cause
                        }
                        Some(SetTidFailure::NotPermitted) => format!(
                            "the rewrite with ID {} in Hermit's PID namespace failed with EPERM, \
                             taken as the guest task's lacking CAP_SYS_ADMIN and \
                             CAP_CHECKPOINT_RESTORE there (for example in a user namespace of its \
                             own)",
                            id.unwrap()
                        ),
                        Some(SetTidFailure::PossiblyOutOfRange) => format!(
                            "the rewrite with ID {} in Hermit's PID namespace failed with EINVAL; \
                             the added ID lacks complete unchanged-input, stable-pid-range and \
                             registration evidence for attributing that error to the guest",
                            id.unwrap()
                        ),
                    },
                }
            }
            Err(InputInjectionError::Unavailable(error)) => {
                format!("its reserved input is unavailable: {error}")
            }
            Err(InputInjectionError::Fatal(error)) => {
                // The backend has already failed the run before permitting a
                // continuation. This must not become a guest errno or retry.
                panic!("reserved-input native ownership failure: {error}")
            }
        };
        // A run that needs every ID chosen ends here, before the guest's own
        // call could create a task with a host ID (and, for vfork, run it).
        if refuse_host_ids {
            refuse_host_id(
                guest,
                format!(
                    "this {} cannot create its task with a chosen ID: {cause}",
                    call.number()
                ),
            )
            .await
        }
        self.issue_as_made(guest, call, &cause, untraced, vfork)
            .await
    }

    /// Issue the guest's own call `call`, whose task, if it creates one, has
    /// a host ID for `cause` ([`Self::gave_host_id`], or for a `vfork` call
    /// [`Self::vfork_gets_host_id`] before it). An untraced call is not
    /// issued: its task would run outside Detcore's scheduler, so even a run
    /// that takes host IDs ends.
    async fn issue_as_made<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
        cause: &str,
        untraced: bool,
        vfork: bool,
    ) -> Result<i64, Errno> {
        if untraced {
            refuse_untraced(
                guest,
                format!(
                    "this {} cannot create its task with its chosen ID ({cause}), and the guest's \
                     own call would create a task the tracer does not follow (CLONE_UNTRACED)",
                    call.number()
                ),
            )
            .await
        }
        if vfork {
            return self.issue_reported_vfork(guest, call, cause).await;
        }
        let original = crate::reserved_input::inject_original_clone(guest, call).await;
        if let Ok(child) = original {
            self.gave_host_id(guest, child, cause).await;
        }
        original
    }
}

/// A legacy `clone`'s arguments `args` without `CLONE_UNTRACED`, in its
/// flags (the first argument), and nothing else changed.
fn traced_legacy_clone(mut args: SyscallArgs) -> SyscallArgs {
    args.arg0 &= !(libc::CLONE_UNTRACED as usize);
    args
}

/// The ID a restarted clone-family call took (`ThreadState::restarted_clone_id`),
/// and `call`, the call as Detcore normalized it (`RestartedCloneId::call`).
/// Linux restarts the same call with the same arguments, so the restart
/// matches it and keeps the ID. The binding is by arguments, not by call
/// instance: a call in between with other arguments (a signal handler's
/// `clone` with its own stack) takes an ID of its own and retires this one;
/// one with the same arguments (a handler's `fork`) takes this ID, and the
/// restart then takes the next. Either way no ID is given twice, and where
/// the handler runs is decided by the scheduler, so the pairing is the same
/// in every run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RestartedCloneId {
    pub(crate) id: i32,
    pub(crate) call: Vec<u8>,
}

impl RestartedCloneId {
    /// The call's number, the size passed to `clone3`, and the `struct
    /// clone_args` bytes before `set_tid` is filled in, as one byte string.
    fn call(sysno: Sysno, args: &[u8], size: usize) -> Vec<u8> {
        let mut call = Vec::with_capacity(16 + args.len());
        call.extend_from_slice(&(sysno as u64).to_ne_bytes());
        call.extend_from_slice(&(size as u64).to_ne_bytes());
        call.extend_from_slice(args);
        call
    }
}

/// `args` with `CLONE_UNTRACED` cleared.
fn without_untraced(mut args: [u8; CLONE_ARGS_SIZE_VER2]) -> [u8; CLONE_ARGS_SIZE_VER2] {
    let flags = u64::from_ne_bytes(args[..8].try_into().unwrap());
    args[..8].copy_from_slice(&(flags & !(libc::CLONE_UNTRACED as u64)).to_ne_bytes());
    args
}

/// The host's pid_max, read at each call, since it can change: an ID at or
/// above it is not one `set_tid` can give.
fn pid_max() -> Option<i32> {
    std::fs::read_to_string("/proc/sys/kernel/pid_max")
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// A preflight rejection, not proof that a later kernel allocation will
/// accept this ID. A missing or changing pid_max leaves EINVAL ambiguous.
fn chosen_id_range_refusal(id: i32, maximum: Option<i32>) -> Option<String> {
    if id <= 0 {
        Some(format!("chosen ID {id} is not positive"))
    } else {
        maximum
            .filter(|maximum| id >= *maximum)
            .map(|maximum| format!("ID {id} is not below pid_max, {maximum}"))
    }
}

/// How the rewritten `clone3` may have failed because of the ID Detcore
/// added to its `set_tid` array. These are taken as that ID's, the
/// conservative side: a run that needs every ID chosen then ends, and any
/// other run issues the guest's own call, which gets its own answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SetTidFailure {
    /// `EEXIST`: the ID is still allocated, as a live task's or only as an
    /// exited task's process group or session, which no procfs scan can
    /// rule out (alloc_pid refuses any ID still in the namespace's IDR).
    /// Linux's own clone picks a free ID, so without entries of the guest's
    /// it is always the added ID's; with them, one of the guest's own may be
    /// the one taken.
    Taken,
    /// `EPERM`: choosing an ID in Hermit's namespace takes `CAP_SYS_ADMIN`
    /// or `CAP_CHECKPOINT_RESTORE` over it, which the guest's own call may
    /// not need (a task in a user namespace of its own has both there, not
    /// in Hermit's). Another cause of the call's own (a guest entry, an LSM)
    /// cannot be told from it.
    NotPermitted,
    /// `EINVAL`: alloc_pid rejects an out-of-range set_tid entry before
    /// checking the capability. A sampled pid_max cannot rule this out.
    PossiblyOutOfRange,
}

/// Failures the added set_tid entry may cause (kernel/pid.c, alloc_pid).
/// EINVAL remains ambiguous without the complete conditional AddedIdEvidence.
/// When that evidence is absent and fallback is permitted, only the original
/// call can establish its own result.
fn set_tid_failure(errno: Errno) -> Option<SetTidFailure> {
    match errno {
        Errno::EEXIST => Some(SetTidFailure::Taken),
        Errno::EPERM => Some(SetTidFailure::NotPermitted),
        Errno::EINVAL => Some(SetTidFailure::PossiblyOutOfRange),
        _ => None,
    }
}

/// The kernel's limit on PID namespace nesting (`MAX_PID_NS_LEVEL`), and so
/// on `set_tid_size`.
const MAX_SET_TID: usize = 32;

/// A guest `clone3`'s own `set_tid` array, from its `struct clone_args`
/// `args`, or `None` where the kernel refuses the call on that array before
/// creating a task (`copy_clone_args_from_user`): an address without a size
/// or the reverse, or more than `MAX_PID_NS_LEVEL` entries (`EINVAL`), or an
/// array it cannot read (`EFAULT`). The entries themselves are left to the
/// kernel.
fn read_guest_set_tid<M: MemoryAccess>(
    memory: &M,
    witness: FaultWitness,
    args: &[u8; CLONE_ARGS_SIZE_VER2],
) -> Result<Vec<libc::pid_t>, SetTidUnread> {
    let (address, size) = guest_set_tid_shape(args).ok_or(SetTidUnread::Refused)?;
    let mut bytes = vec![0u8; size * std::mem::size_of::<libc::pid_t>()];
    if !within_user_memory(address as usize, bytes.len()) {
        return Err(if range_faults(witness, address as usize, bytes.len()) {
            SetTidUnread::Refused
        } else {
            SetTidUnread::Unreadable
        });
    }
    // Exactly the array, with the guest's own access, as copy_from_user
    // reads it: a debugger read could reach past it into an unmapped page.
    let read = memory.read_exact_with_user_access(
        Addr::<u8>::from_raw(address as usize).ok_or(SetTidUnread::Refused)?,
        &mut bytes,
    );
    if read.is_err() {
        return Err(if range_faults(witness, address as usize, bytes.len()) {
            SetTidUnread::Refused
        } else {
            SetTidUnread::Unreadable
        });
    }
    Ok(bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| libc::pid_t::from_ne_bytes(*chunk))
        .collect())
}

/// Read the guest's `pid_t` array at `address` into `tids` exactly, as
/// `copy_from_user` reads it, with the guest's own access: never a debugger
/// read, which could reach past the array into the next page, or read a page
/// the guest cannot. `None` if that read fails; the caller then decides by
/// [`range_faults`] whether the kernel must fault too.
pub(crate) fn read_pid_array<M: MemoryAccess>(
    memory: &M,
    address: usize,
    tids: &mut [libc::pid_t],
) -> Option<()> {
    const ENTRY: usize = std::mem::size_of::<libc::pid_t>();
    let mut bytes = vec![0u8; tids.len() * ENTRY];
    memory
        .read_exact_with_user_access(Addr::<u8>::from_raw(address)?, &mut bytes)
        .ok()?;
    for (tid, chunk) in tids.iter_mut().zip(bytes.as_chunks::<ENTRY>().0) {
        *tid = libc::pid_t::from_ne_bytes(*chunk);
    }
    Some(())
}

/// Why a guest `clone3`'s own `set_tid` array was not read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SetTidUnread {
    /// Every kernel refuses the call before creating a task: the array's
    /// address and size disagree, or it is longer than `MAX_PID_NS_LEVEL`
    /// (`EINVAL`), or reading it is proved to fault (`EFAULT`,
    /// [`range_faults`]).
    Refused,
    /// Detcore could not read it with the guest's own access, and no fault
    /// is proved: the kernel may read it (a page mapped writable, not
    /// readable).
    Unreadable,
}

/// The address and length of a guest `clone3`'s own `set_tid` array, unless
/// `copy_clone_args_from_user` refuses them (`EINVAL`).
fn guest_set_tid_shape(args: &[u8; CLONE_ARGS_SIZE_VER2]) -> Option<(u64, usize)> {
    let field = |offset: usize| u64::from_ne_bytes(args[offset..offset + 8].try_into().unwrap());
    let (address, size) = (field(SET_TID_OFFSET), field(SET_TID_SIZE_OFFSET));
    (address != 0 && size != 0 && size <= MAX_SET_TID as u64).then_some((address, size as usize))
}

/// How many PID namespaces out of the one a clone-family call with `flags`
/// by thread `tid` creates its task in Hermit's own is: 0 for Hermit's, one
/// more with `CLONE_NEWPID`, and the depth of a namespace the guest made for
/// its children (`pid_for_children`), walked up with `NS_GET_PARENT`. A
/// kernel without PID namespaces (`/proc/self/ns` without `pid`) has only
/// the one. `None` if a namespace cannot be read (no `/proc/self/ns` at
/// all), or the walk does not reach Hermit's within the kernel's nesting
/// limit; a depth beyond that limit, where Linux would refuse a
/// `CLONE_NEWPID` with `ENOSPC`, also takes the host-ID path.
fn pid_namespace_depth(tid: i32, flags: u64) -> Option<usize> {
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;
    use std::os::unix::fs::MetadataExt;
    // ioctl_ns(2): _IO(0xb7, 0x2).
    const NS_GET_PARENT: u64 = 0xb702;
    let new = usize::from(flags & libc::CLONE_NEWPID as u64 != 0);
    let own = match std::fs::metadata("/proc/self/ns/pid") {
        Ok(own) => own,
        // No `pid` entry beside the others: a kernel without PID namespaces.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return std::path::Path::new("/proc/self/ns")
                .is_dir()
                .then_some(new);
        }
        Err(_) => return None,
    };
    let mut namespace = std::fs::File::open(format!("/proc/{tid}/ns/pid_for_children")).ok()?;
    for depth in 0..=MAX_SET_TID {
        let here = namespace.metadata().ok()?;
        if (here.dev(), here.ino()) == (own.dev(), own.ino()) {
            return Some(depth + new);
        }
        // SAFETY: NS_GET_PARENT takes no argument and returns a new file
        // descriptor, owned from here on, or -1.
        let parent = unsafe { libc::ioctl(namespace.as_raw_fd(), NS_GET_PARENT as _) };
        if parent < 0 {
            return None;
        }
        // SAFETY: `parent` is a file descriptor nothing else owns.
        namespace = unsafe { std::fs::File::from_raw_fd(parent) };
    }
    None
}

/// Whether `errno` is one of the kernel's restart codes, which a call returns
/// only to be restarted.
fn is_restart(errno: Errno) -> bool {
    [
        Errno::ERESTARTSYS,
        Errno::ERESTARTNOINTR,
        Errno::ERESTARTNOHAND,
        Errno::ERESTART_RESTARTBLOCK,
    ]
    .contains(&errno)
}

/// A `clone3` call's `struct clone_args`, all the bytes it passes, if they
/// can be read and number at most a page, the kernel's limit (E2BIG).
pub(crate) fn read_clone_args<M: MemoryAccess>(
    memory: &M,
    witness: FaultWitness,
    clone: &syscalls::Clone3,
) -> Clone3Read {
    let Ok(page) = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }) else {
        return Clone3Read::Unreadable { flags: None };
    };
    let size = clone.size();
    let raw = clone.args().map_or(0, |args| args.as_raw());
    // Only a range within user memory is read: past it there is nothing to
    // read, and the address arithmetic of a read near the top of the address
    // space would overflow.
    let address = Addr::<u8>::from_raw(raw).filter(|_| within_user_memory(raw, size));
    clone3_read_decision(
        size,
        page,
        || {
            // Exactly the struct, with the guest's own access, as
            // copy_from_user reads it. Detcore cannot read NULL at all.
            let mut bytes = vec![0u8; size];
            memory
                .read_exact_with_user_access(address?, &mut bytes)
                .ok()
                .map(|()| bytes)
        },
        || range_faults(witness, raw, size),
        || {
            // Its flags, read past the protection, still say whether the
            // call is untraced.
            let mut flags = [0u8; 8];
            memory
                .read_exact(address?, &mut flags)
                .ok()
                .map(|()| u64::from_ne_bytes(flags))
        },
    )
}

/// How a guest `clone3`'s `struct clone_args` of `size` bytes is taken, on
/// pages of `page` bytes: refused before any read (`EINVAL` under the first
/// version, `E2BIG` over a page, as `copy_clone_args_from_user` checks), its
/// bytes if `read` gets them, refused if reading it is proved to fault
/// (`faults`, NULL included: page 0 can be mapped), else unreadable, with
/// the flags `flags` gets, if any.
fn clone3_read_decision(
    size: usize,
    page: usize,
    read: impl FnOnce() -> Option<Vec<u8>>,
    faults: impl FnOnce() -> bool,
    flags: impl FnOnce() -> Option<u64>,
) -> Clone3Read {
    if size < CLONE_ARGS_SIZE_VER0 || size > page {
        return Clone3Read::Refused;
    }
    if let Some(bytes) = read() {
        return Clone3Read::Bytes(bytes);
    }
    if faults() {
        return Clone3Read::Refused;
    }
    Clone3Read::Unreadable { flags: flags() }
}

/// A guest `clone3`'s `struct clone_args`, as Detcore read it to plan the
/// call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Clone3Read {
    /// Its bytes, read as `copy_from_user` reads them.
    Bytes(Vec<u8>),
    /// Every kernel refuses the call before creating a task: the struct is
    /// shorter than its first version (`EINVAL`) or larger than a page
    /// (`E2BIG`), or reading it is proved to fault (`EFAULT`,
    /// [`range_faults`]).
    Refused,
    /// Detcore could not read it with the guest's access, and no fault is
    /// proved: the kernel may read it (a page mapped writable, not
    /// readable). Its flags, if Detcore could read them otherwise.
    Unreadable { flags: Option<u64> },
}

/// The end of user memory, exclusive (`TASK_SIZE_MAX`, page aligned), from
/// `user_ptr_max`, the highest start `access_ok` accepts for a four-byte
/// word. Kernels up to 6.1 check the word's end, so that start is four bytes
/// short of the end; later x86 kernels check only a small access's start,
/// against `TASK_SIZE_MAX` itself, relying on the unmapped page there. Both
/// round down to the end.
fn user_memory_end() -> Option<usize> {
    let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok()?;
    let word_end = super::threads::user_ptr_max().checked_add(std::mem::size_of::<u32>())?;
    Some(word_end & !(page - 1))
}

/// What tells Detcore whether a guest range that it could not read with the
/// guest's own access must fault in the kernel too.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FaultWitness {
    /// The calling thread's host mappings (`/proc/<tid>/smaps`; the leader's
    /// are empty once it has exited), on a backend through whose host tasks
    /// a tool may read guest threads (`host_task_reads_reach_guest_threads`,
    /// today ptrace): see [`range_faults`].
    HostMaps(i32),
    /// Nothing: on any other backend no host `/proc` entry is known to
    /// describe the guest thread (KVM's and DBT's tids are their own, and no
    /// tool read through LiteInst's or SaBRe's host tasks is validated), and
    /// the backend's own failed read may not be the kernel's (an in-guest
    /// runtime reads its own memory with `process_vm_readv`, which a page
    /// mapped writable but not readable refuses while the guest's `clone3`
    /// reads it). A failed read proves no fault, so the call is refused by
    /// name rather than handed to the kernel unchecked.
    Unproven,
}

impl FaultWitness {
    /// The witness for the calling thread `tid` under `backend`.
    pub(crate) fn for_backend(backend: &reverie::BackendCapabilities, tid: i32) -> Self {
        if backend.host_task_reads_reach_guest_threads {
            FaultWitness::HostMaps(tid)
        } else {
            FaultWitness::Unproven
        }
    }
}

/// Whether some byte of the guest's `address..address + len`, which Detcore
/// could not read with the guest's own access, is one the kernel's
/// `copy_from_user` faults on too (`EFAULT`), by `witness`: from the calling
/// thread's mappings (`/proc/<tid>/smaps`, see [`range_faults_in`]) where
/// those are known to be the guest's; false when nothing is proved, as when
/// the mappings cannot be read or are not known to be the guest's. Reading smaps walks the page
/// tables of every mapping, which is why it is read only after a read with
/// the guest's own access has failed, a rare event.
pub(crate) fn range_faults(witness: FaultWitness, address: usize, len: usize) -> bool {
    // An address in the kernel half faults on every kernel, whatever the
    // backend.
    if in_kernel_half(address) {
        return true;
    }
    let tid = match witness {
        FaultWitness::HostMaps(tid) => tid,
        FaultWitness::Unproven => return false,
    };
    let Some(limit) = user_memory_end() else {
        return false;
    };
    let Ok(smaps) = std::fs::read_to_string(format!("/proc/{tid}/smaps")) else {
        return false;
    };
    range_faults_in(&smaps, limit, address, len)
}

/// Whether `address..address + len` lies within user memory, where Detcore
/// can try to read it. Reverie's remote read splits a range at the next page
/// with arithmetic that overflows in the top page
/// (https://github.com/rrnewton/reverie/issues/1185), so nothing past user
/// memory is ever passed to it.
pub(crate) fn within_user_memory(address: usize, len: usize) -> bool {
    user_memory_end()
        .zip(address.checked_add(len))
        .is_some_and(|(end, last)| last <= end)
}

/// Whether an address at or beyond the end of user memory faults whatever
/// the task: one in the kernel half does, but one that may be a tagged user
/// pointer proves nothing. arm64's `access_ok` untags a pointer for a task
/// that enabled the tagged-address ABI (`PR_SET_TAGGED_ADDR_CTRL`) by sign
/// extending bit 55, so bit 55 set is the kernel half and a nonzero top byte
/// with it clear may be a tag. x86 linear address masking would tag bits 48
/// (or 57) to 62, never bit 63. Detcore's prctl allowlist does not let a
/// guest enable tagging today, so the tagged case is a guard, not a path a
/// guest takes.
fn past_end_faults(address: usize) -> bool {
    in_kernel_half(address) || (cfg!(target_arch = "aarch64") && address >> 56 == 0)
}

/// Whether `address` lies in the kernel half of the address space, which no
/// user access reaches on any kernel, tagged or not: bit 63 on x86-64, bit 55
/// on arm64 (where untagging sign-extends it).
fn in_kernel_half(address: usize) -> bool {
    if cfg!(target_arch = "aarch64") {
        address & (1 << 55) != 0
    } else {
        address & (1 << 63) != 0
    }
}

/// Whether some byte of `address..address + len` is, by `smaps` (a
/// `/proc/<tid>/smaps` text), one the kernel's `copy_from_user` faults on in
/// every case:
/// - from below `limit`, the end of user memory (exclusive), to at or beyond
///   it;
/// - in a mapping without any permission (`---p`, such as a thread stack's
///   guard);
/// - in no mapping, below a mapping that does not grow down. A mapping that
///   grows down (`VmFlags` `gd`, a stack) is extended into the hole below it
///   on a fault, within the faulting task's own `RLIMIT_STACK`, which is the
///   guest's, not the tracer's, so such a hole proves nothing.
///
/// False when nothing is proved, as when `smaps` lists no mapping or a
/// record is malformed. An address at or beyond `limit` is judged by its
/// shape alone ([`past_end_faults`]).
fn range_faults_in(smaps: &str, limit: usize, address: usize, len: usize) -> bool {
    if address >= limit {
        return past_end_faults(address);
    }
    let Some(end) = address.checked_add(len.max(1)) else {
        return true;
    };
    if end > limit {
        return true;
    }
    // Each mapping's start, end, whether it permits any access (unknown for
    // a malformed permission field) and whether it grows down (unknown
    // until exactly one well-formed VmFlags record follows it).
    struct Mapping {
        start: usize,
        stop: usize,
        permitted: Option<bool>,
        grows_down: Option<bool>,
        flag_records: u32,
    }
    let mut mappings: Vec<Mapping> = Vec::new();
    for line in smaps.lines() {
        if let Some(flags) = line.strip_prefix("VmFlags:") {
            if let Some(last) = mappings.last_mut() {
                last.flag_records += 1;
                let flags: Vec<&str> = flags.split_whitespace().collect();
                let well_formed = !flags.is_empty()
                    && flags.iter().all(|flag| {
                        flag.len() == 2 && flag.bytes().all(|b| b.is_ascii_lowercase())
                    });
                last.grows_down =
                    (well_formed && last.flag_records == 1).then(|| flags.contains(&"gd"));
            }
            continue;
        }
        let mut fields = line.split_whitespace();
        let Some((start, stop)) = fields.next().and_then(|range| range.split_once('-')) else {
            continue;
        };
        let (Ok(start), Ok(stop), Some(perms)) = (
            usize::from_str_radix(start, 16),
            usize::from_str_radix(stop, 16),
            fields.next(),
        ) else {
            continue;
        };
        let perms = perms.as_bytes();
        let permitted = (perms.len() == 4
            && matches!(perms[0], b'r' | b'-')
            && matches!(perms[1], b'w' | b'-')
            && matches!(perms[2], b'x' | b'-')
            && matches!(perms[3], b'p' | b's'))
        .then(|| perms[..3] != *b"---");
        mappings.push(Mapping {
            start,
            stop,
            permitted,
            grows_down: None,
            flag_records: 0,
        });
    }
    if mappings.is_empty() {
        return false;
    }
    let mut at = address;
    while at < end {
        match mappings
            .iter()
            .find(|mapping| (mapping.start..mapping.stop).contains(&at))
        {
            Some(Mapping {
                permitted: Some(false),
                ..
            }) => return true,
            Some(Mapping {
                permitted: Some(true),
                stop,
                ..
            }) => at = *stop,
            Some(Mapping {
                permitted: None, ..
            }) => return false,
            None => {
                let above = mappings
                    .iter()
                    .filter(|mapping| mapping.start > at)
                    .min_by_key(|mapping| mapping.start);
                // Proved only below a mapping known not to grow down (or
                // below none at all).
                return matches!(
                    above,
                    None | Some(Mapping {
                        grows_down: Some(false),
                        ..
                    })
                );
            }
        }
    }
    false
}

/// End a run that needs every ID chosen (`GuestIds::Chosen::refuse_host_ids`:
/// `--strict` or an explicit `--pid-base`) because a task would have, or has,
/// a host ID: `reason` says why. The run ends with Hermit's policy-refusal
/// status.
async fn refuse_host_id<T, G>(guest: &mut G, reason: String) -> !
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let reason = format!(
        "this run needs every guest process ID chosen (--strict or --pid-base), and a task would \
         have, or has, a host ID, which differs from run to run: {reason}"
    );
    tracing::error!("[detcore, dtid {}] {reason}", guest.thread_state().dettid);
    crate::tool_global::report_policy_refusal(guest, reason).await;
    crate::tool_global::unrecoverable_shutdown(guest, detcore_model::HERMIT_POLICY_REFUSAL_EXIT)
        .await
}

/// End the run, whatever its guest IDs, because the guest's clone-family
/// call, as made, would create a task the tracer does not follow
/// (`CLONE_UNTRACED`), which would run outside Detcore's scheduler, and
/// Detcore cannot rewrite it without the flag: `reason` says why. The run
/// ends with Hermit's policy-refusal status.
async fn refuse_untraced<T, G>(guest: &mut G, reason: String) -> !
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let reason = format!(
        "a task the tracer does not follow (CLONE_UNTRACED) would run outside Detcore's \
         scheduler: {reason}"
    );
    tracing::error!("[detcore, dtid {}] {reason}", guest.thread_state().dettid);
    crate::tool_global::report_policy_refusal(guest, reason).await;
    crate::tool_global::unrecoverable_shutdown(guest, detcore_model::HERMIT_POLICY_REFUSAL_EXIT)
        .await
}

/// What holds `id` on the host, for a refusal: the task's name and its
/// process, or that only a process group or session holds it.
pub fn holder_of(id: i32) -> String {
    let status = std::fs::read_to_string(format!("/proc/{id}/status")).ok();
    let field = |name: &str| {
        status.as_deref().and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix(name))
                .map(|value| value.trim().to_owned())
        })
    };
    match (field("Name:"), field("Tgid:")) {
        (Some(name), Some(tgid)) if tgid == id.to_string() => {
            format!("process {id} ({name})")
        }
        (Some(name), Some(tgid)) => format!("thread {id} ({name}) of process {tgid}"),
        _ => format!(
            "no live task: a process group or session of an exited process keeps ID {id}, or \
             its holder has exited since"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(call: &ChosenIdCall) -> ([u64; 11], usize) {
        match call {
            ChosenIdCall::Clone3 { args, size } => {
                let mut fields = [0u64; 11];
                for (field, chunk) in fields.iter_mut().zip(args.as_chunks::<8>().0) {
                    *field = u64::from_ne_bytes(*chunk);
                }
                (fields, *size)
            }
            other => panic!("expected a clone3 form, got {other:?}"),
        }
    }

    fn clone_args(flags: u64, sp: u64, ptid: u64, ctid: u64, tls: u64) -> SyscallArgs {
        #[cfg(target_arch = "aarch64")]
        let (arg3, arg4) = (tls, ctid);
        #[cfg(not(target_arch = "aarch64"))]
        let (arg3, arg4) = (ctid, tls);
        SyscallArgs {
            arg0: flags as usize,
            arg1: sp as usize,
            arg2: ptid as usize,
            arg3: arg3 as usize,
            arg4: arg4 as usize,
            arg5: 0,
        }
    }

    const THREAD_FLAGS: u64 = (libc::CLONE_VM
        | libc::CLONE_FS
        | libc::CLONE_FILES
        | libc::CLONE_SIGHAND
        | libc::CLONE_THREAD
        | libc::CLONE_SYSVSEM
        | libc::CLONE_SETTLS
        | libc::CLONE_PARENT_SETTID
        | libc::CLONE_CHILD_CLEARTID) as u64;

    /// A C library's thread creation keeps every field, in clone3's order,
    /// with the new stack pointer as the top of a one-byte stack.
    #[test]
    fn a_thread_creation_keeps_every_field() {
        let call = clone3_form(
            Sysno::clone,
            &clone_args(THREAD_FLAGS, 0x7000_1000, 0x5000, 0x5000, 0x6000),
            None,
        );
        assert_eq!(
            fields(&call),
            (
                [
                    THREAD_FLAGS,
                    0x5000,
                    0x5000,
                    0x5000,
                    0,
                    0x7000_0fff,
                    1,
                    0x6000,
                    0,
                    0,
                    0
                ],
                CLONE_ARGS_SIZE_VER1
            )
        );
    }

    /// The exit signal leaves the flags; beside CLONE_THREAD or CLONE_PARENT
    /// it becomes 0, which copy_process ignores anyway.
    #[test]
    fn the_exit_signal_moves_out_of_the_flags() {
        let sigchld = libc::SIGCHLD as u64;
        let (process, _) = fields(&clone3_form(
            Sysno::clone,
            &clone_args(sigchld, 0, 0, 0, 0),
            None,
        ));
        assert_eq!(
            (process[0], process[4], process[5], process[6]),
            (0, sigchld, 0, 0)
        );
        for shared in [
            libc::CLONE_THREAD | libc::CLONE_SIGHAND | libc::CLONE_VM,
            libc::CLONE_PARENT,
        ] {
            let (fields, _) = fields(&clone3_form(
                Sysno::clone,
                &clone_args(shared as u64 | sigchld, 0, 0, 0, 0),
                None,
            ));
            assert_eq!((fields[0], fields[4]), (shared as u64, 0));
        }
    }

    #[test]
    fn invalid_original_signals_are_not_normalized_into_valid_clones() {
        for shared in [
            libc::CLONE_PARENT,
            libc::CLONE_THREAD | libc::CLONE_SIGHAND | libc::CLONE_VM,
        ] {
            for signal in [65, 128, 255] {
                let call = clone3_form(
                    Sysno::clone,
                    &clone_args(shared as u64 | signal, 0, 0, 0, 0),
                    None,
                );
                assert_eq!(call, ChosenIdCall::Invalid(Errno::EINVAL));
            }
            for signal in [0, libc::SIGCHLD as u64, 64] {
                let (valid, _) = fields(&clone3_form(
                    Sysno::clone,
                    &clone_args(shared as u64 | signal, 0, 0, 0, 0),
                    None,
                ));
                assert_eq!((valid[0], valid[4]), (shared as u64, 0));
            }
        }
    }

    /// Bits above the low 32 never reach the kernel from clone; neither does
    /// CLONE_DETACHED, which clone3 refuses.
    #[test]
    fn high_bits_and_clone_detached_are_dropped() {
        let flags = (1u64 << 40) | libc::CLONE_DETACHED as u64 | libc::CLONE_FILES as u64;
        let (fields, _) = fields(&clone3_form(
            Sysno::clone,
            &clone_args(flags, 0, 0, 0, 0),
            None,
        ));
        assert_eq!(fields[0], libc::CLONE_FILES as u64);
    }

    /// A call every kernel refuses before creating a task goes unchanged;
    /// CLONE_PIDFD beside CLONE_THREAD, which only kernels before 6.9 refuse,
    /// is rewritten, and its clone3 fails as the call would. An exit signal
    /// clone3 cannot carry is inexpressible, except beside CLONE_THREAD or
    /// CLONE_PARENT, where original-signal validation must precede conversion.
    #[test]
    fn refused_and_inexpressible_calls() {
        let detached = (libc::CLONE_PIDFD | libc::CLONE_DETACHED) as u64;
        assert_eq!(
            clone3_form(Sysno::clone, &clone_args(detached, 0, 0, 0, 0), None),
            ChosenIdCall::Unchanged
        );
        let pidfd_thread =
            (libc::CLONE_PIDFD | libc::CLONE_THREAD | libc::CLONE_SIGHAND | libc::CLONE_VM) as u64;
        let (pidfd_fields, _) = fields(&clone3_form(
            Sysno::clone,
            &clone_args(pidfd_thread, 0, 0, 0, 0),
            None,
        ));
        assert_eq!(pidfd_fields[0], pidfd_thread);
        assert!(matches!(
            clone3_form(Sysno::clone, &clone_args(0x80, 0, 0, 0, 0), None),
            ChosenIdCall::Inexpressible(_)
        ));
        let thread = (libc::CLONE_THREAD | libc::CLONE_SIGHAND | libc::CLONE_VM) as u64;
        assert_eq!(
            clone3_form(Sysno::clone, &clone_args(thread | 0x80, 0, 0, 0, 0), None),
            ChosenIdCall::Invalid(Errno::EINVAL)
        );

        // Known no-child combinations go to the original kernel even if
        // their stack cannot be converted. UNTRACED still takes the
        // existing traced-original path, retaining all other registers.
        for untraced in [0, libc::CLONE_UNTRACED] {
            for invalid in [
                libc::CLONE_THREAD,
                libc::CLONE_THREAD | libc::CLONE_VM,
                libc::CLONE_SIGHAND,
                libc::CLONE_THREAD | libc::CLONE_SIGHAND,
            ] {
                let args = clone_args(
                    (invalid | untraced) as u64 | libc::SIGCHLD as u64,
                    1,
                    0x1234,
                    0x2345,
                    0x3456,
                );
                assert_eq!(
                    clone3_form(Sysno::clone, &args, None),
                    ChosenIdCall::Unchanged
                );
                let mut expected = args;
                expected.arg0 &= !(libc::CLONE_UNTRACED as usize);
                assert_eq!(traced_legacy_clone(args), expected);
            }
            for valid in [
                0,
                libc::CLONE_VM,
                libc::CLONE_SIGHAND | libc::CLONE_VM,
                libc::CLONE_THREAD | libc::CLONE_SIGHAND | libc::CLONE_VM,
            ] {
                let flags = (valid | untraced) as u64;
                let (converted, _) = fields(&clone3_form(
                    Sysno::clone,
                    &clone_args(flags, 0, 0, 0, 0),
                    None,
                ));
                assert_eq!(converted[0], flags);
            }
        }
        // Original-signal validation still precedes this no-child route.
        assert_eq!(
            clone3_form(
                Sysno::clone,
                &clone_args(libc::CLONE_THREAD as u64 | 65, 1, 0, 0, 0),
                None
            ),
            ChosenIdCall::Invalid(Errno::EINVAL)
        );
        // The legacy-only proof does not alter clone3 size or set_tid handling.
        let mut guest_args = [0; CLONE_ARGS_SIZE_VER2];
        guest_args[..8].copy_from_slice(&(libc::CLONE_SIGHAND as u64).to_ne_bytes());
        let (converted, _) = fields(&clone3_form(
            Sysno::clone3,
            &clone_args(0, 0, 0, 0, 0),
            Some(&guest_args),
        ));
        assert_eq!(converted[0], libc::CLONE_SIGHAND as u64);
    }

    /// clone's last two arguments are the child TID address and the TLS on
    /// x86-64, and the other way round on arm64 (`CONFIG_CLONE_BACKWARDS`):
    /// each order puts each in its clone3 field, whichever architecture runs
    /// the test.
    #[test]
    fn both_clone_argument_orders() {
        // clone(flags, newsp, parent_tidptr, arg3, arg4).
        let args = SyscallArgs::new(THREAD_FLAGS as usize, 0, 0x5000, 0x3333, 0x4444, 0);
        for (order, child_tid, tls) in [
            (CloneArgOrder::ChildTidThenTls, 0x3333, 0x4444),
            (CloneArgOrder::TlsThenChildTid, 0x4444, 0x3333),
        ] {
            let (fields, _) = fields(&LegacyClone::of_clone(&args, order).as_clone3());
            // clone_args: flags, pidfd, child_tid, parent_tid, exit_signal,
            // stack, stack_size, tls.
            assert_eq!((fields[2], fields[3], fields[7]), (child_tid, 0x5000, tls));
        }
        #[cfg(target_arch = "aarch64")]
        assert_eq!(CloneArgOrder::NATIVE, CloneArgOrder::TlsThenChildTid);
        #[cfg(target_arch = "x86_64")]
        assert_eq!(CloneArgOrder::NATIVE, CloneArgOrder::ChildTidThenTls);
    }

    /// fork and vfork are the kernel's fixed argument sets.
    #[cfg(not(target_arch = "aarch64"))]
    #[test]
    fn fork_and_vfork() {
        let none = clone_args(0, 0, 0, 0, 0);
        let (fork, _) = fields(&clone3_form(Sysno::fork, &none, None));
        assert_eq!((fork[0], fork[4]), (0, libc::SIGCHLD as u64));
        let (vfork, _) = fields(&clone3_form(Sysno::vfork, &none, None));
        assert_eq!(
            (vfork[0], vfork[4]),
            (
                (libc::CLONE_VFORK | libc::CLONE_VM) as u64,
                libc::SIGCHLD as u64
            )
        );
    }

    /// A guest's clone3 keeps its bytes and grows to the size that carries
    /// set_tid. One that sets set_tid itself (also within a short struct,
    /// whose bytes from 64 are set_tid's) is the guest's choice. One the
    /// kernel refuses for its size or bytes (under 64; over 88 with a nonzero
    /// byte past them; unreadable) goes unchanged, and a longer one whose
    /// extra bytes are zero is its first 88.
    #[test]
    fn a_guest_clone3() {
        let none = SyscallArgs::new(0, 0, 0, 0, 0, 0);
        let mut ver0 = [0u8; CLONE_ARGS_SIZE_VER0];
        ver0[..8].copy_from_slice(&(libc::CLONE_FILES as u64).to_ne_bytes());
        let (ver0_fields, size) = fields(&clone3_form(Sysno::clone3, &none, Some(&ver0)));
        assert_eq!(
            (ver0_fields[0], size),
            (libc::CLONE_FILES as u64, CLONE_ARGS_SIZE_VER1)
        );

        let mut own = [0u8; CLONE_ARGS_SIZE_VER2];
        own[SET_TID_OFFSET..SET_TID_OFFSET + 8].copy_from_slice(&0x1000u64.to_ne_bytes());
        own[SET_TID_SIZE_OFFSET..SET_TID_SIZE_OFFSET + 8].copy_from_slice(&1u64.to_ne_bytes());
        assert_eq!(
            clone3_form(Sysno::clone3, &none, Some(&own)),
            ChosenIdCall::GuestChosen {
                args: own,
                size: CLONE_ARGS_SIZE_VER2
            }
        );
        let mut short = [0u8; 70];
        short[SET_TID_OFFSET] = 1;
        assert!(matches!(
            clone3_form(Sysno::clone3, &none, Some(&short)),
            ChosenIdCall::GuestChosen { .. }
        ));
        // Shorter than the first version: EINVAL from every kernel.
        assert_eq!(
            clone3_form(Sysno::clone3, &none, Some(&[0u8; 32])),
            ChosenIdCall::Unchanged
        );
        // A nonzero tail a newer kernel may read, or a struct Detcore could
        // not read, is not proved refused: it takes the host-ID path.
        for unproven in [Some(&[[0u8; 96], [1u8; 96]].concat()[..]), None] {
            assert!(matches!(
                clone3_form(Sysno::clone3, &none, unproven),
                ChosenIdCall::Inexpressible(_)
            ));
        }
        assert!(matches!(
            clone3_form(Sysno::getpid, &none, None),
            ChosenIdCall::Inexpressible(_)
        ));
        let mut long = [0u8; 96];
        long[..8].copy_from_slice(&(libc::CLONE_FILES as u64).to_ne_bytes());
        let (long_fields, size) = fields(&clone3_form(Sysno::clone3, &none, Some(&long)));
        assert_eq!(
            (long_fields[0], size),
            (libc::CLONE_FILES as u64, CLONE_ARGS_SIZE_VER2)
        );
    }

    #[test]
    fn set_tid_is_filled_in() {
        let args = ChosenIdCall::with_set_tid([0u8; CLONE_ARGS_SIZE_VER2], 0x1234, 2);
        assert_eq!(
            u64::from_ne_bytes(args[SET_TID_OFFSET..SET_TID_OFFSET + 8].try_into().unwrap()),
            0x1234
        );
        assert_eq!(
            u64::from_ne_bytes(
                args[SET_TID_SIZE_OFFSET..SET_TID_SIZE_OFFSET + 8]
                    .try_into()
                    .unwrap()
            ),
            2
        );
    }

    /// The depth of the PID namespace a call creates its task in, out of
    /// Hermit's: this test's own namespace is Hermit's here, so 0, and one
    /// more with CLONE_NEWPID.
    #[test]
    fn a_pid_namespace_depth_counts_from_hermits() {
        let own = unsafe { libc::gettid() };
        assert_eq!(super::pid_namespace_depth(own, 0), Some(0));
        assert_eq!(
            super::pid_namespace_depth(own, libc::CLONE_NEWPID as u64),
            Some(1)
        );
    }

    /// A restarted call keeps its ID only for the same call: the same number,
    /// size and normalized arguments, compared byte for byte.
    #[test]
    fn a_restarted_clone_keeps_its_id_only_for_the_same_call() {
        use super::RestartedCloneId;
        let args = [7u8; super::CLONE_ARGS_SIZE_VER2];
        let mut other = args;
        other[0] ^= 1;
        let call = RestartedCloneId::call(Sysno::clone, &args, 64);
        assert_eq!(call, RestartedCloneId::call(Sysno::clone, &args, 64));
        assert_ne!(call, RestartedCloneId::call(Sysno::clone, &other, 64));
        assert_ne!(call, RestartedCloneId::call(Sysno::clone3, &args, 64));
        assert_ne!(call, RestartedCloneId::call(Sysno::clone, &args, 88));
    }

    /// A range sample cannot establish that EINVAL is the original call's
    /// error; the handler must obtain that call's own result on fallback.
    #[test]
    fn an_added_id_can_also_make_the_rewrite_invalid() {
        use super::SetTidFailure;
        assert_eq!(
            super::set_tid_failure(Errno::EEXIST),
            Some(SetTidFailure::Taken)
        );
        assert_eq!(
            super::set_tid_failure(Errno::EPERM),
            Some(SetTidFailure::NotPermitted)
        );
        assert_eq!(
            super::set_tid_failure(Errno::EINVAL),
            Some(SetTidFailure::PossiblyOutOfRange)
        );
        for errno in [
            Errno::EFAULT,
            Errno::EBADF,
            Errno::EACCES,
            Errno::EAGAIN,
            Errno::EINTR,
        ] {
            assert_eq!(super::set_tid_failure(errno), None, "{errno}");
        }
        for maximum in [None, Some(100)] {
            for id in [i32::MIN, -1, 0] {
                assert!(super::chosen_id_range_refusal(id, maximum).is_some());
            }
            assert_eq!(super::chosen_id_range_refusal(99, maximum), None);
        }
        assert!(super::chosen_id_range_refusal(100, Some(100)).is_some());
        assert_eq!(super::chosen_id_range_refusal(100, None), None);
    }

    /// A guest's own set_tid array is read only where the kernel would: an
    /// address and a size both set, at most MAX_PID_NS_LEVEL entries.
    #[test]
    fn a_guest_set_tid_array_has_the_shape_the_kernel_reads() {
        let shape = |address: u64, size: u64| {
            let mut args = [0u8; CLONE_ARGS_SIZE_VER2];
            args[SET_TID_OFFSET..SET_TID_OFFSET + 8].copy_from_slice(&address.to_ne_bytes());
            args[SET_TID_SIZE_OFFSET..SET_TID_SIZE_OFFSET + 8].copy_from_slice(&size.to_ne_bytes());
            super::guest_set_tid_shape(&args)
        };
        assert_eq!(shape(0x1000, 1), Some((0x1000, 1)));
        assert_eq!(shape(0x1000, 32), Some((0x1000, 32)));
        assert_eq!(shape(0x1000, 0), None);
        assert_eq!(shape(0, 1), None);
        assert_eq!(shape(0x1000, 33), None);
    }

    /// The rewrite drops CLONE_UNTRACED and nothing else.
    #[test]
    fn the_rewrite_drops_only_clone_untraced() {
        let flags = (libc::CLONE_VM | libc::CLONE_UNTRACED | libc::CLONE_SETTLS) as u64;
        let mut args = [9u8; CLONE_ARGS_SIZE_VER2];
        args[..8].copy_from_slice(&flags.to_ne_bytes());
        let cleared = super::without_untraced(args);
        assert_eq!(
            u64::from_ne_bytes(cleared[..8].try_into().unwrap()),
            (libc::CLONE_VM | libc::CLONE_SETTLS) as u64
        );
        assert_eq!(cleared[8..], args[8..]);
    }

    /// A legacy clone whose new stack pointer clone3 cannot express (1, or
    /// beyond user memory) is inexpressible rather than rewritten into an
    /// EINVAL of clone3's own.
    #[test]
    fn a_stack_pointer_clone3_cannot_express_is_inexpressible() {
        for sp in [1usize, (super::USER_ADDRESS_LIMIT + 1) as usize] {
            let args = SyscallArgs::new(libc::SIGCHLD as usize, sp, 0, 0, 0, 0);
            assert!(
                matches!(
                    clone3_form(Sysno::clone, &args, None),
                    ChosenIdCall::Inexpressible(why) if why.contains("stack pointer")
                ),
                "{sp:#x}"
            );
        }
        let args = SyscallArgs::new(libc::SIGCHLD as usize, 0x7fff_0000_1000, 0, 0, 0, 0);
        assert!(matches!(
            clone3_form(Sysno::clone, &args, None),
            ChosenIdCall::Clone3 { .. }
        ));
    }

    /// A failed read proves EFAULT only for a range that starts in user
    /// memory and crosses its end, or a byte in a mapping without
    /// permissions, or one in a hole below a mapping known not to grow
    /// down.
    #[test]
    fn only_a_definite_fault_is_taken_as_the_kernels_refusal() {
        let smaps = "\
1000-2000 rw-p 00000000 00:00 0
VmFlags: rd wr mr mw me ac
2000-3000 ---p 00000000 00:00 0
VmFlags: mr mw me ac
5000-6000 -w-p 00000000 00:00 0
VmFlags: wr mr mw me ac
9000-a000 rw-p 00000000 00:00 0 [stack]
VmFlags: rd wr mr mw me gd ac
";
        let limit = 0x10000;
        let faults =
            |address: usize, len: usize| super::range_faults_in(smaps, limit, address, len);
        // Mapped and permitted, even writable only: nothing is proved.
        assert!(!faults(0x1ff0, 0x10));
        assert!(!faults(0x5000, 88));
        // A mapping without permissions, entered from a readable one.
        assert!(faults(0x1ff8, 0x10));
        // A hole below a mapping that does not grow down.
        assert!(faults(0x4000, 8));
        assert!(faults(0x1000 - 4, 8));
        // A hole below a stack, which the guest's fault may grow into.
        assert!(!faults(0x8ff8, 8));
        // The end of user memory, exclusive, for 1, 4, 8 and 88 bytes.
        for len in [1, 4, 8, 88] {
            assert!(!super::range_faults_in(
                "f000-10000 -w-p 00000000 00:00 0\nVmFlags: wr\n",
                limit,
                limit - len,
                len
            ));
            // One byte more crosses the end (a single byte cannot, as its
            // address would be the end itself).
            if len > 1 {
                assert!(faults(limit - len + 1, len));
            }
        }
        // At or beyond the end, an address in the kernel half faults, but
        // one that may be a tagged user pointer proves nothing.
        assert!(faults(usize::MAX - 4095, 88));
        assert!(faults(usize::MAX - 2, 8));
        assert!(!faults(0x0f00_0000_0000_4000, 8));
        // Untagged, just past the end: arm64 faults; x86 cannot tell it from
        // a linear-address-masked pointer.
        assert_eq!(faults(limit, 8), cfg!(target_arch = "aarch64"));
        // No mappings listed: nothing is proved.
        assert!(!super::range_faults_in("", limit, 0x4000, 8));
        // Only ptrace's guest threads are read through their host tasks; on
        // every other backend a failed read proves nothing.
        use reverie::BackendCapabilities as B;
        for backend in [B::PTRACE, B::E9PATCH] {
            assert_eq!(
                super::FaultWitness::for_backend(&backend, 7),
                super::FaultWitness::HostMaps(7)
            );
        }
        for backend in [B::LITEINST_IN_GUEST, B::SABRE, B::DBT, B::KVM] {
            assert_eq!(
                super::FaultWitness::for_backend(&backend, 7),
                super::FaultWitness::Unproven
            );
        }
        // Except in the kernel half, which faults on every backend.
        assert!(!super::range_faults(
            super::FaultWitness::Unproven,
            0x4000,
            8
        ));
        assert!(super::range_faults(
            super::FaultWitness::Unproven,
            usize::MAX - 4095,
            88
        ));
        // A mapping whose VmFlags are missing might grow down: a hole below
        // it proves nothing. Below no mapping at all, it does.
        let unflagged = "5000-6000 rw-p 00000000 00:00 0\n";
        assert!(!super::range_faults_in(unflagged, limit, 0x4000, 8));
        assert!(super::range_faults_in(unflagged, limit, 0x7000, 8));
        // Page zero, when mapped with some permission, is read like any other
        // page.
        assert!(!super::range_faults_in(
            "0-1000 rw-p 00000000 00:00 0\nVmFlags: rd wr\n",
            limit,
            0,
            88
        ));
        // An empty, malformed or repeated VmFlags record, or a malformed
        // permission field, is no evidence.
        for record in [
            "VmFlags:\n",
            "VmFlags: rd wr GD\n",
            "VmFlags: rd wr\nVmFlags: rd wr\n",
        ] {
            let smaps = format!("5000-6000 rw-p 00000000 00:00 0\n{record}");
            assert!(
                !super::range_faults_in(&smaps, limit, 0x4000, 8),
                "{record:?}"
            );
        }
        assert!(!super::range_faults_in(
            "5000-6000 --- 00000000 00:00 0\nVmFlags: mr\n",
            limit,
            0x5000,
            8
        ));
        // The end of user memory is a page boundary, at most four bytes past
        // the last four-byte word access_ok accepts.
        let end = super::user_memory_end().unwrap();
        let word = super::super::threads::user_ptr_max();
        assert_eq!(end % 4096, 0, "{end:#x}");
        assert!(word < end + 4096 && end <= word + 4, "{word:#x} {end:#x}");
    }

    /// A clone3 struct is refused before any read for its size, read when it
    /// can be, refused when reading it is proved to fault (NULL included),
    /// and otherwise unreadable, with whatever flags could be read.
    #[test]
    fn a_clone3_struct_is_refused_only_when_every_kernel_refuses_it() {
        use super::Clone3Read;
        let decide = |size: usize, read: Option<Vec<u8>>, faults: bool, flags: Option<u64>| {
            super::clone3_read_decision(size, 4096, || read, || faults, || flags)
        };
        assert_eq!(
            decide(32, Some(vec![0; 32]), false, None),
            Clone3Read::Refused
        );
        assert_eq!(decide(8192, None, false, None), Clone3Read::Refused);
        assert_eq!(
            decide(88, Some(vec![1; 88]), false, None),
            Clone3Read::Bytes(vec![1; 88])
        );
        // NULL, or any other address: unread, and a fault proved or not.
        assert_eq!(decide(88, None, true, None), Clone3Read::Refused);
        assert_eq!(
            decide(88, None, false, None),
            Clone3Read::Unreadable { flags: None }
        );
        assert_eq!(
            decide(88, None, false, Some(7)),
            Clone3Read::Unreadable { flags: Some(7) }
        );
    }

    /// A legacy clone issued without CLONE_UNTRACED keeps every other bit
    /// and argument.
    #[test]
    fn a_traced_legacy_clone_drops_only_clone_untraced() {
        let flags = (libc::CLONE_UNTRACED | libc::CLONE_PIDFD | libc::CLONE_DETACHED) as usize
            | libc::SIGCHLD as usize;
        let args = SyscallArgs::new(flags, 0x1000, 0x2000, 0x3000, 0x4000, 0x5000);
        let traced = super::traced_legacy_clone(args);
        assert_eq!(traced.arg0, flags & !(libc::CLONE_UNTRACED as usize));
        assert_eq!(
            (
                traced.arg1,
                traced.arg2,
                traced.arg3,
                traced.arg4,
                traced.arg5
            ),
            (0x1000, 0x2000, 0x3000, 0x4000, 0x5000)
        );
    }
}
