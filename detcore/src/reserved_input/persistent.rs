/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Persistent native references, classified before their actual import. A
//! mutable nested-pointer snapshot is not a certificate for its later use.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::Weak;

use reverie::Guest;
use reverie::native_syscall::NativeOperation;
use reverie::native_syscall::NativeOperationCompletion;
use reverie::native_syscall::NativeSyscallAbi;
use reverie::native_syscall::NativeSyscallCall;
use reverie::native_syscall::NativeSyscallEvent;
use reverie::native_syscall::NativeSyscallFacts;
use reverie::native_syscall::NativeSyscallId;
use reverie::native_syscall::NativeSyscallObserver;
use reverie::native_syscall::NativeSyscallPurpose;
use reverie::reserved_input::FileTableLifetime;
use reverie::reserved_input::InputAccess;
use reverie::reserved_input::InputAccessDirection;
use reverie::reserved_input::InputAccessLease;
use reverie::reserved_input::InputAccessLifetime;
use reverie::reserved_input::InputDescriptorFacts;
use reverie::reserved_input::InputExposure;
use reverie::reserved_input::InputFilesEvent;
use reverie::reserved_input::InputFilesObserver;
use reverie::reserved_input::InputNativeFacts;
use reverie::reserved_input::InputRunId;
use reverie::reserved_input::InputSpan;
use reverie::reserved_input::InputTarget;
use reverie::reserved_input::InputTaskId;
use reverie::reserved_input::InputUnavailable;
use reverie::reserved_input::ReservedInputControl;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;

use super::Span;
use super::capability;
use super::effective_spans;
use crate::Detcore;
use crate::record_or_replay::RecordOrReplay;

const PROC_SUPER_MAGIC: u64 = 0x9fa0;
const PR_SET_MEMORY_MERGE: usize = 67;

/// A negative is established from the actual retained OFD. A guessed target
/// PID is never converted into a RegionLifetime: proc mem keeps its original
/// MM across exec and numeric PID reuse.
fn may_be_process_memory(facts: &InputDescriptorFacts, mountinfo: Option<&[u8]>) -> bool {
    match facts.filesystem {
        Ok(fs) if fs != PROC_SUPER_MAGIC => return false,
        Err(_) => return true,
        _ => {}
    }
    let Some(path) = mountinfo.and_then(|mounts| super::objects::internal_path(facts, mounts))
    else {
        return true;
    };
    let parts: Vec<_> = path
        .split(|byte| *byte == b'/')
        .filter(|s| !s.is_empty())
        .collect();
    let pid = |s: &[u8]| !s.is_empty() && s.iter().all(u8::is_ascii_digit);
    match parts.as_slice() {
        [id, leaf] => pid(id) && *leaf == b"mem",
        [id, task, tid, leaf] => pid(id) && *task == b"task" && pid(tid) && *leaf == b"mem",
        _ => false,
    }
}

fn is_known_uffd_import(request: usize) -> bool {
    // Linux UAPI userfaultfd.h: _IOWR(0xaa, 0, uffdio_register[32])
    // and _IOWR(0xaa, 5, uffdio_move[40]), on both admitted native ABIs.
    // REGISTER retains a range; MOVE changes its mapping. Do not classify
    // another direction/size or an unrelated ioctl by its low command byte.
    matches!(request, 0xc020_aa00 | 0xc028_aa05)
}

/// Whether a future background read/write has an unbound file effect.
///
/// Neither a private table nor a sampled ordinary OFD certifies the later
/// native lookup: ordinary injection preparation can forward a caught signal
/// and single-step a handler instruction before restoring the syscall inputs.
/// The current backend exposes no final stopped binding for ordinary fdget.
/// A possibly shared table also permits a peer replacement after turn release.
///
/// The caller therefore adds UnknownTarget transient access for any possible
/// descriptor read/write, alongside its ordinary local buffers, and retains
/// it through actual native completion, decoder/copyout and deterministic
/// readmission. This is not a permanent taint on every read or write.
pub(crate) fn background_descriptor_effect_is_unverified<
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
>(
    guest: &G,
    call: Syscall,
) -> bool {
    capability(guest).is_some() && descriptor_io_may_access(call)
}

fn descriptor_io_may_access(call: Syscall) -> bool {
    if !matches!(
        call.name(),
        "read"
            | "pread64"
            | "readv"
            | "preadv"
            | "preadv2"
            | "write"
            | "pwrite64"
            | "writev"
            | "pwritev"
            | "pwritev2"
    ) {
        return false;
    }
    let (_, args) = call.into_parts();
    // These are immutable syscall-register negatives, not sampled OFD facts.
    if (args.arg0 as i32) < 0 || args.arg2 == 0 {
        return false;
    }
    !matches!(call.name(), "pread64" | "pwrite64") || (args.arg3 as i64) >= 0
}

fn ancillary_import_may_export(call: Syscall) -> bool {
    let (_, args) = call.into_parts();
    (args.arg0 as i32) >= 0
        && match call.name() {
            "sendmsg" => true,
            "sendmmsg" => args.arg2 != 0,
            _ => false,
        }
}

/// A direct positional offset is immutable syscall input; a file cursor or
/// indirect vector length is not. Never read lseek/fdinfo and call the sampled
/// shared OFD cursor a bound on the later native write. A proc-mem description
/// targets the MM retained in file->private_data, including across holder exec,
/// so an unproved target remains global even when the offset is known.
fn proc_write_may_reach(call: Syscall, reserved: Span) -> bool {
    let (_, args) = call.into_parts();
    if args.arg2 == 0 {
        return false;
    }
    if call.name() == "pwrite64" {
        if (args.arg3 as i64) < 0 {
            return false; // ksys_pwrite64 rejects before fdget/vfs_write.
        }
        return effective_spans(args.arg3 as u64, args.arg2 as u64)
            .into_iter()
            .any(|span| span.overlaps(reserved));
    }
    true // Shared cursor or mutable iovec extent is unproved.
}

/// The old early hook is no longer an authority to classify persistent native
/// effects. The actual final-stopped Tool observation below runs after native
/// preparation, including any caught-signal instruction. Caller removal is a
/// separate mechanical cleanup; retaining this seam publishes no cause.
pub(crate) fn before_native_call<T: RecordOrReplay, G: Guest<Detcore<T>>>(
    _guest: &G,
    _call: Syscall,
) -> Result<(), reverie::Error> {
    Ok(())
}

/// Scheduler export bookkeeping may still inspect an early ancillary snapshot;
/// it does not certify what the native kernel will import. The final observer
/// independently classifies sendmsg/sendmmsg, including a readable empty list.
pub(crate) fn before_descriptor_export<T: RecordOrReplay, G: Guest<Detcore<T>>>(
    _guest: &G,
    _descriptors: &[i32],
    _unread: bool,
) -> Result<(), reverie::Error> {
    Ok(())
}

/// Actual Tool observation. A background observation must be associated with
/// its explicit outer operation, which retains facts until logical readmission.
/// Publishing a permanent cause at a host-timed stop could change even the
/// named refusal while the temporary access lease is still present.
pub(crate) fn observe_native_syscall(
    facts: &dyn NativeSyscallFacts,
    call: &NativeSyscallCall,
) -> Result<Option<Arc<dyn NativeSyscallObserver>>, reverie::Error> {
    if call.purpose == NativeSyscallPurpose::BackendPrivate {
        return Ok(None);
    }
    let authority = facts.authority().map_err(anyhow::Error::new)?;
    if authority.grant.is_none() {
        if !call.operation_bound {
            return Err(anyhow::anyhow!(
                "reserved-input native effect lacks an explicit background operation owner"
            )
            .into());
        }
        // The explicitly associated BackgroundAccess operation invokes
        // collect_native_exposures at this same stop, without recapturing it.
        return Ok(None);
    }
    let control = facts
        .control()
        .ok_or_else(|| anyhow::anyhow!("reserved-input native observation lacks control"))?;
    publish_native_exposures(&control, collect_native_exposures(facts, call)?)?;
    Ok(None)
}

/// Publish only under an actual foreground grant or at the outer operation's
/// authenticated logical readmission. Never call from a background completion
/// observer, from Drop, or while holding a policy/history lock.
pub(crate) fn publish_native_exposures(
    control: &ReservedInputControl,
    exposures: Vec<InputExposure>,
) -> Result<(), reverie::Error> {
    for exposure in exposures {
        control.add_exposure(exposure).map_err(anyhow::Error::new)?;
    }
    Ok(())
}

/// Classify exact immutable syscall operands and final-stopped native facts.
/// This function records no exposure and does not change staging availability.
/// The background owner retains the returned facts with that physical attempt;
/// neither receipt arrival order nor a later benign FD census can erase them.
pub(crate) fn collect_native_exposures(
    facts: &dyn NativeSyscallFacts,
    native: &NativeSyscallCall,
) -> Result<Vec<InputExposure>, reverie::Error> {
    if native.purpose == NativeSyscallPurpose::BackendPrivate {
        return Ok(Vec::new());
    }
    let control = facts
        .control()
        .ok_or_else(|| anyhow::anyhow!("reserved-input native classifier lacks control"))?;
    let authority = facts.authority().map_err(anyhow::Error::new)?;
    if facts.task() != Some(authority.registration.task) {
        return Err(anyhow::anyhow!("reserved-input native classifier task mismatch").into());
    }
    if !native_syscall_abi_is_verified(native) {
        // This is facility unavailability, not rejection or reinterpretation
        // of the native call. The background owner publishes only at its
        // existing logical readmission; a host-timed callback never does.
        return Ok(vec![InputExposure {
            target: InputTarget::Unknown,
            cause: "native-syscall-abi-unverified".into(),
        }]);
    }
    let Some(call) = policy_syscall(native) else {
        return Ok(Vec::new());
    };
    let local = InputTarget::Region(authority.registration.region);
    let reserved = control
        .layout(authority.registration.task)
        .map_or(Span::Unknown, |layout| {
            Span::bytes(layout.base, layout.page_bytes as u64)
        });
    let overlaps = |address, length| {
        effective_spans(address, length)
            .into_iter()
            .any(|span| span.overlaps(reserved))
    };
    let mut exposures = Vec::new();
    let mut expose = |target, cause: &'static str| {
        exposures.push(InputExposure {
            target,
            cause: cause.into(),
        });
    };
    let (_, args) = call.into_parts();
    match call.name() {
        "madvise" if args.arg2 == libc::MADV_MERGEABLE as usize => {
            if overlaps(args.arg0 as u64, args.arg1 as u64) {
                expose(local, "mergeable-reserved-page");
            }
        }
        "prctl" if args.arg0 == PR_SET_MEMORY_MERGE && args.arg1 != 0 => {
            expose(local, "mergeable-reserved-page");
        }
        "vmsplice" if (args.arg0 as i32) >= 0 && args.arg2 != 0 => {
            // An ordinary iovec remains mutable during kernel import. No
            // supplied memory snapshot is an immutable native payload receipt.
            expose(local, "unverified-persistent-import");
        }
        "sendto" if args.arg3 & libc::MSG_ZEROCOPY as usize != 0 => {
            if (args.arg0 as i32) >= 0 && overlaps(args.arg1 as u64, args.arg2 as u64) {
                expose(local, "pinned-reserved-source");
            }
        }
        "sendmsg" | "sendmmsg" if ancillary_import_may_export(call) => {
            let flags = if call.name() == "sendmsg" {
                args.arg2
            } else {
                args.arg3
            };
            if flags & libc::MSG_ZEROCOPY as usize != 0 {
                expose(local, "unverified-persistent-import");
            }
            // A final header snapshot would still be mutable to an already
            // running kernel writer. No empty-control or model-peer exemption.
            expose(InputTarget::Unknown, "unverified-persistent-import");
            expose(InputTarget::Unknown, "exported-process-memory-authority");
            expose(
                InputTarget::Unknown,
                "exported-description-state-unverified",
            );
        }
        "ioctl" => {
            if (args.arg0 as i32) >= 0 && is_known_uffd_import(args.arg1) {
                // A private native table binds this final pin through lookup.
                // On a possibly shared table the pin alone cannot discharge a
                // known persistent import after background release.
                let unproved_uffd = !authority.files_proven_unshared
                    || facts.descriptor(args.arg0 as i32).map_or(true, |fd| {
                        // userfaultfd_fops is installed on an anon_inode by
                        // Linux fs/userfaultfd.c; a different native filesystem
                        // cannot dispatch this object's REGISTER/MOVE handler.
                        fd.filesystem.as_ref().map_or(true, |fs| *fs == 0x0904_1934)
                    });
                if unproved_uffd {
                    expose(InputTarget::Unknown, "unverified-persistent-import");
                }
            }
            // Accepted S5 covers the direct argument only. Opaque nested
            // retained pointers and unknown-driver effects remain uncovered.
            if overlaps(args.arg2 as u64, 1) {
                expose(local, "unknown-ioctl-reserved-argument");
            }
        }
        "write" | "pwrite64" | "writev" | "pwritev" | "pwritev2" => {
            if (args.arg0 as i32) < 0 || !proc_write_may_reach(call, reserved) {
                return Ok(exposures);
            }
            // An unbound shared-table background write is handled by the
            // separate history/outer-operation protocol; a host-timed pin is
            // not evidence of which OFD the future fdget will import. That
            // protocol's overlap-membership closure remains an explicit
            // implementation obligation, not an ordinary-file exemption here.
            if authority.grant.is_none() && !authority.files_proven_unshared {
                return Ok(exposures);
            }
            let snapshot = facts.descriptors();
            let possible = snapshot.as_ref().map_or(true, |snapshot| {
                snapshot
                    .descriptors
                    .iter()
                    .find(|fd| fd.fd == args.arg0 as i32)
                    .is_none_or(|fd| {
                        let cannot_write = fd.status_flags.as_ref().is_ok_and(|flags| {
                            *flags & libc::O_PATH != 0 || *flags & libc::O_ACCMODE == libc::O_RDONLY
                        });
                        !cannot_write
                            && may_be_process_memory(fd, snapshot.mountinfo.as_deref().ok())
                    })
            });
            if possible {
                expose(InputTarget::Unknown, "forced-reserved-write");
            }
        }
        _ => {}
    }
    Ok(exposures)
}

/// Decode only explicitly classified native syscall numbers. Unknown numbers
/// remain unknown; narrowing/truncating them into a different Sysno is forbidden.
fn policy_syscall(call: &NativeSyscallCall) -> Option<Syscall> {
    if !native_syscall_abi_is_verified(call) {
        return None;
    }
    use reverie::syscalls::SyscallArgs;
    use reverie::syscalls::Sysno;
    let number = [
        Sysno::madvise,
        Sysno::prctl,
        Sysno::vmsplice,
        Sysno::sendto,
        Sysno::sendmsg,
        Sysno::sendmmsg,
        Sysno::ioctl,
        Sysno::write,
        Sysno::pwrite64,
        Sysno::writev,
        Sysno::pwritev,
        Sysno::pwritev2,
    ]
    .into_iter()
    .find(|number| *number as u64 == call.number)?;
    let [arg0, arg1, arg2, arg3, arg4, arg5] = call.args.map(|arg| usize::try_from(arg).ok());
    Some(Syscall::from_raw(
        number,
        SyscallArgs {
            arg0: arg0?,
            arg1: arg1?,
            arg2: arg2?,
            arg3: arg3?,
            arg4: arg4?,
            arg5: arg5?,
        },
    ))
}

/// Native-number decoding requires the actual stopped ABI, not the host Rust
/// target alone. x32 retains its distinguishing bit and is never native x86-64.
pub(crate) fn native_syscall_abi_is_verified(call: &NativeSyscallCall) -> bool {
    native_abi_is_verified(call.abi, call.number)
}

/// The same ABI predicate at background admission, before the final native
/// request exists. It does not publish a cause or certify instruction ownership.
pub(crate) fn native_abi_is_verified(abi: NativeSyscallAbi, number: u64) -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        abi == NativeSyscallAbi::Linux {
            audit_arch: 0xc000_003e,
        } && number & 0x4000_0000 == 0
    }
    #[cfg(target_arch = "aarch64")]
    {
        let _ = number;
        abi == NativeSyscallAbi::Linux {
            audit_arch: 0xc000_00b7,
        }
    }
}

/// C and W_seen are Linux policy history, outside all serialized Tool state.
/// Native completion callbacks retain facts only. Logical readmission publishes
/// the conjunction before the caller retires its last temporary exclusion.
#[derive(Debug, Default, Clone)]
struct TableHistory {
    capability_seen: bool,
    write_seen: bool,
    pending: BTreeSet<u64>,
    descriptor_unavailability: Option<Arc<str>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReferenceKind {
    Acquisition,
    Write,
}

#[derive(Debug, Default)]
struct AttemptEvidence {
    issued: bool,
    not_run: bool,
    ended: bool,
    eligible_write: bool,
    acquired_capability: bool,
}

impl AttemptEvidence {
    fn observe(&mut self, event: NativeSyscallEvent) -> Result<(), reverie::Error> {
        match event {
            NativeSyscallEvent::ResumeIssued => {
                if self.issued || self.ended {
                    return Err(
                        anyhow::anyhow!("native reference issue repeated or settled").into(),
                    );
                }
                self.issued = true;
            }
            NativeSyscallEvent::ProvedNotRun => {
                if self.ended {
                    return Err(anyhow::anyhow!("native reference not-run after settlement").into());
                }
                self.not_run = true;
                self.ended = true;
            }
            NativeSyscallEvent::NativeResult(_) => {
                if !self.issued || self.ended {
                    return Err(
                        anyhow::anyhow!("native reference result without live issue").into(),
                    );
                }
                self.ended = true;
            }
            NativeSyscallEvent::EffectsComplete => {
                if !self.issued || self.not_run {
                    return Err(anyhow::anyhow!("native reference completion without issue").into());
                }
                // This event is an authenticated native completion even when
                // cancellation lost the result. It establishes no descriptor
                // membership and does not itself retire an access lease.
                self.ended = true;
            }
            NativeSyscallEvent::ResultUnobserved => {
                if !self.issued {
                    return Err(anyhow::anyhow!("native reference lost result before issue").into());
                }
                // Uncertainty never establishes effects-ended or NotRun.
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
struct ReferenceRecord {
    kind: ReferenceKind,
    source: FileTableLifetime,
    task: InputTaskId,
    tables: BTreeSet<FileTableLifetime>,
    attempts: HashMap<NativeSyscallId, AttemptEvidence>,
    copied_accesses: Vec<InputAccessLease>,
    completion: Option<NativeOperationCompletion>,
}

#[derive(Debug, Default)]
struct NativeHistory {
    next_operation: u64,
    tables: BTreeMap<FileTableLifetime, TableHistory>,
    operations: BTreeMap<u64, ReferenceRecord>,
}

/// The control owns this observer and hence its run's policy state. The weak
/// index owns neither the control nor any run state. There is no control/state
/// reference cycle, and indexing does not order guest decisions.
#[derive(Debug)]
struct FilesHistoryObserver(Arc<Mutex<NativeHistory>>);

impl InputFilesObserver for FilesHistoryObserver {
    fn observe(
        &self,
        control: &ReservedInputControl,
        event: InputFilesEvent,
    ) -> Result<(), InputUnavailable> {
        if event.source.files == event.destination.files {
            let cause = self
                .0
                .lock()
                .unwrap()
                .tables
                .get(&event.source.files)
                .ok_or_else(|| InputUnavailable::new("native-file-history-source-missing"))?
                .descriptor_unavailability
                .clone();
            if let Some(cause) = cause {
                control.add_exposure(InputExposure {
                    target: InputTarget::Region(event.destination.region),
                    cause,
                })?;
            }
            return Ok(());
        }
        if event.shared != Some(false) {
            return Err(InputUnavailable::new(
                "native-file-history-sharing-unverified",
            ));
        }
        let (pending_acquisitions, descriptor_unavailability) = {
            let mut state = self.0.lock().unwrap();
            let copied = state
                .tables
                .get(&event.source.files)
                .cloned()
                .ok_or_else(|| InputUnavailable::new("native-file-history-source-missing"))?;
            if state.tables.contains_key(&event.destination.files) {
                return Err(InputUnavailable::new("native-file-history-copy-repeated"));
            }
            // Snapshot C and W_seen NOW. Link only operations pending at this
            // authenticated birth/exec cut; later parent operations cannot leak
            // into a distinct table through a dynamic parent lookup.
            let mut pending_acquisitions = Vec::new();
            for id in &copied.pending {
                let operation = state.operations.get_mut(id).ok_or_else(|| {
                    InputUnavailable::new("native-file-history-operation-missing")
                })?;
                operation.tables.insert(event.destination.files);
                if operation.kind == ReferenceKind::Acquisition {
                    pending_acquisitions.push((
                        *id,
                        operation.task,
                        operation.completion.clone().ok_or_else(|| {
                            InputUnavailable::new("native-file-history-operation-unbound")
                        })?,
                    ));
                }
            }
            let descriptor_unavailability = copied.descriptor_unavailability.clone();
            state.tables.insert(event.destination.files, copied);
            (pending_acquisitions, descriptor_unavailability)
        };
        // An exec census does not prove the earlier inherited descriptor
        // history. Keep its exact existing local unavailability, never C=true
        // or a fabricated actual-acquisition receipt.
        if let Some(cause) = descriptor_unavailability {
            control.add_exposure(InputExposure {
                target: InputTarget::Region(event.destination.region),
                cause,
            })?;
        }
        // A private source acquisition may not own a global access lease.
        // Protect only this newly visible MM until its captured pending owner
        // classifies the native membership at logical readmission. This runs
        // before child publication, under the parent's existing ordinary grant
        // or pending-vfork barrier; no readmission can interleave this copy cut.
        // Neither registry is entered while holding the other one's leaf lock.
        for (id, task, completion) in pending_acquisitions {
            let lease = control.acquire_access(
                task,
                InputAccess {
                    target: InputTarget::Region(event.destination.region),
                    span: InputSpan::Unknown,
                    direction: InputAccessDirection::ReadWrite,
                    lifetime: InputAccessLifetime::Transient,
                    cause: "native-descriptor-acquisition".into(),
                },
            )?;
            control.bind_access_completion(&lease, task, &completion)?;
            self.0
                .lock()
                .unwrap()
                .operations
                .get_mut(&id)
                .ok_or_else(|| InputUnavailable::new("native-file-history-copy-cut-interleaved"))?
                .copied_accesses
                .push(lease);
        }
        Ok(())
    }
}

type HistoryState = Arc<Mutex<NativeHistory>>;

fn histories() -> &'static Mutex<HashMap<InputRunId, Weak<Mutex<NativeHistory>>>> {
    static HISTORIES: OnceLock<Mutex<HashMap<InputRunId, Weak<Mutex<NativeHistory>>>>> =
        OnceLock::new();
    HISTORIES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn install_native_history(control: &ReservedInputControl) -> Result<(), reverie::Error> {
    let state = Arc::new(Mutex::new(NativeHistory::default()));
    control
        .install_files_observer(Arc::new(FilesHistoryObserver(state.clone())))
        .map_err(anyhow::Error::new)?;
    let mut index = histories().lock().unwrap();
    index.retain(|_, state| state.strong_count() != 0);
    if index
        .insert(control.run_identity(), Arc::downgrade(&state))
        .is_some()
    {
        return Err(anyhow::anyhow!("native file history installed twice").into());
    }
    Ok(())
}

fn history(control: &ReservedInputControl) -> Result<HistoryState, reverie::Error> {
    histories()
        .lock()
        .unwrap()
        .get(&control.run_identity())
        .and_then(Weak::upgrade)
        .ok_or_else(|| anyhow::anyhow!("native file history not installed").into())
}

fn writable_process_memory(
    descriptor: &InputDescriptorFacts,
    mountinfo: Option<&[u8]>,
) -> Result<bool, InputUnavailable> {
    let flags = descriptor
        .status_flags
        .as_ref()
        .map_err(|_| InputUnavailable::new("descriptor-native-status-unverified"))?;
    if *flags & libc::O_PATH != 0 || *flags & libc::O_ACCMODE == libc::O_RDONLY {
        return Ok(false);
    }
    let (_, _, mode) = descriptor
        .identity
        .as_ref()
        .map_err(|_| InputUnavailable::new("descriptor-native-identity-unverified"))?;
    if *mode & libc::S_IFMT != libc::S_IFREG {
        return Ok(false);
    }
    let filesystem = descriptor
        .filesystem
        .as_ref()
        .map_err(|_| InputUnavailable::new("descriptor-native-filesystem-unverified"))?;
    if *filesystem != PROC_SUPER_MAGIC {
        return Ok(false);
    }
    let mounts =
        mountinfo.ok_or_else(|| InputUnavailable::new("descriptor-control-object-unverified"))?;
    if super::objects::internal_path(descriptor, mounts).is_none() {
        return Err(InputUnavailable::new(
            "descriptor-control-object-unverified",
        ));
    }
    Ok(may_be_process_memory(descriptor, Some(mounts)))
}

/// Establish the actual native table identity even when another initial-state
/// check makes staging unavailable. This is identity bookkeeping only; it does
/// not certify absent process-memory authority or qualify the image.
pub(crate) fn register_initial_history(
    control: &ReservedInputControl,
    task: InputTaskId,
) -> Result<(), reverie::Error> {
    let registration = control
        .registration(task)
        .ok_or_else(|| anyhow::anyhow!("initial native descriptor owner unregistered"))?;
    history(control)?
        .lock()
        .unwrap()
        .tables
        .entry(registration.files)
        .or_default();
    Ok(())
}

/// Inspect the actual census independently of other image admission checks.
/// Known C is retained even if local rseq or another unrelated condition made
/// this image unqualified. Unknown descriptor proof keeps its exact existing
/// initial cause through copied tables and exec, without manufacturing C.
/// The caller combines this with the original admission; setup qualification
/// remains a separate authenticated backend receipt.
pub(crate) fn observe_initial_descriptors(
    control: &ReservedInputControl,
    facts: &InputNativeFacts,
) -> Result<Option<InputUnavailable>, reverie::Error> {
    let registration = control
        .registration(facts.task)
        .ok_or_else(|| anyhow::anyhow!("initial native descriptor owner unregistered"))?;
    let mut capability_seen = false;
    let mut unavailability = None;
    match &facts.descriptors {
        Ok(descriptors) => {
            for descriptor in descriptors {
                match writable_process_memory(descriptor.facts(), facts.mountinfo.as_deref().ok()) {
                    Ok(known) => capability_seen |= known,
                    Err(cause) => {
                        unavailability.get_or_insert(cause);
                    }
                }
            }
        }
        Err(_) => {
            unavailability = Some(InputUnavailable::new(
                "initial-descriptor-census-unverified",
            ));
        }
    }
    let state = history(control)?;
    let (publish, retained) = {
        let mut state = state.lock().unwrap();
        let table = state.tables.entry(registration.files).or_default();
        table.capability_seen |= capability_seen;
        if let Some(cause) = unavailability {
            table
                .descriptor_unavailability
                .get_or_insert_with(|| cause.reason().into());
        }
        (
            table.capability_seen && table.write_seen,
            table.descriptor_unavailability.clone(),
        )
    };
    if publish {
        publish_native_exposures(control, vec![forced_write_exposure()])?;
    }
    Ok(retained.map(InputUnavailable::new))
}

fn forced_write_exposure() -> InputExposure {
    InputExposure {
        target: InputTarget::Unknown,
        cause: "forced-reserved-write".into(),
    }
}

/// Local operation owner. Dropping this handle does not remove its registry
/// record. The caller separately owns access leases and authenticated cleanup.
#[derive(Debug, Clone)]
pub(crate) struct NativeReferenceOperation {
    state: HistoryState,
    operation: u64,
    control: ReservedInputControl,
    task: InputTaskId,
}

impl NativeReferenceOperation {
    pub(crate) fn acquisition(
        control: &ReservedInputControl,
        task: InputTaskId,
    ) -> Result<Self, reverie::Error> {
        Self::begin(control, task, ReferenceKind::Acquisition)
    }

    pub(crate) fn background_write(
        control: &ReservedInputControl,
        task: InputTaskId,
        call: Syscall,
    ) -> Result<Option<Self>, reverie::Error> {
        if !matches!(
            call.name(),
            "write" | "pwrite64" | "writev" | "pwritev" | "pwritev2"
        ) || !descriptor_io_may_access(call)
        {
            return Ok(None);
        }
        let reserved = control.layout(task).map_or(Span::Unknown, |layout| {
            Span::bytes(layout.base, layout.page_bytes as u64)
        });
        if !proc_write_may_reach(call, reserved) {
            return Ok(None);
        }
        Self::begin(control, task, ReferenceKind::Write).map(Some)
    }

    fn begin(
        control: &ReservedInputControl,
        task: InputTaskId,
        kind: ReferenceKind,
    ) -> Result<Self, reverie::Error> {
        let registration = control
            .registration(task)
            .ok_or_else(|| anyhow::anyhow!("native reference owner unregistered"))?;
        let state = history(control)?;
        let operation = {
            let mut state = state.lock().unwrap();
            if !state.tables.contains_key(&registration.files) {
                return Err(
                    anyhow::anyhow!("native reference table has no initial history").into(),
                );
            }
            let operation = state.next_operation;
            state.next_operation = operation
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("native reference operation exhausted"))?;
            state
                .tables
                .get_mut(&registration.files)
                .unwrap()
                .pending
                .insert(operation);
            state.operations.insert(
                operation,
                ReferenceRecord {
                    kind,
                    source: registration.files,
                    task,
                    tables: BTreeSet::from([registration.files]),
                    attempts: HashMap::new(),
                    copied_accesses: Vec::new(),
                    completion: None,
                },
            );
            operation
        };
        Ok(Self {
            state,
            operation,
            control: control.clone(),
            task,
        })
    }

    /// Bind the exact explicit operation before release. Retain only its
    /// mechanical completion tracker: retaining its observer here would create
    /// a policy-state/observer ownership cycle.
    pub(crate) fn bind_operation(&self, binding: &NativeOperation) -> Result<(), reverie::Error> {
        self.bind_completion(&binding.completion())
    }

    /// Dynamic guest-visible helper acquisitions use the already-bound owner's
    /// observer-free tracker; the observer never retains its own binding.
    pub(crate) fn bind_completion(
        &self,
        completion: &NativeOperationCompletion,
    ) -> Result<(), reverie::Error> {
        let mut state = self.state.lock().unwrap();
        let operation = state
            .operations
            .get_mut(&self.operation)
            .ok_or_else(|| anyhow::anyhow!("native reference binding after retirement"))?;
        if operation.completion.is_some() || !operation.attempts.is_empty() {
            return Err(anyhow::anyhow!("native reference binding repeated or late").into());
        }
        operation.completion = Some(completion.clone());
        Ok(())
    }

    /// Bind this outer intent to one actual application attempt. Backend helper
    /// attempts and arbitrary future completions cannot certify it.
    pub(crate) fn observe_native(
        &self,
        facts: &dyn NativeSyscallFacts,
        call: &NativeSyscallCall,
    ) -> Result<Option<Arc<dyn NativeSyscallObserver>>, reverie::Error> {
        if call.purpose != NativeSyscallPurpose::Application {
            return Ok(None);
        }
        let authority = facts.authority().map_err(anyhow::Error::new)?;
        if authority.registration.task != self.task
            || facts
                .control()
                .is_none_or(|control| control.run_identity() != self.control.run_identity())
        {
            return Err(anyhow::anyhow!("native reference physical owner mismatch").into());
        }
        let eligible_write = policy_syscall(call).is_some_and(|call| {
            matches!(
                call.name(),
                "write" | "pwrite64" | "writev" | "pwritev" | "pwritev2"
            ) && descriptor_io_may_access(call)
                && !authority.files_proven_unshared
                && proc_write_may_reach(
                    call,
                    self.control
                        .layout(self.task)
                        .map_or(Span::Unknown, |layout| {
                            Span::bytes(layout.base, layout.page_bytes as u64)
                        }),
                )
        });
        {
            let mut state = self.state.lock().unwrap();
            let operation = state
                .operations
                .get_mut(&self.operation)
                .ok_or_else(|| anyhow::anyhow!("native reference operation already retired"))?;
            if operation.completion.is_none() || !call.operation_bound {
                return Err(anyhow::anyhow!("native reference physical attempt unbound").into());
            }
            if operation.source != authority.registration.files {
                return Err(
                    anyhow::anyhow!("native reference changed file-table incarnation").into(),
                );
            }
            if operation
                .attempts
                .insert(
                    call.id,
                    AttemptEvidence {
                        eligible_write,
                        ..AttemptEvidence::default()
                    },
                )
                .is_some()
            {
                return Err(anyhow::anyhow!("native reference physical attempt repeated").into());
            }
        }
        Ok(Some(Arc::new(ReferenceAttempt {
            owner: self.clone(),
            id: call.id,
        })))
    }

    /// The acquisition owner calls this only for authenticated actual installed
    /// membership of this physical application. A late whole-table census or
    /// mutable SCM buffer is not that proof; those stay under gap2 uncertainty.
    pub(crate) fn captured_acquisition(
        &self,
        id: NativeSyscallId,
        descriptors: &[InputDescriptorFacts],
        mountinfo: Option<&[u8]>,
    ) -> Result<bool, reverie::Error> {
        let mut acquired_capability = false;
        let mut object_proof_complete = true;
        for descriptor in descriptors {
            match writable_process_memory(descriptor, mountinfo) {
                Ok(writable) => acquired_capability |= writable,
                Err(_) => object_proof_complete = false,
            }
        }
        let mut state = self.state.lock().unwrap();
        let operation = state
            .operations
            .get_mut(&self.operation)
            .ok_or_else(|| anyhow::anyhow!("native acquisition history already retired"))?;
        if operation.kind != ReferenceKind::Acquisition {
            return Err(
                anyhow::anyhow!("native acquisition receipt belongs to write intent").into(),
            );
        }
        let attempt = operation
            .attempts
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("native acquisition has no physical issue owner"))?;
        if !attempt.issued || attempt.not_run {
            return Err(anyhow::anyhow!("native acquisition membership lacks actual issue").into());
        }
        if !object_proof_complete {
            // Only missing native object proof takes the accepted acquisition
            // uncertainty path. Protocol/identity errors above remain errors.
            return Ok(false);
        }
        attempt.acquired_capability |= acquired_capability;
        Ok(true)
    }

    /// Existing serialized continuation/terminal ownership authorizes this cut;
    /// native callback completion alone never does. The outer caller invokes
    /// this before retiring its last covering access lease. No host callback or
    /// destructor calls it. Issued errno remains possible effect; NotRun is an
    /// explicit independent proof, not inferred from an error result.
    pub(crate) fn readmitted(self, binding: &NativeOperation) -> Result<(), reverie::Error> {
        let completion = binding.completion();
        let grant = self
            .control
            .current_grant(self.task)
            .ok_or_else(|| anyhow::anyhow!("native reference lacks a readmission grant"))?;
        self.finish(&completion, Some(grant))
    }

    /// The caller owns the scheduler's exact exit/exec fence and calls this
    /// before generic access settlement. A supplied task list alone is not a
    /// completion proof: the exact bound operation must also be closed and all
    /// its native effects ended. This publishes retained policy but leaves
    /// access retirement to that authenticated fence, never to Drop.
    pub(crate) fn terminal_fenced(
        self,
        completion: &NativeOperationCompletion,
        fenced: &[InputTaskId],
    ) -> Result<(), reverie::Error> {
        if !fenced.contains(&self.task) {
            return Err(anyhow::anyhow!("native reference missing exact terminal fence").into());
        }
        self.finish(completion, None)
    }

    fn finish(
        self,
        completion: &NativeOperationCompletion,
        grant: Option<u64>,
    ) -> Result<(), reverie::Error> {
        if !completion.effects_complete() {
            return Err(
                anyhow::anyhow!("native reference operation effects remain unresolved").into(),
            );
        }
        let (publish, copied_accesses) = {
            let mut state = self.state.lock().unwrap();
            let operation = state
                .operations
                .get(&self.operation)
                .ok_or_else(|| anyhow::anyhow!("native reference readmission repeated"))?;
            if !operation
                .completion
                .as_ref()
                .is_some_and(|bound| bound.same_operation(completion))
            {
                return Err(
                    anyhow::anyhow!("native reference readmission operation mismatch").into(),
                );
            }
            if operation
                .attempts
                .values()
                .any(|attempt| attempt.issued && !attempt.ended && !attempt.not_run)
            {
                return Err(anyhow::anyhow!("native reference effects remain unresolved").into());
            }
            let capability_seen = operation.kind == ReferenceKind::Acquisition
                && operation.attempts.values().any(|attempt| {
                    attempt.issued && !attempt.not_run && attempt.acquired_capability
                });
            let write_seen = operation.kind == ReferenceKind::Write
                && operation
                    .attempts
                    .values()
                    .any(|attempt| attempt.issued && !attempt.not_run && attempt.eligible_write);
            let affected: Vec<_> = operation.tables.iter().copied().collect();
            let mut publish = false;
            for files in affected {
                let table = state
                    .tables
                    .get_mut(&files)
                    .ok_or_else(|| anyhow::anyhow!("native reference destination table missing"))?;
                table.capability_seen |= capability_seen;
                table.write_seen |= write_seen;
                publish |= table.capability_seen && table.write_seen;
            }
            let operation = state.operations.get_mut(&self.operation).unwrap();
            (publish, std::mem::take(&mut operation.copied_accesses))
        };
        // Leaf history lock is gone before entering the backend leaf registry.
        if publish {
            publish_native_exposures(&self.control, vec![forced_write_exposure()])?;
        }
        for lease in copied_accesses {
            self.control
                .complete_access_after_completion(&lease, self.task, completion)
                .map_err(anyhow::Error::new)?;
            if let Some(grant) = grant {
                self.control
                    .readmit_access(lease, self.task, grant)
                    .map_err(anyhow::Error::new)?;
            }
            // Without a live grant the completed lease stays registry-owned
            // until the exact terminal fence settles it. Dropping its Rust
            // token does not remove that guard.
        }
        let mut state = self.state.lock().unwrap();
        let operation = state.operations.get(&self.operation).unwrap();
        if !operation.copied_accesses.is_empty() {
            return Err(anyhow::anyhow!("native reference copy interleaved readmission").into());
        }
        let retired = state.operations.remove(&self.operation).unwrap();
        for files in retired.tables {
            state
                .tables
                .get_mut(&files)
                .unwrap()
                .pending
                .remove(&self.operation);
        }
        Ok(())
    }
}

#[derive(Debug)]
struct ReferenceAttempt {
    owner: NativeReferenceOperation,
    id: NativeSyscallId,
}

impl NativeSyscallObserver for ReferenceAttempt {
    fn observe(
        &self,
        call: &NativeSyscallCall,
        event: NativeSyscallEvent,
        _facts: Option<&dyn NativeSyscallFacts>,
    ) -> Result<(), reverie::Error> {
        if call.id != self.id || call.purpose != NativeSyscallPurpose::Application {
            return Err(
                anyhow::anyhow!("native reference event physical identity mismatch").into(),
            );
        }
        let mut state = self.owner.state.lock().unwrap();
        let attempt = state
            .operations
            .get_mut(&self.owner.operation)
            .and_then(|operation| operation.attempts.get_mut(&self.id))
            .ok_or_else(|| anyhow::anyhow!("native reference event has no live owner"))?;
        attempt.observe(event)
    }
}

#[cfg(test)]
mod tests {
    use reverie::native_syscall::NativeDescriptorSnapshot;
    use reverie::native_syscall::NativeOperationObserver;
    use reverie::native_syscall::NativeSyscallAuthority;
    use reverie::reserved_input::InputGrantKind;
    use reverie::reserved_input::InputNativeError;
    use reverie::reserved_input::InputPathContext;
    use reverie::reserved_input::InputTaskRegistration;
    use reverie::syscalls::SyscallArgs;
    use reverie::syscalls::Sysno;

    use super::*;

    fn call(number: Sysno, args: [usize; 6]) -> Syscall {
        Syscall::from_raw(
            number,
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

    fn facts(filesystem: u64, path: &[u8]) -> InputDescriptorFacts {
        InputDescriptorFacts {
            fd: 3,
            status_flags: Ok(libc::O_RDWR),
            async_signal: Ok(0),
            async_owner: Ok((crate::host_timed_producers::F_OWNER_PID, 0)),
            identity: Ok((libc::makedev(0, 23), 7, libc::S_IFREG | 0o600)),
            filesystem: Ok(filesystem),
            mount_id: Ok(27),
            proc_path: Ok(path.to_vec()),
            path_context: Ok(InputPathContext {
                reader_mount_namespace: (1, 2),
                target_mount_namespace: (1, 2),
                reader_root: (3, 4),
                target_root: (3, 4),
                reader_cgroup_namespace: (5, 6),
                target_cgroup_namespace: (5, 6),
            }),
        }
    }

    #[derive(Debug)]
    struct EmptyOwner;
    impl NativeOperationObserver for EmptyOwner {
        fn observe(
            &self,
            _facts: &dyn NativeSyscallFacts,
            _call: &NativeSyscallCall,
        ) -> Result<Option<Arc<dyn NativeSyscallObserver>>, reverie::Error> {
            Ok(None)
        }
    }

    struct StoppedFacts {
        control: ReservedInputControl,
        task: InputTaskId,
    }
    impl NativeSyscallFacts for StoppedFacts {
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
            let mut value = facts(0xef53, b"/ordinary");
            value.fd = fd;
            Ok(value)
        }
        fn descriptors(&self) -> Result<NativeDescriptorSnapshot, InputUnavailable> {
            Ok(NativeDescriptorSnapshot {
                descriptors: vec![self.descriptor(3)?],
                mountinfo: Ok(Vec::new()),
            })
        }
    }

    fn native(number: Sysno, id: u64) -> NativeSyscallCall {
        #[cfg(target_arch = "x86_64")]
        let audit_arch = 0xc000_003e;
        #[cfg(target_arch = "aarch64")]
        let audit_arch = 0xc000_00b7;
        NativeSyscallCall {
            id: NativeSyscallId::from_backend_counter(id),
            abi: NativeSyscallAbi::Linux { audit_arch },
            number: number as u64,
            args: [3, 0x1000, 1, 0, 0, 0],
            purpose: NativeSyscallPurpose::Application,
            operation_bound: true,
            input_receipt: None,
            staged_image: None,
        }
    }

    // Tests below exercise policy ownership with real registry identities and
    // mechanical attempt proofs. They are not ptrace execution or handler
    // coverage; the separate native boundary controls remain necessary.
    fn history_context(shared: bool) -> (StoppedFacts, InputTaskRegistration) {
        let control = ReservedInputControl::new_backend().unwrap();
        install_native_history(&control).unwrap();
        let root = control.backend_register_root(10, 10).unwrap();
        #[cfg(target_arch = "x86_64")]
        let abi = reverie::reserved_input::InputAbi::X86_64;
        #[cfg(target_arch = "aarch64")]
        let abi = reverie::reserved_input::InputAbi::Aarch64;
        control
            .backend_set_layout(
                root.task,
                reverie::reserved_input::ReservedInputLayout {
                    base: reverie::reserved_input::RESERVED_INPUT_BASE,
                    page_bytes: 4096,
                    abi,
                },
            )
            .unwrap();
        history(&control)
            .unwrap()
            .lock()
            .unwrap()
            .tables
            .insert(root.files, TableHistory::default());
        if shared {
            control
                .backend_register_child(root.task, 11, 10, Some(true), Some(true))
                .unwrap();
        }
        control.grant(root.task, InputGrantKind::Ordinary).unwrap();
        (
            StoppedFacts {
                control,
                task: root.task,
            },
            root,
        )
    }

    fn staging_gate(facts: &StoppedFacts) -> Result<(), InputUnavailable> {
        use reverie::reserved_input::InputCallInput;
        use reverie::reserved_input::InputExpectedSharing;
        use reverie::reserved_input::PhysicalInjectionDescription;
        use reverie::reserved_input::ReservedInputImage;
        let scope = facts.control.begin_operation(facts.task, 100).unwrap();
        let receipt = facts
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
        let image =
            ReservedInputImage::new(facts.control.layout(facts.task).unwrap(), &[]).unwrap();
        facts
            .control
            .backend_begin_preparation(&receipt, &scope, image)
            .unwrap();
        facts
            .control
            .backend_prepared_stopped(receipt.id())
            .map(|_| ())
    }

    fn write_owner(facts: &StoppedFacts) -> (NativeReferenceOperation, NativeOperation) {
        let owner = NativeReferenceOperation::background_write(
            &facts.control,
            facts.task,
            call(Sysno::write, [3, 0x1000, 1, 0, 0, 0]),
        )
        .unwrap()
        .unwrap();
        let binding = NativeOperation::new(Arc::new(EmptyOwner));
        owner.bind_operation(&binding).unwrap();
        (owner, binding)
    }

    fn acquisition_owner(facts: &StoppedFacts) -> (NativeReferenceOperation, NativeOperation) {
        let owner = NativeReferenceOperation::acquisition(&facts.control, facts.task).unwrap();
        let binding = NativeOperation::new(Arc::new(EmptyOwner));
        owner.bind_operation(&binding).unwrap();
        (owner, binding)
    }

    fn finish_attempt(
        facts: &StoppedFacts,
        owner: &NativeReferenceOperation,
        binding: &NativeOperation,
        call: &NativeSyscallCall,
        capture_proc_mem: bool,
    ) {
        let proof = binding.begin_native_attempt().unwrap();
        let observer = owner.observe_native(facts, call).unwrap().unwrap();
        proof.transition_started();
        observer
            .observe(call, NativeSyscallEvent::ResumeIssued, None)
            .unwrap();
        observer
            .observe(call, NativeSyscallEvent::NativeResult(Ok(3)), Some(facts))
            .unwrap();
        if capture_proc_mem {
            assert!(
                owner
                    .captured_acquisition(
                        call.id,
                        &[self::facts(PROC_SUPER_MAGIC, b"/proc/31/mem")],
                        Some(b"27 1 0:23 / /proc rw - proc proc rw\n"),
                    )
                    .unwrap()
            );
        }
        proof.effects_ended();
        binding.close();
    }

    #[test]
    fn abi_classification_keeps_native_control_and_never_aliases_compat_numbers() {
        let (facts, _) = history_context(false);
        let mut request = native(Sysno::getpid, 1);
        assert!(native_syscall_abi_is_verified(&request));
        assert!(
            collect_native_exposures(&facts, &request)
                .unwrap()
                .is_empty()
        );
        for abi in [
            NativeSyscallAbi::Unverified,
            NativeSyscallAbi::Linux {
                audit_arch: 0x4000_0003,
            },
        ] {
            request.abi = abi;
            assert!(!native_syscall_abi_is_verified(&request));
            assert!(policy_syscall(&request).is_none());
            let exposures = collect_native_exposures(&facts, &request).unwrap();
            assert_eq!(exposures.len(), 1);
            assert_eq!(&*exposures[0].cause, "native-syscall-abi-unverified");
            assert_eq!(exposures[0].target, InputTarget::Unknown);
        }
        request = native(Sysno::write, 2);
        request.number |= 0x4000_0000;
        assert!(policy_syscall(&request).is_none());
        request.number = (1u64 << 32) | Sysno::write as u64;
        assert!(policy_syscall(&request).is_none());
        request.purpose = NativeSyscallPurpose::BackendPrivate;
        request.abi = NativeSyscallAbi::Unverified;
        assert!(
            collect_native_exposures(&facts, &request)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn foreground_native_abi_unavailability_reaches_the_real_staging_gate() {
        for unsupported in [false, true] {
            let (facts, _) = history_context(false);
            let mut request = native(Sysno::getpid, 1);
            if unsupported {
                request.abi = NativeSyscallAbi::Unverified;
            }
            assert!(observe_native_syscall(&facts, &request).unwrap().is_none());
            let result = staging_gate(&facts);
            if unsupported {
                assert_eq!(
                    result.unwrap_err().reason(),
                    "native-syscall-abi-unverified"
                );
            } else {
                assert!(result.is_ok(), "adjacent native ABI must retain admission");
            }
        }
    }

    fn initial_facts(
        task: InputTaskId,
        descriptor: Option<InputDescriptorFacts>,
        rseq: bool,
    ) -> InputNativeFacts {
        use reverie::reserved_input::InputImagePhase;
        use reverie::reserved_input::InputNativeLimit;
        use reverie::reserved_input::InputNativeRseq;
        use reverie::reserved_input::PinnedInputDescriptor;
        InputNativeFacts {
            phase: InputImagePhase::Target,
            task,
            cpu: Ok(InputNativeLimit {
                current: u64::MAX,
                maximum: u64::MAX,
            }),
            realtime: Ok(InputNativeLimit {
                current: u64::MAX,
                maximum: u64::MAX,
            }),
            rseq: Ok(InputNativeRseq {
                address: if rseq { 0x2000 } else { 0 },
                length: 32,
                signature: 0,
                flags: 0,
            }),
            // Explicit unit fixture, not a native descriptor attestation.
            descriptors: Ok(descriptor
                .into_iter()
                .map(|facts| {
                    PinnedInputDescriptor::from_native_parts(
                        facts,
                        std::fs::File::open("/dev/null").unwrap().into(),
                    )
                })
                .collect()),
            mountinfo: Ok(b"27 1 0:23 / /proc rw - proc proc rw\n".to_vec()),
            mount_namespace: Ok((1, 2)),
            root: Ok((3, 4)),
        }
    }

    #[test]
    fn rseq_unqualified_initial_census_keeps_descriptor_history_through_copy_and_exec() {
        use reverie::reserved_input::InputAdmission;
        for case in ["known-process-memory", "unknown-filesystem", "ordinary"] {
            let (stopped, root) = history_context(false);
            let mut descriptor = facts(PROC_SUPER_MAGIC, b"/proc/31/mem");
            if case == "unknown-filesystem" {
                descriptor.filesystem = Err(InputNativeError {
                    operation: "fstatfs",
                    errno: Some(libc::EIO),
                });
            } else if case == "ordinary" {
                descriptor.filesystem = Ok(0xef53);
            }
            let initial = initial_facts(root.task, Some(descriptor), true);
            assert_eq!(
                super::super::objects::admit_initial(&initial),
                InputAdmission::Unqualified(InputUnavailable::new(
                    "native-rseq-reference-unverified"
                ))
            );
            register_initial_history(&stopped.control, root.task).unwrap();
            let initial_cause = observe_initial_descriptors(&stopped.control, &initial).unwrap();
            assert_eq!(initial_cause.is_some(), case == "unknown-filesystem");
            let child = stopped
                .control
                .backend_register_child(root.task, 12, 12, Some(false), Some(false))
                .unwrap();
            let image = stopped.control.backend_exec(child.task).unwrap();
            let after_exec = initial_facts(image.task, None, false);
            assert_eq!(
                super::super::objects::admit_initial(&after_exec),
                InputAdmission::Qualified
            );
            let retained = observe_initial_descriptors(&stopped.control, &after_exec).unwrap();
            if case == "unknown-filesystem" {
                assert_eq!(
                    retained.unwrap().reason(),
                    "descriptor-native-filesystem-unverified"
                );
            } else {
                assert!(
                    retained.is_none(),
                    "local rseq refusal must not be retained as descriptor proof loss"
                );
            }
            let state = history(&stopped.control).unwrap();
            let state = state.lock().unwrap();
            assert_eq!(
                state.tables[&image.files].capability_seen,
                case == "known-process-memory"
            );
            assert!(!state.tables[&image.files].write_seen);
        }
    }

    #[test]
    fn completed_write_history_meets_later_capability_without_overlap_membership() {
        for write_first in [false, true] {
            let (facts, root) = history_context(true);
            let (write, write_binding) = write_owner(&facts);
            let (acquisition, acquisition_binding) = acquisition_owner(&facts);
            finish_attempt(
                &facts,
                &write,
                &write_binding,
                &native(Sysno::write, 1),
                false,
            );
            finish_attempt(
                &facts,
                &acquisition,
                &acquisition_binding,
                &native(Sysno::openat, 2),
                true,
            );
            if write_first {
                write.readmitted(&write_binding).unwrap();
                acquisition.readmitted(&acquisition_binding).unwrap();
            } else {
                acquisition.readmitted(&acquisition_binding).unwrap();
                write.readmitted(&write_binding).unwrap();
            }
            let state = history(&facts.control).unwrap();
            let state = state.lock().unwrap();
            let table = &state.tables[&root.files];
            assert!(table.capability_seen && table.write_seen);
            assert!(table.pending.is_empty());
            drop(state);
            assert_eq!(
                staging_gate(&facts).unwrap_err().reason(),
                "forced-reserved-write"
            );
        }
        // Different membership: A is not even registered until W readmitted.
        let (facts, root) = history_context(true);
        let (write, binding) = write_owner(&facts);
        finish_attempt(&facts, &write, &binding, &native(Sysno::write, 1), false);
        write.readmitted(&binding).unwrap();
        let (acquisition, binding) = acquisition_owner(&facts);
        finish_attempt(
            &facts,
            &acquisition,
            &binding,
            &native(Sysno::openat, 2),
            true,
        );
        acquisition.readmitted(&binding).unwrap();
        let state = history(&facts.control).unwrap();
        let state = state.lock().unwrap();
        assert!(state.tables[&root.files].capability_seen && state.tables[&root.files].write_seen);
        drop(state);
        assert_eq!(
            staging_gate(&facts).unwrap_err().reason(),
            "forced-reserved-write"
        );
    }

    #[test]
    fn file_table_copy_snapshots_completed_and_pending_history_without_parent_leak() {
        for complete_before_copy in [false, true] {
            let (facts, root) = history_context(true);
            let (write, binding) = write_owner(&facts);
            finish_attempt(&facts, &write, &binding, &native(Sysno::write, 1), false);
            if complete_before_copy {
                write.clone().readmitted(&binding).unwrap();
            }
            let child = facts
                .control
                .backend_register_child(root.task, 12, 12, Some(false), Some(false))
                .unwrap();
            if !complete_before_copy {
                write.readmitted(&binding).unwrap();
            }
            let (acquisition, binding) = acquisition_owner(&facts);
            finish_attempt(
                &facts,
                &acquisition,
                &binding,
                &native(Sysno::openat, 2),
                true,
            );
            acquisition.readmitted(&binding).unwrap();
            let state = history(&facts.control).unwrap();
            let state = state.lock().unwrap();
            assert!(state.tables[&root.files].capability_seen);
            assert!(state.tables[&child.files].write_seen);
            assert!(!state.tables[&child.files].capability_seen);
            assert!(state.tables[&child.files].pending.is_empty());
        }
    }

    #[test]
    fn copied_pending_acquisition_has_local_guard_until_exact_owner_readmission() {
        let (facts, root) = history_context(false);
        let (acquisition, binding) = acquisition_owner(&facts);
        let child = facts
            .control
            .backend_register_child(root.task, 12, 12, Some(false), Some(false))
            .unwrap();
        {
            let state = history(&facts.control).unwrap();
            let state = state.lock().unwrap();
            assert_eq!(
                state.operations[&acquisition.operation]
                    .copied_accesses
                    .len(),
                1
            );
            assert!(
                state.tables[&child.files]
                    .pending
                    .contains(&acquisition.operation)
            );
        }
        finish_attempt(
            &facts,
            &acquisition,
            &binding,
            &native(Sysno::openat, 1),
            true,
        );
        acquisition.readmitted(&binding).unwrap();
        let state = history(&facts.control).unwrap();
        let state = state.lock().unwrap();
        assert!(state.tables[&child.files].capability_seen);
        assert!(state.tables[&child.files].pending.is_empty());
        assert!(state.operations.is_empty());
    }

    #[test]
    fn cancellation_needs_authenticated_effects_end_and_the_exact_operation() {
        let (facts, root) = history_context(true);
        let (owner, binding) = write_owner(&facts);
        let request = native(Sysno::write, 1);
        let proof = binding.begin_native_attempt().unwrap();
        let observer = owner.observe_native(&facts, &request).unwrap().unwrap();
        proof.transition_started();
        observer
            .observe(&request, NativeSyscallEvent::ResumeIssued, None)
            .unwrap();
        observer
            .observe(&request, NativeSyscallEvent::ResultUnobserved, None)
            .unwrap();
        binding.close();
        assert!(owner.clone().readmitted(&binding).is_err());
        let unrelated = NativeOperation::new(Arc::new(EmptyOwner));
        unrelated.close();
        assert!(owner.clone().readmitted(&unrelated).is_err());
        drop(observer.clone());
        assert!(
            history(&facts.control).unwrap().lock().unwrap().tables[&root.files]
                .pending
                .contains(&owner.operation)
        );
        observer
            .observe(&request, NativeSyscallEvent::EffectsComplete, None)
            .unwrap();
        proof.effects_ended();
        owner.readmitted(&binding).unwrap();
        let state = history(&facts.control).unwrap();
        let state = state.lock().unwrap();
        assert!(state.tables[&root.files].write_seen);
        assert!(state.tables[&root.files].pending.is_empty());
    }

    #[test]
    fn dropping_every_cancelled_owner_retains_unresolved_history() {
        let (facts, root) = history_context(true);
        let (owner, binding) = write_owner(&facts);
        let id = owner.operation;
        let request = native(Sysno::write, 1);
        let proof = binding.begin_native_attempt().unwrap();
        let observer = owner.observe_native(&facts, &request).unwrap().unwrap();
        proof.transition_started();
        observer
            .observe(&request, NativeSyscallEvent::ResumeIssued, None)
            .unwrap();
        observer
            .observe(&request, NativeSyscallEvent::ResultUnobserved, None)
            .unwrap();
        binding.close();
        drop(observer);
        drop(owner);
        drop(proof);
        drop(binding);
        let state = history(&facts.control).unwrap();
        let state = state.lock().unwrap();
        assert!(state.operations.contains_key(&id));
        assert!(state.tables[&root.files].pending.contains(&id));
        assert!(
            !state.tables[&root.files].write_seen,
            "a host-timed destructor cannot publish logical history either"
        );
    }

    #[test]
    fn native_errno_keeps_possible_effects_but_prepared_only_does_not_mint_history() {
        for issued in [false, true] {
            let (facts, root) = history_context(true);
            let (owner, binding) = write_owner(&facts);
            let request = native(Sysno::write, 1);
            let proof = binding.begin_native_attempt().unwrap();
            let observer = owner.observe_native(&facts, &request).unwrap().unwrap();
            if issued {
                proof.transition_started();
                observer
                    .observe(&request, NativeSyscallEvent::ResumeIssued, None)
                    .unwrap();
                observer
                    .observe(
                        &request,
                        NativeSyscallEvent::NativeResult(Err(reverie::Errno::EFAULT)),
                        None,
                    )
                    .unwrap();
                proof.effects_ended();
            } else {
                proof.cancelled_before_transition();
            }
            binding.close();
            owner.readmitted(&binding).unwrap();
            let state = history(&facts.control).unwrap();
            let state = state.lock().unwrap();
            assert_eq!(state.tables[&root.files].write_seen, issued);
            assert!(state.tables[&root.files].pending.is_empty());
        }
    }

    #[test]
    fn nonissued_private_and_independently_not_run_writes_never_mint_write_history() {
        for private in [false, true] {
            let (facts, root) = history_context(!private);
            for issue in [false, true] {
                let (owner, binding) = write_owner(&facts);
                let request = native(Sysno::write, u64::from(issue));
                let proof = binding.begin_native_attempt().unwrap();
                let observer = owner.observe_native(&facts, &request).unwrap().unwrap();
                if issue {
                    proof.transition_started();
                    observer
                        .observe(&request, NativeSyscallEvent::ResumeIssued, None)
                        .unwrap();
                }
                observer
                    .observe(&request, NativeSyscallEvent::ProvedNotRun, None)
                    .unwrap();
                proof.effects_ended();
                binding.close();
                owner.readmitted(&binding).unwrap();
                assert!(
                    !history(&facts.control).unwrap().lock().unwrap().tables[&root.files]
                        .write_seen
                );
            }
            if private {
                let (owner, binding) = write_owner(&facts);
                finish_attempt(&facts, &owner, &binding, &native(Sysno::write, 3), false);
                owner.readmitted(&binding).unwrap();
                assert!(
                    !history(&facts.control).unwrap().lock().unwrap().tables[&root.files]
                        .write_seen
                );
            }
        }
    }

    #[test]
    fn process_memory_classification_uses_native_filesystem_and_bind_root() {
        let mounts = b"27 1 0:23 / /proc rw - proc proc rw\n";
        for path in [b"/proc/31/mem".as_slice(), b"/proc/31/task/32/mem"] {
            assert!(may_be_process_memory(
                &facts(PROC_SUPER_MAGIC, path),
                Some(mounts)
            ));
        }
        let alias = facts(PROC_SUPER_MAGIC, b"/ordinary-name");
        let bound = b"27 1 0:23 /31/mem /ordinary-name rw - proc proc rw\n";
        assert!(may_be_process_memory(&alias, Some(bound)));
        for path in [
            b"/proc/31/maps".as_slice(),
            b"/proc/sys/kernel/pid_max",
            b"/proc/31/memory",
        ] {
            assert!(!may_be_process_memory(
                &facts(PROC_SUPER_MAGIC, path),
                Some(mounts)
            ));
        }
        // A pathname alone neither grants nor removes process-memory authority.
        assert!(!may_be_process_memory(
            &facts(0xef53, b"/proc/31/mem"),
            None
        ));
        assert!(may_be_process_memory(&alias, None));
        let mut hidden = facts(PROC_SUPER_MAGIC, b"/proc/31/maps");
        hidden.path_context.as_mut().unwrap().target_root = (9, 10);
        assert!(may_be_process_memory(&hidden, Some(mounts)));
        hidden.filesystem = Err(InputNativeError {
            operation: "fstatfs",
            errno: Some(libc::EIO),
        });
        assert!(may_be_process_memory(&hidden, Some(mounts)));
    }

    #[test]
    fn background_descriptor_effects_use_only_immutable_no_access_negatives() {
        for number in [Sysno::read, Sysno::write, Sysno::readv, Sysno::writev] {
            assert!(descriptor_io_may_access(call(
                number,
                [3, 0x1000, 1, 0, 0, 0]
            )));
            assert!(!descriptor_io_may_access(call(
                number,
                [usize::MAX, 0x1000, 1, 0, 0, 0],
            )));
            assert!(!descriptor_io_may_access(call(
                number,
                [3, 0x1000, 0, 0, 0, 0]
            )));
        }
        for number in [Sysno::pread64, Sysno::pwrite64] {
            assert!(descriptor_io_may_access(call(
                number,
                [3, 0x1000, 1, 0, 0, 0]
            )));
            assert!(!descriptor_io_may_access(call(
                number,
                [3, 0x1000, 1, usize::MAX, 0, 0],
            )));
        }
        // pwritev2's -1 offset selects the shared file cursor; it is not the
        // native EINVAL negative of pwrite64. It still requires the lease.
        assert!(descriptor_io_may_access(call(
            Sysno::pwritev2,
            [3, 0x1000, 1, usize::MAX, usize::MAX, 0],
        )));
        assert!(!descriptor_io_may_access(call(Sysno::getpid, [0; 6])));
    }

    #[test]
    fn ancillary_import_never_uses_a_pre_prologue_peer_or_empty_buffer_snapshot() {
        // The payload/header address does not prove immutable native import;
        // NULL or an apparently empty msghdr must not manufacture an exemption.
        for pointer in [0, 0x1000] {
            assert!(ancillary_import_may_export(call(
                Sysno::sendmsg,
                [3, pointer, 0, 0, 0, 0],
            )));
            assert!(ancillary_import_may_export(call(
                Sysno::sendmmsg,
                [3, pointer, 1, 0, 0, 0],
            )));
        }
        assert!(!ancillary_import_may_export(call(
            Sysno::sendmsg,
            [usize::MAX, 0x1000, 0, 0, 0, 0],
        )));
        assert!(!ancillary_import_may_export(call(
            Sysno::sendmmsg,
            [3, 0x1000, 0, 0, 0, 0],
        )));
        assert!(!ancillary_import_may_export(call(Sysno::getpid, [0; 6])));
    }

    #[test]
    fn positional_proc_writes_bound_only_immutable_scalar_extent() {
        let base = 0x7101_0000;
        let page = Span::bytes(base, 4096);
        for (offset, length, expected) in [
            (base - 1, 1, false),
            (base - 1, 2, true),
            (base, 4096, true),
            (base + 4096, 1, false),
            (base, 0, false),
            (u64::MAX, 1, false),
            ((1u64 << 62) | base, 1, true),
        ] {
            assert_eq!(
                proc_write_may_reach(
                    call(Sysno::pwrite64, [3, 0x1000, length, offset as usize, 0, 0]),
                    page
                ),
                expected,
                "offset={offset:#x} length={length}"
            );
        }
        assert!(proc_write_may_reach(
            call(Sysno::write, [3, 0x1000, 1, 0, 0, 0]),
            page
        ));
        assert!(proc_write_may_reach(
            call(Sysno::pwritev, [3, 0x1000, 1, 0, 0, 0]),
            page
        ));
        assert!(proc_write_may_reach(
            call(Sysno::pwritev2, [3, 0x1000, 1, usize::MAX, usize::MAX, 0]),
            page
        ));
        assert!(!proc_write_may_reach(
            call(Sysno::writev, [3, 0x1000, 0, 0, 0, 0]),
            page
        ));
        assert!(proc_write_may_reach(
            call(Sysno::pwrite64, [3, 0x1000, 1, base as usize, 0, 0]),
            Span::Unknown
        ));
    }

    #[test]
    fn persistent_uffd_commands_do_not_swallow_unrelated_ioctl_space() {
        assert!(is_known_uffd_import(0xc020_aa00));
        assert!(is_known_uffd_import(0xc028_aa05));
        for request in [
            0x8010_aa01,
            0xc018_aa3f,
            0xc028_aa03,
            0x8020_aa00,
            0xc018_aa00,
            libc::FIONREAD as usize,
        ] {
            assert!(!is_known_uffd_import(request), "{request:#x}");
        }
    }
}
