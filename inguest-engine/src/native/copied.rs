//! Real independent-COW child post, followed by its later native initializer.
//!
//! The C owner supplies the original operation/result before its complete
//! post body clears the birth transaction, then samples the actual child
//! context after that body and register canonicalization. A shared-VM/vfork
//! child never enters this transfer. No inherited process lock is acquired.

use std::collections::HashMap;
use std::ffi::c_void;
use std::io;
use std::mem::ManuallyDrop;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use detcore::Detcore;
use detcore::GlobalState;
use reverie::Pid;
use reverie::syscalls::CloneFlags;
use reverie_inguest::guest::host::native_state::CopiedNativeParent;
use reverie_inguest::guest::native_rpc::NativeRpc;

use super::abi::Callback;
use super::entry::status;
use super::owner::Host;
use super::owner::Thread;
use super::owner::{self};

/// Actual native birth transaction, borrowed across its unchanged post body.
/// These scalar facts are not the affine Rust transfer authority.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct CopyPost {
    pub number: i64,
    pub result: i64,
    pub flags: u64,
}

// On a fatal error after the original continuation has moved, retaining this
// allocation is deliberate. Dropping an inherited parent Record outside its
// real later initialization/start scopes could run an inherited destructor or
// lock. The native fatal path terminates that process; it cannot retry it.
struct Transfer {
    host: Host,
    rpc: NativeRpc<GlobalState>,
    parent: Option<CopiedNativeParent<Detcore>>,
}

/// # Safety
/// This is the real child's returning post for fork/separate-VM clone/clone3.
/// `facts` was sampled from that original birth transaction before its native
/// post body; the supplied callback is sampled after ALL of that body and
/// canonical register writes. The child has an independent COW address space,
/// actual leader identity, original live image, allocator/TLS/stack entry and
/// no remaining concurrent native use of the original scratch. This must not
/// be called at a synthetic child post or for any shared-VM/vfork child.
/// `output` is a unique null replacement-locator slot in the real C owner.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hermit_engine_native_copied_post_v1(
    opaque: *mut c_void,
    callback: *const Callback,
    facts: *const CopyPost,
    output: *mut *mut c_void,
) -> i32 {
    status((|| {
        if opaque.is_null()
            || callback.is_null()
            || facts.is_null()
            || output.is_null()
            || unsafe { !output.read().is_null() }
        {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let callback = unsafe { &*callback };
        let facts = unsafe { facts.read() };
        let flags = CloneFlags::from_bits_truncate(facts.flags);
        let prohibited = CloneFlags::CLONE_VM
            | CloneFlags::CLONE_VFORK
            | CloneFlags::CLONE_THREAD
            | CloneFlags::CLONE_FILES;
        if facts.result != 0
            || flags.intersects(prohibited)
            || !matches!(
                facts.number,
                libc::SYS_fork | libc::SYS_clone | libc::SYS_clone3
            )
        {
            return Err(io::Error::from_raw_os_error(libc::ESTALE));
        }
        let thread = unsafe { &*opaque.cast::<Thread>() };
        let mut entry = unsafe { thread.enter_copied(callback)? };
        let inherited = unsafe { owner::inherited(callback, thread.scalar_identity())? };
        let original = entry
            .state()
            .pending
            .as_ref()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ESTALE))?;
        let (number, arguments) = original.operation();
        if number != facts.number || (number == libc::SYS_clone && arguments[0] != facts.flags) {
            return Err(io::Error::from_raw_os_error(libc::ESTALE));
        }
        let ordinal = inherited.next_child_ordinal.load(Ordering::SeqCst);
        if ordinal == 0 {
            return Err(io::Error::from_raw_os_error(libc::ESTALE));
        }
        let identity = callback.identity;
        let native_identity = unsafe { identity.admitted()? };
        let table = callback.validate(identity)?.selected()?;
        let auxv = unsafe { callback.copy_auxv()? };
        let services = unsafe { callback.select(&table, &auxv)? };
        let rebound = unsafe {
            inherited.host.rebind_copied_native_process(
                &mut entry.state_mut().pending,
                &services,
                &inherited.rpc,
                super::bootstrap::fresh_runtime(),
            )?
        };
        // All pre-transfer refusals left the original Deferred present. From
        // this point a fatal failure must retain, not drop, its real Record.
        let mut retained = ManuallyDrop::new(Transfer {
            host: rebound.host,
            rpc: rebound.rpc,
            parent: Some(rebound.parent),
        });
        let transfer = &mut *retained;
        let parent = unsafe {
            transfer.host.complete_copied_native_post(
                &services,
                &transfer.rpc,
                &mut transfer.parent,
            )?
        };
        let transfer = ManuallyDrop::into_inner(retained);
        let created = Box::into_raw(Box::new(Thread::copied(
            identity,
            native_identity,
            parent,
            flags,
        )));
        // Preserve the Runtime's actual inherited ordinal value. The later
        // actual initializer alone increments it for the unchanged RNG pack;
        // a copied process never resets it to a fresh-image value of one.
        let engine = owner::Engine {
            host: transfer.host,
            rpc: transfer.rpc,
            config: Arc::clone(&inherited.config),
            root: Pid::from_raw(identity.tool_pid),
            physical_owner: identity.physical_pid,
            image: identity.image,
            pending_parents: Mutex::new(HashMap::new()),
            next_child_ordinal: AtomicU64::new(ordinal),
        };
        // Failure retains both new allocations and the original Record for
        // the fatal process exit. It never destroys an inherited Engine.
        unsafe {
            owner::publish_copied(inherited, engine)?;
        }
        entry.state_mut().retired = true;
        drop(entry);
        // rebind moved the original Deferred/Record; this old locator now
        // holds no state, scratch or copied-parent destructor to release.
        unsafe {
            drop(Box::from_raw(opaque.cast::<Thread>()));
        }
        unsafe {
            output.write(created.cast());
        }
        Ok(())
    })())
}

/// # Safety
/// The actual native first-subsequent-event initializer selected by the
/// existing copied-process owner, after the real child post above. It may not
/// be replaced by an arbitrary syscall callback or a request to create a Tool.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hermit_engine_native_initialize_copied_thread_v1(
    opaque: *mut c_void,
    callback: *const Callback,
) -> i32 {
    status((|| {
        if opaque.is_null() || callback.is_null() {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let callback = unsafe { &*callback };
        let thread = unsafe { &*opaque.cast::<Thread>() };
        let mut entry = thread.enter(callback)?;
        let engine = owner::installed()?;
        if entry.state().handle.is_some()
            || entry.state().pending.is_some()
            || engine.physical_owner != callback.identity.physical_pid
            || engine.root.as_raw() != callback.identity.tool_pid
            || callback.identity.physical_pid != callback.identity.physical_tid
        {
            return Err(io::Error::from_raw_os_error(libc::ESTALE));
        }
        let table = callback.validate(callback.identity)?.selected()?;
        let auxv = unsafe { callback.copy_auxv()? };
        let services = unsafe { callback.select(&table, &auxv)? };
        let parent = entry
            .state_mut()
            .copied
            .take()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ESTALE))?;
        let result = unsafe {
            engine.host.initialize_postcompleted_copied_native_thread(
                &services,
                &engine.rpc,
                parent.parent,
                |original| {
                    original.clone_flags = Some(parent.flags);
                    Ok(())
                },
                |state, original| {
                    super::lifecycle::project(state, callback.identity);
                    state.reset_copied_open_file_locks();
                    // The returning copied child cannot touch inherited Tool
                    // locks in post. Before ANY child Tool callback, its fresh
                    // state receives the maintained successful-copy invalidation.
                    state.forget_flock_modes();
                    let entropy = super::lifecycle::entropy(engine, callback.identity.tool_pid)?;
                    state.reseed_child_rngs(original, entropy);
                    Ok(())
                },
            )
        };
        let handle = match result {
            Ok(handle) => handle,
            Err(error) => {
                entry.fail();
                return Err(error);
            }
        };
        entry.state_mut().handle = Some(handle);
        drop(entry);
        // Common lifecycle invokes actual thread_start once, no post_exec,
        // then drops the whole inherited parent under its original scopes.
        unsafe { super::lifecycle::finish_first_entry(thread, callback) }
    })())
}

const _: () = assert!(size_of::<CopyPost>() == 24);
