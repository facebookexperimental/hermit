//! One first-party Detcore engine DSO for ordinary in-guest entry mechanisms.
//!
//! No InitialImage, successor, native-exec or mapped-image entry is exported.
//! Those issuers and affine authorities remain in the maintained leaves. This
//! package cannot inherit their source or finite qualification proofs.

#[cfg(not(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu")))]
compile_error!("the common in-guest engine supports GNU Linux x86-64 only");

#[path = "../../common/inguest/glibc_compat.rs"]
mod glibc_compat;
mod install;
mod loader_state;
// One concrete engine owns native mechanisms as well as saved-frame entry.
// These exports issue no initial/successor/private phase authority.
mod native;
#[cfg(feature = "dispatch-observer")]
mod observation;
mod runtime;

// The only allocator declaration in either shim/engine cohort. All Tool,
// Detcore, continuation, futures and coordinator storage stays in this DSO.
reverie_liteinst::declare_fixed_tool_allocator!();

use core::ffi::c_char;
use core::sync::atomic::AtomicU8;
use core::sync::atomic::Ordering;

use reverie_inguest::guest::context::RegisterContext;

// The C shim borrows this exact public repr(C) layout. Drift requires a new
// source/artifact cohort and ABI version rather than silent reinterpretation.
const _: () = {
    assert!(core::mem::size_of::<RegisterContext>() == 176);
    assert!(core::mem::align_of::<RegisterContext>() == 8);
    assert!(core::mem::offset_of!(RegisterContext, r11) == 48);
    assert!(core::mem::offset_of!(RegisterContext, rflags) == 136);
};

// Ordinary one-shot installation bookkeeping, NOT a private phase permit.
// There is no C input that can turn an initial image into a resumable Guest.
static INSTALLATION: AtomicU8 = AtomicU8::new(0);

static mut LOADER_ARGC: usize = 0;
static mut LOADER_ARGV: *const *const c_char = core::ptr::null();

/// The real ordinary loader negotiates this bounded transport before post_load.
/// Only the callback pointer is retained: no allocation, TLS, Tool, RPC or
/// lifecycle permission is created on this pre-bootstrap path.
///
/// # Safety
/// The callback must be the loaded, lifetime-stable loader's bounded transfer
/// function. It is never accepted from argv, environment or serialized state.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hermit_engine_install_loader_state_v1(
    callback: Option<loader_state::TakeStateFn>,
) -> i32 {
    if INSTALLATION.load(Ordering::Acquire) != 0 {
        return -libc::EPROTO;
    }
    let Some(callback) = callback else {
        return -libc::EINVAL;
    };
    // SAFETY: the genuine loader's symbol negotiation supplies this function.
    match unsafe { loader_state::install(callback) } {
        Ok(()) => 0,
        Err(errno) => -errno,
    }
}

/// Entry for the C preload constructor. Establishes the existing guarded
/// bootstrap stack/backing before Rust installation, allocation, environment
/// or Tool TLS use. The existing GNU symbol-compatibility initializer remains
/// separately owned by glibc_compat; this is not a private-image entry proof.
///
/// # Safety
/// Dynamic-loader constructor, one initial thread, before a seccomp filter.
#[unsafe(no_mangle)]
#[unsafe(naked)]
pub unsafe extern "C" fn hermit_engine_preload_initialize_v1() {
    core::arch::naked_asm!(
        "endbr64",
        "lea rdi, [rip + {body}]",
        "jmp {entry}",
        body = sym preload_body,
        entry = sym reverie_inguest::guest::tool_region::constructor_entry,
    );
}

/// A saved-frame loader's genuine post-load entry, before client constructors. The native
/// stub retains only borrowed loader arguments, then establishes the same
/// guarded stack/backing. The body copies settings before returning.
///
/// # Safety
/// Exact header-declared ordinary loader ABI; argv has argc valid NUL-terminated entries and the
/// initial thread alone is live. No foreign seccomp filter may be present.
#[unsafe(no_mangle)]
#[unsafe(naked)]
pub unsafe extern "C" fn hermit_engine_loader_initialize_v1(
    _argc: usize,
    _argv: *const *const c_char,
) {
    core::arch::naked_asm!(
        "endbr64",
        "mov [rip + {argc}], rdi",
        "mov [rip + {argv}], rsi",
        "lea rdi, [rip + {body}]",
        "jmp {entry}",
        argc = sym LOADER_ARGC,
        argv = sym LOADER_ARGV,
        body = sym loader_body,
        entry = sym reverie_inguest::guest::tool_region::constructor_entry,
    );
}

// Bind every entry locally before the stack switch; do not allow executable
// interposition or a lazy dynamic resolver on the pre-switch path.
core::arch::global_asm!(
    ".protected hermit_engine_preload_initialize_v1",
    ".protected hermit_engine_loader_initialize_v1",
    ".protected hermit_engine_install_loader_state_v1",
    ".hidden hermit_engine_preload_body_v1",
    ".hidden hermit_engine_loader_body_v1",
);

fn begin_installation() {
    if INSTALLATION
        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        install::fail("the common engine was initialized more than once");
    }
}

#[unsafe(export_name = "hermit_engine_preload_body_v1")]
unsafe extern "C" fn preload_body() {
    begin_installation();
    // SAFETY: entry has established the ordinary constructor's fixed backing.
    unsafe { install::preload() };
    INSTALLATION.store(2, Ordering::Release);
}

#[unsafe(export_name = "hermit_engine_loader_body_v1")]
unsafe extern "C" fn loader_body() {
    begin_installation();
    // SAFETY: only the native one-shot entry writes these before the body;
    // loader-owned strings remain live until post_load returns.
    let (argc, argv) = unsafe { (LOADER_ARGC, LOADER_ARGV) };
    // SAFETY: as above; the body bounds and copies all selected settings.
    unsafe { install::loader(argc, argv) };
    // Drop borrowed loader addresses before any guest event is admitted.
    unsafe {
        LOADER_ARGC = 0;
        LOADER_ARGV = core::ptr::null();
    }
    INSTALLATION.store(2, Ordering::Release);
}

pub(crate) fn poison() {
    INSTALLATION.store(3, Ordering::Release);
}

/// Synchronous C register entry for a genuine saved-frame trampoline. No Rust
/// object crosses this boundary: the borrowed repr(C) block is stack-local
/// and all Tool callbacks, futures and references stay inside this engine.
///
/// # Safety
/// Only the armed shim may call, with its validated live trampoline context.
/// The obligations of dispatch_saved_syscall apply; this is not admission
/// authority and cannot be used to construct a private lifecycle permit.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hermit_engine_saved_syscall_v1(
    context: *mut RegisterContext,
    number: i64,
) {
    if INSTALLATION.load(Ordering::Acquire) != 2 || context.is_null() {
        install::refuse(b"common engine: saved syscall before successful installation\n");
    }
    // Diagnostic only: this records the real native route before the unchanged
    // dispatcher. A corresponding actual Detcore entry and client result are
    // required separately; this line alone does not establish Tool execution.
    #[cfg(feature = "dispatch-observer")]
    observation::native_route(unsafe { &*context }, number);
    // SAFETY: the shim's genuine synchronous frame is live for this call.
    unsafe {
        reverie_inguest::guest::installed_tool::dispatch_saved_syscall(
            context,
            Some(number),
            reverie_inguest::guest::event::SyscallDispatch::InstalledHook,
            None,
            reverie_inguest::guest::runtime_seam::process_tool_syscall_callback,
            reverie_inguest::guest::runtime_seam::observe_registered_mapping,
            None,
        );
    }
}

unsafe extern "C" {
    // Exact pinned reverie-inguest assembly ABI: seven SysV scalar arguments.
    // This hidden gate is the existing filter's trusted syscall instruction.
    fn reverie_inguest_trusted_syscall(
        number: u64,
        a0: u64,
        a1: u64,
        a2: u64,
        a3: u64,
        a4: u64,
        a5: u64,
    ) -> i64;
}

/// Native forwarding/refusal gate for the C shim, including before fixed
/// Rust storage exists. No Rust/TLS/allocation runs in this entry.
///
/// # Safety
/// The caller owns Linux syscall pointer/lifetime obligations. This ordinary
/// preload gate supplies no private-image protection or native exec authority.
#[unsafe(no_mangle)]
#[unsafe(naked)]
pub unsafe extern "C" fn hermit_engine_raw_syscall_v1(
    _number: u64,
    _a0: u64,
    _a1: u64,
    _a2: u64,
    _a3: u64,
    _a4: u64,
    _a5: u64,
) -> i64 {
    core::arch::naked_asm!("jmp {gate}", gate = sym reverie_inguest_trusted_syscall);
}

core::arch::global_asm!(".protected hermit_engine_raw_syscall_v1");
