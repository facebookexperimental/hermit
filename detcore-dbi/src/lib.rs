/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// AUTONOMOUS-BOT-IMPLEMENTED

//! DynamoRIO callback runtime that executes the real Detcore [`Tool`] over
//! [`reverie_dbi::DbiGuest`].

#![deny(missing_docs)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::fs;
use std::future::Future;
use std::io;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::pin::pin;
use std::process::Command;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

use detcore::Config;
use detcore::Detcore;
use detcore::GlobalState;
use detcore::UnsupportedSyscallError;
use rand::RngExt as _;
use reverie::Error;
use reverie::ExitStatus;
use reverie::Pid;
use reverie::Tid;
use reverie::Tool;
use reverie::syscalls::CloneFlags;
use reverie::syscalls::Errno;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;
use reverie_dbi::DbiGuest;
use reverie_dbi::DbiSyscallOutcome;
use reverie_dbi::MemoryReader;
use reverie_dbi::RegisterReader;
use reverie_dbi::RegisterWriter;
use reverie_dbi::SyscallInvoker;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
const MAX_OBSERVED_BUFFER: usize = 1024 * 1024;
const RANDOM_FILL_CHUNK_BYTES: usize = 4096;
const GETRANDOM_MAX_BYTES: usize = (i32::MAX as usize) & !4095;
const GETRANDOM_ALLOWED_FLAGS: u32 = libc::GRND_NONBLOCK | libc::GRND_RANDOM | libc::GRND_INSECURE;

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-644): Review the inherited DBI report descriptor.
/// Fixed inherited descriptor receiving unsupported syscall records.
pub const UNSUPPORTED_SYSCALL_REPORT_FD: i32 = 199;

type DetcoreThreadState = <Detcore as Tool>::ThreadState;
type Emitter = reverie_dbi::RuntimeEmitter;
type Idler = reverie_dbi::RuntimeIdler;

fn emit_marker(emit: Emitter, message: &'static [u8]) {
    unsafe { emit(message.as_ptr(), message.len()) };
}

fn info_logging_enabled() -> bool {
    matches!(
        std::env::var("HERMIT_LOG")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "info" | "debug" | "trace"
    )
}

/// Environment variable through which `hermit run --backend dbi` hands the
/// CLI-derived Detcore [`Config`] (JSON) to this in-guest runtime.
///
/// The guest process inherits it from `drrun` (see the DBI launcher), so it is
/// the cross-process channel that lets flags like `--strict`, `--seed`, and the
/// time/CPUID virtualization switches reach the DBI Detcore Tool the same way
/// they reach the ptrace backend.
pub const DETCONFIG_ENV: &str = "HERMIT_DBI_DETCONFIG";

/// Where the effective Detcore [`Config`] came from, for native diagnostics.
enum ConfigSource {
    /// Deserialized from [`DETCONFIG_ENV`] provided by `hermit run`.
    Cli,
    /// [`DETCONFIG_ENV`] was set but could not be parsed; strict default used.
    ParseFallback,
    /// [`DETCONFIG_ENV`] was absent (e.g. a bare `drrun -c client.so` run).
    Default,
}

/// A strict, deterministic default configuration for standalone DBI runs.
fn default_dbi_config() -> Config {
    Config {
        sequentialize_threads: true,
        deterministic_io: true,
        max_timeslice: None,
        ..Config::default()
    }
}

/// Builds the Detcore [`Config`] for this DBI runtime.
///
/// The configuration is taken from the CLI-derived Detcore config serialized
/// into [`DETCONFIG_ENV`] when present; otherwise a strict default is used.
/// Regardless of the source, the DBI execution-model invariants are re-asserted:
/// the backend drives the Detcore global scheduler externally on a branch count
/// rather than PMU retired-conditional-branch preemption, so timeslice
/// preemption (`max_timeslice`) is disabled and threads stay sequentialized for
/// the single external scheduler.
fn load_dbi_config() -> (Config, ConfigSource) {
    let (mut config, source) = match std::env::var(DETCONFIG_ENV) {
        Ok(value) if !value.is_empty() => match serde_json::from_str::<Config>(&value) {
            Ok(config) => (config, ConfigSource::Cli),
            Err(_) => (default_dbi_config(), ConfigSource::ParseFallback),
        },
        _ => (default_dbi_config(), ConfigSource::Default),
    };
    config.max_timeslice = None;
    config.sequentialize_threads = true;
    (config, source)
}

// TODO-HUMAN-REVIEW(PR-587): Confirm DynamoRIO-native process lifecycle boundaries.
// TODO-HUMAN-REVIEW(PR-743): Review native clone scheduling and registration ordering.
fn requires_native_lifecycle(sysnum: i64) -> bool {
    match sysnum {
        // AUTONOMOUS-BOT-IMPLEMENTED
        libc::SYS_fork
        | libc::SYS_vfork
        | libc::SYS_clone
        | libc::SYS_clone3
        | libc::SYS_rt_sigreturn
        | libc::SYS_execve => true,
        _ => false,
    }
}

fn run_cooperative<F: Future<Output = ()>>(future: F, idle: Idler) {
    let mut future = pin!(future);
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    loop {
        if RUNTIME_SHUTDOWN.load(Ordering::Acquire) {
            return;
        }
        // TODO-HUMAN-REVIEW(PR-587): Preserve scheduler continuation across failed exec.
        if RUNTIME_PAUSE_REQUESTED.load(Ordering::Acquire) {
            RUNTIME_PAUSED.store(true, Ordering::Release);
            while RUNTIME_PAUSE_REQUESTED.load(Ordering::Acquire)
                && !RUNTIME_SHUTDOWN.load(Ordering::Acquire)
            {
                unsafe { idle() };
            }
            RUNTIME_PAUSED.store(false, Ordering::Release);
            continue;
        }
        match future.as_mut().poll(&mut context) {
            Poll::Ready(()) => return,
            Poll::Pending => unsafe { idle() },
        }
    }
}

fn run_ready<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

struct Runtime {
    config: Config,
    global: GlobalState,
    tool: OnceLock<Detcore>,
}

struct ThreadRuntime {
    tid: Pid,
    state: DetcoreThreadState,
    initialized: bool,
    post_exec_pending: bool,
}

// TODO-HUMAN-REVIEW(PR-743): Review the scratch ABI shared with DynamoRIO.
#[repr(C)]
struct NativeThreadScratch {
    branches: u64,
    observed_syscalls: u64,
    rewritten_syscalls: u64,
    runtime_state: *mut ThreadRuntime,
    pending_thread_clone: u64,
    thread_clone_flags: u64,
    thread_clone_ctid: u64,
    pending_thread_start: u64,
    // TODO-HUMAN-REVIEW(PR-723): Review virtual-identity scratch ABI alignment.
    virtual_pid: i32,
    virtual_ppid: i32,
    virtual_tid: i32,
    pending_virtual_child: i32,
    pending_clone_flags: u64,
}

static RUNTIME: LazyLock<RwLock<Option<Arc<Runtime>>>> = LazyLock::new(|| RwLock::new(None));
static PENDING_THREAD_PARENTS: LazyLock<Mutex<HashMap<i32, (Tid, DetcoreThreadState)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static IMAGE_GENERATION: AtomicU64 = AtomicU64::new(0);
static READY_IMAGE: AtomicU64 = AtomicU64::new(0);
static RUNTIME_SHUTDOWN: AtomicBool = AtomicBool::new(false);
static COPIED_PANIC_ON_UNSUPPORTED: AtomicBool = AtomicBool::new(false);
static COPIED_UNSUPPORTED_REPORT_FD: AtomicI32 = AtomicI32::new(-1);
static RUNTIME_PAUSE_REQUESTED: AtomicBool = AtomicBool::new(false);
static RUNTIME_PAUSED: AtomicBool = AtomicBool::new(false);
static TOTAL_BRANCHES: AtomicU64 = AtomicU64::new(0);
static TOTAL_SYSCALLS: AtomicU64 = AtomicU64::new(0);
static TOTAL_REWRITTEN: AtomicU64 = AtomicU64::new(0);
static MEMORY_HASH: AtomicU64 = AtomicU64::new(FNV_OFFSET);

fn current_runtime() -> Arc<Runtime> {
    Arc::clone(
        RUNTIME
            .read()
            .expect("Detcore DBI runtime lock poisoned")
            .as_ref()
            .expect("Detcore DBI runtime was not initialized"),
    )
}

fn update_memory_hash(sysnum: i64, args: &[u64], read_memory: MemoryReader) {
    if sysnum != libc::SYS_write {
        return;
    }
    let address = args[1] as usize;
    let length = args[2] as usize;
    if address == 0 || length > MAX_OBSERVED_BUFFER {
        return;
    }

    let mut bytes = vec![0; length];
    if unsafe { read_memory(address, bytes.as_mut_ptr(), length) } == 0 {
        return;
    }

    let mut hash = FNV_OFFSET;
    for byte in sysnum
        .to_le_bytes()
        .into_iter()
        .chain(args[0].to_le_bytes())
        .chain((length as u64).to_le_bytes())
        .chain(bytes)
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    MEMORY_HASH.fetch_add(hash, Ordering::SeqCst);
}

fn getrandom_flags_are_valid(flags: u64) -> bool {
    let flags = flags as u32;
    let random = flags & libc::GRND_RANDOM != 0;
    let insecure = flags & libc::GRND_INSECURE != 0;

    flags & !GETRANDOM_ALLOWED_FLAGS == 0 && !(random && insecure)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GetrandomProbe {
    requested: usize,
    writable: usize,
}

impl GetrandomProbe {
    fn consumed(self) -> usize {
        if self.writable == self.requested {
            return self.requested;
        }

        let failed_chunk = self.writable / RANDOM_FILL_CHUNK_BYTES;
        ((failed_chunk + 1) * RANDOM_FILL_CHUNK_BYTES).min(self.requested)
    }
}

fn write_process_memory(
    pid: i32,
    remote_address: usize,
    bytes: &[u8],
    mut invoke: impl FnMut(&[u64; 6]) -> i64,
) -> Result<usize, Errno> {
    let page_size = 4096;
    let mut written = 0;
    while written < bytes.len() {
        let Some(remote) = remote_address.checked_add(written) else {
            return Ok(written);
        };
        let segment_len = (page_size - remote % page_size).min(bytes.len() - written);
        let local_iov = libc::iovec {
            iov_base: bytes[written..].as_ptr().cast_mut().cast(),
            iov_len: segment_len,
        };
        let remote_iov = libc::iovec {
            iov_base: remote as *mut c_void,
            iov_len: segment_len,
        };
        let process_vm_writev_args = [
            pid as u64,
            (&raw const local_iov) as u64,
            1,
            (&raw const remote_iov) as u64,
            1,
            0,
        ];
        let result = loop {
            let result = invoke(&process_vm_writev_args);
            if result != -(Errno::EINTR.into_raw() as i64) {
                break result;
            }
        };
        if result == -(Errno::EFAULT.into_raw() as i64) {
            return Ok(written);
        }
        if result < 0 {
            return Err(Errno::EIO);
        }
        let count = (result as usize).min(segment_len);
        if count == 0 {
            return Ok(written);
        }
        written += count;
    }
    Ok(written)
}

fn getrandom_writable_prefix(
    args: &[u64],
    mut write: impl FnMut(usize, &[u8]) -> Result<usize, Errno>,
) -> Option<Result<GetrandomProbe, Errno>> {
    if args[1] == 0 || !getrandom_flags_are_valid(args[2]) {
        return None;
    }

    let requested = (args[1] as usize).min(GETRANDOM_MAX_BYTES);
    let zeros = [0_u8; RANDOM_FILL_CHUNK_BYTES];
    let mut writable = 0;
    while writable < requested {
        let Some(remote) = (args[0] as usize).checked_add(writable) else {
            break;
        };
        let chunk_len = (requested - writable).min(RANDOM_FILL_CHUNK_BYTES);
        let count = match write(remote, &zeros[..chunk_len]) {
            Ok(count) => count.min(chunk_len),
            Err(error) => return Some(Err(error)),
        };
        writable += count;
        if count < chunk_len {
            break;
        }
    }
    Some(Ok(GetrandomProbe {
        requested,
        writable,
    }))
}

fn advance_getrandom_prng(prng: &mut impl rand::Rng, bytes: usize) {
    let mut words = [0_u64; RANDOM_FILL_CHUNK_BYTES / std::mem::size_of::<u64>()];
    let mut advanced = 0;
    while advanced < bytes {
        let chunk_len = (bytes - advanced).min(RANDOM_FILL_CHUNK_BYTES);
        let chunk =
            unsafe { std::slice::from_raw_parts_mut(words.as_mut_ptr().cast::<u8>(), chunk_len) };
        prng.fill(chunk);
        advanced += chunk_len;
    }
}

fn report_fd_is_available() -> bool {
    (unsafe { libc::fcntl(UNSUPPORTED_SYSCALL_REPORT_FD, libc::F_GETFD) }) != -1
}

fn append_copied_syscall_record(sysnum: i64) {
    let report_fd = COPIED_UNSUPPORTED_REPORT_FD.load(Ordering::Acquire);
    if report_fd == -1 {
        return;
    }
    let mut buffer = [0_u8; 24];
    let mut index = buffer.len() - 1;
    buffer[index] = b'\n';
    let mut value = sysnum as u64;
    loop {
        index -= 1;
        buffer[index] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    index -= 1;
    buffer[index] = b'@';
    let _ = unsafe {
        libc::write(
            report_fd,
            buffer[index..].as_ptr().cast(),
            buffer.len() - index,
        )
    };
}

fn error_result(error: Error) -> i64 {
    match error {
        Error::Errno(errno) => -(errno.into_raw() as i64),
        _ => -(Errno::EIO.into_raw() as i64),
    }
}

/// Returns the Detcore DBI cdylib built beside the running Hermit binary or in Cargo's deps directory.
// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-738): Review native-client linkage to the minimal DBI runtime.
pub fn runtime_library_path() -> io::Result<PathBuf> {
    let executable = std::env::current_exe()?;
    let [deps, direct] = runtime_library_candidates(&executable)?;
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#598): Confirm deps-first lookup matches Cargo artifact placement.
    [deps, direct]
        .into_iter()
        .find(|runtime| runtime.is_file())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "Hermit DBI runtime was not built beside {} or in its deps directory",
                    executable.display()
                ),
            )
        })
}
fn runtime_library_candidates(executable: &std::path::Path) -> io::Result<[PathBuf; 2]> {
    let directory = executable.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Hermit executable has no parent directory",
        )
    })?;
    Ok([
        directory.join("deps/libdetcore_dbi.so"),
        directory.join("libdetcore_dbi.so"),
    ])
}

fn lock_native_client_build(directory: &std::path::Path) -> io::Result<fs::File> {
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join("build.lock"))?;
    loop {
        // SAFETY: lock owns this valid file descriptor for the lifetime of the lock.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(lock);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Builds the DynamoRIO native client against the Detcore runtime if needed.
pub fn prepare_native_client() -> io::Result<(PathBuf, PathBuf)> {
    let runtime = runtime_library_path()?;
    let source = reverie_dbi::native_client_source_dir();
    let source_identity = source
        .parent()
        .and_then(std::path::Path::parent)
        .and_then(std::path::Path::file_name)
        .unwrap_or_else(|| std::ffi::OsStr::new("source"));
    let directory = runtime
        .parent()
        .expect("runtime library path must have a parent")
        .join(format!(
            "detcore-dbi-native-{}",
            source_identity.to_string_lossy()
        ));
    fs::create_dir_all(&directory)?;
    let _build_lock = lock_native_client_build(&directory)?;

    let configure = Command::new("cmake")
        .arg("-S")
        .arg(source)
        .arg("-B")
        .arg(&directory)
        .arg("-DCMAKE_BUILD_TYPE=Release")
        .arg(format!(
            "-DDynamoRIO_DIR={}",
            reverie_dbi::bundled_dynamorio_cmake_dir().display()
        ))
        .arg(format!("-DREVERIE_DBI_RUNTIME={}", runtime.display()))
        .output()?;
    if !configure.status.success() {
        return Err(io::Error::other(format!(
            "failed to configure Detcore DBI client: {}",
            String::from_utf8_lossy(&configure.stderr)
        )));
    }

    let build = Command::new("cmake")
        .arg("--build")
        .arg(&directory)
        .arg("--parallel")
        .output()?;
    if !build.status.success() {
        return Err(io::Error::other(format!(
            "failed to build Detcore DBI client: {}",
            String::from_utf8_lossy(&build.stderr)
        )));
    }

    let client = directory.join("libreverie_dbi_client.so");
    if !client.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Detcore DBI client was not built at {}", client.display()),
        ));
    }
    Ok((reverie_dbi::bundled_drrun_path().to_path_buf(), client))
}

/// Begins a new DynamoRIO application image and returns its generation.
#[unsafe(no_mangle)]
pub extern "C" fn reverie_dbi_runtime_image_init() -> u64 {
    IMAGE_GENERATION.fetch_add(1, Ordering::SeqCst) + 1
}

/// Runs Detcore's async global scheduler on a DynamoRIO-managed client thread.
///
/// The native client starts this entry point before registering guest events
/// and waits for [`reverie_dbi_runtime_ready`] before allowing callbacks.
///
/// # Safety
///
/// `argument` must point to a valid [`reverie_dbi::DbiRuntimeCallbacks`] value.
#[unsafe(no_mangle)]
// TODO-HUMAN-REVIEW(PR-587): Confirm external scheduler callback and restart semantics.
pub unsafe extern "C" fn reverie_dbi_runtime_background_init(argument: *mut c_void) {
    let image_generation = IMAGE_GENERATION.load(Ordering::SeqCst);
    let callbacks = unsafe { &*argument.cast::<reverie_dbi::DbiRuntimeCallbacks>() };
    let emit = callbacks.emit;
    RUNTIME_SHUTDOWN.store(false, Ordering::Release);
    RUNTIME_PAUSE_REQUESTED.store(false, Ordering::Release);
    RUNTIME_PAUSED.store(false, Ordering::Release);
    emit_marker(emit, b"detcore-dbi: background client thread entered\n");
    let runtime = {
        let mut slot = RUNTIME.write().expect("Detcore DBI runtime lock poisoned");
        if slot.is_none() {
            emit_marker(emit, b"detcore-dbi: constructing Detcore Config\n");
            let (mut config, source) = load_dbi_config();
            match source {
                ConfigSource::Cli => {
                    emit_marker(emit, b"detcore-dbi: using CLI-provided Detcore Config\n")
                }
                ConfigSource::ParseFallback => emit_marker(
                    emit,
                    b"detcore-dbi: WARNING could not parse HERMIT_DBI_DETCONFIG; using strict default\n",
                ),
                ConfigSource::Default => {
                    emit_marker(emit, b"detcore-dbi: using strict default Detcore Config\n")
                }
            }
            // Fail-closed unsupported-syscall handling (PR #644): the rest of the
            // Config arrives via the CLI env above, but the panic flag comes from
            // the DBI callback (the `-panic-on-unsupported-syscalls` client
            // argument), because DynamoRIO re-injects the client across execve
            // while an empty-env exec would drop the serialized config. Set up
            // the protected report descriptor the guest children write aggregated
            // unsupported-syscall records to, and force the exit+report path so a
            // child terminates the process tree deterministically.
            let panic_on_unsupported_syscalls = callbacks.panic_on_unsupported_syscalls != 0;
            config.panic_on_unsupported_syscalls = panic_on_unsupported_syscalls;
            COPIED_PANIC_ON_UNSUPPORTED.store(panic_on_unsupported_syscalls, Ordering::Release);
            let copied_report_fd = unsafe {
                libc::fcntl(
                    UNSUPPORTED_SYSCALL_REPORT_FD,
                    libc::F_DUPFD_CLOEXEC,
                    UNSUPPORTED_SYSCALL_REPORT_FD + 1,
                )
            };
            COPIED_UNSUPPORTED_REPORT_FD.store(copied_report_fd, Ordering::Release);
            // The DBI backend reports and aborts through the exit path plus the
            // protected report descriptor, not the ptrace-style unrecoverable
            // shutdown: unrecoverable_shutdown runs first in the handler and
            // would suppress the UnsupportedSyscallError that carries the
            // "unsupported syscall" diagnostic the parent aggregates. Force the
            // exit+report path regardless of what the serialized config carried.
            config.exit_on_unsupported_syscall = true;
            config.shutdown_on_unsupported_syscall = false;
            config.unsupported_syscall_report_fd =
                report_fd_is_available().then_some(UNSUPPORTED_SYSCALL_REPORT_FD);
            config.validate();

            emit_marker(emit, b"detcore-dbi: initializing Detcore GlobalState\n");
            let global = GlobalState::init_for_external_scheduler(&config);
            emit_marker(emit, b"detcore-dbi: GlobalState initialized\n");
            *slot = Some(Arc::new(Runtime {
                config,
                global,
                tool: OnceLock::new(),
            }));
        }
        Arc::clone(slot.as_ref().expect("Detcore DBI runtime was initialized"))
    };
    emit_marker(emit, b"detcore-dbi: background scheduler ready\n");
    READY_IMAGE.store(image_generation, Ordering::SeqCst);
    let log_scheduler = info_logging_enabled();
    let observer = Arc::new(move |event: &'static str| {
        if log_scheduler {
            let line = format!("INFO detcore::scheduler: {event}\n");
            unsafe { emit(line.as_ptr(), line.len()) };
        }
    });
    run_cooperative(
        runtime.global.run_external_scheduler(observer),
        callbacks.idle,
    );
    emit_marker(emit, b"detcore-dbi: background scheduler completed\n");
}

/// Requests shutdown of the backend-owned scheduler at process exit.
#[unsafe(no_mangle)]
// TODO-HUMAN-REVIEW(PR-587): Confirm process-exit scheduler ownership.
pub extern "C" fn reverie_dbi_runtime_process_exit() {
    READY_IMAGE.store(0, Ordering::Release);
    RUNTIME_SHUTDOWN.store(true, Ordering::Release);
}

/// Reports whether the Detcore global scheduler is ready for this image.
#[unsafe(no_mangle)]
// TODO-HUMAN-REVIEW(PR-587): Confirm image-generation readiness ordering.
pub extern "C" fn reverie_dbi_runtime_ready(image_generation: u64) -> i32 {
    i32::from(
        READY_IMAGE.load(Ordering::Acquire) == image_generation
            && !RUNTIME_PAUSE_REQUESTED.load(Ordering::Acquire)
            && !RUNTIME_PAUSED.load(Ordering::Acquire),
    )
}

/// Initializes native per-thread scratch state and registers the application
/// thread with Detcore before it begins executing guest code.
///
/// Copied process runtimes retain scratch-only state until exec installs a new
/// scheduler owned by that process.
///
/// Returns a positive retry status when a native child's parent snapshot is not
/// published yet, so the client can retry outside DynamoRIO's thread-init path.
///
/// # Safety
///
/// The native client must pass a valid writable `scratch` pointer, a live
/// DynamoRIO `context`, and callback pointers valid for this application.
// TODO-HUMAN-REVIEW(PR-743): Review the native thread initialization ABI and state handoff.
// TODO-HUMAN-REVIEW(PR-874): Review compatibility with Reverie's expanded DBI callback ABI.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn reverie_dbi_runtime_thread_init(
    scratch: *mut c_void,
    context: *mut c_void,
    tid: i32,
    pid: i32,
    _in_tree_ppid: i32,
    branch_count: u64,
    defer_runtime: i32,
    invoke_syscall: SyscallInvoker,
    read_registers: RegisterReader,
    write_registers: RegisterWriter,
) -> i32 {
    unsafe {
        scratch
            .cast::<NativeThreadScratch>()
            .write(NativeThreadScratch {
                branches: branch_count,
                observed_syscalls: 0,
                rewritten_syscalls: 0,
                runtime_state: std::ptr::null_mut(),
                pending_thread_clone: 0,
                thread_clone_flags: 0,
                thread_clone_ctid: 0,
                pending_thread_start: 0,
                virtual_pid: 0,
                virtual_ppid: 0,
                virtual_tid: 0,
                pending_virtual_child: 0,
                pending_clone_flags: 0,
            });
    }
    if defer_runtime != 0 {
        return 0;
    }

    let runtime = current_runtime();
    let tool = runtime
        .tool
        .get_or_init(|| Detcore::new(Pid::from_raw(pid), &runtime.config));
    let parent = if tid == pid {
        None
    } else {
        let parent = PENDING_THREAD_PARENTS
            .lock()
            .expect("pending DBI thread parent lock poisoned")
            .remove(&tid);
        let Some(parent) = parent else {
            return 1;
        };
        Some(parent)
    };
    let parent_ref = parent
        .as_ref()
        .map(|(parent_tid, state)| (*parent_tid, state));
    let tid = Pid::from_raw(tid);
    let pid = Pid::from_raw(pid);
    let mut thread = Box::new(ThreadRuntime {
        tid,
        state: tool.init_thread_state(Tid::from_raw(tid.into()), parent_ref),
        initialized: false,
        post_exec_pending: tid == pid,
    });
    if reverie_dbi::run_tool_thread_start(
        tool,
        context as usize,
        tid,
        pid,
        branch_count,
        &mut thread.state,
        &runtime.global,
        &runtime.config,
        invoke_syscall,
        read_registers,
        write_registers,
    )
    .is_err()
    {
        return -1;
    }
    thread.initialized = true;
    unsafe {
        (*scratch.cast::<NativeThreadScratch>()).runtime_state = Box::into_raw(thread);
    }
    0
}

/// Registers a child thread created by a native clone syscall.
///
/// # Safety
///
/// `scratch` must name the initialized parent state, `context` must be its
/// live DynamoRIO context, and callback pointers must remain valid.
// TODO-HUMAN-REVIEW(PR-743): Review parent-side native child registration.
// TODO-HUMAN-REVIEW(PR-874): Review register-writer propagation to child registration.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn reverie_dbi_runtime_thread_created(
    scratch: *mut c_void,
    context: *mut c_void,
    parent_tid: i32,
    pid: i32,
    branch_count: u64,
    child_tid: i32,
    child_tid_addr: u64,
    flags: u64,
    invoke_syscall: SyscallInvoker,
    read_registers: RegisterReader,
    write_registers: RegisterWriter,
) -> i32 {
    let scratch = unsafe { &mut *scratch.cast::<NativeThreadScratch>() };
    if scratch.runtime_state.is_null() {
        return -1;
    }

    let runtime = current_runtime();
    let tool = runtime
        .tool
        .get()
        .expect("Detcore DBI tool was initialized");
    let parent = unsafe { &mut *scratch.runtime_state };
    let flags = CloneFlags::from_bits_truncate(flags);
    parent.state.clone_flags = Some(flags);
    let parent_snapshot = parent.state.clone();
    if PENDING_THREAD_PARENTS
        .lock()
        .expect("pending DBI thread parent lock poisoned")
        .insert(child_tid, (Tid::from_raw(parent_tid), parent_snapshot))
        .is_some()
    {
        parent.state.clone_flags = None;
        return -1;
    }

    {
        let mut guest = DbiGuest::new(
            context as usize,
            parent.tid,
            Pid::from_raw(pid),
            None,
            branch_count,
            &mut parent.state,
            &runtime.global,
            &runtime.config,
            invoke_syscall,
            read_registers,
            write_registers,
        );
        run_ready(tool.register_external_child(
            &mut guest,
            Tid::from_raw(child_tid),
            child_tid_addr as usize,
            flags,
        ));
    }
    parent.state.clone_flags = None;
    0
}

/// Releases Detcore state owned by a DynamoRIO application thread.
///
/// # Safety
///
/// `scratch` must be the pointer initialized by
/// [`reverie_dbi_runtime_thread_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reverie_dbi_runtime_thread_exit(scratch: *mut c_void) {
    let scratch = unsafe { &mut *scratch.cast::<NativeThreadScratch>() };
    if scratch.runtime_state.is_null() {
        return;
    }
    let ThreadRuntime {
        tid,
        state,
        initialized,
        ..
    } = *unsafe { Box::from_raw(scratch.runtime_state) };
    scratch.runtime_state = std::ptr::null_mut();
    if initialized {
        let runtime = current_runtime();
        let tool = runtime
            .tool
            .get()
            .expect("Detcore DBI tool was initialized");
        let _ = reverie_dbi::run_tool_thread_exit(
            tool,
            tid,
            state,
            &runtime.global,
            &runtime.config,
            ExitStatus::SUCCESS,
        );
    }
}

fn resume_paused_runtime() {
    RUNTIME_PAUSE_REQUESTED.store(false, Ordering::Release);
    while RUNTIME_PAUSED.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    READY_IMAGE.store(IMAGE_GENERATION.load(Ordering::Acquire), Ordering::Release);
}

/// Restarts the existing scheduler after the kernel rejects a native exec.
///
/// # Safety
///
/// `_scratch` must be the pointer supplied by the native DBI callback. It is not
/// dereferenced because a failed exec preserves the current Detcore thread state.
#[unsafe(no_mangle)]
// TODO-HUMAN-REVIEW(PR-587): Confirm failed-exec preserves Runtime and thread state.
pub unsafe extern "C" fn reverie_dbi_runtime_exec_failed(_scratch: *mut c_void, _pid: i32) {
    assert!(
        RUNTIME
            .read()
            .expect("Detcore DBI runtime lock poisoned")
            .is_some(),
        "failed exec had no Detcore runtime"
    );
    resume_paused_runtime();
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-644): Review fork-safe policy enforcement before copied children bypass.
// TODO-HUMAN-REVIEW(PR-978): Review extending the copied-child gate from the
// Unsupported set to the full deterministic-refusal boundary.
/// Applies the deterministic-refusal policy in a copied pre-exec DBI child.
///
/// A copied pre-exec child runs natively on the DynamoRIO client stack with no
/// Detcore tool, so every syscall it makes bypasses `handle_syscall_event`.
/// Returning 0 lets the syscall run natively; returning 1 fail-closes by
/// aborting the runtime tree. There is no errno-injection channel in this ABI,
/// so a fixed-ENOSYS/EPERM syscall cannot be emulated here — the only way to
/// avoid leaking host state is to refuse the whole child.
///
/// The gate covers the classic Unsupported set plus the broader fixed-error
/// boundary. Unconditional deterministic refusals fail closed in every mode;
/// compatibility families that the root process refuses only under strict
/// execution (`rseq`, zero-copy pipes, keyrings) retain native non-strict
/// behavior. Before PR-978 both groups could execute natively in a copied child
/// despite the root Detcore policy.
#[unsafe(no_mangle)]
pub extern "C" fn reverie_dbi_runtime_copied_syscall(sysnum: i64) -> i32 {
    let sysno = Sysno::from(sysnum as i32);
    let strict = COPIED_PANIC_ON_UNSUPPORTED.load(Ordering::Acquire);
    // TODO-HUMAN-REVIEW(PR-981): Copied DBI children cannot enter the Rust
    // Detcore Tool, and this callback receives no syscall arguments with which
    // to distinguish timestamp ioctls or timestamp-enabled receive buffers.
    // Strict mode therefore fails closed for the three syscall classes that can
    // expose native socket timestamps. Non-strict mode retains native behavior.
    if matches!(sysno, Sysno::ioctl | Sysno::recvmsg | Sysno::recvmmsg) && strict {
        return 1;
    }
    if detcore::is_deterministically_refused_syscall(sysno)
        && (strict || !detcore::is_strict_only_deterministic_refusal_syscall(sysno))
    {
        return 1;
    }
    if !detcore::is_unsupported_syscall(sysno) {
        return 0;
    }
    if strict {
        1
    } else {
        append_copied_syscall_record(sysnum);
        0
    }
}

// TODO-HUMAN-REVIEW(PR-874): Review deferred DBI syscall encoding.
unsafe fn write_deferred_syscall(syscall: Syscall, number: *mut i64, args: *mut u64) {
    let (sysno, syscall_args) = syscall.into_parts();
    unsafe { number.write(sysno.id() as i64) };
    let values = [
        syscall_args.arg0 as u64,
        syscall_args.arg1 as u64,
        syscall_args.arg2 as u64,
        syscall_args.arg3 as u64,
        syscall_args.arg4 as u64,
        syscall_args.arg5 as u64,
    ];
    unsafe { std::slice::from_raw_parts_mut(args, values.len()) }.copy_from_slice(&values);
}

/// Dispatches one DynamoRIO syscall event through the real Detcore Tool.
///
/// # Safety
///
/// All pointers and callbacks must remain valid for this callback. `args` must
/// address six syscall arguments and `result` must be writable.
#[allow(clippy::too_many_arguments)]
#[unsafe(no_mangle)]
// TODO-HUMAN-REVIEW(PR-587): Confirm native process dispatch pauses only exec.
// TODO-HUMAN-REVIEW(PR-874): Review deferred-syscall and register-writer ABI compatibility.
pub unsafe extern "C" fn reverie_dbi_runtime_pre_syscall(
    context: *mut c_void,
    scratch: *mut c_void,
    tid: i32,
    pid: i32,
    image_generation: u64,
    sysnum: i64,
    args: *const u64,
    branches: u64,
    result: *mut i64,
    deferred_sysnum: *mut i64,
    deferred_args: *mut u64,
    invoke_syscall: SyscallInvoker,
    read_registers: RegisterReader,
    write_registers: RegisterWriter,
    read_memory: MemoryReader,
    emit: unsafe extern "C" fn(*const u8, usize),
) -> i32 {
    let first_event = TOTAL_SYSCALLS.fetch_add(1, Ordering::Relaxed) == 0;
    if first_event {
        let message = b"detcore-dbi: entered Rust syscall callback\n";
        unsafe { emit(message.as_ptr(), message.len()) };
    }
    let raw_args = unsafe { std::slice::from_raw_parts(args, 6) };
    let mut dispatch_args: [u64; 6] = raw_args.try_into().expect("six syscall arguments");
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-849): Review fault-safe DBI getrandom writes.
    // Probe with deterministic zeros through process_vm_writev, then let Detcore overwrite
    // the entire writable prefix before the application resumes.
    let getrandom_probe = if sysnum == libc::SYS_getrandom {
        match getrandom_writable_prefix(raw_args, |remote, bytes| {
            write_process_memory(pid, remote, bytes, |process_vm_writev_args| unsafe {
                invoke_syscall(
                    context as usize,
                    libc::SYS_process_vm_writev,
                    process_vm_writev_args.as_ptr(),
                )
            })
        }) {
            Some(Ok(probe)) => {
                dispatch_args[1] = probe.writable as u64;
                Some(probe)
            }
            Some(Err(error)) => {
                unsafe { result.write(-(error.into_raw() as i64)) };
                TOTAL_REWRITTEN.fetch_add(1, Ordering::Relaxed);
                return 1;
            }
            None => None,
        }
    } else {
        None
    };
    if sysnum == libc::SYS_execveat {
        unsafe { result.write(-(Errno::ENOSYS.into_raw() as i64)) };
        TOTAL_REWRITTEN.fetch_add(1, Ordering::Relaxed);
        return 1;
    }
    // clone(2) and clone3(2) return in both the parent and child. Injecting
    // either from this callback makes the child return on the client stack.
    if requires_native_lifecycle(sysnum) {
        if sysnum == libc::SYS_execve {
            READY_IMAGE.store(0, Ordering::Release);
            RUNTIME_PAUSE_REQUESTED.store(true, Ordering::Release);
            while !RUNTIME_PAUSED.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            assert_eq!(
                IMAGE_GENERATION.load(Ordering::Acquire),
                image_generation,
                "DBI image generation changed while pausing for exec"
            );
        }
        return 0;
    }
    TOTAL_BRANCHES.store(branches, Ordering::Relaxed);
    update_memory_hash(sysnum, raw_args, read_memory);
    let runtime = current_runtime();
    let tool = runtime
        .tool
        .get_or_init(|| Detcore::new(Pid::from_raw(pid), &runtime.config));
    let tid = Pid::from_raw(tid);
    let pid = Pid::from_raw(pid);
    let syscall = Syscall::from_raw(
        Sysno::from(sysnum as i32),
        SyscallArgs::new(
            dispatch_args[0] as usize,
            dispatch_args[1] as usize,
            dispatch_args[2] as usize,
            dispatch_args[3] as usize,
            dispatch_args[4] as usize,
            dispatch_args[5] as usize,
        ),
    );

    if first_event {
        let message = b"detcore-dbi: initializing Detcore thread state\n";
        unsafe { emit(message.as_ptr(), message.len()) };
    }
    let scratch = unsafe { &mut *scratch.cast::<NativeThreadScratch>() };
    if scratch.runtime_state.is_null() {
        if first_event {
            let message = b"detcore-dbi: constructing Detcore thread state\n";
            unsafe { emit(message.as_ptr(), message.len()) };
        }
        let state = tool.init_thread_state(Tid::from_raw(tid.into()), None);
        if first_event {
            let message = b"detcore-dbi: Detcore thread state constructed\n";
            unsafe { emit(message.as_ptr(), message.len()) };
        }
        scratch.runtime_state = Box::into_raw(Box::new(ThreadRuntime {
            tid,
            state,
            initialized: false,
            post_exec_pending: true,
        }));
    }
    let thread = unsafe { &mut *scratch.runtime_state };
    if !thread.initialized {
        if first_event {
            let message = b"detcore-dbi: running Detcore thread-start hook\n";
            unsafe { emit(message.as_ptr(), message.len()) };
        }
        if let Err(error) = reverie_dbi::run_tool_thread_start(
            tool,
            context as usize,
            tid,
            pid,
            branches,
            &mut thread.state,
            &runtime.global,
            &runtime.config,
            invoke_syscall,
            read_registers,
            write_registers,
        ) {
            unsafe { result.write(error_result(error)) };
            TOTAL_REWRITTEN.fetch_add(1, Ordering::Relaxed);
            return 1;
        }
        thread.initialized = true;
    }
    if thread.post_exec_pending {
        if first_event {
            let message = b"detcore-dbi: thread-start hook completed; running post-exec\n";
            unsafe { emit(message.as_ptr(), message.len()) };
        }
        if let Err(errno) = reverie_dbi::run_tool_post_exec(
            tool,
            context as usize,
            tid,
            pid,
            branches,
            &mut thread.state,
            &runtime.global,
            &runtime.config,
            invoke_syscall,
            read_registers,
            write_registers,
        ) {
            unsafe { result.write(-(errno.into_raw() as i64)) };
            TOTAL_REWRITTEN.fetch_add(1, Ordering::Relaxed);
            return 1;
        }
        if first_event {
            let message = b"detcore-dbi: post-exec hook completed\n";
            unsafe { emit(message.as_ptr(), message.len()) };
        }
        thread.post_exec_pending = false;
    }

    if first_event {
        let message = b"detcore-dbi: dispatching first syscall through Detcore\n";
        unsafe { emit(message.as_ptr(), message.len()) };
    }
    let getrandom_prng = getrandom_probe
        .filter(|probe| probe.writable < probe.requested)
        .map(|probe| (probe, thread.state.prng.clone()));
    let mut outcome = reverie_dbi::run_tool_syscall(
        tool,
        context as usize,
        tid,
        pid,
        branches,
        &mut thread.state,
        &runtime.global,
        &runtime.config,
        syscall,
        invoke_syscall,
        read_registers,
        write_registers,
    );
    if let Some((probe, original_prng)) = getrandom_prng {
        // The shortened safe write must consume exactly the stream portion that the shared
        // Detcore handler consumes before its first guest-memory fault.
        thread.state.prng = original_prng;
        advance_getrandom_prng(&mut thread.state.prng, probe.consumed());
        if matches!(outcome, Ok(DbiSyscallOutcome::Suppress(_))) {
            let value = if probe.writable == 0 {
                -(Errno::EFAULT.into_raw() as i64)
            } else {
                probe.writable as i64
            };
            outcome = Ok(DbiSyscallOutcome::Suppress(value));
        }
    }
    match outcome {
        Ok(DbiSyscallOutcome::Suppress(value)) => {
            unsafe { result.write(value) };
            TOTAL_REWRITTEN.fetch_add(1, Ordering::Relaxed);
            1
        }
        Ok(DbiSyscallOutcome::ExecuteOriginal(syscall)) => {
            unsafe { write_deferred_syscall(syscall, deferred_sysnum, deferred_args) };
            2
        }
        Err(Error::Tool(error)) => {
            if let Some(unsupported) = error.downcast_ref::<UnsupportedSyscallError>() {
                let message = format!("detcore-dbi: {unsupported}\n");
                unsafe { emit(message.as_ptr(), message.len()) };
                -1
            } else {
                unsafe { result.write(error_result(Error::Tool(error))) };
                TOTAL_REWRITTEN.fetch_add(1, Ordering::Relaxed);
                1
            }
        }
        Err(error) => {
            unsafe { result.write(error_result(error)) };
            TOTAL_REWRITTEN.fetch_add(1, Ordering::Relaxed);
            1
        }
    }
}

/// Returns the linked Reverie Tool name for native DBI-path evidence.
#[unsafe(no_mangle)]
pub extern "C" fn reverie_dbi_runtime_name() -> *const libc::c_char {
    c"Detcore".as_ptr()
}

/// Returns Detcore DBI counters and the observed guest-memory hash.
///
/// # Safety
///
/// Every output pointer must be aligned and writable for one `u64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reverie_dbi_runtime_totals(
    branches: *mut u64,
    syscalls: *mut u64,
    rewritten: *mut u64,
    memory_hash: *mut u64,
) {
    unsafe {
        branches.write(TOTAL_BRANCHES.load(Ordering::Relaxed));
        syscalls.write(TOTAL_SYSCALLS.load(Ordering::Relaxed));
        rewritten.write(TOTAL_REWRITTEN.load(Ordering::Relaxed));
        memory_hash.write(MEMORY_HASH.load(Ordering::SeqCst));
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_client_links_only_the_dedicated_dbi_runtime() {
        let executable = std::path::Path::new("/workspace/target/debug/hermit");
        let [deps, direct] = runtime_library_candidates(executable).unwrap();
        assert_eq!(
            deps,
            std::path::Path::new("/workspace/target/debug/deps/libdetcore_dbi.so")
        );
        assert_eq!(
            direct,
            std::path::Path::new("/workspace/target/debug/libdetcore_dbi.so")
        );
    }

    #[test]
    fn only_dynamorio_managed_lifecycle_stays_native() {
        for sysnum in [
            libc::SYS_fork,
            libc::SYS_vfork,
            libc::SYS_clone,
            libc::SYS_clone3,
            libc::SYS_rt_sigreturn,
            libc::SYS_execve,
        ] {
            assert!(requires_native_lifecycle(sysnum));
        }
        for sysnum in [
            libc::SYS_execveat,
            libc::SYS_wait4,
            libc::SYS_waitid,
            libc::SYS_read,
        ] {
            assert!(!requires_native_lifecycle(sysnum));
        }
    }

    // TODO-HUMAN-REVIEW(PR-916): Regression for the copied-DBI-child keyring
    // isolation boundary. A copied pre-exec child runs no Rust Detcore Tool, so
    // the gate must refuse the (now Determinized) keyring family in strict mode
    // rather than let it execute natively against the host keyring.
    #[test]
    fn copied_child_refuses_keyring_syscalls_under_strict() {
        let saved = COPIED_PANIC_ON_UNSUPPORTED.load(Ordering::Acquire);

        // Strict (panic-on-unsupported): keyring syscalls are refused so the
        // copied child cannot mutate host keyrings or trigger request-key
        // upcalls. `1` tells the native client to exit the isolated runtime
        // tree (fail closed), matching the pre-848 Unsupported behavior.
        COPIED_PANIC_ON_UNSUPPORTED.store(true, Ordering::Release);
        assert_eq!(reverie_dbi_runtime_copied_syscall(libc::SYS_keyctl), 1);
        assert_eq!(reverie_dbi_runtime_copied_syscall(libc::SYS_add_key), 1);
        assert_eq!(reverie_dbi_runtime_copied_syscall(libc::SYS_request_key), 1);
        // A supported syscall still runs natively even under strict mode.
        assert_eq!(reverie_dbi_runtime_copied_syscall(libc::SYS_getpid), 0);

        // Non-strict: keyring syscalls fall through to native pass-through,
        // matching the root process's non-strict keyring behavior.
        COPIED_PANIC_ON_UNSUPPORTED.store(false, Ordering::Release);
        assert_eq!(reverie_dbi_runtime_copied_syscall(libc::SYS_keyctl), 0);
        assert_eq!(reverie_dbi_runtime_copied_syscall(libc::SYS_add_key), 0);
        assert_eq!(reverie_dbi_runtime_copied_syscall(libc::SYS_request_key), 0);

        COPIED_PANIC_ON_UNSUPPORTED.store(saved, Ordering::Release);
    }

    #[test]
    fn getrandom_flag_validation_matches_detcore_policy() {
        for flags in [
            0,
            u64::from(libc::GRND_NONBLOCK),
            u64::from(libc::GRND_RANDOM),
            u64::from(libc::GRND_NONBLOCK | libc::GRND_RANDOM),
            1_u64 << 32,
        ] {
            assert!(getrandom_flags_are_valid(flags), "flags={flags:#x}");
        }
        assert!(!getrandom_flags_are_valid(u64::from(
            libc::GRND_RANDOM | libc::GRND_INSECURE
        )));
        assert!(!getrandom_flags_are_valid(0x8000_0000));
    }

    #[test]
    fn getrandom_probe_uses_zero_writes_and_tracks_shared_consumption() {
        let args = [0x1000, 16, 0, 0, 0, 0];
        let partial = getrandom_writable_prefix(&args, |remote, bytes| {
            assert_eq!(remote, 0x1000);
            assert_eq!(bytes, [0_u8; 16]);
            Ok(8)
        });
        let partial = partial.unwrap().unwrap();
        assert_eq!(
            partial,
            GetrandomProbe {
                requested: 16,
                writable: 8,
            }
        );
        assert_eq!(partial.consumed(), 16);

        let huge = [1, u64::MAX, 0, 0, 0, 0];
        let fault = getrandom_writable_prefix(&huge, |_, _| Ok(0))
            .unwrap()
            .unwrap();
        assert_eq!(fault.writable, 0);
        assert_eq!(fault.requested, GETRANDOM_MAX_BYTES);
        assert_eq!(fault.consumed(), RANDOM_FILL_CHUNK_BYTES);

        let invalid_flags = [0x1000, 16, 0x8000_0000, 0, 0, 0];
        let mut invoked = false;
        assert_eq!(
            getrandom_writable_prefix(&invalid_flags, |_, _| {
                invoked = true;
                Ok(16)
            }),
            None
        );
        assert!(!invoked);
    }

    #[test]
    fn process_memory_writer_retries_interrupts_and_stops_at_faults() {
        let mut calls = 0;
        let written = write_process_memory(7, 0x1ff8, &[0_u8; 16], |args| {
            assert_eq!(args[0], 7);
            assert_eq!(args[2], 1);
            assert_eq!(args[4], 1);
            calls += 1;
            match calls {
                1 => -(Errno::EINTR.into_raw() as i64),
                2 => 8,
                3 => -(Errno::EFAULT.into_raw() as i64),
                _ => unreachable!(),
            }
        })
        .unwrap();
        assert_eq!(written, 8);
        assert_eq!(calls, 3);
    }

    #[test]
    fn native_thread_init_uses_the_expanded_success_returning_abi() {
        unsafe extern "C" fn invoke_syscall(
            _context: usize,
            _sysnum: i64,
            _args: *const u64,
        ) -> i64 {
            0
        }
        unsafe extern "C" fn read_registers(
            _context: usize,
            _registers: *mut libc::user_regs_struct,
        ) -> i32 {
            0
        }
        unsafe extern "C" fn write_registers(
            _context: usize,
            _registers: *const libc::user_regs_struct,
        ) -> i32 {
            0
        }

        let mut scratch = std::mem::MaybeUninit::<NativeThreadScratch>::uninit();
        let status = unsafe {
            reverie_dbi_runtime_thread_init(
                scratch.as_mut_ptr().cast(),
                std::ptr::null_mut(),
                7,
                7,
                -1,
                99,
                1,
                invoke_syscall,
                read_registers,
                write_registers,
            )
        };

        assert_eq!(status, 0);
        let scratch = unsafe { scratch.assume_init() };
        assert_eq!(scratch.branches, 99);
        assert_eq!(scratch.observed_syscalls, 0);
        assert_eq!(scratch.rewritten_syscalls, 0);
        assert!(scratch.runtime_state.is_null());
        assert_eq!(scratch.pending_thread_clone, 0);
        assert_eq!(scratch.thread_clone_flags, 0);
        assert_eq!(scratch.thread_clone_ctid, 0);
        assert_eq!(scratch.pending_thread_start, 0);
        assert_eq!(scratch.virtual_pid, 0);
        assert_eq!(scratch.virtual_ppid, 0);
        assert_eq!(scratch.virtual_tid, 0);
        assert_eq!(scratch.pending_virtual_child, 0);
        assert_eq!(scratch.pending_clone_flags, 0);
    }

    #[test]
    fn copied_child_gate_refuses_deterministic_refusal_families_in_strict() {
        // The copied pre-exec child runs natively with no Detcore tool. Under
        // strict mode the gate must fail-close (return 1) not only for the
        // classic Unsupported set but for the full deterministic-refusal
        // boundary (splice/tee/vmsplice, perf_event_open, the keyring family),
        // otherwise strict guests execute those syscalls natively against the
        // host. Report fd is left at its -1 default so `append_copied_syscall_record`
        // is a no-op and the non-strict branch has no observable side effect.
        let previous = COPIED_PANIC_ON_UNSUPPORTED.load(Ordering::Acquire);

        // Every family member must be recognized by the shared predicate.
        for sysno in [
            Sysno::splice,
            Sysno::tee,
            Sysno::vmsplice,
            Sysno::perf_event_open,
            Sysno::keyctl,
            Sysno::add_key,
            Sysno::request_key,
        ] {
            assert!(
                detcore::is_deterministically_refused_syscall(sysno),
                "{sysno:?} should be in Detcore's deterministic-refusal boundary"
            );
        }

        // Strict: refused families and Unsupported syscalls fail-close (1);
        // ordinary passthrough syscalls continue natively (0).
        COPIED_PANIC_ON_UNSUPPORTED.store(true, Ordering::Release);
        for sysnum in [
            libc::SYS_splice,
            libc::SYS_tee,
            libc::SYS_vmsplice,
            libc::SYS_perf_event_open,
            libc::SYS_keyctl,
            libc::SYS_add_key,
            libc::SYS_request_key,
            libc::SYS_ioctl,
            libc::SYS_recvmsg,
            libc::SYS_recvmmsg,
        ] {
            assert_eq!(
                reverie_dbi_runtime_copied_syscall(sysnum),
                1,
                "strict copied child must refuse syscall {sysnum}"
            );
        }
        for sysnum in [libc::SYS_read, libc::SYS_write, libc::SYS_getpid] {
            assert_eq!(
                reverie_dbi_runtime_copied_syscall(sysnum),
                0,
                "strict copied child must allow ordinary syscall {sysnum}"
            );
        }

        // Non-strict: strict-only compatibility families continue natively,
        // while unconditional fixed-error families still fail closed because
        // the copied-child ABI cannot inject their deterministic errno.
        COPIED_PANIC_ON_UNSUPPORTED.store(false, Ordering::Release);
        for sysnum in [
            libc::SYS_splice,
            libc::SYS_keyctl,
            libc::SYS_ioctl,
            libc::SYS_recvmsg,
            libc::SYS_recvmmsg,
            libc::SYS_read,
        ] {
            assert_eq!(
                reverie_dbi_runtime_copied_syscall(sysnum),
                0,
                "non-strict copied child must allow syscall {sysnum}"
            );
        }
        for sysnum in [
            libc::SYS_perf_event_open,
            libc::SYS_openat2,
            libc::SYS_io_uring_setup,
        ] {
            assert_eq!(
                reverie_dbi_runtime_copied_syscall(sysnum),
                1,
                "unconditional refusal must fail closed for syscall {sysnum}"
            );
        }

        COPIED_PANIC_ON_UNSUPPORTED.store(previous, Ordering::Release);
    }
}
