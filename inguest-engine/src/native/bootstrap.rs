//! Native background configuration and the actual application-root factory.
//! The scheduler keeps its original executor and event order. No process
//! owner lock spans Tool initialization, a Future, a callback, or an RPC.

use std::ffi::OsStr;
use std::future::Future;
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

use detcore::Config;
use detcore::Detcore;
use detcore::GlobalState;
use reverie::Pid;
use reverie::Tool;
use reverie_inguest::guest::host::HostRuntime;
use reverie_inguest::guest::native_rpc::NativeRpc;
use reverie_inguest::guest::rpc::CoordinatorRpc;

use super::abi::Callback;
use super::abi::VERSION;
use super::config::Mode;
use super::config::Source;
use super::config::{self};
use super::diagnostics::Emitter;
use super::diagnostics::{self};
use super::image;
use super::owner;
use crate::runtime::EngineRuntime;

type Idler = unsafe extern "C" fn();
const CONFIG_LIMIT: usize = 16 * 1024 * 1024;
const UNSUPPORTED_REPORT_FD: i32 = 199;

/// An explicitly selected launch's borrowed facts. These are supplied by the
/// real native callback owner; no environment-name convention is a core API.
#[repr(C)]
pub struct Startup {
    pub version: u32,
    pub size: u32,
    pub mode: u32,
    pub panic_on_unsupported: i32,
    pub image: u64,
    pub physical_owner: i32,
    pub evidence_level: i32,
    pub background_pid: i32,
    pub reserved: u32,
    pub diagnostic: Option<Emitter>,
    pub idle: Option<Idler>,
    pub evidence: Option<Emitter>,
    pub control: *const u8,
    pub control_len: usize,
    pub fingerprint: *const u8,
    pub fingerprint_len: usize,
    pub standalone_json: *const u8,
    pub standalone_json_len: usize,
}

const _: () = assert!(size_of::<Startup>() == 112);

struct Bootstrap {
    config: Arc<Config>,
    global: Arc<GlobalState>,
    coordinator: Option<PathBuf>,
    physical_owner: i32,
    // Preserve the maintained background's libc getpid observation for its
    // exec pause predicate. The helper's raw PID and application owner are
    // distinct launch facts and must not replace this observation.
    pause_owner: i32,
    image: u64,
    // The original protected duplicate belongs to this image, including COW.
    // It is never a forged application RPC or a replaced report descriptor.
    _copied_report: Option<OwnedFd>,
}

static BOOTSTRAP: OnceLock<Bootstrap> = OnceLock::new();
static ROOT_INSTALLATION: AtomicU8 = AtomicU8::new(0);

unsafe fn borrowed<'a>(
    pointer: *const u8,
    length: usize,
    limit: usize,
) -> io::Result<Option<&'a [u8]>> {
    if length > limit || (pointer.is_null() && length != 0) {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    if pointer.is_null() {
        return Ok(None);
    }
    Ok(Some(unsafe { std::slice::from_raw_parts(pointer, length) }))
}

fn current_physical_pid() -> i32 {
    unsafe { libc::syscall(libc::SYS_getpid) as i32 }
}

/// Borrow only immutable launch policy and its original protected report FD.
/// A shared-VM child must not enter the inherited host/Tool/RPC locks or clone
/// an owning handle. BOOTSTRAP was completely published before native READY.
pub(super) fn copied_policy(image: u64, physical_pid: i32) -> io::Result<(bool, i32)> {
    let boot = BOOTSTRAP
        .get()
        .ok_or_else(|| io::Error::from_raw_os_error(libc::EAGAIN))?;
    if crate::INSTALLATION.load(Ordering::Acquire) != 5
        || physical_pid <= 0
        || physical_pid != current_physical_pid()
        || physical_pid == boot.physical_owner
        || image == 0
        || image != boot.image
        || image != super::image::generation()
    {
        return Err(io::Error::from_raw_os_error(libc::ESTALE));
    }
    Ok((
        boot.config.panic_on_unsupported_syscalls,
        boot._copied_report.as_ref().map_or(-1, AsRawFd::as_raw_fd),
    ))
}

/// # Safety
/// Called on the genuine native background thread after the real image's
/// fixed allocator backing, ordinary FS/TLS and native stack are established.
/// Borrowed startup bytes remain live until copied below; callbacks remain
/// in the exact held/hash-bound native client for this process image.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hermit_engine_native_background_v1(startup: *const Startup) -> i32 {
    let result = unsafe { start(startup) };
    match result {
        Ok(()) => 0,
        Err(error) => {
            diagnostics::emit(format!("native engine: startup refused: {error}\n").as_bytes());
            -error.raw_os_error().unwrap_or(libc::EPROTO)
        }
    }
}

unsafe fn start(startup: *const Startup) -> io::Result<()> {
    if startup.is_null() {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    let startup = unsafe { &*startup };
    if startup.version != VERSION
        || startup.size as usize != size_of::<Startup>()
        || crate::INSTALLATION.load(Ordering::Acquire) != 5
        || startup.image == 0
        || startup.image != image::generation()
        || startup.physical_owner <= 0
        || startup.background_pid != current_physical_pid()
        || startup.reserved != 0
        || !matches!(startup.panic_on_unsupported, 0 | 1)
    {
        return Err(io::Error::from_raw_os_error(libc::ESTALE));
    }
    let mode = match startup.mode {
        1 => Mode::Standalone,
        2 => Mode::Coordinated,
        _ => return Err(io::Error::from_raw_os_error(libc::EINVAL)),
    };
    let diagnostic = startup
        .diagnostic
        .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
    let evidence = startup
        .evidence
        .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
    let idle = startup
        .idle
        .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
    let control = unsafe { borrowed(startup.control, startup.control_len, 107)? };
    let fingerprint = unsafe { borrowed(startup.fingerprint, startup.fingerprint_len, 256)? };
    let json = unsafe {
        borrowed(
            startup.standalone_json,
            startup.standalone_json_len,
            CONFIG_LIMIT,
        )?
    };
    let tracing_active = diagnostics::install(
        diagnostic,
        evidence,
        startup.evidence_level,
        control.is_some(),
    )?;
    let selected = config::select(
        mode,
        control.map(OsStr::from_bytes),
        fingerprint.map(OsStr::from_bytes),
        json.and_then(|bytes| std::str::from_utf8(bytes).ok()),
    )?;
    let mut config = selected.config;
    config.panic_on_unsupported_syscalls = startup.panic_on_unsupported != 0;
    // Preserve the native runtime's existing exit+protected-report path. A
    // ptrace unrecoverable shutdown would suppress that report and is disabled.
    config.exit_on_unsupported_syscall = true;
    config.shutdown_on_unsupported_syscall = false;
    config.unsupported_syscall_report_fd =
        (unsafe { libc::fcntl(UNSUPPORTED_REPORT_FD, libc::F_GETFD) } >= 0)
            .then_some(UNSUPPORTED_REPORT_FD);
    let duplicate = unsafe {
        libc::fcntl(
            UNSUPPORTED_REPORT_FD,
            libc::F_DUPFD_CLOEXEC,
            UNSUPPORTED_REPORT_FD + 1,
        )
    };
    let copied_report = (duplicate >= 0).then(|| unsafe { OwnedFd::from_raw_fd(duplicate) });
    config.validate();
    if diagnostics::info_enabled() {
        let source: &[u8] = match selected.source {
            Source::Coordinator => b"native engine: using coordinator-provided Detcore Config\n",
            Source::StandaloneJson => b"native engine: using standalone Detcore Config\n",
            Source::StandaloneParseFallback => {
                b"native engine: using strict default after standalone parse failure\n"
            }
            Source::StandaloneDefault => b"native engine: using strict default Detcore Config\n",
        };
        diagnostics::emit(source);
    }
    if matches!(selected.source, Source::StandaloneParseFallback) {
        diagnostics::emit(b"native engine: WARNING could not parse standalone Detcore Config; using strict default\n");
    }
    let config = Arc::new(config);
    let global = Arc::new(GlobalState::init_for_external_scheduler(&config));
    BOOTSTRAP
        .set(Bootstrap {
            config,
            global: Arc::clone(&global),
            coordinator: selected.coordinator,
            physical_owner: startup.physical_owner,
            pause_owner: unsafe { libc::getpid() },
            image: startup.image,
            _copied_report: copied_report,
        })
        .map_err(|_| io::Error::from_raw_os_error(libc::EALREADY))?;
    detcore::install_shared_open_file_channel(Box::new(detcore::ChunkedOpenFileChannel::new(
        OpenFiles,
    )))
    .map_err(|_| io::Error::other("native open-file channel already installed"))?;
    let log_scheduler = diagnostics::info_enabled() && !tracing_active;
    let observer = Arc::new(move |event: &'static str| {
        if log_scheduler {
            diagnostics::emit(format!("INFO detcore::scheduler: {event}\n").as_bytes());
        }
    });
    // This call creates the original future and emits its startup INFO before
    // READY releases the application's real root initialization.
    let scheduler = global.run_external_scheduler(observer);
    image::publish_ready(startup.image);
    cooperative(scheduler, idle);
    Ok(())
}

fn cooperative<F: Future<Output = ()>>(future: F, idle: Idler) {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        if image::SHUTDOWN.load(Ordering::Acquire) {
            return;
        }
        if image::PAUSE_REQUESTED.load(Ordering::Acquire) {
            image::PAUSED.store(true, Ordering::Release);
            while image::PAUSE_REQUESTED.load(Ordering::Acquire)
                && !image::SHUTDOWN.load(Ordering::Acquire)
            {
                unsafe {
                    idle();
                }
            }
            image::PAUSED.store(false, Ordering::Release);
            continue;
        }
        match future.as_mut().poll(&mut context) {
            Poll::Ready(()) => return,
            Poll::Pending => unsafe {
                idle();
            },
        }
    }
}

/// The actual application's first root event constructs the one concrete
/// Detcore. Background initialization never fabricates a Tool or root TID.
pub(super) fn install_root(callback: &Callback) -> io::Result<&'static owner::Engine> {
    callback.validate(callback.identity)?;
    let identity = callback.identity;
    // A genuine copied process has already installed its fresh owner through
    // the real child post-event. It reuses the original authenticated config,
    // never another background/GlobalState or a second Detcore::new call.
    if let Ok(engine) = owner::installed() {
        return Ok(engine);
    }
    let boot = BOOTSTRAP
        .get()
        .ok_or_else(|| io::Error::from_raw_os_error(libc::EAGAIN))?;
    if boot.physical_owner != current_physical_pid()
        || boot.physical_owner != identity.physical_pid
        || boot.image != identity.image
        || boot.image != image::generation()
    {
        return Err(io::Error::from_raw_os_error(libc::ESTALE));
    }
    if identity.physical_tid != identity.physical_pid {
        return Err(io::Error::from_raw_os_error(libc::EAGAIN));
    }
    ROOT_INSTALLATION
        .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
        .map_err(|_| io::Error::from_raw_os_error(libc::EBUSY))?;
    // No process lock is held while constructing the original Tool. Failure
    // poisons this image's one-shot installation rather than creating another.
    let result = (|| {
        let config = Arc::clone(&boot.config);
        let (cached, rpc) = match &boot.coordinator {
            Some(path) => (
                CoordinatorRpc::from_native_configuration(path, Arc::clone(&config))?,
                NativeRpc::new(path.clone(), Arc::clone(&config))?,
            ),
            None => (
                CoordinatorRpc::from_native_local_configuration(Arc::clone(&config)),
                NativeRpc::local(Arc::clone(&boot.global), Arc::clone(&config)),
            ),
        };
        let root = Pid::from_raw(identity.tool_pid);
        let subscriptions = <Detcore as Tool>::subscriptions(&config);
        let tool = Detcore::new(root, &config);
        let host = unsafe {
            owner::Host::new_native(
                tool,
                cached,
                root,
                subscriptions.iter_syscalls().collect(),
                subscriptions.has_cpuid(),
                fresh_runtime(),
            )
        };
        owner::publish(owner::Engine {
            host,
            rpc,
            config,
            root,
            physical_owner: boot.physical_owner,
            image: boot.image,
            pending_parents: Mutex::new(std::collections::HashMap::new()),
            next_child_ordinal: AtomicU64::new(1),
        })
    })();
    ROOT_INSTALLATION.store(if result.is_ok() { 2 } else { 3 }, Ordering::Release);
    result
}

/// Immutable real launch choice. The admitted Engine owns all mutable work.
pub(super) fn coordinated() -> bool {
    BOOTSTRAP
        .get()
        .is_some_and(|boot| boot.coordinator.is_some())
}

/// Preserve the maintained background's observed libc PID pause predicate.
/// The application owner is separate; a COW child's Engine cannot replace
/// this observation or fabricate an additional scheduler background.
pub(super) fn owns_background(physical_pid: i32) -> bool {
    BOOTSTRAP.get().is_some_and(|boot| {
        boot.pause_owner == physical_pid
            && physical_pid == current_physical_pid()
            && boot.image == image::generation()
    })
}

struct OpenFiles;
impl detcore::OpenFileControlTransport for OpenFiles {
    fn call(
        &self,
        request: detcore::OpenFileControl,
    ) -> Result<detcore::OpenFileControlReply, detcore::SharedOpenFileError> {
        let boot = BOOTSTRAP.get().ok_or_else(|| {
            detcore::SharedOpenFileError("native configuration is not installed".into())
        })?;
        if boot.coordinator.is_none() {
            return Err(detcore::SharedOpenFileError(
                "shared OFDs require the external coordinator".into(),
            ));
        }
        let response = diagnostics::global_call(detcore::shared_open_file_request(request))
            .map_err(|error| detcore::SharedOpenFileError(error.to_string()))?;
        detcore::shared_open_file_reply(response)
    }
}

/// Native Guest operations use the selected per-callback service table. A
/// mistaken local dispatcher entry has no permissive HostRuntime substitute.
struct NativeRuntime;
pub(super) fn fresh_runtime() -> EngineRuntime {
    EngineRuntime::new(Box::new(NativeRuntime))
}
impl HostRuntime for NativeRuntime {
    fn fork_child_rebind(&self) {
        panic!("native fork requires its original native lifecycle owner");
    }
    fn emit_stage(&self, stage: &[u8]) {
        diagnostics::emit(stage);
    }
    #[cfg(feature = "native-first-instruction-observer")]
    fn observe_native_entry(
        &self,
        stage: reverie_inguest::guest::native_services::NativeEntryStage,
        observation: reverie_inguest::guest::native_services::NativeEntryObservation<'_, '_>,
    ) {
        super::first_instruction_observer::observe(stage, observation);
    }
    fn fork_child_reset(&self, _event: &reverie_inguest::guest::event::SyscallEvent) {
        panic!("native fork cannot use the local runtime reset");
    }
    fn cpuid_interception_enabled(&self) -> bool {
        true
    }
    fn exit_process_stats(&self, _tid: Pid) -> io::Result<()> {
        Err(io::Error::other(
            "native statistics retain their native finalization owner",
        ))
    }
    fn read_clock(&self) -> io::Result<u64> {
        Err(io::Error::other(
            "native clock requires the actual callback branch fact",
        ))
    }
    fn signal_action_supported(&self, _number: i64, _args: [u64; 6]) -> bool {
        false
    }
    fn reserved_signal_mask(&self) -> u64 {
        0
    }
}

const _: () = assert!(size_of::<Startup>() == 112);
