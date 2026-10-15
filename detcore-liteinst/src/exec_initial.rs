//! Explicit InitialImage initialization on the runner's private Tool FS/stack.
//!
//! The packet and native control provide mechanisms, not authentication. The
//! coordinator must redeem the one-use nonce against this actual watched birth.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::net::UnixStream;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicPtr;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use detcore_model::liteinst_initial_image;
use reverie_inguest::guest::exec_private::NativeControl;
use reverie_inguest::guest::rpc::GuestRpcStream;
use reverie_rpc_transport::BlockingRpcClient;

static INITIAL_IMAGE_ACTIVE: AtomicBool = AtomicBool::new(false);
static NATIVE_CONTROL: AtomicPtr<NativeControl> = AtomicPtr::new(core::ptr::null_mut());
// Bounded startup diagnostics only; never authority or a readiness witness.
static INITIAL_STEP: AtomicU8 = AtomicU8::new(0);
static DISPATCH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

static OBSERVER_PUBLISHED: AtomicBool = AtomicBool::new(false);
#[cfg(feature = "exec-native-injection")]
static SEQUENCE_RESTORED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SequenceError {
    #[cfg(feature = "exec-native-injection")]
    Unavailable,
    Overflow,
    #[cfg(feature = "exec-native-injection")]
    Changed,
    #[cfg(feature = "exec-native-injection")]
    NotFresh,
    #[cfg(feature = "exec-native-injection")]
    DuplicateRestore,
}

#[cfg(feature = "exec-native-injection")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DispatchSequenceCut {
    last: u64,
}

#[cfg(feature = "exec-native-injection")]
impl DispatchSequenceCut {
    pub(crate) fn last(self) -> u64 {
        self.last
    }
}

// This exact checked operation is used by the actual observer frame writer.
fn next_dispatch_sequence() -> Result<u64, SequenceError> {
    DISPATCH_SEQUENCE
        .try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            value.checked_add(1)
        })
        .map(|previous| previous + 1)
        .map_err(|_| SequenceError::Overflow)
}

/// # Safety
/// The genuine live native owner excludes concurrent observer/handler work at
/// the frozen post-entry cut. Capture does not clear or rebase the sequence.
#[cfg(feature = "exec-native-injection")]
pub(crate) unsafe fn capture_dispatch_sequence() -> Result<DispatchSequenceCut, SequenceError> {
    if !active() || !OBSERVER_PUBLISHED.load(Ordering::Acquire) {
        return Err(SequenceError::Unavailable);
    }
    let last = DISPATCH_SEQUENCE.load(Ordering::Acquire);
    last.checked_add(1).ok_or(SequenceError::Overflow)?;
    Ok(DispatchSequenceCut { last })
}

/// # Safety
/// The same genuine owner retains the sealed exclusive cut. This final check
/// performs only atomic reads/comparison, no allocation or syscall.
#[cfg(feature = "exec-native-injection")]
pub(crate) unsafe fn check_dispatch_sequence(
    cut: DispatchSequenceCut,
) -> Result<(), SequenceError> {
    let current = unsafe { capture_dispatch_sequence()? };
    if current != cut {
        return Err(SequenceError::Changed);
    }
    Ok(())
}

/// # Safety
/// The consumer already authenticated the composite bytes and exact new-image
/// owner, excludes all work, and calls before publishing the observer. The
/// last value must be the actual admitted predecessor sequence; an integer is
/// not authority. No existing InitialImage caller invokes this API.
#[cfg(feature = "exec-native-injection")]
pub(crate) unsafe fn restore_dispatch_sequence(last: u64) -> Result<(), SequenceError> {
    last.checked_add(1).ok_or(SequenceError::Overflow)?;
    if SEQUENCE_RESTORED.load(Ordering::Acquire) {
        return Err(SequenceError::DuplicateRestore);
    }
    if !active()
        || OBSERVER_PUBLISHED.load(Ordering::Acquire)
        || DISPATCH_SEQUENCE.load(Ordering::Acquire) != 0
    {
        return Err(SequenceError::NotFresh);
    }
    SEQUENCE_RESTORED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| SequenceError::DuplicateRestore)?;
    DISPATCH_SEQUENCE
        .compare_exchange(0, last, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| SequenceError::NotFresh)?;
    Ok(())
}

#[cfg(all(test, feature = "exec-native-injection"))]
mod native_sequence_tests {
    use super::*;

    const ROLE: &str = "HERMIT_NATIVE_SEQUENCE_TEST_ROLE";

    // The actual observer counter/checked increment is exercised; these flag
    // fixtures do not install a NativeControl or publish a Linux observer.
    fn isolated(name: &str) -> bool {
        if std::env::var(ROLE).ok().as_deref() == Some(name) {
            return false;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("exec_initial::native_sequence_tests::{name}"),
                "--nocapture",
            ])
            .env(ROLE, name)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("running 1 test"), "{stdout}");
        assert!(stdout.contains("1 passed"), "{stdout}");
        true
    }

    #[test]
    fn fresh_bootstrap_zero_and_checked_next() {
        if isolated("fresh_bootstrap_zero_and_checked_next") {
            return;
        }
        assert_eq!(DISPATCH_SEQUENCE.load(Ordering::Acquire), 0);
        assert_eq!(
            unsafe { capture_dispatch_sequence() },
            Err(SequenceError::Unavailable)
        );
        assert_eq!(next_dispatch_sequence(), Ok(1));
        assert_eq!(DISPATCH_SEQUENCE.load(Ordering::Acquire), 1);
    }

    #[test]
    fn restore_once_before_publication_then_exact_next() {
        if isolated("restore_once_before_publication_then_exact_next") {
            return;
        }
        INITIAL_IMAGE_ACTIVE.store(true, Ordering::Release);
        unsafe {
            restore_dispatch_sequence(53).unwrap();
        }
        assert_eq!(DISPATCH_SEQUENCE.load(Ordering::Acquire), 53);
        assert_eq!(
            unsafe { restore_dispatch_sequence(53) },
            Err(SequenceError::DuplicateRestore)
        );
        assert_eq!(DISPATCH_SEQUENCE.load(Ordering::Acquire), 53);
        OBSERVER_PUBLISHED.store(true, Ordering::Release);
        let cut = unsafe { capture_dispatch_sequence().unwrap() };
        assert_eq!(cut.last(), 53);
        assert_eq!(unsafe { check_dispatch_sequence(cut) }, Ok(()));
        assert_eq!(next_dispatch_sequence(), Ok(54));
        assert_eq!(
            unsafe { check_dispatch_sequence(cut) },
            Err(SequenceError::Changed)
        );
        assert_eq!(DISPATCH_SEQUENCE.load(Ordering::Acquire), 54);
    }

    #[test]
    fn work_before_restore_refuses_without_reset() {
        if isolated("work_before_restore_refuses_without_reset") {
            return;
        }
        INITIAL_IMAGE_ACTIVE.store(true, Ordering::Release);
        assert_eq!(next_dispatch_sequence(), Ok(1));
        assert_eq!(
            unsafe { restore_dispatch_sequence(53) },
            Err(SequenceError::NotFresh)
        );
        assert_eq!(DISPATCH_SEQUENCE.load(Ordering::Acquire), 1);
        assert!(!SEQUENCE_RESTORED.load(Ordering::Acquire));
    }

    #[test]
    fn publication_before_restore_refuses_without_reset() {
        if isolated("publication_before_restore_refuses_without_reset") {
            return;
        }
        INITIAL_IMAGE_ACTIVE.store(true, Ordering::Release);
        OBSERVER_PUBLISHED.store(true, Ordering::Release);
        assert_eq!(
            unsafe { restore_dispatch_sequence(53) },
            Err(SequenceError::NotFresh)
        );
        assert_eq!(DISPATCH_SEQUENCE.load(Ordering::Acquire), 0);
        assert!(!SEQUENCE_RESTORED.load(Ordering::Acquire));
    }

    #[test]
    fn checked_overflow_and_wrong_installation_refuse() {
        if isolated("checked_overflow_and_wrong_installation_refuse") {
            return;
        }
        assert_eq!(
            unsafe { restore_dispatch_sequence(53) },
            Err(SequenceError::NotFresh)
        );
        assert_eq!(
            unsafe { restore_dispatch_sequence(u64::MAX) },
            Err(SequenceError::Overflow)
        );
        assert_eq!(DISPATCH_SEQUENCE.load(Ordering::Acquire), 0);
        INITIAL_IMAGE_ACTIVE.store(true, Ordering::Release);
        OBSERVER_PUBLISHED.store(true, Ordering::Release);
        // Boundary setup in the actual counter, not an alternate sequence map.
        DISPATCH_SEQUENCE.store(u64::MAX - 1, Ordering::Release);
        let cut = unsafe { capture_dispatch_sequence().unwrap() };
        assert_eq!(cut.last(), u64::MAX - 1);
        assert_eq!(next_dispatch_sequence(), Ok(u64::MAX));
        assert_eq!(next_dispatch_sequence(), Err(SequenceError::Overflow));
        assert_eq!(DISPATCH_SEQUENCE.load(Ordering::Acquire), u64::MAX);
        assert_eq!(
            unsafe { capture_dispatch_sequence() },
            Err(SequenceError::Overflow)
        );
    }
}

#[cfg_attr(
    feature = "exec-native-injection",
    expect(
        clippy::large_enum_variant,
        reason = "Keep the bounded pre-install controller by value on the private initializer stack; do not add an allocation or ownership path for this size heuristic."
    )
)]
pub(crate) enum InitialImageInputs {
    Initial {
        prepared: reverie_liteinst::PreparedInitialImage<detcore::Detcore>,
    },
    #[cfg(feature = "exec-native-injection")]
    Native(super::exec_native::entry::Prepared),
}

#[cfg_attr(
    feature = "exec-native-injection",
    expect(
        clippy::large_enum_variant,
        reason = "Keep the one-use post-install completion controller by value through finish; do not add an allocation or drop path for this size heuristic."
    )
)]
pub(crate) enum InstalledImage {
    Initial(reverie_liteinst::InstalledInitialImage),
    #[cfg(feature = "exec-native-injection")]
    Native(super::exec_native::entry::Installed),
}

impl InstalledImage {
    pub(crate) unsafe fn finish(self) -> std::io::Result<()> {
        match self {
            Self::Initial(control) => {
                install_dispatch_observer();
                reverie_liteinst::initialize_initial_image_lifecycle(&control)?;
                control.finish()
            }
            #[cfg(feature = "exec-native-injection")]
            Self::Native(control) => unsafe { control.finish() },
        }
    }
}

#[cfg(feature = "exec-native-injection")]
static NATIVE_OBSERVER_IDENTITY: std::sync::OnceLock<(u64, u64)> = std::sync::OnceLock::new();

#[cfg(feature = "exec-native-injection")]
pub(crate) fn retain_native_observer_identity(identity: (u64, u64)) -> Result<(), &'static str> {
    NATIVE_OBSERVER_IDENTITY
        .set(identity)
        .map_err(|_| "native observer installed twice")
}

pub(crate) fn install_dispatch_observer() {
    if OBSERVER_PUBLISHED.swap(true, Ordering::AcqRel) {
        fail();
    }
    if detcore::dispatch_observer::install(observe_dispatch).is_err() {
        fail();
    }
}

fn observe_dispatch(entry: detcore::dispatch_observer::Entry) {
    let tid = reverie_inguest::guest::exec_private::callback_native_tid().unwrap_or_else(|| fail());
    if !active() || entry.pid != tid || entry.tid != tid || entry.dettid <= 0 {
        fail();
    }
    #[cfg(feature = "exec-native-injection")]
    let selected_native = NATIVE_OBSERVER_IDENTITY.get().map(|identity| {
        (
            reverie_inguest::guest::native_exec_fds::socket_fd(
                reverie_inguest::guest::native_exec_fds::NativeSocket::Observer,
            )
            .unwrap_or_else(|| fail()),
            *identity,
        )
    });
    #[cfg(not(feature = "exec-native-injection"))]
    let selected_native: Option<(i32, (u64, u64))> = None;
    let (socket, identity) = selected_native.unwrap_or_else(|| {
        (
            reverie_liteinst::tool_output_fd().unwrap_or_else(|| fail()),
            super::TOOL_OUTPUT_IDENTITY
                .get()
                .copied()
                .unwrap_or_else(|| fail()),
        )
    });
    if detcore::detlog::socket_identity(socket) != Some(identity) {
        fail();
    }
    let sequence = next_dispatch_sequence().unwrap_or_else(|_| fail());
    let mut frame = [0u8; 96];
    frame[..8].copy_from_slice(b"HLISE001");
    frame[8..16].copy_from_slice(&sequence.to_le_bytes());
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
    #[cfg(feature = "exec-native-injection")]
    if bytes.starts_with(b"HLNEXEC1") {
        let (prepared, socket, forward_request, forward_policy) =
            unsafe { super::exec_native::entry::prepare(native, bytes) }.unwrap_or_else(|error| {
                report_initialization_error(&reverie_inguest::guest::support::describe_io_error(
                    &error,
                ));
                fail()
            });
        let inputs = super::InitializationInputs {
            coordinator_socket: socket,
            forward_request: Some(forward_request),
            forward_policy: Some(forward_policy),
            coordinator_fingerprint: None,
            initial_image: Some(InitialImageInputs::Native(prepared)),
        };
        unsafe {
            super::initialize_detcore(inputs);
        }
        return 0;
    }
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
    // The compatibility InitialImage route keeps its ordinary socket behavior;
    // the native packet branch above alone selects inherited-gate I/O.
    let stream = UnixStream::connect(std::path::Path::new(&socket)).unwrap_or_else(|error| {
        report_rpc_failure(&reverie_rpc_transport::RpcError::Io(error));
        fail()
    });
    let client = BlockingRpcClient::<detcore::GlobalState, GuestRpcStream>::initial_image_from_connected_stream(
        GuestRpcStream::from(stream),
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
        initial_image: Some(InitialImageInputs::Initial { prepared }),
    };
    // SAFETY: no Guest ran; owned dispatch/allocator and Tool FS remain live.
    INITIAL_STEP.store(10, Ordering::Relaxed);
    unsafe {
        super::initialize_detcore(inputs);
    }
    0
}

core::arch::global_asm!(".protected detcore_liteinst_exec_initialize");
