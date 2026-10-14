//! Explicit InitialImage initialization on the runner's private Tool FS/stack.
//!
//! The packet and native control provide mechanisms, not authentication. The
//! coordinator must redeem the one-use nonce against this actual watched birth.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicPtr;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use detcore_model::liteinst_initial_image;
use reverie_inguest::guest::exec_private::NativeControl;
use reverie_rpc_transport::BlockingRpcClient;

static INITIAL_IMAGE_ACTIVE: AtomicBool = AtomicBool::new(false);
static NATIVE_CONTROL: AtomicPtr<NativeControl> = AtomicPtr::new(core::ptr::null_mut());
// Bounded startup diagnostics only; never authority or a readiness witness.
static INITIAL_STEP: AtomicU8 = AtomicU8::new(0);
static DISPATCH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) struct InitialImageInputs {
    pub(crate) prepared: reverie_liteinst::PreparedInitialImage<detcore::Detcore>,
}

pub(crate) fn install_dispatch_observer() {
    if detcore::dispatch_observer::install(observe_dispatch).is_err() {
        fail();
    }
}

fn observe_dispatch(entry: detcore::dispatch_observer::Entry) {
    let tid = reverie_inguest::guest::exec_private::callback_native_tid().unwrap_or_else(|| fail());
    if !active() || entry.pid != tid || entry.tid != tid || entry.dettid <= 0 {
        fail();
    }
    let socket = reverie_liteinst::tool_output_fd().unwrap_or_else(|| fail());
    if super::TOOL_OUTPUT_IDENTITY
        .get()
        .is_none_or(|identity| detcore::detlog::socket_identity(socket) != Some(*identity))
    {
        fail();
    }
    let previous = DISPATCH_SEQUENCE
        .try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            value.checked_add(1)
        })
        .unwrap_or_else(|_| fail());
    let mut frame = [0u8; 96];
    frame[..8].copy_from_slice(b"HLISE001");
    frame[8..16].copy_from_slice(&(previous + 1).to_le_bytes());
    frame[16..20].copy_from_slice(&entry.pid.to_le_bytes());
    frame[20..24].copy_from_slice(&entry.tid.to_le_bytes());
    frame[24..32].copy_from_slice(&(entry.dettid as u64).to_le_bytes());
    frame[32..40].copy_from_slice(&entry.number.to_le_bytes());
    frame[40..48].copy_from_slice(&entry.instruction_pointer.to_le_bytes());
    for (index, argument) in entry.arguments.into_iter().enumerate() {
        frame[48 + index * 8..56 + index * 8].copy_from_slice(&argument.to_le_bytes());
    }
    // A separate diagnostic datagram: no detlog forwarding count or INFO index
    // changes. Raw sending preserves errno and cannot recursively log a call.
    let sent = unsafe {
        reverie_inguest::trap::raw_syscall6(
            libc::SYS_sendto,
            [
                socket as u64,
                frame.as_ptr() as u64,
                frame.len() as u64,
                (libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) as u64,
                0,
                0,
            ],
        )
    };
    if sent != frame.len() as i64 {
        fail();
    }
}

pub(crate) fn active() -> bool {
    INITIAL_IMAGE_ACTIVE.load(Ordering::Acquire)
}

pub(crate) fn report_initialization_error(message: &str) {
    // Best effort only: MSG_DONTWAIT prevents diagnostics from delaying the
    // terminal syscall. A pipe/non-socket stderr omits the diagnostic.
    let bytes = message.as_bytes();
    let bytes = &bytes[..bytes.len().min(1024)];
    for part in [b"exec-private initialization: ".as_slice(), bytes, b"\n"] {
        unsafe {
            let _ = reverie_inguest::trap::raw_syscall6(
                libc::SYS_sendto,
                [
                    2,
                    part.as_ptr() as u64,
                    part.len() as u64,
                    (libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) as u64,
                    0,
                    0,
                ],
            );
        }
    }
}

pub(crate) fn fail() -> ! {
    // The controlled runner still owns its private stack and bootstrap syscall
    // relay, including before fixed-gate publication. No formatting, allocation
    // or libc/TLS access is needed to retain this failure's stage. A short/error
    // write changes neither the refusal nor its exit class.
    let mut message = *b"exec-private initializer refused stage=00\n";
    let step = INITIAL_STEP.load(Ordering::Relaxed);
    let end = message.len();
    message[end - 3] = b"0123456789abcdef"[(step >> 4) as usize];
    message[end - 2] = b"0123456789abcdef"[(step & 15) as usize];
    unsafe {
        let _ = reverie_inguest::trap::raw_syscall6(
            libc::SYS_sendto,
            [
                2,
                message.as_ptr() as u64,
                message.len() as u64,
                (libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) as u64,
                0,
                0,
            ],
        );
    }
    // SAFETY: the explicit native entry supplied the retained control and fixed
    // gate contract before any Rust initialization. This exit does not allocate,
    // perform libc I/O, wait for RPC or return into guest code.
    unsafe {
        reverie_inguest::guest::exec_private::initialization_failed(
            NATIVE_CONTROL.load(Ordering::Acquire),
        )
    }
}

// Diagnostic only: report the actual transport error class and raw OS error.
// An absent OS code is all ones. Do not format or allocate on this error path.
fn report_rpc_failure(error: &reverie_rpc_transport::RpcError) {
    use reverie_rpc_transport::RpcError;
    let (class, errno) = match error {
        RpcError::Io(error) => (1u8, error.raw_os_error().map(|x| x as u32)),
        RpcError::Closed => (2, None),
        RpcError::Encode(_) => (3, None),
        RpcError::Decode(_) => (4, None),
        RpcError::FrameTooLarge { .. } => (5, None),
        RpcError::TrailingBytes { .. } => (6, None),
    };
    let mut message = *b"exec-private RPC class=0 errno=00000000\n";
    message[23] = b'0' + class;
    let errno = errno.unwrap_or(u32::MAX);
    for i in 0..8 {
        message[31 + i] = b"0123456789abcdef"[((errno >> (28 - 4 * i)) & 15) as usize];
    }
    unsafe {
        let _ = reverie_inguest::trap::raw_syscall6(
            libc::SYS_sendto,
            [
                2,
                message.as_ptr() as u64,
                message.len() as u64,
                (libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) as u64,
                0,
                0,
            ],
        );
    }
}

/// Install genuine Detcore before executing any instruction of the guest loader.
///
/// The feature suppresses only automatic `.init_array` registration. This entry
/// requires the native runner's owned bootstrap stack and private GNU Tool TLS,
/// its exact inherited fixed gate/filter and bounded input mapping. Successful
/// return means Config/RPC, DETLOG and shared OFD setup completed; only the native
/// runner may subsequently restore guest registrations and enter Guest. Every
/// explicit failure exits 127 through the fixed native gate.
///
/// # Safety
/// `native` must point to the runner's aligned, complete retained NativeControl.
/// The process must be single-threaded on private Tool FS and owned stack, with
/// usable owned bootstrap SIGSYS state. No guest code may have executed. The
/// coordinator must hold and consume genuine one-use authorization for this
/// exact watched launch instance; packet bytes and file seals are not authority.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn detcore_liteinst_exec_initialize(native: *mut NativeControl) -> i64 {
    INITIAL_STEP.store(1, Ordering::Relaxed);
    NATIVE_CONTROL.store(native, Ordering::Release);
    if INITIAL_IMAGE_ACTIVE.swap(true, Ordering::AcqRel) {
        fail();
    }
    // Establish the fixed allocator before fingerprinting, Config decoding or
    // any dispatch/TLS work that can allocate. Successful reservation is scalar.
    INITIAL_STEP.store(2, Ordering::Relaxed);
    if reverie_inguest::guest::tool_region::ToolRegion::reserve().is_err() {
        fail();
    }
    let _private_allocations = reverie_inguest::guest::alloc::enter_dispatch();
    INITIAL_STEP.store(3, Ordering::Relaxed);
    let bytes = unsafe { reverie_inguest::guest::exec_private::initial_packet(native) }
        .unwrap_or_else(|_| fail());
    INITIAL_STEP.store(4, Ordering::Relaxed);
    let packet = liteinst_initial_image::decode(bytes).unwrap_or_else(|_| fail());
    // Mandatory real schema comparison precedes the socket and all guest Config
    // deserialization. This fingerprint includes the actual Config/RPC sources.
    INITIAL_STEP.store(5, Ordering::Relaxed);
    if detcore::config_wire_fingerprint().as_bytes() != packet.schema_fingerprint {
        fail();
    }
    INITIAL_STEP.store(6, Ordering::Relaxed);
    let tid = reverie::Pid::from_raw(
        unsafe { reverie_inguest::guest::exec_private::native_tid(native) }
            .unwrap_or_else(|_| fail()),
    );
    let socket = OsString::from_vec(packet.coordinator.to_vec());
    INITIAL_STEP.store(7, Ordering::Relaxed);
    let client = BlockingRpcClient::<detcore::GlobalState>::connect_initial_image(
        std::path::Path::new(&socket),
        tid,
        packet.nonce,
        *packet.config_digest,
    )
    .unwrap_or_else(|error| {
        report_rpc_failure(&error);
        fail()
    });
    INITIAL_STEP.store(8, Ordering::Relaxed);
    if liteinst_initial_image::config_violation(client.config()).is_some()
        || client.config().in_guest_detlog_forward_policy.as_deref() != Some(packet.forward_policy)
    {
        fail();
    }
    INITIAL_STEP.store(9, Ordering::Relaxed);
    let prepared =
        unsafe { reverie_liteinst::prepare_tool_initial_image::<detcore::Detcore>(client, native) }
            .unwrap_or_else(|_| fail());
    let inputs = super::InitializationInputs {
        coordinator_socket: socket,
        forward_request: Some(OsString::from(packet.detlog_identity)),
        forward_policy: Some(OsString::from(packet.forward_policy)),
        coordinator_fingerprint: Some(OsString::from(
            std::str::from_utf8(packet.schema_fingerprint).unwrap_or_else(|_| fail()),
        )),
        initial_image: Some(InitialImageInputs { prepared }),
    };
    // SAFETY: no Guest ran; owned dispatch/allocator and Tool FS remain live.
    INITIAL_STEP.store(10, Ordering::Relaxed);
    unsafe {
        super::initialize_detcore(inputs);
    }
    0
}

core::arch::global_asm!(".protected detcore_liteinst_exec_initialize");
