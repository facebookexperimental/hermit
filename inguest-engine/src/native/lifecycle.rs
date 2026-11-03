//! The concrete Detcore root/child projection at actual native entry.
//!
//! Common ToolHost owns Tool/state/scratch/RPC. This module supplies the
//! existing Detcore identity and RNG projection, using the real parent's
//! one-use snapshot before its registration mutates the parent's pedigree.
//! No native C code or separately linked Rust runtime owns a Detcore here.

use std::ffi::c_void;
use std::io;
use std::sync::atomic::Ordering;

use detcore::DetTid;
use reverie::Guest;
use reverie::Tid;
use reverie::syscalls::CloneFlags;
use reverie_inguest::guest::host::NativeCallResult;
use reverie_inguest::guest::host::native_state::NativeParentSnapshot;
use reverie_inguest::tool_host::NativeCallbackOutcome;
use reverie_inguest::tool_host::drive_native_ready;

use super::abi::Callback;
use super::abi::Identity;
use super::entry::event;
use super::entry::status;
use super::owner::State;
use super::owner::Thread;
use super::owner::{self};

pub(super) struct PendingParent {
    snapshot: NativeParentSnapshot<State>,
    entropy: u128,
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct Child {
    pub identity: Identity,
    pub child_tid_address: u64,
    pub flags: u64,
    pub exit_signal: i32,
    pub reserved: u32,
}

pub(super) fn project(state: &mut State, identity: Identity) {
    state.detpid = Some(DetTid::from_raw(identity.tool_pid));
    state.physical_tid = Some(identity.physical_tid);
}

pub(super) fn entropy(engine: &owner::Engine, pid: i32) -> io::Result<u128> {
    let ordinal = engine.next_child_ordinal.fetch_add(1, Ordering::SeqCst);
    if pid <= 0 || ordinal == 0 {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    Ok(((pid as u32 as u128) << 64) | u128::from(ordinal))
}

pub(super) unsafe fn finish_first_entry(thread: &Thread, callback: &Callback) -> io::Result<()> {
    let engine = owner::installed()?;
    let raw = callback.validate(callback.identity)?;
    let table = raw.selected()?;
    let auxv = unsafe { callback.copy_auxv()? };
    let services = unsafe { callback.select(&table, &auxv)? };
    let mut entry = thread.enter(callback)?;
    let lease = unsafe {
        engine
            .host
            .enter_native_callback(entry.state().handle()?, entry.identity())?
    };
    let mut first = event(callback, -1, [0; 6], 0);
    let outcome = unsafe {
        engine.host.with_native_guest(
            lease,
            &services,
            &engine.rpc,
            &mut first,
            |_tool, _guest, _tail| NativeCallbackOutcome::Ready(Ok(())),
        )
    };
    match outcome {
        Ok(NativeCallResult::Return(())) => Ok(()),
        Ok(NativeCallResult::Deferred(_)) => {
            entry.fail();
            Err(io::Error::other(
                "native first lifecycle deferred before admission",
            ))
        }
        Err(error) => {
            entry.fail();
            Err(io::Error::other(error))
        }
    }
}

/// # Safety
/// The real native image/thread owner has established the current task,
/// published virtual identity, actual parent handoff and allocator/FS/stack
/// entry. `output` is its unique null locator slot, not a copied inherited
/// parent's locator. A positive retry means the real parent has not queued its
/// snapshot yet; no Tool state or replacement parent is constructed then.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hermit_engine_native_initialize_thread_v1(
    callback: *const Callback,
    output: *mut *mut c_void,
) -> i32 {
    let attempt = (|| -> io::Result<bool> {
        if callback.is_null() || output.is_null() || unsafe { !output.read().is_null() } {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let callback = unsafe { &*callback };
        let identity = callback.identity;
        let raw = callback.validate(identity)?;
        let native_identity = unsafe { identity.admitted()? };
        // Only the actual application root creates the one Tool. A copied
        // physical process is refused before its parent's queue/ordinal can
        // be read; its real COW lifecycle rebind is a separate required join.
        let engine = super::bootstrap::install_root(callback)?;
        if engine.physical_owner != identity.physical_pid
            || engine.image != identity.image
            || engine.root.as_raw() != identity.tool_pid
        {
            return Err(io::Error::from_raw_os_error(libc::ESTALE));
        }
        let table = raw.selected()?;
        let auxv = unsafe { callback.copy_auxv()? };
        let services = unsafe { callback.select(&table, &auxv)? };
        let handle = if identity.physical_tid == identity.physical_pid {
            unsafe {
                engine
                    .host
                    .initialize_native_image_thread(&services, &engine.rpc, |state| {
                        project(state, identity);
                        state.set_open_file_creator(DetTid::from_raw(identity.tool_tid));
                        Ok(())
                    })?
            }
        } else {
            let parent = {
                let mut pending = engine
                    .pending_parents
                    .lock()
                    .expect("native parent handoff lock poisoned");
                let Some(parent) = pending.get(&identity.physical_tid) else {
                    return Ok(false);
                };
                if parent.snapshot.child_identity() != native_identity {
                    // A stale virtual TID consumes the stale handoff; numeric
                    // PID reuse cannot turn it into the requested new parent.
                    pending.remove(&identity.physical_tid);
                    return Err(io::Error::from_raw_os_error(libc::ESTALE));
                }
                pending
                    .remove(&identity.physical_tid)
                    .expect("checked native child handoff")
            };
            unsafe {
                engine.host.initialize_native_thread(
                    &services,
                    &engine.rpc,
                    parent.snapshot,
                    |state, original_parent| {
                        project(state, identity);
                        let (_, original_parent) = original_parent
                            .ok_or_else(|| io::Error::from_raw_os_error(libc::ESTALE))?;
                        state.reseed_child_rngs(original_parent, parent.entropy);
                        Ok(())
                    },
                )?
            }
        };
        let thread = Box::new(Thread::new(identity, native_identity, handle));
        // The native admission gate gets no locator between thread_start and
        // the fresh image's post_exec. Child initialization never gains an
        // invented post_exec; common lifecycle flags preserve that distinction.
        unsafe {
            finish_first_entry(&thread, callback)?;
        }
        unsafe {
            output.write(Box::into_raw(thread).cast());
        }
        Ok(true)
    })();
    match attempt {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(error) => -error.raw_os_error().unwrap_or(libc::EPROTO),
    }
}

/// # Safety
/// This is the actual parent's successful same-image CLONE_THREAD returning
/// post-event. `child` comes from the actual native birth/identity publication,
/// not requested clone IDs. The original deferred operation is still retained
/// by `opaque`; C must finish its remaining post bookkeeping before completion.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hermit_engine_native_thread_created_v1(
    opaque: *mut c_void,
    callback: *const Callback,
    child: *const Child,
) -> i32 {
    status((|| {
        if opaque.is_null() || callback.is_null() || child.is_null() {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let callback = unsafe { &*callback };
        let child = unsafe { child.read() };
        let thread = unsafe { &*opaque.cast::<Thread>() };
        let mut entry = thread.enter(callback)?;
        let result = (|| -> io::Result<()> {
            let engine = owner::installed()?;
            let raw = callback.validate(callback.identity)?;
            let child_identity = unsafe { child.identity.admitted()? };
            let flags = CloneFlags::from_bits_truncate(child.flags);
            if child.reserved != 0
                || !flags.contains(CloneFlags::CLONE_THREAD)
                || child.identity.physical_pid != callback.identity.physical_pid
                || child.identity.tool_pid != callback.identity.tool_pid
                || child.identity.image != callback.identity.image
                || child.identity.physical_tid == callback.identity.physical_tid
                || child.exit_signal != (flags.bits() & 0xff) as i32
            {
                return Err(io::Error::from_raw_os_error(libc::ESTALE));
            }
            let table = raw.selected()?;
            let auxv = unsafe { callback.copy_auxv()? };
            let services = unsafe { callback.select(&table, &auxv)? };
            let pending = entry
                .state_mut()
                .pending
                .as_mut()
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EPROTO))?;
            let snapshot = unsafe {
                engine.host.snapshot_native_parent(
                    pending,
                    &services,
                    &engine.rpc,
                    child_identity,
                    |state| {
                        state.clone_flags = Some(flags);
                        Ok(())
                    },
                )?
            };
            let entropy = entropy(engine, callback.identity.tool_pid)?;
            let queued = {
                let mut queue = engine
                    .pending_parents
                    .lock()
                    .expect("native parent handoff lock poisoned");
                match queue.entry(child.identity.physical_tid) {
                    std::collections::hash_map::Entry::Vacant(slot) => {
                        slot.insert(PendingParent { snapshot, entropy });
                        true
                    }
                    std::collections::hash_map::Entry::Occupied(slot) => {
                        // Preserve duplicate handoff refusal and remove the
                        // stale original; neither may be reused as a parent.
                        slot.remove();
                        false
                    }
                }
            };
            let mut post = event(callback, -1, [0; 6], 0);
            if !queued {
                unsafe {
                    engine.host.with_native_post_step(
                        pending,
                        &services,
                        &engine.rpc,
                        &mut post,
                        |_tool, guest, _tail| {
                            guest.thread_state_mut().clone_flags = None;
                            NativeCallbackOutcome::Ready(Ok(()))
                        },
                    )
                }
                .map_err(io::Error::other)?;
                return Err(io::Error::from_raw_os_error(libc::ESTALE));
            }
            // Snapshot/queue precede this call: register_external_child changes
            // the actual parent's pedigree. No handoff lock survives RPC.
            unsafe {
                engine.host.with_native_post_step(
                    pending,
                    &services,
                    &engine.rpc,
                    &mut post,
                    |tool, guest, _tail| {
                        drive_native_ready(tool.register_external_child(
                            guest,
                            Tid::from_raw(child.identity.tool_tid),
                            child.child_tid_address as usize,
                            flags,
                            child.exit_signal,
                            Some((child.identity.physical_pid, child.identity.physical_tid)),
                        ));
                        guest.thread_state_mut().clone_flags = None;
                        NativeCallbackOutcome::Ready(Ok(()))
                    },
                )
            }
            .map_err(io::Error::other)
        })();
        if result.is_err() {
            entry.fail();
        }
        result
    })())
}

const _: () = assert!(size_of::<Child>() == 48);
