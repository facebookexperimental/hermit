//! Native C event entries into the existing common Guest and Detcore.

use std::ffi::c_void;
use std::io;

use detcore::Detcore;
use reverie::ExitStatus;
use reverie::Signal;
use reverie_inguest::guest::event::SyscallDispatch;
use reverie_inguest::guest::event::SyscallEvent;
use reverie_inguest::guest::host::NativeCallResult;
use reverie_inguest::guest::host::NativeExitRpc;
use reverie_inguest::guest::host::NativeInstruction;
use reverie_inguest::guest::host::NativeInstructionResult;
use reverie_inguest::guest::host::NativeSignalResult;
use reverie_inguest::tool_host::NativeCallbackOutcome;

use super::abi::Answer;
use super::abi::Callback;
use super::owner::Entry;
use super::owner::Thread;
use super::owner::{self};

pub(super) fn status(result: io::Result<()>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(error) => -error.raw_os_error().unwrap_or(libc::EPROTO),
    }
}
fn tool_error(error: reverie::Error) -> io::Error {
    io::Error::other(error)
}

fn retain<V>(
    entry: &mut Entry<'_>,
    result: NativeCallResult<'static, Detcore, V>,
    answer: &mut Answer,
    complete: impl FnOnce(V, &mut Answer),
) -> io::Result<()> {
    match result {
        NativeCallResult::Return(value) => complete(value, answer),
        NativeCallResult::Deferred(deferred) => {
            if entry.state().pending.is_some() {
                return Err(io::Error::from_raw_os_error(libc::EBUSY));
            }
            let (number, arguments) = deferred.operation();
            answer.action = 2;
            answer.number = number;
            answer.arguments = arguments;
            entry.state_mut().pending = Some(deferred);
        }
    }
    Ok(())
}

unsafe fn borrowed<'a>(
    opaque: *mut c_void,
    callback: *const Callback,
    answer: *mut Answer,
) -> io::Result<(&'a Thread, &'a Callback, &'a mut Answer)> {
    if opaque.is_null() || callback.is_null() || answer.is_null() {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    // The real C owner holds this engine-created locator and callback frame.
    // There is no pointer/basename discovery or serialized Rust object input.
    let callback = unsafe { &*callback };
    callback.validate(callback.identity)?;
    let thread = unsafe { &*opaque.cast::<Thread>() };
    thread.validate(callback)?;
    let answer = unsafe { &mut *answer };
    *answer = Answer::default();
    Ok((thread, callback, answer))
}

pub(super) fn event(callback: &Callback, number: i64, args: [u64; 6], ip: u64) -> SyscallEvent {
    SyscallEvent {
        number,
        args,
        instruction_pointer: ip,
        result: 0,
        context: callback.context,
        dispatch: SyscallDispatch::InstalledHook,
        guest_pkru: None,
    }
}

/// # Safety
/// Actual admitted native context, engine-created thread locator, initialized
/// scalar args and writable output. All real TLS/allocator/stack conditions of
/// the common native Guest entry apply. This is not private-image admission.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hermit_engine_native_syscall_v1(
    opaque: *mut c_void,
    callback: *const Callback,
    number: i64,
    args: *const [u64; 6],
    ip: u64,
    output: *mut Answer,
) -> i32 {
    status((|| {
        if args.is_null() {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let (thread, callback, answer) = unsafe { borrowed(opaque, callback, output)? };
        let mut entry = thread.enter(callback)?;
        if entry.state().pending.is_some() {
            return Err(io::Error::from_raw_os_error(libc::EBUSY));
        }
        let engine = owner::installed()?;
        let raw = callback.validate(callback.identity)?;
        if raw.read_memory.is_none() || raw.write_memory.is_none() {
            return Err(io::Error::from_raw_os_error(libc::EPROTO));
        }
        let original = unsafe { args.read() };
        let prepared = match super::prelude::prepare(callback, raw, number, original) {
            Ok(prepared) => prepared,
            Err(error) => {
                answer.action = 1;
                answer.value = -i64::from(error.into_raw());
                super::prelude::rewritten();
                return Ok(());
            }
        };
        let table = raw.selected()?;
        let auxv = unsafe { callback.copy_auxv()? };
        let services = unsafe { callback.select(&table, &auxv)? };
        let lease = unsafe {
            engine
                .host
                .enter_native_callback(entry.state().handle()?, entry.identity())?
        };
        let mut event = event(callback, number, prepared.arguments, ip);
        let pending_exec = &mut entry.state_mut().prepared_exec;
        let result = unsafe {
            engine.host.with_native_guest(
                lease,
                &services,
                &engine.rpc,
                &mut event,
                |tool, guest, tail| {
                    super::prelude::dispatch(
                        super::prelude::DispatchCall {
                            engine,
                            callback,
                            number,
                            original,
                            prepared: &prepared,
                        },
                        pending_exec,
                        tool,
                        guest,
                        tail,
                        &services,
                    )
                },
            )
        }
        .map_err(tool_error)?;
        let original_native = super::prelude::is_native_lifecycle(number);
        retain(&mut entry, result, answer, |value, answer| {
            answer.action = 1;
            answer.value = value;
            super::prelude::rewritten();
        })?;
        if answer.action == 2 && original_native {
            answer.action = 3;
        }
        Ok(())
    })())
}

/// # Safety
/// Same actual native callback ownership as syscall; `kind` is 0 CPUID, 1
/// RDTSC or 2 RDTSCP. No client-local time/CPUID fallback is selected on error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hermit_engine_native_instruction_v1(
    opaque: *mut c_void,
    callback: *const Callback,
    kind: u32,
    leaf: u32,
    subleaf: u32,
    ip: u64,
    output: *mut Answer,
) -> i32 {
    status((|| {
        let instruction = match kind {
            0 => NativeInstruction::Cpuid { leaf, subleaf },
            1 => NativeInstruction::Rdtsc(reverie::Rdtsc::Tsc),
            2 => NativeInstruction::Rdtsc(reverie::Rdtsc::Tscp),
            _ => return Err(io::Error::from_raw_os_error(libc::EINVAL)),
        };
        let (thread, callback, answer) = unsafe { borrowed(opaque, callback, output)? };
        let mut entry = thread.enter(callback)?;
        if entry.state().pending.is_some() {
            return Err(io::Error::from_raw_os_error(libc::EBUSY));
        }
        super::prelude::branches(callback.branches);
        let engine = owner::installed()?;
        let table = callback.validate(callback.identity)?.selected()?;
        let auxv = unsafe { callback.copy_auxv()? };
        let services = unsafe { callback.select(&table, &auxv)? };
        let lease = unsafe {
            engine
                .host
                .enter_native_callback(entry.state().handle()?, entry.identity())?
        };
        let mut event = event(callback, -1, [0; 6], ip);
        let result = unsafe {
            engine.host.dispatch_native_instruction(
                lease,
                &services,
                &engine.rpc,
                &mut event,
                instruction,
            )
        }
        .map_err(tool_error)?;
        retain(&mut entry, result, answer, |value, answer| {
            answer.action = 1;
            match value {
                NativeInstructionResult::Cpuid(value) => {
                    answer.registers = [value.eax, value.ebx, value.ecx, value.edx]
                }
                NativeInstructionResult::Rdtsc(value) => {
                    answer.value = value.tsc as i64;
                    answer.registers[0] = value.aux.unwrap_or(0);
                }
            }
        })
    })())
}

/// # Safety
/// Original owned Linux siginfo and actual pending native signal continuation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hermit_engine_native_signal_v1(
    opaque: *mut c_void,
    callback: *const Callback,
    signal: i32,
    ip: u64,
    output: *mut Answer,
) -> i32 {
    status((|| {
        let (thread, callback, answer) = unsafe { borrowed(opaque, callback, output)? };
        super::prelude::branches(callback.branches);
        let engine = owner::installed()?;
        // Preserve the maintained native signal selection before entering the
        // Tool: its own child-exit report replaces the kernel SIGCHLD copy, and
        // unnamed realtime signals keep the existing native delivery owner.
        let bypasses_tool = match Signal::try_from(signal) {
            Ok(Signal::SIGCHLD) => !engine.config.backend.reports_kernel_child_exit_signals,
            Ok(_) => false,
            Err(_) if (libc::SIGRTMIN()..=libc::SIGRTMAX()).contains(&signal) => true,
            Err(_) => return Err(io::Error::from_raw_os_error(libc::EINVAL)),
        };
        if bypasses_tool {
            if callback.signal_info.is_null() {
                return Err(io::Error::from_raw_os_error(libc::EPROTO));
            }
            answer.action = 0;
            answer.signal = signal;
            answer.signal_info = unsafe { callback.signal_info.read() };
            return Ok(());
        }
        let mut entry = thread.enter(callback)?;
        if entry.state().pending.is_some() || callback.signal_info.is_null() {
            return Err(io::Error::from_raw_os_error(libc::EPROTO));
        }
        let table = callback.validate(callback.identity)?.selected()?;
        let auxv = unsafe { callback.copy_auxv()? };
        let services = unsafe { callback.select(&table, &auxv)? };
        let lease = unsafe {
            engine
                .host
                .enter_native_callback(entry.state().handle()?, entry.identity())?
        };
        let mut event = event(callback, -1, [0; 6], ip);
        let result = unsafe {
            engine
                .host
                .dispatch_native_signal(lease, &services, &engine.rpc, &mut event, signal)
        }
        .map_err(tool_error)?;
        retain(&mut entry, result, answer, |value, answer| match value {
            NativeSignalResult::Suppress => answer.action = 1,
            NativeSignalResult::Deliver { signal, info } => {
                answer.action = 0;
                answer.signal = signal as i32;
                answer.signal_info_changed =
                    u32::from(info != unsafe { callback.signal_info.read() });
                answer.signal_info = info;
            }
        })
    })())
}

/// Called by the genuine native post-event owner AFTER all registers and
/// clone/exec/signal bookkeeping and scratch use. Not by a Rust-return shim.
/// # Safety
/// Exactly one real post-event for the original deferred operation and task.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hermit_engine_native_completed_v1(
    opaque: *mut c_void,
    callback: *const Callback,
) -> i32 {
    status((|| {
        let mut ignored = Answer::default();
        let (thread, callback, _) = unsafe { borrowed(opaque, callback, &mut ignored)? };
        let mut entry = thread.enter(callback)?;
        let table = callback.validate(callback.identity)?.selected()?;
        let auxv = unsafe { callback.copy_auxv()? };
        let services = unsafe { callback.select(&table, &auxv)? };
        let pending = entry
            .state_mut()
            .pending
            .take()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EPROTO))?;
        unsafe { pending.complete(services.invocation()) }
    })())
}

/// The maintained capability-clear native thread-exit owner. Its original
/// protocol supplies no explicit terminal fact: the actual Tool hook receives
/// SUCCESS, its completed Result is ignored, and no terminal receipt is issued.
/// C retains physical exit, evidence framing, RPC shutdown and drain.
///
/// # Safety
/// The original initialized task has stopped using all deferred scratch and
/// this is its unique engine locator. The C owner clears that locator before
/// entering, so a consumed lifecycle or freed Box cannot be retried. An absent
/// locator represents the maintained uninitialized/no-Tool path only. Context,
/// owner/RPC/retirement failures and unexpected tail injection remain fatal.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hermit_engine_native_thread_exit_v1(
    opaque: *mut c_void,
    callback: *const Callback,
    _code: i32,
) -> i32 {
    if opaque.is_null() {
        return 0;
    }
    status((|| {
        let mut ignored = Answer::default();
        let (thread, callback, _) = unsafe { borrowed(opaque, callback, &mut ignored)? };
        let mut entry = thread.enter(callback)?;
        let engine = owner::installed()?;
        // A post-completed copied child has no initialized Tool state until
        // the maintained later initializer. It must not manufacture an exit
        // hook or drop its retained parent through this consuming entry.
        let handle = entry.state().handle()?;
        let table = callback.validate(callback.identity)?.selected()?;
        let auxv = unsafe { callback.copy_auxv()? };
        let services = unsafe { callback.select(&table, &auxv)? };
        let identity = entry.identity();
        let rpc = NativeExitRpc::Application(engine.rpc.application(services.invocation()));
        entry.state_mut().retired = true;
        // Keep the original Deferred, if present. Authenticating another
        // available-state lease would lose its scratch/consumption lifetime.
        let outcome = match entry.state_mut().pending.take() {
            Some(deferred) => unsafe {
                engine.host.dispatch_native_deferred_thread_exit_outcome(
                    deferred,
                    identity,
                    rpc,
                    ExitStatus::SUCCESS,
                )
            },
            None => unsafe {
                engine.host.dispatch_native_thread_exit_outcome(
                    handle,
                    identity,
                    rpc,
                    ExitStatus::SUCCESS,
                )
            },
        };
        drop(entry);
        // Retire this native locator after the exclusive Entry releases. A
        // pre-consumption mechanism error may retain original common state,
        // but it cannot authorize a second callback or a replacement Tool.
        unsafe {
            drop(Box::from_raw(opaque.cast::<Thread>()));
        }
        match outcome.map_err(tool_error)? {
            // This is the existing native wrapper's returned-Tool-error
            // convention, not permission to ignore a mechanism failure.
            NativeCallbackOutcome::Ready(_tool_result) => Ok(()),
            NativeCallbackOutcome::Deferred { .. } => Err(io::Error::other(
                "native thread-exit handler unexpectedly tail-injected",
            )),
            NativeCallbackOutcome::Unsupported(reason) => Err(io::Error::other(reason)),
        }
    })())
}
