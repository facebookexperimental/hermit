//! Maintained native process/syscall glue around the ONE common Guest.
//!
//! Clone/exec remain real native operations: no child returns on a Rust client
//! stack. Concrete Detcore preparation and partial-random repair run inside
//! the original callback, before its unchanged common outcome classifier.

use std::ffi::c_void;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use detcore::Detcore;
use rand::RngExt as _;
use reverie::Error;
use reverie::Guest;
use reverie::syscalls::Errno;
use reverie_inguest::guest::host::InGuest;
use reverie_inguest::guest::native_services::MemoryReader;
use reverie_inguest::guest::native_services::NativeServices;
use reverie_inguest::tool_host::NativeCallbackOutcome;
use reverie_inguest::tool_host::TailResult;
use reverie_inguest::tool_host::drive_native_ready;

use super::abi::Callback;
use super::abi::Identity;
use super::abi::Mechanisms;
use super::entry::event;
use super::entry::status;
use super::owner::Engine;
use super::owner::Thread;
use super::owner::{self};
use super::policy::*;
use crate::runtime::EngineRuntime;

type ConcreteGuest<'a> = InGuest<'a, Detcore, EngineRuntime>;
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
const MAX_OBSERVED_BUFFER: usize = 1024 * 1024;
static TOTAL_BRANCHES: AtomicU64 = AtomicU64::new(0);
static TOTAL_SYSCALLS: AtomicU64 = AtomicU64::new(0);
static TOTAL_REWRITTEN: AtomicU64 = AtomicU64::new(0);
static MEMORY_HASH: AtomicU64 = AtomicU64::new(FNV_OFFSET);

/// Private one-use witness created only after the real PrepareExec RPC. It
/// retains mechanism identity and scalar observations, not a callback frame,
/// borrowed auxv pointer, Rust Guest, synthetic flag or a replacement lease.
pub(super) struct PreparedExec {
    identity: Identity,
    context: usize,
    mechanisms: usize,
    branches: u64,
}

impl PreparedExec {
    fn new(callback: &Callback) -> Self {
        Self {
            identity: callback.identity,
            context: callback.context,
            mechanisms: callback.mechanisms as usize,
            branches: callback.branches,
        }
    }
    fn validate(&self, callback: &Callback) -> io::Result<()> {
        callback.validate(self.identity)?;
        if callback.context != self.context || callback.mechanisms as usize != self.mechanisms {
            return Err(io::Error::from_raw_os_error(libc::ESTALE));
        }
        Ok(())
    }
}

pub(super) struct PreparedCall {
    pub arguments: [u64; 6],
    probe: Option<GetrandomProbe>,
    clones: ProcessCloneProperties,
    first: bool,
}

pub(super) fn branches(value: u64) {
    TOTAL_BRANCHES.store(value, Ordering::Relaxed);
}
pub(super) fn is_native_lifecycle(number: i64) -> bool {
    requires_native_lifecycle(number)
}
pub(super) fn rewritten() {
    TOTAL_REWRITTEN.fetch_add(1, Ordering::Relaxed);
}

pub(super) fn prepare(
    callback: &Callback,
    raw: &Mechanisms,
    number: i64,
    arguments: [u64; 6],
) -> Result<PreparedCall, Errno> {
    let first = TOTAL_SYSCALLS.fetch_add(1, Ordering::Relaxed) == 0;
    if first {
        super::diagnostics::emit(b"detcore-dynamorio: entered Rust syscall callback\n");
    }
    let read = raw.read_memory.ok_or(Errno::EIO)?;
    let write = raw.write_memory.ok_or(Errno::EIO)?;
    let mut selected = arguments;
    // Exact Current-ABI self-prlimit lowering. Queued signal targets remain
    // in the Tool namespace; the native injector lowers them after handling.
    if number == libc::SYS_prlimit64 && selected[0] as i32 == callback.identity.tool_pid {
        selected[0] = callback.identity.physical_pid as u32 as u64;
    }
    if !prlimit_new_limit_is_readable(number, &arguments, |address, bytes| unsafe {
        read(address, bytes.as_mut_ptr(), bytes.len()) != 0
    }) || !multiplexed_io_inputs_are_readable(number, &arguments, |address, bytes| unsafe {
        read(address, bytes.as_mut_ptr(), bytes.len()) != 0
    }) {
        return Err(Errno::EFAULT);
    }
    let probe = if number == libc::SYS_getrandom {
        match getrandom_writable_prefix(&arguments, |address, bytes| unsafe {
            Ok(write(address, bytes.as_ptr(), bytes.len()))
        }) {
            Some(Ok(probe)) => {
                selected[1] = probe.writable as u64;
                Some(probe)
            }
            Some(Err(error)) => return Err(error),
            None => None,
        }
    } else {
        None
    };
    branches(callback.branches);
    let clones = process_clone_properties(number, &arguments, |address, bytes| unsafe {
        read(address, bytes.as_mut_ptr(), bytes.len()) != 0
    });
    Ok(PreparedCall {
        arguments: selected,
        probe,
        clones,
        first,
    })
}

fn failure(message: &'static [u8]) -> NativeCallbackOutcome<Result<i64, Error>> {
    super::diagnostics::emit(message);
    NativeCallbackOutcome::Ready(Err(io::Error::other("native lifecycle refused").into()))
}

/// Immutable inputs borrowed from the same admitted native callback.
pub(super) struct DispatchCall<'a> {
    pub engine: &'a Engine,
    pub callback: &'a Callback,
    pub number: i64,
    pub original: [u64; 6],
    pub prepared: &'a PreparedCall,
}

/// Concrete work inside the already admitted common Guest. The outer entry
/// alone retains/completes its native lease; this function cannot reenter it.
/// # Safety
/// All borrowed arguments belong to the same with_native_guest callback.
pub(super) unsafe fn dispatch(
    call: DispatchCall<'_>,
    pending_exec: &mut Option<PreparedExec>,
    tool: &Detcore,
    guest: &mut ConcreteGuest<'_>,
    tail: &TailResult,
    services: &NativeServices<'_>,
) -> NativeCallbackOutcome<Result<i64, Error>> {
    let DispatchCall {
        engine,
        callback,
        number,
        original,
        prepared,
    } = call;
    if number == libc::SYS_execveat {
        return NativeCallbackOutcome::Ready(Ok(-i64::from(Errno::ENOSYS.into_raw())));
    }
    if prepared.clones.shares_files_without_thread {
        return failure(b"detcore-dynamorio: refusing process clone with CLONE_FILES without CLONE_THREAD; copied child cannot share descriptor-table provenance\n");
    }
    if prepared.clones.shares_memory_without_thread {
        return failure(b"detcore-dynamorio: refusing process clone with CLONE_VM without CLONE_THREAD or CLONE_VFORK; copied child cannot own inherited Detcore state\n");
    }
    if prepared.clones.blocks_parent && guest.thread_state().has_unsafe_vfork_flock_state() {
        return failure(b"detcore-dynamorio: refusing vfork/CLONE_VFORK while an open file description may hold a flock\n");
    }
    if prepared.clones.blocks_parent && guest.thread_state().has_unsafe_vfork_ofd_state() {
        return failure(b"detcore-dynamorio: refusing vfork/CLONE_VFORK while an open file description may hold an OFD lock\n");
    }
    if requires_native_lifecycle(number) {
        if engine.config.sequentialize_threads
            && super::bootstrap::coordinated()
            && (number == libc::SYS_execve || prepared.clones.copies_process)
        {
            guest
                .thread_state()
                .promote_shared_open_files()
                .expect("could not publish shared OFDs before native lifecycle");
        }
        if number == libc::SYS_execve {
            if pending_exec.is_some() {
                return NativeCallbackOutcome::Ready(Err(io::Error::from_raw_os_error(
                    libc::EBUSY,
                )
                .into()));
            }
            let mm = guest.thread_state().mm_id;
            let blocking = guest.thread_state().exec_blocking_overrides();
            let robust = drive_native_ready(tool.exec_robust_list_wakes(guest));
            let read = callback
                .validate(callback.identity)
                .expect("admitted native callback")
                .read_memory
                .expect("admitted native reader");
            let image = read_guest_path(original[0], read)
                .and_then(|path| tool.exec_image_at(guest, libc::AT_FDCWD, &path));
            drive_native_ready(detcore::prepare_exec(guest, mm, blocking, robust, image));
            *pending_exec = Some(PreparedExec::new(callback));
            if super::bootstrap::owns_background(callback.identity.physical_pid) {
                super::image::request_pause();
                assert_eq!(
                    super::image::generation(),
                    callback.identity.image,
                    "native image generation changed while pausing for exec"
                );
            }
        }
        // This affine continuation is still active through the entire real C
        // post body. Scalar action3 preserves the old execute-original route,
        // rather than selecting action2's rewritten-operation path.
        return NativeCallbackOutcome::Deferred {
            number,
            args: original,
        };
    }
    let read = callback
        .validate(callback.identity)
        .expect("admitted native callback")
        .read_memory
        .expect("admitted native reader");
    update_memory_hash(number, &original, read);
    if prepared.first {
        super::diagnostics::emit(b"detcore-dynamorio: dispatching first syscall through Detcore\n");
    }
    let saved = prepared
        .probe
        .filter(|probe| probe.writable < probe.requested)
        .map(|probe| (probe, guest.thread_state().prng.clone()));
    unsafe {
        engine.host.dispatch_native_syscall_in_guest_with(
            tool,
            guest,
            tail,
            services,
            &engine.rpc,
            move |guest, mut outcome| {
                if let Some((probe, original_prng)) = saved {
                    guest.thread_state_mut().prng = original_prng;
                    advance_getrandom_prng(&mut guest.thread_state_mut().prng, probe.consumed());
                    // Observe the REAL Tool success here, before the common
                    // classifier converts Errno. A concurrent later write
                    // fault must not be relabelled a successful short fill.
                    if matches!(outcome, NativeCallbackOutcome::Ready(Ok(_))) {
                        let value = if probe.writable == 0 {
                            -i64::from(Errno::EFAULT.into_raw())
                        } else {
                            probe.writable as i64
                        };
                        outcome = NativeCallbackOutcome::Ready(Ok(value));
                    }
                }
                outcome
            },
        )
    }
}

/// Actual process-clone post facts from the native birth transaction. They
/// select no authority: the original Deferred owner authenticates this work.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ClonePost {
    pub number: i64,
    pub result: i64,
    pub child_tid_address: u64,
    pub flags: u64,
    pub tool_child_tid: i32,
    pub exit_signal: i32,
}

/// # Safety
/// Real returning PARENT/native same-process post work only. A copied/vfork
/// child must not enter its inherited Tool/registry here. C retains its birth
/// owner and completes all post bookkeeping before final Deferred completion.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hermit_engine_native_process_clone_result_v1(
    opaque: *mut c_void,
    callback: *const Callback,
    facts: *const ClonePost,
) -> i32 {
    status((|| {
        if opaque.is_null() || callback.is_null() || facts.is_null() {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let callback = unsafe { &*callback };
        let facts = unsafe { facts.read() };
        let thread = unsafe { &*opaque.cast::<Thread>() };
        let mut entry = thread.enter(callback)?;
        let engine = owner::installed()?;
        let table = callback.validate(callback.identity)?.selected()?;
        let auxv = unsafe { callback.copy_auxv()? };
        let services = unsafe { callback.select(&table, &auxv)? };
        let pending = entry
            .state_mut()
            .pending
            .as_mut()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EPROTO))?;
        let mut post = event(callback, facts.number, [0; 6], 0);
        unsafe {
            engine.host.with_native_post_step(
                pending,
                &services,
                &engine.rpc,
                &mut post,
                |tool, guest, _tail| {
                    if successful_process_clone_result(facts.number, facts.result) {
                        guest.thread_state().forget_flock_modes();
                    }
                    if facts.result > 0
                        && facts.tool_child_tid > 0
                        && matches!(
                            facts.number,
                            libc::SYS_fork | libc::SYS_clone | libc::SYS_clone3
                        )
                        && facts.flags & (libc::CLONE_THREAD | libc::CLONE_VFORK) as u64 == 0
                        && facts.exit_signal < 0
                    {
                        return NativeCallbackOutcome::Ready(Err(io::Error::other(
                            "invalid native child exit signal",
                        )
                        .into()));
                    }
                    if let Some((tid, flags, signal)) = process_child_registration(
                        facts.number,
                        facts.result,
                        facts.tool_child_tid,
                        facts.flags,
                        facts.exit_signal,
                    ) {
                        drive_native_ready(tool.register_external_child(
                            guest,
                            tid,
                            facts.child_tid_address as usize,
                            flags,
                            signal,
                            Some((facts.result as i32, facts.result as i32)),
                        ));
                    }
                    NativeCallbackOutcome::Ready(Ok(()))
                },
            )
        }
        .map_err(io::Error::other)
    })())
}

/// # Safety
/// Actual rejected native exec on the same original task/image. The private
/// PreparedExec and original Deferred are consumed/borrowed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hermit_engine_native_exec_failed_v1(
    opaque: *mut c_void,
    callback: *const Callback,
) -> i32 {
    // Same maintained ordering: scheduler progress is restored BEFORE the
    // CancelExec RPC, including the uninitialized/no-preparation path.
    super::image::resume();
    if opaque.is_null() {
        return 0;
    }
    status((|| {
        if callback.is_null() {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let actual = unsafe { &*callback };
        let thread = unsafe { &*opaque.cast::<Thread>() };
        let mut entry = thread.enter(actual)?;
        let Some(prepared) = entry.state().prepared_exec.as_ref() else {
            return Ok(());
        };
        prepared.validate(actual)?;
        let branches = prepared.branches;
        // Only the scalar branch observation is retained from the original
        // pre-event. All pointed-to current facts are borrowed afresh now.
        let callback = Callback {
            branches,
            ..*actual
        };
        let engine = owner::installed()?;
        let table = callback.validate(callback.identity)?.selected()?;
        let auxv = unsafe { callback.copy_auxv()? };
        let services = unsafe { callback.select(&table, &auxv)? };
        let _prepared = entry
            .state_mut()
            .prepared_exec
            .take()
            .expect("checked prepared exec");
        let pending = entry
            .state_mut()
            .pending
            .as_mut()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EPROTO))?;
        let mut post = event(&callback, libc::SYS_execve, [0; 6], 0);
        unsafe {
            engine.host.with_native_post_step(
                pending,
                &services,
                &engine.rpc,
                &mut post,
                |_tool, guest, _tail| {
                    drive_native_ready(detcore::cancel_exec(guest));
                    NativeCallbackOutcome::Ready(Ok(()))
                },
            )
        }
        .map_err(io::Error::other)
    })())
}

/// Read the actual native Tool counters; C retains its original evidence wire.
/// # Safety
/// Each output is aligned/writable for one u64 and owned by this native call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hermit_engine_native_totals_v1(
    branches: *mut u64,
    syscalls: *mut u64,
    rewritten: *mut u64,
    memory_hash: *mut u64,
) -> i32 {
    if branches.is_null() || syscalls.is_null() || rewritten.is_null() || memory_hash.is_null() {
        return -libc::EINVAL;
    }
    unsafe {
        branches.write(TOTAL_BRANCHES.load(Ordering::Relaxed));
        syscalls.write(TOTAL_SYSCALLS.load(Ordering::Relaxed));
        rewritten.write(TOTAL_REWRITTEN.load(Ordering::Relaxed));
        memory_hash.write(MEMORY_HASH.load(Ordering::SeqCst));
    }
    0
}

const _: () = assert!(size_of::<ClonePost>() == 40);

fn read_guest_path(address: u64, read_memory: MemoryReader) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt as _;
    const PAGE: u64 = 4096;
    let mut path = Vec::new();
    let mut at = address;
    while path.len() < libc::PATH_MAX as usize {
        let chunk = (PAGE - at % PAGE) as usize;
        let mut bytes = vec![0_u8; chunk];
        if unsafe { read_memory(at as usize, bytes.as_mut_ptr(), chunk) } == 0 {
            return None;
        }
        if let Some(end) = bytes.iter().position(|&byte| byte == 0) {
            path.extend_from_slice(&bytes[..end]);
            return (path.len() < libc::PATH_MAX as usize)
                .then(|| PathBuf::from(std::ffi::OsString::from_vec(path)));
        }
        path.extend_from_slice(&bytes);
        at += chunk as u64;
    }
    None
}

fn advance_getrandom_prng(prng: &mut impl rand::Rng, bytes: usize) {
    let mut words = [0_u64; RANDOM_FILL_CHUNK_BYTES / std::mem::size_of::<u64>()];
    let mut advanced = 0;
    while advanced < bytes {
        let chunk_len = (bytes - advanced).min(RANDOM_FILL_CHUNK_BYTES);
        let chunk =
            unsafe { std::slice::from_raw_parts_mut(words.as_mut_ptr().cast::<u8>(), chunk_len) };
        prng.fill(chunk);
        advanced += chunk_len;
    }
}

fn update_memory_hash(sysnum: i64, args: &[u64], read_memory: MemoryReader) {
    if sysnum != libc::SYS_write {
        return;
    }
    let address = args[1] as usize;
    let length = args[2] as usize;
    if address == 0 || length > MAX_OBSERVED_BUFFER {
        return;
    }

    let mut bytes = vec![0; length];
    if unsafe { read_memory(address, bytes.as_mut_ptr(), length) } == 0 {
        return;
    }

    let mut hash = FNV_OFFSET;
    for byte in sysnum
        .to_le_bytes()
        .into_iter()
        .chain(args[0].to_le_bytes())
        .chain((length as u64).to_le_bytes())
        .chain(bytes)
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    MEMORY_HASH.fetch_add(hash, Ordering::SeqCst);
}
