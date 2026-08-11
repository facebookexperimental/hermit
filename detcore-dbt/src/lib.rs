/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// AUTONOMOUS-BOT-IMPLEMENTED

//! DynamoRIO callback runtime that executes the real Detcore [`Tool`] over
//! [`reverie_dbt::DbtGuest`].

#![deny(missing_docs)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::fs;
use std::future::Future;
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;
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
use detcore::DetTid;
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
use reverie_dbt::DbtGuest;
use reverie_dbt::DbtSyscallOutcome;
use reverie_dbt::MemoryReader;
use reverie_dbt::MemoryWriter;
use reverie_dbt::RegisterReader;
use reverie_dbt::RegisterWriter;
use reverie_dbt::SyscallInvoker;
use tracing::Event;
use tracing::Metadata;
use tracing::Subscriber;
use tracing::field::Field;
use tracing::field::Visit;
use tracing::span;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
const MAX_OBSERVED_BUFFER: usize = 1024 * 1024;
const RANDOM_FILL_CHUNK_BYTES: usize = 4096;
const GETRANDOM_MAX_BYTES: usize = (i32::MAX as usize) & !4095;
const GETRANDOM_ALLOWED_FLAGS: u32 = libc::GRND_NONBLOCK | libc::GRND_RANDOM | libc::GRND_INSECURE;

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-644): Review the inherited DBT report descriptor.
/// Fixed inherited descriptor receiving unsupported syscall records.
pub const UNSUPPORTED_SYSCALL_REPORT_FD: i32 = 199;

type DetcoreThreadState = <Detcore as Tool>::ThreadState;
type Emitter = reverie_dbt::RuntimeEmitter;
type Idler = reverie_dbt::RuntimeIdler;

static DBT_TRACING_ACTIVE: AtomicBool = AtomicBool::new(false);
static NEXT_SPAN_ID: AtomicU64 = AtomicU64::new(1);

struct DbtSubscriber {
    emit: Emitter,
    level: DbtLogLevel,
}

impl Subscriber for DbtSubscriber {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.level.enables(metadata.level())
    }

    fn new_span(&self, _span: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(NEXT_SPAN_ID.fetch_add(1, Ordering::Relaxed))
    }

    fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}

    fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

    fn event(&self, event: &Event<'_>) {
        let metadata = event.metadata();
        let mut visitor = DbtEventVisitor::default();
        event.record(&mut visitor);
        let line = format!(
            "{} {}: {}\n",
            metadata.level(),
            metadata.target(),
            visitor.fields
        );
        unsafe { (self.emit)(line.as_ptr(), line.len()) };
    }

    fn enter(&self, _span: &span::Id) {}

    fn exit(&self, _span: &span::Id) {}
}

#[derive(Default)]
struct DbtEventVisitor {
    fields: String,
}

impl DbtEventVisitor {
    fn push(&mut self, field: &Field, value: String) {
        if !self.fields.is_empty() {
            self.fields.push(' ');
        }
        if field.name() != "message" {
            self.fields.push_str(field.name());
            self.fields.push('=');
        }
        self.fields.push_str(&value);
    }
}

impl Visit for DbtEventVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.push(field, format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.push(field, value.to_owned());
    }
}

#[derive(Clone, Copy)]
enum DbtLogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl DbtLogLevel {
    fn enables(self, level: &tracing::Level) -> bool {
        match self {
            Self::Error => *level == tracing::Level::ERROR,
            Self::Warn => matches!(*level, tracing::Level::ERROR | tracing::Level::WARN),
            Self::Info => !matches!(*level, tracing::Level::DEBUG | tracing::Level::TRACE),
            Self::Debug => *level != tracing::Level::TRACE,
            Self::Trace => true,
        }
    }
}

fn emit_marker(emit: Emitter, message: &'static [u8]) {
    unsafe { emit(message.as_ptr(), message.len()) };
}

/// Emit a routine per-run lifecycle breadcrumb (`detcore-dbt: …`).
///
/// These progress markers narrate DBT backend startup and are useful when
/// debugging the runtime, but they are noise for a normal `hermit run --backend
/// dbt`. Gate them behind `HERMIT_LOG=info` (or `debug`/`trace`) so a default
/// run is quiet. Genuine warnings and unsupported-syscall diagnostics do not go
/// through this helper and stay unconditional. The decision is read once and
/// cached, so hot callers pay only an atomic load.
fn emit_lifecycle_marker(emit: Emitter, message: &'static [u8]) {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    if *ENABLED.get_or_init(info_logging_enabled) {
        emit_marker(emit, message);
    }
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

fn dbt_log_level() -> Option<DbtLogLevel> {
    match std::env::var("HERMIT_LOG")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "error" => Some(DbtLogLevel::Error),
        "warn" => Some(DbtLogLevel::Warn),
        "info" => Some(DbtLogLevel::Info),
        "debug" => Some(DbtLogLevel::Debug),
        "trace" => Some(DbtLogLevel::Trace),
        _ => None,
    }
}

fn init_dbt_tracing(emit: Emitter) -> bool {
    if DBT_TRACING_ACTIVE.load(Ordering::Acquire) {
        return true;
    }
    let Some(level) = dbt_log_level() else {
        return false;
    };
    if tracing::subscriber::set_global_default(DbtSubscriber { emit, level }).is_err() {
        return false;
    }
    DBT_TRACING_ACTIVE.store(true, Ordering::Release);
    true
}

/// Environment variable through which `hermit run --backend dbt` hands the
/// CLI-derived Detcore [`Config`] (JSON) to this in-guest runtime.
///
/// The guest process inherits it from `drrun` (see the DBT launcher), so it is
/// the cross-process channel that lets flags like `--strict`, `--seed`, and the
/// time/CPUID virtualization switches reach the DBT Detcore Tool the same way
/// they reach the ptrace backend.
pub const DETCONFIG_ENV: &str = "HERMIT_DBT_DETCONFIG";

/// Where the effective Detcore [`Config`] came from, for native diagnostics.
enum ConfigSource {
    /// Deserialized from [`DETCONFIG_ENV`] provided by `hermit run`.
    Cli,
    /// [`DETCONFIG_ENV`] was set but could not be parsed; strict default used.
    ParseFallback,
    /// [`DETCONFIG_ENV`] was absent (e.g. a bare `drrun -c client.so` run).
    Default,
}

/// A strict, deterministic default configuration for standalone DBT runs.
fn default_dbt_config() -> Config {
    Config {
        sequentialize_threads: true,
        deterministic_io: true,
        max_timeslice: None,
        ..Config::default()
    }
}

/// Builds the Detcore [`Config`] for this DBT runtime.
///
/// The configuration is taken from the CLI-derived Detcore config serialized
/// into [`DETCONFIG_ENV`] when present; otherwise a strict default is used.
/// Regardless of the source, the DBT execution-model invariants are re-asserted:
/// the backend drives the Detcore global scheduler externally on a branch count
/// rather than PMU retired-conditional-branch preemption, so timeslice
/// preemption (`max_timeslice`) is disabled and threads stay sequentialized for
/// the single external scheduler.
fn load_dbt_config() -> (Config, ConfigSource) {
    let (mut config, source) = match std::env::var(DETCONFIG_ENV) {
        Ok(value) if !value.is_empty() => match serde_json::from_str::<Config>(&value) {
            Ok(config) => (config, ConfigSource::Cli),
            Err(_) => (default_dbt_config(), ConfigSource::ParseFallback),
        },
        _ => (default_dbt_config(), ConfigSource::Default),
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

// TODO-HUMAN-REVIEW(PR-1038): Review DBT self-target queued-signal identity translation.
// TODO-HUMAN-REVIEW(PR-1065): Review DBT self-target prlimit64 translation.
fn translate_self_identity_targets(
    sysnum: i64,
    args: &mut [u64; 6],
    virtual_pid: i32,
    virtual_tid: i32,
    host_pid: i32,
    host_tid: i32,
) {
    if virtual_pid <= 0 || host_pid <= 0 {
        return;
    }
    // AUTONOMOUS-BOT-IMPLEMENTED
    if sysnum == libc::SYS_prlimit64 && args[0] as i32 == virtual_pid {
        args[0] = host_pid as u32 as u64;
    }
    if virtual_tid <= 0 || host_tid <= 0 {
        return;
    }
    // AUTONOMOUS-BOT-IMPLEMENTED
    if sysnum == libc::SYS_rt_tgsigqueueinfo
        && args[0] as i32 == virtual_pid
        && args[1] as i32 == virtual_tid
    {
        args[0] = host_pid as u32 as u64;
        args[1] = host_tid as u32 as u64;
    }
    // AUTONOMOUS-BOT-IMPLEMENTED
    if sysnum == libc::SYS_rt_sigqueueinfo && args[0] as i32 == virtual_pid {
        args[0] = host_pid as u32 as u64;
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
    next_child_ordinal: AtomicU64,
}

struct ThreadRuntime {
    tid: Pid,
    state: DetcoreThreadState,
    initialized: bool,
    post_exec_pending: bool,
}

struct PendingThreadParent {
    parent_tid: Tid,
    rng_entropy: u128,
    state: DetcoreThreadState,
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
static PENDING_THREAD_PARENTS: LazyLock<Mutex<HashMap<i32, PendingThreadParent>>> =
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
            .expect("Detcore DBT runtime lock poisoned")
            .as_ref()
            .expect("Detcore DBT runtime was not initialized"),
    )
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1060): Review the stable DBT child RNG identity encoding.
fn dbt_child_rng_entropy(virtual_pid: i32, child_ordinal: u64) -> Option<u128> {
    if virtual_pid <= 0 || child_ordinal == 0 {
        return None;
    }
    Some(((virtual_pid as u32 as u128) << 64) | u128::from(child_ordinal))
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1060): Review preservation of physical DBT child TIDs.
fn dbt_scheduler_tid(host_tid: i32) -> Option<Tid> {
    (host_tid > 0).then(|| Tid::from_raw(host_tid))
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

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1065): Review fault-safe DBT prlimit64 input validation.
fn prlimit_new_limit_is_readable(
    sysnum: i64,
    args: &[u64],
    mut read: impl FnMut(usize, &mut [u8]) -> bool,
) -> bool {
    if sysnum != libc::SYS_prlimit64 || args[2] == 0 {
        return true;
    }
    let mut limit = [0_u8; std::mem::size_of::<libc::rlimit64>()];
    read(args[2] as usize, &mut limit)
}

// TODO-HUMAN-REVIEW(PR-1079): Review fault-safe DBT multiplexed-IO input validation.
fn multiplexed_io_inputs_are_readable(
    sysnum: i64,
    args: &[u64],
    mut read: impl FnMut(usize, &mut [u8]) -> bool,
) -> bool {
    // AUTONOMOUS-BOT-IMPLEMENTED
    if sysnum == libc::SYS_ppoll {
        if args[2] == 0 {
            return true;
        }
        let mut timeout = [0_u8; std::mem::size_of::<libc::timespec>()];
        return read(args[2] as usize, &mut timeout);
    }
    // AUTONOMOUS-BOT-IMPLEMENTED
    if sysnum != libc::SYS_pselect6 {
        return true;
    }

    let nfds = args[0] as i64;
    if nfds < 0 {
        return true;
    }
    if args[4] != 0 {
        let mut timeout = [0_u8; std::mem::size_of::<libc::timespec>()];
        if !read(args[4] as usize, &mut timeout) {
            return false;
        }
    }

    const INTERNAL_MAX_NFDS: i64 = (std::mem::size_of::<libc::c_ulong>() * 8) as i64;
    if nfds > INTERNAL_MAX_NFDS {
        return true;
    }
    if nfds > 0 {
        let mut fd_set = [0_u8; std::mem::size_of::<libc::c_ulong>()];
        for address in &args[1..=3] {
            if *address != 0 && !read(*address as usize, &mut fd_set) {
                return false;
            }
        }
    }
    if args[5] != 0 {
        let mut sigmask_argument = [0_u8; 2 * std::mem::size_of::<usize>()];
        if !read(args[5] as usize, &mut sigmask_argument) {
            return false;
        }
    }
    true
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

/// Returns the Detcore DBT cdylib built beside the running Hermit binary or in Cargo's deps directory.
// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-738): Review native-client linkage to the minimal DBT runtime.
pub fn runtime_library_path() -> io::Result<PathBuf> {
    if let Some(runtime) = hermit_resources::resource("libdetcore_dbt.so")?
        && runtime.is_file()
    {
        return Ok(runtime);
    }

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
                    "Hermit DBT runtime was not built beside {} or in its deps directory",
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
        directory.join("deps/libdetcore_dbt.so"),
        directory.join("libdetcore_dbt.so"),
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

fn native_client_source_path_hash(source: &Path) -> u64 {
    source
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .fold(FNV_OFFSET, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME)
        })
}

fn native_client_build_directory(runtime: &Path, source: &Path) -> PathBuf {
    let source_identity = source
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .unwrap_or_else(|| std::ffi::OsStr::new("source"));
    runtime
        .parent()
        .expect("runtime library path must have a parent")
        .join(format!(
            "detcore-dbt-native-{}-{:016x}",
            source_identity.to_string_lossy(),
            native_client_source_path_hash(source)
        ))
}

/// Builds the DynamoRIO native client against the Detcore runtime if needed.
// TODO-HUMAN-REVIEW(PR-1002): Review packaged DBT runtime and client discovery.
pub fn prepare_native_client() -> io::Result<(PathBuf, PathBuf)> {
    if let Some(install_dir) = hermit_resources::install_dir()? {
        let resources = install_dir.join("rsrcs");
        let drrun = resources.join("dynamorio/bin64/drrun");
        let client = resources.join("libreverie_dbt_client.so");
        if drrun.is_file() && client.is_file() {
            return Ok((drrun, client));
        }
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "Hermit installation {} is missing its packaged DynamoRIO launcher or DBT client",
                install_dir.display()
            ),
        ));
    }

    let runtime = runtime_library_path()?;
    let source = fs::canonicalize(reverie_dbt::native_client_source_dir())?;
    let directory = native_client_build_directory(&runtime, &source);
    fs::create_dir_all(&directory)?;
    let _build_lock = lock_native_client_build(&directory)?;

    let configure = Command::new("cmake")
        .arg("-S")
        .arg(&source)
        .arg("-B")
        .arg(&directory)
        .arg("-DCMAKE_BUILD_TYPE=Release")
        .arg(format!(
            "-DDynamoRIO_DIR={}",
            reverie_dbt::bundled_dynamorio_cmake_dir().display()
        ))
        .arg(format!("-DREVERIE_DBT_RUNTIME={}", runtime.display()))
        .output()?;
    if !configure.status.success() {
        return Err(io::Error::other(format!(
            "failed to configure Detcore DBT client: {}",
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
            "failed to build Detcore DBT client: {}",
            String::from_utf8_lossy(&build.stderr)
        )));
    }

    let client = directory.join("libreverie_dbt_client.so");
    if !client.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Detcore DBT client was not built at {}", client.display()),
        ));
    }
    Ok((reverie_dbt::bundled_drrun_path().to_path_buf(), client))
}

/// Begins a new DynamoRIO application image and returns its generation.
#[unsafe(no_mangle)]
pub extern "C" fn reverie_dbt_runtime_image_init() -> u64 {
    IMAGE_GENERATION.fetch_add(1, Ordering::SeqCst) + 1
}

/// Runs Detcore's async global scheduler on a DynamoRIO-managed client thread.
///
/// The native client starts this entry point before registering guest events
/// and waits for [`reverie_dbt_runtime_ready`] before allowing callbacks.
///
/// # Safety
///
/// `argument` must point to a valid [`reverie_dbt::DbtRuntimeCallbacks`] value.
#[unsafe(no_mangle)]
// TODO-HUMAN-REVIEW(PR-587): Confirm external scheduler callback and restart semantics.
pub unsafe extern "C" fn reverie_dbt_runtime_background_init(argument: *mut c_void) {
    let image_generation = IMAGE_GENERATION.load(Ordering::SeqCst);
    let callbacks = unsafe { &*argument.cast::<reverie_dbt::DbtRuntimeCallbacks>() };
    let emit = callbacks.emit;
    RUNTIME_SHUTDOWN.store(false, Ordering::Release);
    RUNTIME_PAUSE_REQUESTED.store(false, Ordering::Release);
    RUNTIME_PAUSED.store(false, Ordering::Release);
    emit_lifecycle_marker(emit, b"detcore-dbt: background client thread entered\n");
    let tracing_active = init_dbt_tracing(emit);
    let runtime = {
        let mut slot = RUNTIME.write().expect("Detcore DBT runtime lock poisoned");
        if slot.is_none() {
            emit_lifecycle_marker(emit, b"detcore-dbt: constructing Detcore Config\n");
            let (mut config, source) = load_dbt_config();
            match source {
                ConfigSource::Cli => {
                    emit_lifecycle_marker(emit, b"detcore-dbt: using CLI-provided Detcore Config\n")
                }
                ConfigSource::ParseFallback => emit_marker(
                    emit,
                    b"detcore-dbt: WARNING could not parse HERMIT_DBT_DETCONFIG; using strict default\n",
                ),
                ConfigSource::Default => {
                    emit_lifecycle_marker(emit, b"detcore-dbt: using strict default Detcore Config\n")
                }
            }
            // Fail-closed unsupported-syscall handling (PR #644): the rest of the
            // Config arrives via the CLI env above, but the panic flag comes from
            // the DBT callback (the `-panic-on-unsupported-syscalls` client
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
            // The DBT backend reports and aborts through the exit path plus the
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

            emit_lifecycle_marker(emit, b"detcore-dbt: initializing Detcore GlobalState\n");
            let global = GlobalState::init_for_external_scheduler(&config);
            emit_lifecycle_marker(emit, b"detcore-dbt: GlobalState initialized\n");
            *slot = Some(Arc::new(Runtime {
                config,
                global,
                tool: OnceLock::new(),
                next_child_ordinal: AtomicU64::new(1),
            }));
        }
        Arc::clone(slot.as_ref().expect("Detcore DBT runtime was initialized"))
    };
    emit_lifecycle_marker(emit, b"detcore-dbt: background scheduler ready\n");
    READY_IMAGE.store(image_generation, Ordering::SeqCst);
    let log_scheduler = info_logging_enabled() && !tracing_active;
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
    emit_lifecycle_marker(emit, b"detcore-dbt: background scheduler completed\n");
}

/// Requests shutdown of the backend-owned scheduler at process exit.
#[unsafe(no_mangle)]
// TODO-HUMAN-REVIEW(PR-587): Confirm process-exit scheduler ownership.
pub extern "C" fn reverie_dbt_runtime_process_exit() {
    READY_IMAGE.store(0, Ordering::Release);
    RUNTIME_SHUTDOWN.store(true, Ordering::Release);
}

/// Reports whether the Detcore global scheduler is ready for this image.
#[unsafe(no_mangle)]
// TODO-HUMAN-REVIEW(PR-587): Confirm image-generation readiness ordering.
pub extern "C" fn reverie_dbt_runtime_ready(image_generation: u64) -> i32 {
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
// TODO-HUMAN-REVIEW(PR-874): Review compatibility with Reverie's expanded DBT callback ABI.
// TODO-HUMAN-REVIEW(PR-1060): Review separation of host thread identity from stable RNG entropy.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn reverie_dbt_runtime_thread_init(
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
    if defer_runtime != 0 {
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
        return 0;
    }
    let scratch = unsafe { &mut *scratch.cast::<NativeThreadScratch>() };

    let host_tid = tid;
    let host_pid = pid;
    let runtime = current_runtime();
    let tool = runtime
        .tool
        .get_or_init(|| Detcore::new(Pid::from_raw(host_pid), &runtime.config));
    let parent = if host_tid == host_pid {
        None
    } else {
        let parent = PENDING_THREAD_PARENTS
            .lock()
            .expect("pending DBT thread parent lock poisoned")
            .remove(&host_tid);
        let Some(parent) = parent else {
            return 1;
        };
        Some(parent)
    };
    let Some(det_tid) = dbt_scheduler_tid(host_tid) else {
        return -1;
    };
    let parent_ref = parent
        .as_ref()
        .map(|parent| (parent.parent_tid, &parent.state));
    let det_pid = Pid::from_raw(det_tid.into());
    let host_pid = Pid::from_raw(host_pid);
    let mut state = tool.init_thread_state(det_tid, parent_ref);
    if let Some(parent) = &parent {
        state.reseed_child_rngs(&parent.state, parent.rng_entropy);
    }
    let mut thread = Box::new(ThreadRuntime {
        tid: det_pid,
        state,
        initialized: false,
        post_exec_pending: host_tid == pid,
    });
    if reverie_dbt::run_tool_thread_start(
        tool,
        context as usize,
        det_pid,
        host_pid,
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
    scratch.runtime_state = Box::into_raw(thread);
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
// TODO-HUMAN-REVIEW(PR-1060): Review deterministic child RNG identity allocation.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn reverie_dbt_runtime_thread_created(
    scratch: *mut c_void,
    context: *mut c_void,
    _parent_tid: i32,
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
        .expect("Detcore DBT tool was initialized");
    let virtual_pid = scratch.virtual_pid;
    let parent = unsafe { &mut *scratch.runtime_state };
    let flags = CloneFlags::from_bits_truncate(flags);
    parent.state.clone_flags = Some(flags);
    let parent_snapshot = parent.state.clone();
    let child_ordinal = runtime.next_child_ordinal.fetch_add(1, Ordering::SeqCst);
    let Some(rng_entropy) = dbt_child_rng_entropy(virtual_pid, child_ordinal) else {
        parent.state.clone_flags = None;
        return -1;
    };
    let Some(child_scheduler_tid) = dbt_scheduler_tid(child_tid) else {
        parent.state.clone_flags = None;
        return -1;
    };
    if PENDING_THREAD_PARENTS
        .lock()
        .expect("pending DBT thread parent lock poisoned")
        .insert(
            child_tid,
            PendingThreadParent {
                parent_tid: Tid::from_raw(parent.tid.into()),
                rng_entropy,
                state: parent_snapshot,
            },
        )
        .is_some()
    {
        parent.state.clone_flags = None;
        return -1;
    }

    {
        let mut guest = DbtGuest::new(
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
            child_scheduler_tid,
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
/// [`reverie_dbt_runtime_thread_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reverie_dbt_runtime_thread_exit(scratch: *mut c_void) {
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
            .expect("Detcore DBT tool was initialized");
        let _ = reverie_dbt::run_tool_thread_exit(
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
/// `_scratch` must be the pointer supplied by the native DBT callback. It is not
/// dereferenced because a failed exec preserves the current Detcore thread state.
#[unsafe(no_mangle)]
// TODO-HUMAN-REVIEW(PR-587): Confirm failed-exec preserves Runtime and thread state.
pub unsafe extern "C" fn reverie_dbt_runtime_exec_failed(_scratch: *mut c_void, _pid: i32) {
    assert!(
        RUNTIME
            .read()
            .expect("Detcore DBT runtime lock poisoned")
            .is_some(),
        "failed exec had no Detcore runtime"
    );
    resume_paused_runtime();
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-644): Review fork-safe policy enforcement before copied children bypass.
// TODO-HUMAN-REVIEW(PR-978): Review extending the copied-child gate from the
// Unsupported set to the full deterministic-refusal boundary.
/// Applies the deterministic-refusal policy in a copied pre-exec DBT child.
///
/// A copied pre-exec child runs natively on the DynamoRIO client stack with no
/// Detcore tool, so every syscall it makes bypasses `handle_syscall_event`.
/// Returning 0 lets the syscall run natively; returning 1 fail-closes by
/// aborting the runtime tree. A negative return value injects that deterministic
/// errno without executing the syscall. Syscalls that need guest-memory access
/// still have to fail closed because this ABI exposes arguments but no memory
/// reader or writer.
///
/// The gate covers the classic Unsupported set plus the broader fixed-error
/// boundary. Unconditional deterministic refusals fail closed in every mode;
/// compatibility families that the root process refuses only under strict
/// execution (`rseq`, zero-copy pipes, keyrings) retain native non-strict
/// behavior. Before PR-978 both groups could execute natively in a copied child
/// despite the root Detcore policy.
///
/// # Safety
///
/// `args` must be null or point to the DBT client's live six-element syscall
/// argument array for the duration of this call.
// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1061): Review copied-child ioctl errno emulation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reverie_dbt_runtime_copied_syscall(sysnum: i64, args: *const u64) -> i32 {
    let sysno = Sysno::from(sysnum as i32);
    let strict = COPIED_PANIC_ON_UNSUPPORTED.load(Ordering::Acquire);
    // Bash probes the foreground process group in a copied child before
    // running a background shell function. Hermit's captured stderr is not a
    // terminal, so the instrumented root observes ENOTTY for the same request.
    // Emulate that result instead of either exposing a host terminal's process
    // group or aborting an otherwise deterministic child.
    //
    // Every other ioctl remains fail-closed: copied children still cannot enter
    // the Rust Detcore Tool, and the ABI has no guest-memory channel for safely
    // handling socket timestamps or arbitrary device operations.
    if sysno == Sysno::ioctl && strict {
        let request = if args.is_null() {
            None
        } else {
            // SAFETY: Reverie's DBT client passes its live six-element syscall
            // argument array for the duration of this callback.
            Some(unsafe { args.add(1).read() })
        };
        if request == Some(libc::TIOCGPGRP) {
            return -libc::ENOTTY;
        }
        return 1;
    }
    // TODO-HUMAN-REVIEW(PR-981): Copied DBT children cannot enter the Rust
    // Detcore Tool. Strict mode therefore fails closed for receive syscalls that
    // can expose native socket timestamps. Non-strict mode retains native
    // behavior.
    // TODO-HUMAN-REVIEW(PR-972): readlink identity canonicalization also requires
    // Detcore mediation. This ABI has neither syscall arguments nor a memory
    // writer, so a copied child must fail closed rather than expose native
    // pipe/socket inode identities.
    if matches!(
        sysno,
        Sysno::recvmsg | Sysno::recvmmsg | Sysno::readlink | Sysno::readlinkat
    ) && strict
    {
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

// TODO-HUMAN-REVIEW(PR-874): Review deferred DBT syscall encoding.
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
// TODO-HUMAN-REVIEW(PR-1060): Review host child DetTid syscall dispatch.
// TODO-HUMAN-REVIEW(PR-1118): Review fault-safe DBT getrandom memory writes.
pub unsafe extern "C" fn reverie_dbt_runtime_pre_syscall(
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
    write_memory: MemoryWriter,
    emit: unsafe extern "C" fn(*const u8, usize),
) -> i32 {
    let first_event = TOTAL_SYSCALLS.fetch_add(1, Ordering::Relaxed) == 0;
    if first_event {
        emit_lifecycle_marker(emit, b"detcore-dbt: entered Rust syscall callback\n");
    }
    let raw_args = unsafe { std::slice::from_raw_parts(args, 6) };
    let mut dispatch_args: [u64; 6] = raw_args.try_into().expect("six syscall arguments");
    let scratch = unsafe { &mut *scratch.cast::<NativeThreadScratch>() };
    translate_self_identity_targets(
        sysnum,
        &mut dispatch_args,
        scratch.virtual_pid,
        scratch.virtual_tid,
        pid,
        tid,
    );
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1065): Review fault-safe DBT prlimit64 input validation.
    if !prlimit_new_limit_is_readable(sysnum, raw_args, |address, bytes| unsafe {
        read_memory(address, bytes.as_mut_ptr(), bytes.len()) != 0
    }) {
        unsafe { result.write(-(Errno::EFAULT.into_raw() as i64)) };
        TOTAL_REWRITTEN.fetch_add(1, Ordering::Relaxed);
        return 1;
    }
    if !multiplexed_io_inputs_are_readable(sysnum, raw_args, |address, bytes| unsafe {
        read_memory(address, bytes.as_mut_ptr(), bytes.len()) != 0
    }) {
        unsafe { result.write(-(Errno::EFAULT.into_raw() as i64)) };
        TOTAL_REWRITTEN.fetch_add(1, Ordering::Relaxed);
        return 1;
    }
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-849): Review fault-safe DBT getrandom writes.
    // Probe with deterministic zeros through DynamoRIO's fault-safe writer, then let Detcore
    // overwrite the entire writable prefix before the application resumes.
    let getrandom_probe = if sysnum == libc::SYS_getrandom {
        match getrandom_writable_prefix(raw_args, |remote, bytes| unsafe {
            Ok(write_memory(remote, bytes.as_ptr(), bytes.len()))
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
                "DBT image generation changed while pausing for exec"
            );
        }
        return 0;
    }
    TOTAL_BRANCHES.store(branches, Ordering::Relaxed);
    update_memory_hash(sysnum, raw_args, read_memory);
    let runtime = current_runtime();
    let tid = Pid::from_raw(tid);
    let pid = Pid::from_raw(pid);
    let tool = runtime
        .tool
        .get_or_init(|| Detcore::new(pid, &runtime.config));
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
        emit_lifecycle_marker(emit, b"detcore-dbt: initializing Detcore thread state\n");
    }
    if scratch.runtime_state.is_null() {
        if first_event {
            emit_lifecycle_marker(emit, b"detcore-dbt: constructing Detcore thread state\n");
        }
        let mut state = tool.init_thread_state(Tid::from_raw(tid.into()), None);
        if scratch.virtual_tid > 0 {
            state.set_open_file_creator(DetTid::from_raw(scratch.virtual_tid));
        }
        if first_event {
            emit_lifecycle_marker(emit, b"detcore-dbt: Detcore thread state constructed\n");
        }
        scratch.runtime_state = Box::into_raw(Box::new(ThreadRuntime {
            tid,
            state,
            initialized: false,
            post_exec_pending: true,
        }));
    }
    let thread = unsafe { &mut *scratch.runtime_state };
    let det_tid = thread.tid;
    if !thread.initialized {
        if first_event {
            emit_lifecycle_marker(emit, b"detcore-dbt: running Detcore thread-start hook\n");
        }
        if let Err(error) = reverie_dbt::run_tool_thread_start(
            tool,
            context as usize,
            det_tid,
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
            emit_lifecycle_marker(
                emit,
                b"detcore-dbt: thread-start hook completed; running post-exec\n",
            );
        }
        if let Err(errno) = reverie_dbt::run_tool_post_exec(
            tool,
            context as usize,
            det_tid,
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
            emit_lifecycle_marker(emit, b"detcore-dbt: post-exec hook completed\n");
        }
        thread.post_exec_pending = false;
    }

    if first_event {
        emit_lifecycle_marker(
            emit,
            b"detcore-dbt: dispatching first syscall through Detcore\n",
        );
    }
    let getrandom_prng = getrandom_probe
        .filter(|probe| probe.writable < probe.requested)
        .map(|probe| (probe, thread.state.prng.clone()));
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1057): Review production fault-safe DBT backtraces.
    // Preserve DynamoRIO's fault containment when Detcore asks DbtGuest for a
    // backtrace. Dropping this callback makes the adapter fall back to direct
    // self-process reads, which cannot distinguish a guest fault from a client
    // fault as reliably as dr_safe_read.
    let mut outcome = reverie_dbt::run_tool_syscall_with_memory_reader(
        tool,
        context as usize,
        det_tid,
        pid,
        branches,
        &mut thread.state,
        &runtime.global,
        &runtime.config,
        syscall,
        invoke_syscall,
        read_registers,
        write_registers,
        read_memory,
    );
    if let Some((probe, original_prng)) = getrandom_prng {
        // The shortened safe write must consume exactly the stream portion that the shared
        // Detcore handler consumes before its first guest-memory fault.
        thread.state.prng = original_prng;
        advance_getrandom_prng(&mut thread.state.prng, probe.consumed());
        if matches!(outcome, Ok(DbtSyscallOutcome::Suppress(_))) {
            let value = if probe.writable == 0 {
                -(Errno::EFAULT.into_raw() as i64)
            } else {
                probe.writable as i64
            };
            outcome = Ok(DbtSyscallOutcome::Suppress(value));
        }
    }
    match outcome {
        Ok(DbtSyscallOutcome::Suppress(value)) => {
            unsafe { result.write(value) };
            TOTAL_REWRITTEN.fetch_add(1, Ordering::Relaxed);
            1
        }
        Ok(DbtSyscallOutcome::ExecuteOriginal(syscall)) => {
            unsafe { write_deferred_syscall(syscall, deferred_sysnum, deferred_args) };
            2
        }
        Err(Error::Tool(error)) => {
            if let Some(unsupported) = error.downcast_ref::<UnsupportedSyscallError>() {
                let message = format!("detcore-dbt: {unsupported}\n");
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

/// Returns the linked Reverie Tool name for native DBT-path evidence.
#[unsafe(no_mangle)]
pub extern "C" fn reverie_dbt_runtime_name() -> *const libc::c_char {
    c"Detcore".as_ptr()
}

/// Returns Detcore DBT counters and the observed guest-memory hash.
///
/// # Safety
///
/// Every output pointer must be aligned and writable for one `u64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reverie_dbt_runtime_totals(
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
    fn child_rng_entropy_is_stable_and_partitioned() {
        let first = dbt_child_rng_entropy(3, 1).unwrap();
        let second = dbt_child_rng_entropy(3, 2).unwrap();
        let next_process = dbt_child_rng_entropy(4, 1).unwrap();

        assert_eq!(first, (3_u128 << 64) | 1);
        assert_ne!(first, second);
        assert_ne!(first, next_process);
    }

    #[test]
    fn child_rng_entropy_has_no_small_thread_lifetime_limit() {
        assert_eq!(dbt_child_rng_entropy(0, 1), None);
        assert_eq!(dbt_child_rng_entropy(3, 0), None);
        assert!(dbt_child_rng_entropy(3, 2_048).is_some());
        assert!(dbt_child_rng_entropy(3, u64::MAX).is_some());
        assert!(dbt_child_rng_entropy(i32::MAX, 1).is_some());
    }

    #[test]
    fn child_scheduler_identity_remains_the_host_tid() {
        let host_tid = 42_001;
        let scheduler_tid: i32 = dbt_scheduler_tid(host_tid).unwrap().into();
        assert_eq!(scheduler_tid, host_tid);
        assert_eq!(dbt_scheduler_tid(0), None);
        assert_eq!(dbt_scheduler_tid(-1), None);
    }

    #[test]
    fn self_identity_syscalls_use_host_identities() {
        let mut targeted = [3, 4, libc::SIGUSR1 as u64, 0, 0, 0];
        translate_self_identity_targets(
            libc::SYS_rt_tgsigqueueinfo,
            &mut targeted,
            3,
            4,
            10_003,
            10_004,
        );
        assert_eq!(targeted[..2], [10_003, 10_004]);

        let mut process = [3, libc::SIGUSR1 as u64, 0, 0, 0, 0];
        translate_self_identity_targets(
            libc::SYS_rt_sigqueueinfo,
            &mut process,
            3,
            4,
            10_003,
            10_004,
        );
        assert_eq!(process[0], 10_003);

        let mut other = [5, 6, libc::SIGUSR1 as u64, 0, 0, 0];
        translate_self_identity_targets(
            libc::SYS_rt_tgsigqueueinfo,
            &mut other,
            3,
            4,
            10_003,
            10_004,
        );
        assert_eq!(other[..2], [5, 6]);

        let mut process_group = [0, libc::SIGUSR1 as u64, 0, 0, 0, 0];
        translate_self_identity_targets(
            libc::SYS_rt_sigqueueinfo,
            &mut process_group,
            0,
            0,
            10_003,
            10_004,
        );
        assert_eq!(process_group[0], 0);

        let mut prlimit = [3, libc::RLIMIT_NOFILE as u64, 0, 0, 0, 0];
        translate_self_identity_targets(libc::SYS_prlimit64, &mut prlimit, 3, 4, 10_003, 10_004);
        assert_eq!(prlimit[0], 10_003);

        let mut prlimit_without_tid = [3, libc::RLIMIT_NOFILE as u64, 0, 0, 0, 0];
        translate_self_identity_targets(
            libc::SYS_prlimit64,
            &mut prlimit_without_tid,
            3,
            0,
            10_003,
            0,
        );
        assert_eq!(prlimit_without_tid[0], 10_003);

        let mut current = [0, libc::RLIMIT_NOFILE as u64, 0, 0, 0, 0];
        translate_self_identity_targets(libc::SYS_prlimit64, &mut current, 3, 4, 10_003, 10_004);
        assert_eq!(current[0], 0);

        let mut other_process = [5, libc::RLIMIT_NOFILE as u64, 0, 0, 0, 0];
        translate_self_identity_targets(
            libc::SYS_prlimit64,
            &mut other_process,
            3,
            4,
            10_003,
            10_004,
        );
        assert_eq!(other_process[0], 5);
    }

    #[test]
    fn prlimit_input_preflight_rejects_unreadable_non_null_limits() {
        let null_limit = [0, libc::RLIMIT_NOFILE as u64, 0, 0, 0, 0];
        assert!(prlimit_new_limit_is_readable(
            libc::SYS_prlimit64,
            &null_limit,
            |_, _| false,
        ));

        let limit = [0, libc::RLIMIT_NOFILE as u64, 1, 0, 0, 0];
        assert!(!prlimit_new_limit_is_readable(
            libc::SYS_prlimit64,
            &limit,
            |_, _| false,
        ));
        assert!(prlimit_new_limit_is_readable(
            libc::SYS_prlimit64,
            &limit,
            |address, bytes| address == 1 && bytes.len() == std::mem::size_of::<libc::rlimit64>(),
        ));

        assert!(prlimit_new_limit_is_readable(
            libc::SYS_getrlimit,
            &limit,
            |_, _| false,
        ));
    }

    #[test]
    fn multiplexed_io_input_preflight_rejects_unreadable_inputs() {
        for (sysnum, timeout_index) in [(libc::SYS_ppoll, 2_usize), (libc::SYS_pselect6, 4_usize)] {
            let mut args = [0; 6];
            assert!(multiplexed_io_inputs_are_readable(sysnum, &args, |_, _| {
                false
            }));

            args[timeout_index] = 1;
            assert!(!multiplexed_io_inputs_are_readable(
                sysnum,
                &args,
                |_, _| false
            ));
            assert!(multiplexed_io_inputs_are_readable(
                sysnum,
                &args,
                |address, bytes| {
                    address == 1 && bytes.len() == std::mem::size_of::<libc::timespec>()
                },
            ));
        }

        let pselect_sets = [1, 1, 0, 0, 0, 0];
        assert!(!multiplexed_io_inputs_are_readable(
            libc::SYS_pselect6,
            &pselect_sets,
            |_, _| false,
        ));
        assert!(multiplexed_io_inputs_are_readable(
            libc::SYS_pselect6,
            &pselect_sets,
            |address, bytes| {
                address == 1 && bytes.len() == std::mem::size_of::<libc::c_ulong>()
            },
        ));

        let ignored_negative_sets = [u64::MAX, 1, 1, 1, 1, 1];
        assert!(multiplexed_io_inputs_are_readable(
            libc::SYS_pselect6,
            &ignored_negative_sets,
            |_, _| false,
        ));

        assert!(multiplexed_io_inputs_are_readable(
            libc::SYS_read,
            &[0, 0, 1, 0, 0, 0],
            |_, _| false,
        ));
    }

    static COPIED_CHILD_POLICY_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn copied_child_action(sysnum: i64) -> i32 {
        copied_child_action_with_args(sysnum, [0; 6])
    }

    fn copied_child_action_with_args(sysnum: i64, args: [u64; 6]) -> i32 {
        // SAFETY: The callback reads the argument array only for the duration
        // of this call.
        unsafe { reverie_dbt_runtime_copied_syscall(sysnum, args.as_ptr()) }
    }

    #[test]
    fn native_client_links_only_the_dedicated_dbt_runtime() {
        let executable = std::path::Path::new("/workspace/target/debug/hermit");
        let [deps, direct] = runtime_library_candidates(executable).unwrap();
        assert_eq!(
            deps,
            std::path::Path::new("/workspace/target/debug/deps/libdetcore_dbt.so")
        );
        assert_eq!(
            direct,
            std::path::Path::new("/workspace/target/debug/libdetcore_dbt.so")
        );
    }

    struct NativeClientCacheTestDir(PathBuf);

    impl NativeClientCacheTestDir {
        fn new() -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time must follow the Unix epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "detcore-dbt-native-cache-key-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("create native-client cache-key test directory");
            Self(path)
        }
    }

    impl Drop for NativeClientCacheTestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_native_client_cache_test_project(source: &Path) {
        fs::create_dir_all(source).expect("create native-client CMake source directory");
        fs::write(
            source.join("CMakeLists.txt"),
            "cmake_minimum_required(VERSION 3.15)\nproject(native_client_cache_key NONE)\n",
        )
        .expect("write native-client CMake test project");
    }

    fn configure_and_build_native_client_cache_test_project(
        source: &Path,
        build: &Path,
    ) -> Result<(), String> {
        let configure = Command::new("cmake")
            .arg("-S")
            .arg(source)
            .arg("-B")
            .arg(build)
            .output()
            .map_err(|error| format!("failed to execute CMake configure: {error}"))?;
        if !configure.status.success() {
            return Err(format!(
                "CMake configure failed: {}",
                String::from_utf8_lossy(&configure.stderr)
            ));
        }
        let build_result = Command::new("cmake")
            .arg("--build")
            .arg(build)
            .output()
            .map_err(|error| format!("failed to execute CMake build: {error}"))?;
        if !build_result.status.success() {
            return Err(format!(
                "CMake build failed: {}",
                String::from_utf8_lossy(&build_result.stderr)
            ));
        }
        Ok(())
    }

    #[test]
    fn native_client_cache_misses_changed_source_path_and_hits_unchanged_path() {
        let temp = NativeClientCacheTestDir::new();
        let runtime = temp.0.join("target/debug/deps/libdetcore_dbt.so");
        let source_a = temp
            .0
            .join("cargo-a/git/checkouts/reverie/source-rev/reverie-dbt/native");
        let source_b = temp
            .0
            .join("cargo-b/git/checkouts/reverie/source-rev/reverie-dbt/native");
        write_native_client_cache_test_project(&source_a);
        write_native_client_cache_test_project(&source_b);

        let first_build = native_client_build_directory(&runtime, &source_a);
        configure_and_build_native_client_cache_test_project(&source_a, &first_build)
            .expect("first source path must configure and build");
        let cache_sentinel = first_build.join("same-source-cache-sentinel");
        fs::write(&cache_sentinel, b"cache remains reusable")
            .expect("write same-source cache sentinel");

        let repeated_build = native_client_build_directory(&runtime, &source_a);
        assert_eq!(repeated_build, first_build);
        configure_and_build_native_client_cache_test_project(&source_a, &repeated_build)
            .expect("unchanged source path must reuse its CMake cache");
        assert!(
            cache_sentinel.is_file(),
            "unchanged source path must hit the existing build directory"
        );

        let changed_build = native_client_build_directory(&runtime, &source_b);
        configure_and_build_native_client_cache_test_project(&source_b, &changed_build)
            .expect("changed source path must miss the old CMake cache and build cleanly");
        assert_ne!(
            changed_build, first_build,
            "changed source path must select a distinct CMake cache"
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

    /// Every `Determinized` syscall that this gate currently lets run natively
    /// in a copied child under strict execution.
    ///
    /// THIS LIST IS AN ACKNOWLEDGED-ESCAPE REGISTER, NOT AN APPROVAL. A copied
    /// pre-exec child runs no Detcore tool, so each of these executes against
    /// the host while the ptrace path would have determinized it. `ioprio_get`
    /// is the clearest live example: `handle_ioprio_get` returns a fixed
    /// host-independent priority on the ptrace path, and a copied child returns
    /// the real host I/O priority instead.
    ///
    /// It is pinned so the failure mode that produced this list cannot recur
    /// silently. Reclassifying a syscall from `Unsupported` to `Determinized`
    /// without giving the copied child a policy adds a row here and FAILS this
    /// test, forcing an explicit decision at the moment of reclassification
    /// rather than leaving a host escape to be discovered in review months
    /// later. That is the whole defect this test exists to prevent: the gate
    /// was correct for the classification table it was written against, and
    /// later table rows routed around it.
    ///
    /// Shrinking this list is the goal. Do not grow it without a stated reason.
    const ACKNOWLEDGED_STRICT_COPIED_CHILD_ESCAPES: &[&str] = &[
        "accept",
        "accept4",
        "adjtimex",
        "alarm",
        "arch_prctl",
        "bind",
        "clock_adjtime",
        "clock_getres",
        "clock_gettime",
        "clock_nanosleep",
        "clone",
        "clone3",
        "close",
        "close_range",
        "connect",
        "creat",
        "dup",
        "dup2",
        "dup3",
        "epoll_create",
        "epoll_create1",
        "epoll_ctl",
        "epoll_pwait",
        "epoll_pwait2",
        "epoll_wait",
        "epoll_wait_old",
        "eventfd",
        "eventfd2",
        "execve",
        "execveat",
        "exit",
        "exit_group",
        "fadvise64",
        "fcntl",
        "flock",
        "fork",
        "fstat",
        "fstatfs",
        "futex",
        "get_mempolicy",
        "getcpu",
        "getdents",
        "getdents64",
        "getegid",
        "geteuid",
        "getgid",
        "getitimer",
        "getpeername",
        "getpriority",
        "getrandom",
        "getresgid",
        "getresuid",
        "getrlimit",
        "getrusage",
        "getsockname",
        "getsockopt",
        "gettimeofday",
        "getuid",
        "inotify_add_watch",
        "inotify_init",
        "inotify_init1",
        "inotify_rm_watch",
        "ioprio_get",
        "ioprio_set",
        "kill",
        "listen",
        "lseek",
        "lstat",
        "madvise",
        "mbind",
        "membarrier",
        "memfd_create",
        "migrate_pages",
        "mincore",
        "mmap",
        "move_pages",
        "mremap",
        "munmap",
        "nanosleep",
        "newfstatat",
        "open",
        "openat",
        "pause",
        "pidfd_getfd",
        "pidfd_open",
        "pidfd_send_signal",
        "pipe",
        "pipe2",
        "poll",
        "ppoll",
        "prctl",
        "pread64",
        "preadv",
        "preadv2",
        "prlimit64",
        "process_madvise",
        "pselect6",
        "pwrite64",
        "pwritev",
        "pwritev2",
        "read",
        "readv",
        "recvfrom",
        "rt_sigaction",
        "rt_sigpending",
        "rt_sigprocmask",
        "rt_sigqueueinfo",
        "rt_sigsuspend",
        "rt_sigtimedwait",
        "rt_tgsigqueueinfo",
        "sched_getaffinity",
        "sched_getattr",
        "sched_getparam",
        "sched_getscheduler",
        "sched_rr_get_interval",
        "sched_setaffinity",
        "sched_setattr",
        "sched_setparam",
        "sched_setscheduler",
        "sched_yield",
        "seccomp",
        "select",
        "sendfile",
        "sendmmsg",
        "sendmsg",
        "sendto",
        "set_mempolicy",
        "set_mempolicy_home_node",
        "setfsgid",
        "setfsuid",
        "setgid",
        "setgroups",
        "setitimer",
        "setpriority",
        "setregid",
        "setresgid",
        "setresuid",
        "setreuid",
        "setrlimit",
        "setsid",
        "setsockopt",
        "setuid",
        "shutdown",
        "signalfd",
        "signalfd4",
        "socket",
        "socketpair",
        "stat",
        "statfs",
        "statx",
        "sysinfo",
        "syslog",
        "tgkill",
        "time",
        "timer_create",
        "timer_delete",
        "timer_getoverrun",
        "timer_gettime",
        "timer_settime",
        "timerfd_create",
        "timerfd_gettime",
        "timerfd_settime",
        "times",
        "tkill",
        "uname",
        "userfaultfd",
        "utime",
        "utimensat",
        "utimes",
        "vfork",
        "wait4",
        "waitid",
        "write",
        "writev",
    ];

    /// Disposition of the three reviewer findings this gate was opened for, so
    /// the positive side of the bracket is a test rather than a claim.
    ///
    /// `perf_event_open` (#876) and `remap_file_pages` (#882) reach the gate and
    /// are refused. `ioprio_get` / `ioprio_set` (#881) are NOT: they are
    /// Determinized by emulation rather than by refusal, and this ABI returns
    /// only native / fail-closed / errno, so it cannot carry the fixed priority
    /// `handle_ioprio_get` produces on the ptrace path. A copied child therefore
    /// still reports the host's real I/O priority. Pinned as a known divergence
    /// so it is visible rather than assumed fixed.
    #[test]
    fn copied_child_disposition_of_the_covered_reviewer_findings() {
        let _guard = COPIED_CHILD_POLICY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved = COPIED_PANIC_ON_UNSUPPORTED.load(Ordering::Acquire);
        COPIED_PANIC_ON_UNSUPPORTED.store(true, Ordering::Release);

        // Refused before reaching the host.
        for sysno in [Sysno::perf_event_open, Sysno::remap_file_pages] {
            assert!(detcore::is_determinized_syscall(sysno));
            assert_eq!(
                copied_child_action(sysno.id() as i64),
                1,
                "{sysno} must not reach the host from a strict copied child"
            );
        }

        // Still diverging: emulated on the ptrace path, native here.
        for sysno in [Sysno::ioprio_get, Sysno::ioprio_set] {
            assert!(detcore::is_determinized_syscall(sysno));
            assert!(!detcore::is_deterministically_refused_syscall(sysno));
            assert_eq!(
                copied_child_action(sysno.id() as i64),
                0,
                "{sysno} disposition changed; update this test and issue #1793"
            );
        }

        COPIED_PANIC_ON_UNSUPPORTED.store(saved, Ordering::Release);
    }

    /// Fails when a `Determinized` syscall gains a silent native escape in a
    /// strict copied child. See `ACKNOWLEDGED_STRICT_COPIED_CHILD_ESCAPES`.
    #[test]
    fn no_new_determinized_syscall_silently_escapes_the_copied_child() {
        let _guard = COPIED_CHILD_POLICY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved = COPIED_PANIC_ON_UNSUPPORTED.load(Ordering::Acquire);
        COPIED_PANIC_ON_UNSUPPORTED.store(true, Ordering::Release);

        let observed: Vec<String> = detcore::all_pinned_syscalls()
            .filter(|sysno| detcore::is_determinized_syscall(*sysno))
            .filter(|sysno| copied_child_action(sysno.id() as i64) == 0)
            .map(|sysno| sysno.to_string())
            .collect();

        COPIED_PANIC_ON_UNSUPPORTED.store(saved, Ordering::Release);

        let expected: Vec<String> = ACKNOWLEDGED_STRICT_COPIED_CHILD_ESCAPES
            .iter()
            .map(|name| (*name).to_string())
            .collect();
        let added: Vec<&String> = observed.iter().filter(|s| !expected.contains(s)).collect();
        let removed: Vec<&String> = expected.iter().filter(|s| !observed.contains(s)).collect();

        assert!(
            added.is_empty(),
            "these Determinized syscalls newly run NATIVELY in a strict copied child, \
             bypassing Detcore: {added:?}. Give each one a copied-child policy \
             (fixed errno or fail-closed), or add it to \
             ACKNOWLEDGED_STRICT_COPIED_CHILD_ESCAPES with a stated reason."
        );
        assert!(
            removed.is_empty(),
            "these syscalls no longer escape — remove them from \
             ACKNOWLEDGED_STRICT_COPIED_CHILD_ESCAPES so the register stays exact: {removed:?}"
        );
    }

    /// CENSUS (measurement, not a policy assertion): how many `Determinized`
    /// syscalls does the copied-child gate actually stop before the host?
    ///
    /// Printed as N-of-M so a coverage regression is visible as a number rather
    /// than as an absent test.
    #[test]
    fn census_determinized_syscalls_reaching_the_copied_child_gate() {
        let _guard = COPIED_CHILD_POLICY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved = COPIED_PANIC_ON_UNSUPPORTED.load(Ordering::Acquire);

        for strict in [true, false] {
            COPIED_PANIC_ON_UNSUPPORTED.store(strict, Ordering::Release);
            let mut determinized = 0usize;
            let mut stopped = 0usize;
            let mut escaping: Vec<String> = Vec::new();
            for sysno in detcore::all_pinned_syscalls() {
                if !detcore::is_determinized_syscall(sysno) {
                    continue;
                }
                determinized += 1;
                if copied_child_action(sysno.id() as i64) == 0 {
                    escaping.push(sysno.to_string());
                } else {
                    stopped += 1;
                }
            }
            println!(
                "copied-child strict={strict}: {stopped}/{determinized} Determinized syscalls \
                 stopped before the host; {} run natively",
                escaping.len()
            );
            println!("  escaping: {}", escaping.join(" "));
        }

        COPIED_PANIC_ON_UNSUPPORTED.store(saved, Ordering::Release);
    }

    // TODO-HUMAN-REVIEW(PR-916): Regression for the copied-DBT-child keyring
    // isolation boundary. A copied pre-exec child runs no Rust Detcore Tool, so
    // the gate must refuse the (now Determinized) keyring family in strict mode
    // rather than let it execute natively against the host keyring.
    #[test]
    fn copied_child_refuses_keyring_syscalls_under_strict() {
        let _guard = COPIED_CHILD_POLICY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved = COPIED_PANIC_ON_UNSUPPORTED.load(Ordering::Acquire);

        // Strict (panic-on-unsupported): keyring syscalls are refused so the
        // copied child cannot mutate host keyrings or trigger request-key
        // upcalls. `1` tells the native client to exit the isolated runtime
        // tree (fail closed), matching the pre-848 Unsupported behavior.
        COPIED_PANIC_ON_UNSUPPORTED.store(true, Ordering::Release);
        assert_eq!(copied_child_action(libc::SYS_keyctl), 1);
        assert_eq!(copied_child_action(libc::SYS_add_key), 1);
        assert_eq!(copied_child_action(libc::SYS_request_key), 1);
        // A supported syscall still runs natively even under strict mode.
        assert_eq!(copied_child_action(libc::SYS_getpid), 0);

        // Non-strict: keyring syscalls fall through to native pass-through,
        // matching the root process's non-strict keyring behavior.
        COPIED_PANIC_ON_UNSUPPORTED.store(false, Ordering::Release);
        assert_eq!(copied_child_action(libc::SYS_keyctl), 0);
        assert_eq!(copied_child_action(libc::SYS_add_key), 0);
        assert_eq!(copied_child_action(libc::SYS_request_key), 0);

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
            reverie_dbt_runtime_thread_init(
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
        let _guard = COPIED_CHILD_POLICY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
            libc::SYS_readlink,
            libc::SYS_readlinkat,
        ] {
            assert_eq!(
                copied_child_action(sysnum),
                1,
                "strict copied child must refuse syscall {sysnum}"
            );
        }
        let mut ioctl_args = [0; 6];
        ioctl_args[1] = libc::TIOCGPGRP;
        assert_eq!(
            copied_child_action_with_args(libc::SYS_ioctl, ioctl_args),
            -libc::ENOTTY,
            "TIOCGPGRP must receive the deterministic non-terminal result"
        );
        ioctl_args[1] = 0x8906; // SIOCGSTAMP_OLD
        assert_eq!(
            copied_child_action_with_args(libc::SYS_ioctl, ioctl_args),
            1,
            "socket timestamp ioctls must remain fail-closed"
        );
        // SAFETY: A null argument vector is an explicit fail-closed ABI test;
        // the callback checks it before dereferencing.
        assert_eq!(
            unsafe { reverie_dbt_runtime_copied_syscall(libc::SYS_ioctl, std::ptr::null()) },
            1,
            "missing ioctl arguments must fail closed"
        );
        for sysnum in [libc::SYS_read, libc::SYS_write, libc::SYS_getpid] {
            assert_eq!(
                copied_child_action(sysnum),
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
            libc::SYS_readlink,
            libc::SYS_readlinkat,
        ] {
            assert_eq!(
                copied_child_action(sysnum),
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
                copied_child_action(sysnum),
                1,
                "unconditional refusal must fail closed for syscall {sysnum}"
            );
        }

        COPIED_PANIC_ON_UNSUPPORTED.store(previous, Ordering::Release);
    }
}
