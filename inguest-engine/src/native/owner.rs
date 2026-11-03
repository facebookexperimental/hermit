//! The one native ToolHost and opaque per-thread continuation owner.
//!
//! The C client stores only a pointer it cannot interpret. Exclusive access
//! begins after the actual context/current-task check and before creating a
//! mutable Rust borrow. No image/process mutex spans a Tool future or RPC.

use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::io;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicPtr;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use detcore::Config;
use detcore::Detcore;
use detcore::GlobalState;
use reverie::Pid;
use reverie_inguest::guest::host::ToolHost;
use reverie_inguest::guest::host::native_state::DeferredNative;
use reverie_inguest::guest::host::native_state::NativeThreadHandle;
use reverie_inguest::guest::host::native_state::NativeThreadIdentity;
use reverie_inguest::guest::host::native_state::PostCompletedCopiedNativeParent;
use reverie_inguest::guest::native_rpc::NativeRpc;

use super::abi::Callback;
use super::abi::Identity;
use crate::runtime::EngineRuntime;

pub(super) type Host = ToolHost<Detcore, EngineRuntime>;
pub(super) type State = <Detcore as reverie::Tool>::ThreadState;

/// This exact concrete host is also the ordinary LiteInst/SaBRe host type.
/// Installation is mutually exclusive with those process-image installations.
pub(super) struct Engine {
    pub host: Host,
    pub rpc: NativeRpc<GlobalState>,
    pub config: Arc<Config>,
    pub root: Pid,
    pub physical_owner: i32,
    pub image: u64,
    // These handoffs and the original ordinal belong to this exact Runtime
    // owner, initialized together with its one Tool. A copied process cannot
    // inspect them before its native process-owner rebind has succeeded.
    pub pending_parents: Mutex<HashMap<i32, super::lifecycle::PendingParent>>,
    pub next_child_ordinal: AtomicU64,
}

// Every published Engine has process-image lifetime. A real COW child may
// replace this pointer with fresh process storage only after transferring the
// original affine continuation. It must never lock or destroy the inherited
// Engine: another vanished thread may have held one of its locks at fork.
// The marker retains the same Engine: Sync requirement as the old OnceLock;
// AtomicPtr alone must not silently remove that compile-time requirement.
struct Publication {
    pointer: AtomicPtr<Engine>,
    _owner: PhantomData<&'static Engine>,
}
static ENGINE: Publication = Publication {
    pointer: AtomicPtr::new(std::ptr::null_mut()),
    _owner: PhantomData,
};

pub(super) struct CopiedInitialization {
    pub parent: PostCompletedCopiedNativeParent<Detcore>,
    pub flags: reverie::syscalls::CloneFlags,
}

pub(super) struct ThreadState {
    pub handle: Option<NativeThreadHandle>,
    pub pending: Option<DeferredNative<'static, Detcore>>,
    pub copied: Option<CopiedInitialization>,
    pub prepared_exec: Option<super::prelude::PreparedExec>,
    pub retired: bool,
    pub failed: bool,
}

impl ThreadState {
    pub(super) fn handle(&self) -> io::Result<NativeThreadHandle> {
        self.handle
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EAGAIN))
    }
}

pub(super) struct Thread {
    identity: Identity,
    native_identity: NativeThreadIdentity,
    entered: AtomicBool,
    state: UnsafeCell<ThreadState>,
}

impl Thread {
    pub(super) fn new(
        identity: Identity,
        native_identity: NativeThreadIdentity,
        handle: NativeThreadHandle,
    ) -> Self {
        Self {
            identity,
            native_identity,
            entered: AtomicBool::new(false),
            state: UnsafeCell::new(ThreadState {
                handle: Some(handle),
                pending: None,
                copied: None,
                prepared_exec: None,
                retired: false,
                failed: false,
            }),
        }
    }

    pub(super) fn copied(
        identity: Identity,
        native_identity: NativeThreadIdentity,
        parent: PostCompletedCopiedNativeParent<Detcore>,
        flags: reverie::syscalls::CloneFlags,
    ) -> Self {
        Self {
            identity,
            native_identity,
            entered: AtomicBool::new(false),
            state: UnsafeCell::new(ThreadState {
                handle: None,
                pending: None,
                copied: Some(CopiedInitialization { parent, flags }),
                prepared_exec: None,
                retired: false,
                failed: false,
            }),
        }
    }

    /// The fixed C pointer is usable only at its actual native task/image.
    /// Wrong-thread checks precede the exclusive mutable-state borrow.
    pub(super) fn enter(&self, callback: &Callback) -> io::Result<Entry<'_>> {
        self.validate(callback)?;
        if self
            .entered
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return Err(io::Error::from_raw_os_error(libc::EBUSY));
        }
        let entry = Entry(self);
        if entry.state().retired || entry.state().failed {
            return Err(io::Error::from_raw_os_error(libc::ESTALE));
        }
        Ok(entry)
    }

    pub(super) fn validate(&self, callback: &Callback) -> io::Result<()> {
        callback.validate(self.identity).map(|_| ())
    }

    /// # Safety
    /// The native post-event owner has established a real independent COW
    /// child, not a shared-VM/vfork child. No native use of the inherited
    /// original scratch remains. This claim only exposes the already-owned
    /// continuation; it grants no access to an inherited registry or Tool
    /// mutex, and its error path must never call Entry::fail there.
    pub(super) unsafe fn enter_copied(&self, callback: &Callback) -> io::Result<Entry<'_>> {
        callback.validate(callback.identity)?;
        if callback.identity.physical_pid == self.identity.physical_pid
            || callback.identity.physical_pid != callback.identity.physical_tid
            || callback.identity.tool_pid != callback.identity.tool_tid
            || callback.identity.image != self.identity.image
        {
            return Err(io::Error::from_raw_os_error(libc::ESTALE));
        }
        if self
            .entered
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return Err(io::Error::from_raw_os_error(libc::EBUSY));
        }
        let entry = Entry(self);
        if entry.state().retired
            || entry.state().failed
            || entry.state().copied.is_some()
            || entry.state().handle.is_none()
            || entry.state().pending.is_none()
        {
            return Err(io::Error::from_raw_os_error(libc::ESTALE));
        }
        Ok(entry)
    }

    pub(super) fn scalar_identity(&self) -> Identity {
        self.identity
    }
}

pub(super) struct Entry<'thread>(&'thread Thread);

impl Entry<'_> {
    pub(super) fn state(&self) -> &ThreadState {
        // SAFETY: this Entry owns the atomic exclusive callback claim.
        unsafe { &*self.0.state.get() }
    }
    pub(super) fn state_mut(&mut self) -> &mut ThreadState {
        // SAFETY: the claim is exclusive, and cannot be moved to another task.
        unsafe { &mut *self.0.state.get() }
    }
    pub(super) fn identity(&self) -> NativeThreadIdentity {
        self.0.native_identity
    }
    pub(super) fn fail(&mut self) {
        self.state_mut().failed = true;
        // A failed post step must not publish the original active state.
        // Dropping its deferred owner invokes common poisoning, never retry.
        drop(self.state_mut().pending.take());
    }
}

impl Drop for Entry<'_> {
    fn drop(&mut self) {
        self.0.entered.store(false, Ordering::Release);
    }
}

pub(super) fn installed() -> io::Result<&'static Engine> {
    let pointer = ENGINE.pointer.load(Ordering::Acquire);
    if pointer.is_null() {
        return Err(io::Error::from_raw_os_error(libc::ENXIO));
    }
    // SAFETY: published owners are never freed or mutably aliased.
    let engine = unsafe { &*pointer };
    if engine.physical_owner != unsafe { libc::syscall(libc::SYS_getpid) as i32 }
        || engine.image != super::image::generation()
    {
        return Err(io::Error::from_raw_os_error(libc::ESTALE));
    }
    Ok(engine)
}

/// Retain one owner only; a failed/duplicate installation is not reusable.
/// The caller has already constructed the authentic config/RPC/Tool closure.
pub(super) fn publish(engine: Engine) -> io::Result<&'static Engine> {
    let pointer = Box::into_raw(Box::new(engine));
    if ENGINE
        .pointer
        .compare_exchange(
            std::ptr::null_mut(),
            pointer,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        // A duplicate fresh-image owner has no copied parent Record.
        unsafe {
            drop(Box::from_raw(pointer));
        }
        return Err(io::Error::from_raw_os_error(libc::EALREADY));
    }
    installed()
}

/// # Safety
/// Only the original returning COW child's post owner may borrow immutable
/// inherited fields. No inherited lock may be entered or destructor run.
pub(super) unsafe fn inherited(
    callback: &Callback,
    parent: Identity,
) -> io::Result<&'static Engine> {
    callback.validate(callback.identity)?;
    let pointer = ENGINE.pointer.load(Ordering::Acquire);
    if pointer.is_null() {
        return Err(io::Error::from_raw_os_error(libc::ENXIO));
    }
    let engine = unsafe { &*pointer };
    if engine.physical_owner != parent.physical_pid
        || engine.physical_owner == callback.identity.physical_pid
        || engine.image != parent.image
        || engine.image != callback.identity.image
        || engine.image != super::image::generation()
    {
        return Err(io::Error::from_raw_os_error(libc::ESTALE));
    }
    Ok(engine)
}

/// Publish only the checked fresh COW storage. Both owners retain process
/// lifetime; even a fatal duplicate publication must not destroy an inherited
/// parent Record or try to retire it through a copied registry.
pub(super) unsafe fn publish_copied(
    inherited: &'static Engine,
    engine: Engine,
) -> io::Result<&'static Engine> {
    let pointer = Box::into_raw(Box::new(engine));
    ENGINE
        .pointer
        .compare_exchange(
            std::ptr::from_ref(inherited).cast_mut(),
            pointer,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .map_err(|_| io::Error::from_raw_os_error(libc::ESTALE))?;
    installed()
}
