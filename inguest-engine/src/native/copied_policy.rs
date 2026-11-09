//! The maintained copied-process syscall gate, inside the one core DSO.
//!
//! Shared-VM/vfork children cannot enter an inherited Tool, RPC registry,
//! allocator or mutex. This exact policy is moved from the maintained native
//! Detcore owner; C forwards only its real closed-copy branch and arguments.
//! It never constructs a NativeGuest or issues a lifecycle permit.

#[path = "copied_action.rs"]
mod policy_action;

/// The genuine client's closed-copy callback. It deliberately carries no
/// virtual TID, Tool handle, native RPC or lifecycle authority. In particular
/// it must not validate an inherited parent locator as a vfork child's own.
#[repr(C)]
pub struct CopiedCallback {
    pub version: u32,
    pub size: u32,
    pub physical_pid: i32,
    pub physical_tid: i32,
    pub image: u64,
    pub context: usize,
    pub current: Option<reverie_inguest::guest::native_rpc::NativeCurrent>,
}

/// # Safety
/// Only the actual native client's maintained closed-copy pre-syscall path
/// may call this entry. That owner supplies its current context and actual
/// physical IDs/image, with its ordinary native stack/FS and fixed allocator
/// backing intact. The held/hash-bound callback and at most six argument
/// words stay live through the call. Numeric fields do not grant Tool entry.
///
/// Status and guest action have separate channels: a negative status is a
/// mechanism refusal, never a guest errno or permission for kernel fallback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hermit_engine_native_copied_syscall_v1(
    callback: *const CopiedCallback,
    number: i64,
    arguments: *const u64,
    action: *mut i32,
) -> i32 {
    if callback.is_null() || action.is_null() {
        return -libc::EINVAL;
    }
    let callback = unsafe { &*callback };
    if callback.version != super::abi::VERSION
        || callback.size as usize != size_of::<CopiedCallback>()
        || callback.physical_pid != unsafe { libc::syscall(libc::SYS_getpid) as i32 }
        || callback.physical_tid != unsafe { libc::syscall(libc::SYS_gettid) as i32 }
        || callback.physical_pid <= 0
        || callback.physical_tid <= 0
        || callback.context == 0
    {
        return -libc::ESTALE;
    }
    let Some(current) = callback.current else {
        return -libc::ESTALE;
    };
    if unsafe { current(callback.context) } != 1 {
        return -libc::ESTALE;
    }
    let (strict, report) =
        match super::bootstrap::copied_policy(callback.image, callback.physical_pid) {
            Ok(policy) => policy,
            Err(error) => return -error.raw_os_error().unwrap_or(libc::EPROTO),
        };
    let result = unsafe {
        policy_action::copied_action(number, arguments, strict, report, super::diagnostics::emit)
    };
    // Recheck the same real callback after the raw diagnostic/report writes;
    // a changed context cannot turn their result into an admitted guest action.
    if unsafe { current(callback.context) } != 1
        || callback.physical_pid != unsafe { libc::syscall(libc::SYS_getpid) as i32 }
        || callback.physical_tid != unsafe { libc::syscall(libc::SYS_gettid) as i32 }
        || super::bootstrap::copied_policy(callback.image, callback.physical_pid).is_err()
    {
        return -libc::ESTALE;
    }
    unsafe {
        action.write(result);
    }
    0
}
