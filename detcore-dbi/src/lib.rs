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
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

use detcore::Config;
use detcore::Detcore;
use detcore::GlobalState;
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

type DetcoreThreadState = <Detcore as Tool>::ThreadState;
type Emitter = unsafe extern "C" fn(*const u8, usize);

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

fn run_cooperative<F: Future<Output = ()>>(future: F) {
    let mut future = pin!(future);
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(()) => return,
            Poll::Pending => std::hint::spin_loop(),
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

fn error_result(error: Error) -> i64 {
    match error {
        Error::Errno(errno) => -(errno.into_raw() as i64),
        _ => -(Errno::EIO.into_raw() as i64),
    }
}

/// Returns the release cdylib path produced beside the running Hermit binary.
pub fn runtime_library_path() -> io::Result<PathBuf> {
    let executable = std::env::current_exe()?;
    let directory = executable.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Hermit executable has no parent directory",
        )
    })?;
    let runtime = directory.join("libhermit.so");
    if runtime.is_file() {
        Ok(runtime)
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Hermit DBI runtime was not built at {}", runtime.display()),
        ))
    }
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
/// `argument` must encode a valid `Emitter` callback pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reverie_dbi_runtime_background_init(argument: *mut c_void) {
    let image_generation = IMAGE_GENERATION.load(Ordering::SeqCst);
    let emit: Emitter = unsafe { std::mem::transmute(argument) };
    emit_marker(emit, b"detcore-dbi: background client thread entered\n");
    let runtime = {
        let mut slot = RUNTIME.write().expect("Detcore DBI runtime lock poisoned");
        if slot.is_none() {
            emit_marker(emit, b"detcore-dbi: constructing Detcore Config\n");
            let mut config = Config {
                sequentialize_threads: true,
                deterministic_io: true,
                preemption_timeout: None,
                ..Config::default()
            };
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
    run_cooperative(runtime.global.run_external_scheduler(observer));
    emit_marker(emit, b"detcore-dbi: background scheduler completed\n");
}

/// Reports whether the Detcore global scheduler is ready for this image.
#[unsafe(no_mangle)]
pub extern "C" fn reverie_dbi_runtime_ready(image_generation: u64) -> i32 {
    i32::from(READY_IMAGE.load(Ordering::SeqCst) == image_generation)
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

/// Replaces terminal per-image state after the kernel rejects an exec.
///
/// # Safety
///
/// `scratch` must be the current thread's initialized native scratch pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reverie_dbi_runtime_exec_failed(scratch: *mut c_void, _pid: i32) {
    let scratch = unsafe { &mut *scratch.cast::<NativeThreadScratch>() };
    assert!(
        scratch.runtime_state.is_null(),
        "failed exec scratch still owns Detcore state"
    );
    let previous = RUNTIME
        .write()
        .expect("Detcore DBI runtime lock poisoned")
        .take();
    assert!(previous.is_some(), "failed exec had no Detcore runtime");
}

/// Dispatches one DynamoRIO syscall event through the real Detcore Tool.
///
/// # Safety
///
/// All pointers and callbacks must remain valid for this callback. `args` must
/// address six syscall arguments and `result` must be writable.
#[allow(clippy::too_many_arguments)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reverie_dbi_runtime_pre_syscall(
    context: *mut c_void,
    scratch: *mut c_void,
    tid: i32,
    pid: i32,
    _image_generation: u64,
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
    let is_exec = matches!(sysnum, libc::SYS_execve | libc::SYS_execveat);
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
        Ok(DbiSyscallOutcome::AllowOriginal) => {
            if is_exec {
                let ThreadRuntime {
                    tid,
                    state,
                    initialized,
                    ..
                } = *unsafe { Box::from_raw(scratch.runtime_state) };
                scratch.runtime_state = std::ptr::null_mut();
                if initialized {
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
            0
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
