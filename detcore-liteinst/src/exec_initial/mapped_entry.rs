//! Inactive physical first entry of the genuine mapped private-musl image.
//! This consumes real mechanisms and stops for the same host owner; no Guest.
#[path = "mapped_entry/bounds.rs"]
mod bounds;

use mapped_initial::libc_relay::RelayStorage;
use reverie_inguest::guest::exec_private::NativeControl;
use reverie_inguest::guest::exec_private::mapped_initial;

// A target-produced scalar descriptor, never an owner/readiness credential.
// Final ELF must independently prove this exact symbol is retained immutable RO.
#[used]
#[unsafe(export_name = "detcore_liteinst_mapped_layout")]
#[unsafe(link_section = ".rodata.hermit_mapped_layout")]
static MAPPED_LAYOUT: [u64; 12] = [
    u64::from_le_bytes(*b"HLMLAY01"),
    1,
    96,
    mapped_initial::MAPPED_INPUT_VERSION,
    mapped_initial::MAPPED_INPUT_HEADER_BYTES as u64,
    mapped_initial::MAPPED_INPUT_MAX_BYTES as u64,
    core::mem::size_of::<NativeControl>() as u64,
    core::mem::align_of::<NativeControl>() as u64,
    core::mem::size_of::<RelayStorage>() as u64,
    core::mem::align_of::<RelayStorage>() as u64,
    reverie_inguest::trap::FIXED_GATE_BASE,
    reverie_inguest::trap::FIXED_NATIVE_ENTRY,
];

// No raw_syscall selector, diagnostic or libc call before gate publication.
// The existing terminal's control argument is unused and grants no authority.
unsafe fn terminal() -> ! {
    // SAFETY: the entry's physical host contract already owns the exact selected
    // gate/filter even before Rust adoption; null is not a NativeControl origin.
    unsafe { reverie_inguest::guest::exec_private::initialization_failed(core::ptr::null_mut()) }
}

/// Actual C-ABI entry before private TLS. It never returns or resumes Guest.
///
/// # Safety
/// Only the trusted host of this exact genuinely stopped initial exec may call.
/// It has spent its affine generation transition and retains pidfd/ChildOwner
/// kill/wait authority, the exact successful no-replace full reservation,
/// source-qualified sealed loader/runtime and immutable final ELF mappings,
/// independent RO gate reference, selected fixed RX gate and admitted filter.
/// No Guest instruction or private constructor has executed. All bootstrap
/// code, including every compiler helper before TLS, must be independently
/// qualified nonallocating/non-TLS/nonunwinding/noninterposable; source shape
/// alone does not prove emitted closure. The host owns an adequately sized
/// guarded key0 private stack (including xstate/handler depth), C-ABI entry
/// alignment, accessible state, DF clear and actual memory permissions.
/// Original FS/mask/registrations remain unchanged for capture; external async
/// execution is excluded until capture installs the actual blocked mask. Do not
/// fabricate a mask/readback or overwrite guest state to satisfy the checks.
/// `ptr..ptr+len` is one valid immutable retained whole scalar blob for this
/// linked consumer, actually mapped RO in its handoff role for process lifetime.
/// Header, actual code/site provenance, original27 registers/mask/PKRU, real
/// env/aux/string/TLS/PHDR facts and every role/FD lease belong to this same birth.
/// Control/relay storage is pristine exclusive zero memory; original guest
/// xstate/context and every physical mapping/FD remain host-owned until confirmed
/// death. Scalar syntax/address resemblance is never that authority.
/// The host's complete numeric predicate has accepted these SAME immutable
/// bytes before any child mutation, and transfer enforces full physical input
/// readback, RO/sealed custody and the same held generation. The child projects
/// only bootstrap operands before TLS, then repeats full validation after real
/// libc/FD cleanup and before adoption. Later parsing cannot repair an earlier
/// transfer of corrupted or substituted bytes.
/// The real child input lease is unaliased by any later-closing child OwnedFd.
/// Actual private libc/TLS/environ/C allocation and ordinary HLNI client satisfy
/// their existing foreign-library contracts. No lang_start/StdReady/argv is
/// invented. Errors, panic or abandonment are terminal under the same host;
/// never unwind, retry, reset, fall back to guest libc or release Guest here.
/// Host must treat the final INT3 as observation of genuine initializer return,
/// not permission to resume Guest: original registrations/context restoration
/// and actual full readiness/producer evidence remain separately required.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn detcore_liteinst_mapped_entry(ptr: *const u8, len: usize) -> ! {
    if !bounds::valid_input_range(
        ptr as usize,
        len,
        mapped_initial::MAPPED_INPUT_HEADER_BYTES,
        mapped_initial::MAPPED_INPUT_MAX_BYTES,
    ) {
        unsafe { terminal() }
    }
    // SAFETY: only the physical host contract above establishes actual backing,
    // complete RO lifetime and exclusive source; numeric bounds are insufficient.
    let bytes: &'static [u8] = unsafe { core::slice::from_raw_parts(ptr, len) };
    let captured = match unsafe { mapped_initial::capture(bytes) } {
        Ok(v) => v,
        Err(_) => unsafe { terminal() },
    };
    let relay = unsafe { captured.install_libc_relay() };
    let captured = unsafe { relay.initialize_private_libc() };
    let mapped = match unsafe { captured.adopt_region() } {
        Ok(v) => v,
        Err(_) => unsafe { terminal() },
    };
    let mapped = match unsafe { mapped.enable_tool_init() } {
        Ok(v) => v,
        Err(_) => unsafe { terminal() },
    };
    // SAFETY: exact same affine physical owner, actual libc/TLS/FD cleanup and
    // adoption. Shared ordinary schema/RPC/Config/install/lifecycle/finish only.
    let result = unsafe { super::detcore_liteinst_mapped_initialize(mapped) };
    if result != 0 {
        unsafe { terminal() }
    }
    // No new Ready check/setter. This point is reachable only after the genuine
    // shared initializer returned through its existing successful finish path.
    unsafe { observe_initializer_return() }
}

// Exact architecture-owned observation cut; no Rust runs after its INT3. A
// mistaken host resume reaches the existing actual selected-gate terminal.
#[unsafe(naked)]
unsafe extern "C" fn observe_initializer_return() -> ! {
    core::arch::naked_asm!(
        "endbr64",
        ".global detcore_liteinst_mapped_entry_observed_int3",
        ".hidden detcore_liteinst_mapped_entry_observed_int3",
        "detcore_liteinst_mapped_entry_observed_int3:",
        "int3",
        "xor edi,edi",
        "jmp {terminal}",
        terminal=sym reverie_inguest::guest::exec_private::initialization_failed,
    );
}
core::arch::global_asm!(".protected detcore_liteinst_mapped_entry");
