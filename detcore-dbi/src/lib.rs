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
use reverie::Error;
use reverie::ExitStatus;
use reverie::Pid;
use reverie::Tid;
use reverie::Tool;
use reverie::syscalls::Errno;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::Sysno;
use reverie_dbi::DbiSyscallOutcome;
use reverie_dbi::MemoryReader;
use reverie_dbi::RegisterReader;
use reverie_dbi::SyscallInvoker;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
const MAX_OBSERVED_BUFFER: usize = 1024 * 1024;

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
fn requires_native_process_lifecycle(sysnum: i64, args: &[u64], clone3_flags: Option<u64>) -> bool {
    match sysnum {
        // AUTONOMOUS-BOT-IMPLEMENTED
        libc::SYS_fork | libc::SYS_vfork | libc::SYS_rt_sigreturn | libc::SYS_execve => true,
        // AUTONOMOUS-BOT-IMPLEMENTED
        libc::SYS_clone => args[0] & libc::CLONE_THREAD as u64 == 0,
        // AUTONOMOUS-BOT-IMPLEMENTED
        libc::SYS_clone3 => {
            clone3_flags.is_some_and(|flags| flags & libc::CLONE_THREAD as u64 == 0)
        }
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

#[repr(C)]
struct NativeThreadScratch {
    branches: u64,
    observed_syscalls: u64,
    rewritten_syscalls: u64,
    runtime_state: *mut ThreadRuntime,
}

static RUNTIME: LazyLock<RwLock<Option<Arc<Runtime>>>> = LazyLock::new(|| RwLock::new(None));
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

/// Initializes native per-thread scratch state. Detcore state is initialized
/// lazily when the callback provides the actual guest tid and pid.
///
/// # Safety
///
/// The native client must pass a valid writable scratch pointer or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reverie_dbi_runtime_thread_init(scratch: *mut c_void) {
    unsafe {
        scratch
            .cast::<NativeThreadScratch>()
            .write(NativeThreadScratch {
                branches: 0,
                observed_syscalls: 0,
                rewritten_syscalls: 0,
                runtime_state: std::ptr::null_mut(),
            });
    }
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
/// Applies unsupported-syscall policy in a copied pre-exec DBI child.
#[unsafe(no_mangle)]
pub extern "C" fn reverie_dbi_runtime_copied_syscall(sysnum: i64) -> i32 {
    if !detcore::is_unsupported_syscall(Sysno::from(sysnum as i32)) {
        return 0;
    }
    if COPIED_PANIC_ON_UNSUPPORTED.load(Ordering::Acquire) {
        1
    } else {
        append_copied_syscall_record(sysnum);
        0
    }
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
    invoke_syscall: SyscallInvoker,
    read_registers: RegisterReader,
    read_memory: MemoryReader,
    emit: unsafe extern "C" fn(*const u8, usize),
) -> i32 {
    let first_event = TOTAL_SYSCALLS.fetch_add(1, Ordering::Relaxed) == 0;
    if first_event {
        let message = b"detcore-dbi: entered Rust syscall callback\n";
        unsafe { emit(message.as_ptr(), message.len()) };
    }
    let raw_args = unsafe { std::slice::from_raw_parts(args, 6) };
    let clone3_flags = if sysnum == libc::SYS_clone3
        && raw_args[0] != 0
        && raw_args[1] >= std::mem::size_of::<u64>() as u64
    {
        let mut flags = 0_u64;
        let read = unsafe {
            read_memory(
                raw_args[0] as usize,
                (&mut flags as *mut u64).cast(),
                std::mem::size_of_val(&flags),
            )
        };
        (read != 0).then_some(flags)
    } else {
        None
    };
    if sysnum == libc::SYS_execveat {
        unsafe { result.write(-(Errno::ENOSYS.into_raw() as i64)) };
        TOTAL_REWRITTEN.fetch_add(1, Ordering::Relaxed);
        return 1;
    }
    if requires_native_process_lifecycle(sysnum, raw_args, clone3_flags) {
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
            raw_args[0] as usize,
            raw_args[1] as usize,
            raw_args[2] as usize,
            raw_args[3] as usize,
            raw_args[4] as usize,
            raw_args[5] as usize,
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
    let outcome = reverie_dbi::run_tool_syscall(
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
    );
    match outcome {
        Ok(DbiSyscallOutcome::Suppress(value)) => {
            unsafe { result.write(value) };
            TOTAL_REWRITTEN.fetch_add(1, Ordering::Relaxed);
            1
        }
        Ok(DbiSyscallOutcome::AllowOriginal) => 0,
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
    fn only_dynamorio_managed_process_lifecycle_stays_native() {
        let args = [0_u64; 6];
        for sysnum in [
            libc::SYS_fork,
            libc::SYS_vfork,
            libc::SYS_rt_sigreturn,
            libc::SYS_execve,
        ] {
            assert!(requires_native_process_lifecycle(sysnum, &args, None));
        }
        for sysnum in [
            libc::SYS_execveat,
            libc::SYS_wait4,
            libc::SYS_waitid,
            libc::SYS_read,
        ] {
            assert!(!requires_native_process_lifecycle(sysnum, &args, None));
        }
    }

    #[test]
    fn clone_classification_separates_processes_from_threads() {
        let mut args = [0_u64; 6];
        args[0] = libc::SIGCHLD as u64;
        assert!(requires_native_process_lifecycle(
            libc::SYS_clone,
            &args,
            None
        ));

        args[0] = libc::CLONE_THREAD as u64;
        assert!(!requires_native_process_lifecycle(
            libc::SYS_clone,
            &args,
            None
        ));

        assert!(requires_native_process_lifecycle(
            libc::SYS_clone3,
            &args,
            Some(libc::SIGCHLD as u64)
        ));
        assert!(!requires_native_process_lifecycle(
            libc::SYS_clone3,
            &args,
            Some(libc::CLONE_THREAD as u64)
        ));
        assert!(!requires_native_process_lifecycle(
            libc::SYS_clone3,
            &args,
            None
        ));
    }
}
