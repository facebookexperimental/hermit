//! Process-image readiness owned by the one shared native engine.
//!
//! The C transport renders its maintained ABI16 capability-clear descriptor.
//! A generation is an observed image identity, never caller-runtime or private
//! phase authority. Readiness is published only by the real scheduler owner.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

static GENERATION: AtomicU64 = AtomicU64::new(0);
static READY: AtomicU64 = AtomicU64::new(0);
pub(super) static SHUTDOWN: AtomicBool = AtomicBool::new(false);
pub(super) static PAUSE_REQUESTED: AtomicBool = AtomicBool::new(false);
pub(super) static PAUSED: AtomicBool = AtomicBool::new(false);

/// Fresh ordinary native image bootstrap, before a background or application
/// callback can use Rust allocation/TLS. This is the existing constructor's
/// guarded key-zero placement, not a private image or resumable authority.
///
/// # Safety
/// The actual native loader calls once during trusted single-threaded image
/// startup with no active runtime filter; its exact C/RET owner remains live.
#[unsafe(no_mangle)]
#[unsafe(naked)]
pub unsafe extern "C" fn hermit_engine_native_image_begin_v1() -> u64 {
    core::arch::naked_asm!(
        "endbr64",
        "sub rsp, 8",
        "lea rdi, [rip + {body}]",
        "call {entry}",
        "add rsp, 8",
        "mov rax, [rip + {generation}]",
        "ret",
        body = sym image_body,
        entry = sym reverie_inguest::guest::tool_region::constructor_entry,
        generation = sym GENERATION,
    );
}

unsafe extern "C" fn image_body() {
    // Distinct ordinary/native installation states also prevent a native
    // image from entering the preload/saved-frame dispatcher. They are
    // process-image bookkeeping and cannot issue phase/lifecycle permits.
    if crate::INSTALLATION
        .compare_exchange(0, 4, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        crate::install::fail("the common engine already has an image installation");
    }
    if let Err(error) = reverie_liteinst::preflight_tool_allocator() {
        crate::install::fail(&format!(
            "native engine allocator bootstrap refused: {error}"
        ));
    }
    SHUTDOWN.store(false, Ordering::Release);
    PAUSE_REQUESTED.store(false, Ordering::Release);
    PAUSED.store(false, Ordering::Release);
    // Same fresh-image generation as the maintained image_init. Subsequent
    // re-entry must fail the one-shot owner rather than reset global state.
    GENERATION.fetch_add(1, Ordering::SeqCst);
    crate::INSTALLATION.store(5, Ordering::Release);
}

core::arch::global_asm!(".protected hermit_engine_native_image_begin_v1");

pub(super) fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

/// Called after constructing the actual external-scheduler future: its first
/// log announcement must precede admission of the root's first lifecycle.
pub(super) fn publish_ready(generation: u64) {
    READY.store(generation, Ordering::SeqCst);
}

#[unsafe(no_mangle)]
pub extern "C" fn hermit_engine_native_ready_v1(generation: u64) -> i32 {
    i32::from(
        generation != 0
            && READY.load(Ordering::Acquire) == generation
            && !PAUSE_REQUESTED.load(Ordering::Acquire)
            && !PAUSED.load(Ordering::Acquire),
    )
}

pub(super) fn request_pause() {
    READY.store(0, Ordering::Release);
    PAUSE_REQUESTED.store(true, Ordering::Release);
    while !PAUSED.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
}

pub(super) fn resume() {
    PAUSE_REQUESTED.store(false, Ordering::Release);
    while PAUSED.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    READY.store(generation(), Ordering::Release);
}

pub(super) fn shutdown() {
    READY.store(0, Ordering::Release);
    SHUTDOWN.store(true, Ordering::Release);
}

/// The actual native finalizer requests scheduler shutdown before its existing
/// background wait and protected evidence drain. It does not call the generic
/// consuming Tool process-exit API or claim a unique ToolProcess Arc.
#[unsafe(no_mangle)]
pub extern "C" fn hermit_engine_native_process_exit_v1() {
    shutdown();
    super::diagnostics::publish_determinism_loss();
}
