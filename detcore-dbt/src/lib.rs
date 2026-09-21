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
use detcore::prepare_exec;
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
const IMPLEMENTED_DBT_RUNTIME_ABI_VERSION: u32 = 4;
const IMPLEMENTED_DBT_RUNTIME_CALLBACKS_SIZE: usize = 48;

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-644): Review the inherited DBT report descriptor.
/// Fixed inherited descriptor receiving unsupported syscall records.
pub const UNSUPPORTED_SYSCALL_REPORT_FD: i32 = 199;

type DetcoreThreadState = <Detcore as Tool>::ThreadState;
type Emitter = reverie_dbt::RuntimeEmitter;
type Idler = reverie_dbt::RuntimeIdler;

#[repr(C)]
struct DbtRuntimeCallbacksV1 {
    emit: Emitter,
    idle: Idler,
    panic_on_unsupported_syscalls: i32,
    unsupported_report_fd: i32,
    emit_stdout: Emitter,
}

fn upgrade_runtime_callbacks_v1(
    callbacks: &DbtRuntimeCallbacksV1,
) -> reverie_dbt::DbtRuntimeCallbacks {
    reverie_dbt::DbtRuntimeCallbacks {
        emit: callbacks.emit,
        idle: callbacks.idle,
        panic_on_unsupported_syscalls: callbacks.panic_on_unsupported_syscalls,
        unsupported_report_fd: callbacks.unsupported_report_fd,
        emit_stdout: callbacks.emit_stdout,
        emit_evidence: callbacks.emit,
        evidence_log_level: 0,
    }
}

fn runtime_callback_channels(
    callbacks: &reverie_dbt::DbtRuntimeCallbacks,
) -> (Emitter, Emitter, i32) {
    (
        callbacks.emit,
        callbacks.emit_evidence,
        callbacks.evidence_log_level,
    )
}

static DBT_TRACING_ACTIVE: AtomicBool = AtomicBool::new(false);
static DBT_EVIDENCE_LOG_LEVEL: AtomicI32 = AtomicI32::new(0);
static DBT_DIAGNOSTIC_EMITTER: OnceLock<Emitter> = OnceLock::new();
static NEXT_SPAN_ID: AtomicU64 = AtomicU64::new(1);

struct DbtSubscriber {
    emit: Emitter,
    level: DbtLogLevel,
    canonical: bool,
}

const DBT_LOG_RECORD_PREFIX: &str = "1970-01-01T00:00:00.000000Z ";

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
        let line = format_dbt_log_record(
            metadata.level().as_str(),
            metadata.target(),
            &visitor.fields,
            self.canonical,
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

fn format_dbt_log_record(level: &str, target: &str, fields: &str, canonical: bool) -> String {
    let prefix = if canonical { DBT_LOG_RECORD_PREFIX } else { "" };
    format!("{prefix}{level} {target}: {fields}\n")
}

fn push_escaped_record_text(output: &mut String, value: &str) {
    for character in value.chars() {
        match character {
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            _ => output.push(character),
        }
    }
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
        push_escaped_record_text(&mut self.fields, &value);
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

fn emit_runtime_diagnostic(message: &str) {
    if let Some(emit) = DBT_DIAGNOSTIC_EMITTER.get() {
        unsafe { emit(message.as_ptr(), message.len()) };
    } else {
        eprint!("{message}");
    }
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
    matches!(effective_dbt_log_level(), 3..=5)
}

fn effective_dbt_log_level() -> i32 {
    let protected = DBT_EVIDENCE_LOG_LEVEL.load(Ordering::Acquire);
    if protected != 0 {
        return protected;
    }
    match std::env::var("HERMIT_LOG")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "error" => 1,
        "warn" => 2,
        "info" => 3,
        "debug" => 4,
        "trace" => 5,
        _ => 0,
    }
}

fn dbt_log_level_from_code(code: i32) -> Option<DbtLogLevel> {
    match code {
        1 => Some(DbtLogLevel::Error),
        2 => Some(DbtLogLevel::Warn),
        3 => Some(DbtLogLevel::Info),
        4 => Some(DbtLogLevel::Debug),
        5 => Some(DbtLogLevel::Trace),
        _ => None,
    }
}

fn dbt_log_level() -> Option<DbtLogLevel> {
    dbt_log_level_from_code(effective_dbt_log_level())
}

fn install_dbt_subscriber_with<E>(
    emit: Emitter,
    level: DbtLogLevel,
    canonical: bool,
    install: impl FnOnce(DbtSubscriber) -> Result<(), E>,
) -> bool {
    if install(DbtSubscriber {
        emit,
        level,
        canonical,
    })
    .is_err()
    {
        return false;
    }
    DBT_TRACING_ACTIVE.store(true, Ordering::Release);
    true
}

fn init_dbt_tracing(emit: Emitter, canonical: bool) -> bool {
    if DBT_TRACING_ACTIVE.load(Ordering::Acquire) {
        return true;
    }
    let Some(level) = dbt_log_level() else {
        return false;
    };
    install_dbt_subscriber_with(
        emit,
        level,
        canonical,
        tracing::subscriber::set_global_default,
    )
}

fn protected_evidence_capture_ready(protected_level: i32, tracing_active: bool) -> bool {
    protected_level == 0 || tracing_active
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
    config.backend_supports_parked_write_signal_interruption = false;
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

#[allow(clippy::too_many_arguments)]
fn send_dbt_prepare_exec(
    context: *mut c_void,
    scheduler_tid: Pid,
    physical_pid: i32,
    branches: u64,
    thread_state: &mut DetcoreThreadState,
    invoke_syscall: SyscallInvoker,
    read_registers: RegisterReader,
    write_registers: RegisterWriter,
) {
    let runtime = current_runtime();
    let mm = thread_state.mm_id;
    let (tid, pid) = prepare_exec_guest_identity(scheduler_tid, physical_pid);
    let mut guest = DbtGuest::<Detcore>::new(
        context as usize,
        tid,
        pid,
        None,
        branches,
        thread_state,
        &runtime.global,
        &runtime.config,
        invoke_syscall,
        read_registers,
        write_registers,
    );
    run_ready(prepare_exec(
        &mut guest,
        mm,
        std::collections::BTreeSet::new(),
    ));
}

fn prepare_exec_guest_identity(scheduler_tid: Pid, physical_pid: i32) -> (Pid, Pid) {
    (scheduler_tid, Pid::from_raw(physical_pid))
}

fn should_send_dbt_prepare_exec(initialized: bool, _physical_tid: i32, _physical_pid: i32) -> bool {
    // Linux permits a nonleader thread to exec; physical thread/process
    // equality must not gate the scheduler reconnect.
    initialized
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeAbi {
    V1,
    Current,
}

impl RuntimeAbi {
    fn scheduler_tid(self, virtual_tid: i32, physical_tid: i32) -> Option<Tid> {
        let tid = match self {
            Self::V1 => physical_tid,
            Self::Current => virtual_tid,
        };
        (tid > 0).then(|| Tid::from_raw(tid))
    }

    fn process_pid(self, virtual_pid: i32, physical_pid: i32) -> Option<Pid> {
        let pid = match self {
            Self::V1 => physical_pid,
            Self::Current => virtual_pid,
        };
        (pid > 0).then(|| Pid::from_raw(pid))
    }

    fn open_file_creator(self, virtual_tid: i32, physical_tid: i32) -> Option<DetTid> {
        let tid = match self {
            Self::V1 if virtual_tid > 0 => virtual_tid,
            Self::V1 => physical_tid,
            Self::Current => virtual_tid,
        };
        (tid > 0).then(|| DetTid::from_raw(tid))
    }

    fn runtime_identity(
        self,
        virtual_tid: i32,
        virtual_pid: i32,
        physical_tid: i32,
        physical_pid: i32,
    ) -> Option<(Tid, Pid)> {
        Some((
            self.scheduler_tid(virtual_tid, physical_tid)?,
            self.process_pid(virtual_pid, physical_pid)?,
        ))
    }
}

struct Runtime {
    abi: RuntimeAbi,
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
    virtual_child_tid: Tid,
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
static RUNTIME_BACKGROUND_OWNER_PID: AtomicI32 = AtomicI32::new(0);
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

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#2348): Single wiring point for the backend-selected detpid.
/// Build a DBT thread's Detcore state and publish its scheduler process ID and
/// host thread ID into it.
///
/// This exists as a named function because the assignment it performs was
/// previously duplicated at the two call sites below and lacked a direct
/// regression test. Routing both sites through one function makes removal of
/// the backend-selected process identity a unit-test failure; see
/// `dbt_thread_state_publishes_scheduler_and_host_identities`.
///
/// `det_pid` is what `RuntimeAbi::runtime_identity` selected: the client's
/// published virtual pid on the current v2+ callback path, the host pid under v1. It reaches
/// `tool_global::thread_start_request` as `ResourceID::MemAddrSpace(detpid)` and
/// identifies the process to Detcore's scheduler. Child RNG identity is derived
/// independently from deterministic creation pedigree/DBT child entropy.
fn init_dbt_thread_state(
    tool: &Detcore,
    det_tid: Tid,
    det_pid: DetTid,
    physical_tid: i32,
    parent: Option<(Tid, &DetcoreThreadState)>,
) -> DetcoreThreadState {
    let mut state = tool.init_thread_state(det_tid, parent);
    state.detpid = Some(det_pid);
    state.physical_tid = Some(physical_tid);
    state
}

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
// TODO-HUMAN-REVIEW(PR-1060): Review preservation of virtual DBT child TIDs.
fn insert_pending_thread_parent(
    pending: &mut HashMap<i32, PendingThreadParent>,
    physical_child_tid: i32,
    parent: PendingThreadParent,
) -> Result<(), Tid> {
    match pending.entry(physical_child_tid) {
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(parent);
            Ok(())
        }
        std::collections::hash_map::Entry::Occupied(entry) => {
            let stale_virtual_tid = entry.remove().virtual_child_tid;
            Err(stale_virtual_tid)
        }
    }
}

fn resolve_pending_thread_parent(
    pending: &mut HashMap<i32, PendingThreadParent>,
    physical_child_tid: i32,
    virtual_child_tid: Tid,
    emit_diagnostic: impl FnOnce(&str),
) -> Result<PendingThreadParent, i32> {
    let Some(parent) = pending.get(&physical_child_tid) else {
        return Err(1);
    };
    if parent.virtual_child_tid != virtual_child_tid {
        let pending_virtual_tid = parent.virtual_child_tid;
        pending.remove(&physical_child_tid);
        emit_diagnostic(&format!(
            "detcore-dbt: ERROR pending child identity mismatch: physical_tid={physical_child_tid} pending_virtual_tid={} observed_virtual_tid={}\n",
            pending_virtual_tid.as_raw(),
            virtual_child_tid.as_raw(),
        ));
        return Err(-1);
    }
    Ok(pending
        .remove(&physical_child_tid)
        .expect("matching pending child disappeared"))
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

unsafe fn runtime_background_init(callbacks: &reverie_dbt::DbtRuntimeCallbacks, abi: RuntimeAbi) {
    let image_generation = IMAGE_GENERATION.load(Ordering::SeqCst);
    let (emit_diagnostic, emit_evidence, protected_level) = runtime_callback_channels(callbacks);
    let _ = DBT_DIAGNOSTIC_EMITTER.set(emit_diagnostic);
    DBT_EVIDENCE_LOG_LEVEL.store(protected_level, Ordering::Release);
    RUNTIME_SHUTDOWN.store(false, Ordering::Release);
    RUNTIME_PAUSE_REQUESTED.store(false, Ordering::Release);
    RUNTIME_PAUSED.store(false, Ordering::Release);
    RUNTIME_BACKGROUND_OWNER_PID.store(unsafe { libc::getpid() }, Ordering::Release);
    emit_lifecycle_marker(
        emit_diagnostic,
        b"detcore-dbt: background client thread entered\n",
    );
    let protected_evidence_requested = protected_level != 0;
    let tracing_active = init_dbt_tracing(
        if protected_evidence_requested {
            emit_evidence
        } else {
            emit_diagnostic
        },
        protected_evidence_requested,
    );
    if !protected_evidence_capture_ready(protected_level, tracing_active) {
        emit_marker(
            emit_diagnostic,
            b"detcore-dbt: ERROR protected evidence subscriber installation failed\n",
        );
        unsafe { libc::_exit(reverie_dbt::CLIENT_THREAD_START_FAILURE_EXIT_CODE) };
    }
    let runtime = {
        let mut slot = RUNTIME.write().expect("Detcore DBT runtime lock poisoned");
        if slot.is_none() {
            emit_lifecycle_marker(
                emit_diagnostic,
                b"detcore-dbt: constructing Detcore Config\n",
            );
            let (mut config, source) = load_dbt_config();
            match source {
                ConfigSource::Cli => {
                    emit_lifecycle_marker(emit_diagnostic, b"detcore-dbt: using CLI-provided Detcore Config\n")
                }
                ConfigSource::ParseFallback => emit_marker(
                    emit_diagnostic,
                    b"detcore-dbt: WARNING could not parse HERMIT_DBT_DETCONFIG; using strict default\n",
                ),
                ConfigSource::Default => {
                    emit_lifecycle_marker(emit_diagnostic, b"detcore-dbt: using strict default Detcore Config\n")
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

            emit_lifecycle_marker(
                emit_diagnostic,
                b"detcore-dbt: initializing Detcore GlobalState\n",
            );
            let global = GlobalState::init_for_external_scheduler(&config);
            emit_lifecycle_marker(emit_diagnostic, b"detcore-dbt: GlobalState initialized\n");
            *slot = Some(Arc::new(Runtime {
                abi,
                config,
                global,
                tool: OnceLock::new(),
                next_child_ordinal: AtomicU64::new(1),
            }));
        } else if slot.as_ref().is_some_and(|runtime| runtime.abi != abi) {
            emit_marker(
                emit_diagnostic,
                b"detcore-dbt: ERROR runtime ABI changed after initialization\n",
            );
            unsafe { libc::_exit(reverie_dbt::CLIENT_THREAD_START_FAILURE_EXIT_CODE) };
        }
        Arc::clone(slot.as_ref().expect("Detcore DBT runtime was initialized"))
    };
    emit_lifecycle_marker(
        emit_diagnostic,
        b"detcore-dbt: background scheduler ready\n",
    );
    READY_IMAGE.store(image_generation, Ordering::SeqCst);
    let log_scheduler = info_logging_enabled() && !tracing_active;
    let observer = Arc::new(move |event: &'static str| {
        if log_scheduler {
            let line = format!("INFO detcore::scheduler: {event}\n");
            unsafe { emit_diagnostic(line.as_ptr(), line.len()) };
        }
    });
    run_cooperative(
        runtime.global.run_external_scheduler(observer),
        callbacks.idle,
    );
    emit_lifecycle_marker(
        emit_diagnostic,
        b"detcore-dbt: background scheduler completed\n",
    );
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
pub unsafe extern "C" fn reverie_dbt_runtime_background_init_v2(argument: *mut c_void) {
    let callbacks = unsafe { &*argument.cast::<reverie_dbt::DbtRuntimeCallbacks>() };
    unsafe { runtime_background_init(callbacks, RuntimeAbi::Current) };
}

/// Compatibility entry point for native clients using callback ABI version 1.
///
/// # Safety
///
/// `argument` must point to a valid version-1 callback structure.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reverie_dbt_runtime_background_init(argument: *mut c_void) {
    let callbacks = unsafe { &*argument.cast::<DbtRuntimeCallbacksV1>() };
    let current = upgrade_runtime_callbacks_v1(callbacks);
    unsafe { runtime_background_init(&current, RuntimeAbi::V1) };
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
    in_tree_ppid: i32,
    branch_count: u64,
    defer_runtime: i32,
    invoke_syscall: SyscallInvoker,
    read_registers: RegisterReader,
    write_registers: RegisterWriter,
) -> i32 {
    if defer_runtime != 0 {
        // PRESERVE THE VIRTUAL IDENTITY THE CLIENT JUST PUBLISHED.
        //
        // The client zeroes this struct, writes the thread's virtual identity
        // into it, and only then calls in here -- `reverie-dbt/native/client.c`
        // sets `virtual_pid`/`virtual_ppid`/`virtual_tid` immediately before the
        // call, under the comment "Publish stable virtual identities before
        // entering Rust. Detcore consumes these fields while constructing its
        // thread state." It is also the client, not Detcore, that answers the
        // identity syscalls: it returns `counters->virtual_pid` for SYS_getpid,
        // `virtual_ppid` for SYS_getppid and `virtual_tid` for SYS_gettid.
        //
        // Writing a whole fresh struct here therefore erased those three fields
        // microseconds after the client set them, and every identity syscall on
        // the DBT path answered 0. Measured on the same guest against the golden
        // ptrace reference: ptrace reports getpid=3 gettid=3 getppid=1; DBT
        // reported 0, 0, 0. Initialise only the fields Hermit owns and leave the
        // client's alone.
        unsafe {
            let scratch = &mut *scratch.cast::<NativeThreadScratch>();
            scratch.branches = branch_count;
            scratch.observed_syscalls = 0;
            scratch.rewritten_syscalls = 0;
            scratch.runtime_state = std::ptr::null_mut();
            scratch.pending_thread_clone = 0;
            scratch.thread_clone_flags = 0;
            scratch.thread_clone_ctid = 0;
            scratch.pending_thread_start = 0;
        }
        return 0;
    }
    let scratch = unsafe { &mut *scratch.cast::<NativeThreadScratch>() };

    let host_tid = tid;
    let host_pid = pid;
    if host_tid <= 0 || host_pid <= 0 {
        return -1;
    }
    reverie_dbt::set_current_ppid(in_tree_ppid);
    let runtime = current_runtime();
    let Some((det_tid, det_pid)) =
        runtime
            .abi
            .runtime_identity(scratch.virtual_tid, scratch.virtual_pid, host_tid, host_pid)
    else {
        return -1;
    };
    let tool = runtime
        .tool
        .get_or_init(|| Detcore::new(det_pid, &runtime.config));
    let mut inherited_parent = if host_tid == host_pid && !scratch.runtime_state.is_null() {
        // A copied DynamoRIO process owns a copy-on-write copy of the
        // parent's allocation. Replace it with state initialized for the
        // child's virtual process identity.
        let parent = Some(unsafe { Box::from_raw(scratch.runtime_state) });
        scratch.runtime_state = std::ptr::null_mut();
        parent
    } else {
        None
    };
    if let Some(parent) = inherited_parent.as_mut() {
        parent.state.clone_flags =
            Some(CloneFlags::from_bits_truncate(scratch.pending_clone_flags));
    }
    let parent = if host_tid == host_pid {
        None
    } else {
        match resolve_pending_thread_parent(
            &mut PENDING_THREAD_PARENTS
                .lock()
                .expect("pending DBT thread parent lock poisoned"),
            host_tid,
            det_tid,
            emit_runtime_diagnostic,
        ) {
            Ok(parent) => Some(parent),
            Err(status) => return status,
        }
    };
    let parent_ref = inherited_parent
        .as_ref()
        .map(|parent| (Tid::from_raw(parent.tid.into()), &parent.state))
        .or_else(|| {
            parent
                .as_ref()
                .map(|parent| (parent.parent_tid, &parent.state))
        });
    let host_pid = Pid::from_raw(host_pid);
    let mut state = init_dbt_thread_state(
        tool,
        det_tid,
        DetTid::from_raw(det_pid.into()),
        host_tid,
        parent_ref,
    );
    if let Some(parent) = &parent {
        state.reseed_child_rngs(&parent.state, parent.rng_entropy);
    } else if let Some(parent) = &inherited_parent {
        let child_ordinal = runtime.next_child_ordinal.fetch_add(1, Ordering::SeqCst);
        let Some(rng_entropy) = dbt_child_rng_entropy(scratch.virtual_pid, child_ordinal) else {
            return -1;
        };
        state.reseed_child_rngs(&parent.state, rng_entropy);
    }
    let mut thread = Box::new(ThreadRuntime {
        tid: Pid::from_raw(det_tid.into()),
        state,
        initialized: false,
        post_exec_pending: host_tid == pid && inherited_parent.is_none(),
    });
    if reverie_dbt::run_tool_thread_start(
        tool,
        context as usize,
        Pid::from_raw(det_tid.into()),
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
    scratch.pending_clone_flags = 0;
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
pub unsafe extern "C" fn reverie_dbt_runtime_thread_created_v2(
    scratch: *mut c_void,
    context: *mut c_void,
    _parent_tid: i32,
    pid: i32,
    branch_count: u64,
    child_tid: i32,
    virtual_child_tid: i32,
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
    let parent = unsafe { &mut *scratch.runtime_state };
    let flags = CloneFlags::from_bits_truncate(flags);
    parent.state.clone_flags = Some(flags);
    let parent_snapshot = parent.state.clone();
    let child_ordinal = runtime.next_child_ordinal.fetch_add(1, Ordering::SeqCst);
    let Some(rng_entropy) = dbt_child_rng_entropy(scratch.virtual_pid, child_ordinal) else {
        parent.state.clone_flags = None;
        return -1;
    };
    let Some(child_scheduler_tid) = runtime.abi.scheduler_tid(virtual_child_tid, child_tid) else {
        parent.state.clone_flags = None;
        return -1;
    };
    if let Err(pending_virtual_tid) = insert_pending_thread_parent(
        &mut PENDING_THREAD_PARENTS
            .lock()
            .expect("pending DBT thread parent lock poisoned"),
        child_tid,
        PendingThreadParent {
            parent_tid: Tid::from_raw(parent.tid.into()),
            virtual_child_tid: child_scheduler_tid,
            rng_entropy,
            state: parent_snapshot,
        },
    ) {
        parent.state.clone_flags = None;
        emit_runtime_diagnostic(&format!(
            "detcore-dbt: ERROR duplicate pending child handoff: physical_tid={child_tid} stale_virtual_tid={} attempted_virtual_tid={}\n",
            pending_virtual_tid.as_raw(),
            child_scheduler_tid.as_raw(),
        ));
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
            (flags.bits() & 0xff) as libc::c_int,
            Some((pid, child_tid)),
        ));
    }
    parent.state.clone_flags = None;
    0
}

fn successful_process_clone_result(sysnum: i64, result: i64) -> bool {
    result >= 0
        && matches!(
            sysnum,
            libc::SYS_fork | libc::SYS_vfork | libc::SYS_clone | libc::SYS_clone3
        )
}

fn process_child_registration(
    sysnum: i64,
    result: i64,
    virtual_child_tid: i32,
    raw_flags: u64,
    exit_signal: i32,
) -> Option<(Tid, CloneFlags, libc::c_int)> {
    if result <= 0 || virtual_child_tid <= 0 {
        return None;
    }
    let flags = CloneFlags::from_bits_truncate(raw_flags);
    if flags.intersects(CloneFlags::CLONE_THREAD | CloneFlags::CLONE_VFORK)
        || !matches!(sysnum, libc::SYS_fork | libc::SYS_clone | libc::SYS_clone3)
    {
        return None;
    }
    Some((Tid::from_raw(virtual_child_tid), flags, exit_signal))
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ProcessCloneProperties {
    blocks_parent: bool,
    shares_files_without_thread: bool,
    shares_memory_without_thread: bool,
}

/// Decode process-lifecycle properties that must be decided before a copy.
///
/// clone3 stores its flags in guest memory, so the caller supplies a
/// fault-safe reader instead of dereferencing the guest pointer.
fn process_clone_properties(
    sysnum: i64,
    args: &[u64],
    mut read: impl FnMut(usize, &mut [u8]) -> bool,
) -> ProcessCloneProperties {
    const CLONE_ARGS_SIZE_VER0: u64 = 64;

    let flags = match sysnum {
        libc::SYS_fork => Some(0),
        libc::SYS_vfork => Some(libc::CLONE_VFORK as u64),
        libc::SYS_clone => args.first().copied(),
        libc::SYS_clone3 => {
            let Some((&address, &size)) = args.first().zip(args.get(1)) else {
                return ProcessCloneProperties::default();
            };
            if address == 0 || size < CLONE_ARGS_SIZE_VER0 {
                return ProcessCloneProperties::default();
            }
            let mut bytes = [0_u8; std::mem::size_of::<u64>()];
            if !read(address as usize, &mut bytes) {
                return ProcessCloneProperties::default();
            }
            Some(u64::from_ne_bytes(bytes))
        }
        _ => None,
    };

    let Some(flags) = flags else {
        return ProcessCloneProperties::default();
    };
    ProcessCloneProperties {
        blocks_parent: flags & libc::CLONE_VFORK as u64 != 0,
        shares_files_without_thread: flags & libc::CLONE_FILES as u64 != 0
            && flags & libc::CLONE_THREAD as u64 == 0,
        shares_memory_without_thread: flags & libc::CLONE_VM as u64 != 0
            && flags & (libc::CLONE_THREAD | libc::CLONE_VFORK) as u64 == 0,
    }
}

/// Applies the result of a native process-clone syscall after the kernel returns.
///
/// Successful process clones share inherited open file descriptions, while the
/// copied DBT runtime can mutate their locks independently. Invalidate both the
/// parent and child copies only after success; a failed clone leaves the known
/// lock state unchanged.
///
/// # Safety
///
/// `scratch` must name initialized per-thread storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reverie_dbt_runtime_process_clone_result(
    scratch: *mut c_void,
    context: *mut c_void,
    _parent_tid: i32,
    pid: i32,
    branch_count: u64,
    sysnum: i64,
    result: i64,
    virtual_child_tid: i32,
    child_tid_addr: u64,
    raw_flags: u64,
    exit_signal: i32,
    invoke_syscall: SyscallInvoker,
    read_registers: RegisterReader,
    write_registers: RegisterWriter,
) -> i32 {
    let scratch = unsafe { &mut *scratch.cast::<NativeThreadScratch>() };
    if successful_process_clone_result(sysnum, result) && !scratch.runtime_state.is_null() {
        unsafe { &mut *scratch.runtime_state }
            .state
            .forget_flock_modes();
    }

    if result > 0
        && virtual_child_tid > 0
        && matches!(sysnum, libc::SYS_fork | libc::SYS_clone | libc::SYS_clone3)
        && raw_flags & (libc::CLONE_THREAD | libc::CLONE_VFORK) as u64 == 0
        && exit_signal < 0
    {
        return -1;
    }
    let Some((child_scheduler_tid, flags, exit_signal)) =
        process_child_registration(sysnum, result, virtual_child_tid, raw_flags, exit_signal)
    else {
        return 0;
    };
    if scratch.runtime_state.is_null() {
        return 0;
    }

    let runtime = current_runtime();
    let tool = runtime
        .tool
        .get()
        .expect("Detcore DBT tool was initialized");
    let parent = unsafe { &mut *scratch.runtime_state };
    let Some(child_scheduler_tid) = runtime
        .abi
        .scheduler_tid(child_scheduler_tid.as_raw(), result as i32)
    else {
        return -1;
    };
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
        exit_signal,
        Some((result as i32, result as i32)),
    ));
    0
}

/// Compatibility entry point for native clients using callback ABI version 1.
///
/// # Safety
///
/// The pointers and callbacks must satisfy the version-1 thread-created ABI.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn reverie_dbt_runtime_thread_created(
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
    unsafe {
        reverie_dbt_runtime_thread_created_v2(
            scratch,
            context,
            parent_tid,
            pid,
            branch_count,
            child_tid,
            child_tid,
            child_tid_addr,
            flags,
            invoke_syscall,
            read_registers,
            write_registers,
        )
    }
}

/// Releases Detcore state owned by a DynamoRIO application thread.
///
/// # Safety
///
/// `scratch` must be the pointer initialized by
/// [`reverie_dbt_runtime_thread_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reverie_dbt_runtime_thread_exit(
    scratch: *mut c_void,
    context: *mut c_void,
    _tid: i32,
    invoke_syscall: SyscallInvoker,
) {
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
        let _ = reverie_dbt::run_tool_thread_exit_from_guest(
            tool,
            context as usize,
            invoke_syscall,
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
fn copied_child_flock_action(operation: Option<i32>) -> i32 {
    let Some(operation) = operation else {
        return -libc::ENOLCK;
    };
    let known_flags = libc::LOCK_SH | libc::LOCK_EX | libc::LOCK_UN | libc::LOCK_NB;
    let base = operation & !libc::LOCK_NB;
    let valid = operation & !known_flags == 0
        && matches!(base, libc::LOCK_SH | libc::LOCK_EX | libc::LOCK_UN);
    if !valid || base == libc::LOCK_UN || operation & libc::LOCK_NB != 0 {
        0
    } else {
        -libc::ENOLCK
    }
}

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
    // A copied pre-exec child has no Detcore Tool. Preserve kernel handling
    // for malformed operations, nonblocking locks, and unlock; only blocking
    // acquisitions can strand the child behind a parent that is waiting for it.
    if sysno == Sysno::flock {
        let operation = (!args.is_null()).then(|| unsafe { args.add(1).read() as i32 });
        return copied_child_flock_action(operation);
    }
    if matches!(
        sysno,
        Sysno::recvmsg | Sysno::recvmmsg | Sysno::readlink | Sysno::readlinkat
    ) && strict
    {
        return 1;
    }
    // setpgid FAILS CLOSED IN STRICT MODE BECAUSE THE COPIED CHILD CAN PERFORM
    // IT BUT CANNOT RECORD IT.
    //
    // `handle_setpgid` in the instrumented root does two things: it injects the
    // call, so the KERNEL really moves the process group, and then it mirrors
    // that move into Detcore's model with `set_process_group`, "so
    // group-selecting waits do not consult host /proc state". A copied child has
    // no Detcore Tool, so it performs the first half and silently skips the
    // second: the kernel's process groups and Detcore's model diverge, and the
    // next `wait` with a negative pgid selects against a model that is now
    // wrong. That is the machinery hermit#bc9d178 reclassified setpgid to
    // determinize in the first place.
    //
    // ⚠️ AND A FIXED ERRNO IS THE WRONG SHAPE HERE, WHICH IS WHY THIS IS NOT
    // `-EPERM`. The ioctl arm above emulates ENOTTY because the instrumented
    // root observes ENOTTY too -- that is faithful. The root observes setpgid
    // SUCCEEDING, so no errno we return is faithful, and the choice is between
    // two divergences. EPERM is the quiet one: setpgid's return is widely
    // ignored, so a child would carry on believing it leads a process group it
    // does not lead. Failing closed is the loud one, and a wrong answer about
    // process-group identity is worse than no answer.
    //
    // Strict-only, matching the receive/readlink arms directly above: non-strict
    // copied children keep native behaviour, and only strict execution -- which
    // is where determinism is actually claimed -- refuses.
    if sysno == Sysno::setpgid && strict {
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
    let scratch = unsafe { &mut *scratch.cast::<NativeThreadScratch>() };
    let runtime = current_runtime();
    let Some((det_tid, det_pid)) =
        runtime
            .abi
            .runtime_identity(scratch.virtual_tid, scratch.virtual_pid, tid, pid)
    else {
        unsafe { result.write(-(Errno::EIO.into_raw() as i64)) };
        TOTAL_REWRITTEN.fetch_add(1, Ordering::Relaxed);
        return 1;
    };
    let raw_args = unsafe { std::slice::from_raw_parts(args, 6) };
    let mut dispatch_args: [u64; 6] = raw_args.try_into().expect("six syscall arguments");
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

    // Clone-family and exec syscalls return through native lifecycle handling
    // below. Initialize the local Detcore state before that early return so a
    // process whose first intercepted syscall is fork/clone/exec still has a
    // parent state for the post-clone callback and can send PrepareExec.
    TOTAL_BRANCHES.store(branches, Ordering::Relaxed);
    let host_pid = Pid::from_raw(pid);
    let det_tid = Pid::from_raw(det_tid.into());
    let tool = runtime
        .tool
        .get_or_init(|| Detcore::new(det_pid, &runtime.config));
    if first_event {
        emit_lifecycle_marker(emit, b"detcore-dbt: initializing Detcore thread state\n");
    }
    if scratch.runtime_state.is_null() {
        if first_event {
            emit_lifecycle_marker(emit, b"detcore-dbt: constructing Detcore thread state\n");
        }
        let mut state = init_dbt_thread_state(
            tool,
            Tid::from_raw(det_tid.into()),
            DetTid::from_raw(det_pid.into()),
            tid,
            None,
        );
        let Some(open_file_creator) = runtime.abi.open_file_creator(scratch.virtual_tid, tid)
        else {
            unsafe { result.write(-(Errno::EIO.into_raw() as i64)) };
            TOTAL_REWRITTEN.fetch_add(1, Ordering::Relaxed);
            return 1;
        };
        state.set_open_file_creator(open_file_creator);
        if first_event {
            emit_lifecycle_marker(emit, b"detcore-dbt: Detcore thread state constructed\n");
        }
        scratch.runtime_state = Box::into_raw(Box::new(ThreadRuntime {
            tid: det_tid,
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
            host_pid,
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
            host_pid,
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

    if sysnum == libc::SYS_execveat {
        unsafe { result.write(-(Errno::ENOSYS.into_raw() as i64)) };
        TOTAL_REWRITTEN.fetch_add(1, Ordering::Relaxed);
        return 1;
    }
    let clone_properties = process_clone_properties(sysnum, raw_args, |address, bytes| unsafe {
        read_memory(address, bytes.as_mut_ptr(), bytes.len()) != 0
    });
    if clone_properties.shares_files_without_thread {
        emit_lifecycle_marker(
            emit,
            b"detcore-dbt: refusing process clone with CLONE_FILES without CLONE_THREAD; copied child cannot share descriptor-table provenance\n",
        );
        return -1;
    }
    if clone_properties.shares_memory_without_thread {
        emit_lifecycle_marker(
            emit,
            b"detcore-dbt: refusing process clone with CLONE_VM without CLONE_THREAD or CLONE_VFORK; copied child cannot own inherited Detcore state\n",
        );
        return -1;
    }
    if clone_properties.blocks_parent
        && !scratch.runtime_state.is_null()
        && unsafe { &*scratch.runtime_state }
            .state
            .has_unsafe_vfork_flock_state()
    {
        emit_lifecycle_marker(
            emit,
            b"detcore-dbt: refusing vfork/CLONE_VFORK while an open file description may hold a flock\n",
        );
        return -1;
    }

    // clone(2) and clone3(2) return in both the parent and child. Injecting
    // either from this callback makes the child return on the client stack.
    if requires_native_lifecycle(sysnum) {
        if sysnum == libc::SYS_execve {
            // Linux permits a nonleader thread to exec. Detcore's PrepareExec
            // reconnect path accepts that thread's scheduler identity, so
            // every initialized thread must notify it before entering exec.
            // Only the external runtime pause below remains process-owner
            // gated.
            if !scratch.runtime_state.is_null() {
                let thread = unsafe { &mut *scratch.runtime_state };
                if should_send_dbt_prepare_exec(thread.initialized, tid, pid) {
                    send_dbt_prepare_exec(
                        context,
                        thread.tid,
                        pid,
                        branches,
                        &mut thread.state,
                        invoke_syscall,
                        read_registers,
                        write_registers,
                    );
                }
            }
            if RUNTIME_BACKGROUND_OWNER_PID.load(Ordering::Acquire) == pid {
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
        }
        return 0;
    }
    update_memory_hash(sysnum, raw_args, read_memory);
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
        host_pid,
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

/// Reports the DBT runtime ABI version this build implements.
///
/// The native client refuses to proceed unless this equals its own
/// `REVERIE_DBT_RUNTIME_ABI_VERSION` and [`reverie_dbt_runtime_callbacks_size`]
/// equals its `sizeof(runtime_callbacks_t)`; on a mismatch it prints
/// "native/runtime ABI version or callback size mismatch" and calls
/// `dr_exit_process(101)`.
///
/// The value is pinned locally rather than inferred from the dependency. The
/// test below requires Reverie's declared version to equal this pin, so an
/// upstream ABI bump fails closed until Hermit's exports have been reviewed and
/// this implemented-version constant is advanced deliberately.
#[unsafe(no_mangle)]
pub extern "C" fn reverie_dbt_runtime_abi_version() -> u32 {
    IMPLEMENTED_DBT_RUNTIME_ABI_VERSION
}

/// Reports the exact callback-structure size for the current ABI version.
///
/// This size is pinned alongside the implemented ABI version. The test below
/// compares it with `size_of::<reverie_dbt::DbtRuntimeCallbacks>()`, so callback
/// layout drift fails closed instead of being advertised automatically as an
/// ABI Hermit already implements.
#[unsafe(no_mangle)]
pub extern "C" fn reverie_dbt_runtime_callbacks_size() -> usize {
    IMPLEMENTED_DBT_RUNTIME_CALLBACKS_SIZE
}

/// Reports the wire code identifying which runtime produced a stats record.
///
/// The codes Reverie defines name its own bundled prototype runtimes --
/// `PrototypeTool` = 0, `Counter1` = 1, `CounterLocal` = 2. Detcore is none of
/// them, and inventing a code for it would collide the moment Reverie assigns
/// that number. `DbtRuntimeKind::Unknown` (255) is the value Reverie reserves
/// for exactly this case: its `from_wire` decodes an unrecognized code to
/// `Unknown` "rather than failing the whole record", so the record's additive
/// counters still land. [`reverie_dbt_runtime_name`] carries the readable
/// identity ("Detcore"); this byte only has to be honest about not being one of
/// the prototype kinds.
#[unsafe(no_mangle)]
pub extern "C" fn reverie_dbt_runtime_kind_code() -> u8 {
    reverie_dbt::backend_stats::DbtRuntimeKind::Unknown.to_wire()
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

    unsafe extern "C" fn test_emit(_bytes: *const u8, _length: usize) {}

    unsafe extern "C" fn test_stdout(_bytes: *const u8, _length: usize) {}

    unsafe extern "C" fn test_emit_evidence(_bytes: *const u8, _length: usize) {}

    unsafe extern "C" fn test_idle() {}

    #[test]
    fn exported_runtime_identity_matches_the_pinned_reverie_abi() {
        assert_eq!(IMPLEMENTED_DBT_RUNTIME_ABI_VERSION, 4);
        assert_eq!(
            reverie_dbt::DBT_RUNTIME_ABI_VERSION,
            IMPLEMENTED_DBT_RUNTIME_ABI_VERSION
        );
        assert_eq!(
            std::mem::size_of::<reverie_dbt::DbtRuntimeCallbacks>(),
            IMPLEMENTED_DBT_RUNTIME_CALLBACKS_SIZE
        );
        assert_eq!(
            reverie_dbt_runtime_abi_version(),
            IMPLEMENTED_DBT_RUNTIME_ABI_VERSION
        );
        assert_eq!(
            reverie_dbt_runtime_callbacks_size(),
            IMPLEMENTED_DBT_RUNTIME_CALLBACKS_SIZE
        );
        assert_eq!(
            reverie_dbt_runtime_kind_code(),
            reverie_dbt::backend_stats::DbtRuntimeKind::Unknown.to_wire()
        );
    }

    #[test]
    fn version_one_callbacks_upgrade_without_changing_existing_channels_or_policy() {
        let legacy = DbtRuntimeCallbacksV1 {
            emit: test_emit,
            idle: test_idle,
            panic_on_unsupported_syscalls: 1,
            unsupported_report_fd: UNSUPPORTED_SYSCALL_REPORT_FD,
            emit_stdout: test_stdout,
        };
        let current = upgrade_runtime_callbacks_v1(&legacy);

        assert_eq!(current.emit as *const (), test_emit as *const ());
        assert_eq!(current.idle as *const (), test_idle as *const ());
        assert_eq!(current.panic_on_unsupported_syscalls, 1);
        assert_eq!(current.unsupported_report_fd, UNSUPPORTED_SYSCALL_REPORT_FD);
        assert_eq!(current.emit_stdout as *const (), test_stdout as *const ());
        assert_eq!(current.emit_evidence as *const (), test_emit as *const ());
        assert_eq!(current.evidence_log_level, 0);
    }

    #[test]
    fn current_callbacks_route_protected_evidence_at_the_declared_level() {
        let callbacks = reverie_dbt::DbtRuntimeCallbacks {
            emit: test_emit,
            idle: test_idle,
            panic_on_unsupported_syscalls: 1,
            unsupported_report_fd: UNSUPPORTED_SYSCALL_REPORT_FD,
            emit_stdout: test_stdout,
            emit_evidence: test_emit_evidence,
            evidence_log_level: 3,
        };
        let (diagnostic, evidence, level) = runtime_callback_channels(&callbacks);

        assert_eq!(diagnostic as *const (), test_emit as *const ());
        assert_eq!(evidence as *const (), test_emit_evidence as *const ());
        assert_eq!(level, 3);
        assert!(protected_evidence_capture_ready(level, true));
        assert!(!protected_evidence_capture_ready(level, false));
        assert!(protected_evidence_capture_ready(0, false));
    }

    #[test]
    fn protected_evidence_records_have_deterministic_prefix_and_escaped_fields() {
        let mut fields = String::new();
        push_escaped_record_text(&mut fields, "line one\nline two\\tail\r");
        assert_eq!(fields, "line one\\nline two\\\\tail\\r");
        assert_eq!(
            format_dbt_log_record(
                tracing::Level::INFO.as_str(),
                "detcore::scheduler",
                &fields,
                true,
            ),
            "1970-01-01T00:00:00.000000Z INFO detcore::scheduler: line one\\nline two\\\\tail\\r\n"
        );
        // The `canonical` flag arrived on main after this test was written
        // (a811f33684). Pin BOTH branches: the deterministic prefix is what a
        // verification record needs, and its ABSENCE is what keeps an ordinary
        // diagnostic run from claiming a fixed timestamp it never had. Asserting
        // only the true branch would let a change that always prefixes pass.
        assert_eq!(
            format_dbt_log_record(
                tracing::Level::INFO.as_str(),
                "detcore::scheduler",
                &fields,
                false,
            ),
            "INFO detcore::scheduler: line one\\nline two\\\\tail\\r\n"
        );
    }

    #[test]
    fn protected_evidence_refuses_when_subscriber_installation_fails() {
        let previous = DBT_TRACING_ACTIVE.swap(false, Ordering::AcqRel);
        let mut selected_emitter = None;
        let installed = install_dbt_subscriber_with(
            test_emit_evidence,
            DbtLogLevel::Info,
            true,
            |subscriber| {
                selected_emitter = Some(subscriber.emit as *const ());
                Err::<(), ()>(())
            },
        );
        assert!(!installed);
        assert_eq!(selected_emitter, Some(test_emit_evidence as *const ()));
        assert!(!DBT_TRACING_ACTIVE.load(Ordering::Acquire));
        assert!(!protected_evidence_capture_ready(3, installed));
        DBT_TRACING_ACTIVE.store(previous, Ordering::Release);
    }

    #[test]
    fn current_exports_match_the_native_client_signatures() {
        let _: extern "C" fn() -> u64 = reverie_dbt_runtime_image_init;
        let _: unsafe extern "C" fn(
            *mut c_void,
            *mut c_void,
            i32,
            i32,
            i32,
            u64,
            i32,
            SyscallInvoker,
            RegisterReader,
            RegisterWriter,
        ) -> i32 = reverie_dbt_runtime_thread_init;
        let _: unsafe extern "C" fn(
            *mut c_void,
            *mut c_void,
            i32,
            i32,
            u64,
            i32,
            i32,
            u64,
            u64,
            SyscallInvoker,
            RegisterReader,
            RegisterWriter,
        ) -> i32 = reverie_dbt_runtime_thread_created_v2;
        let _: unsafe extern "C" fn(*mut c_void, *mut c_void, i32, SyscallInvoker) =
            reverie_dbt_runtime_thread_exit;
        let _: unsafe extern "C" fn(*mut c_void, i32) = reverie_dbt_runtime_exec_failed;
        let _: unsafe extern "C" fn(
            *mut c_void,
            *mut c_void,
            i32,
            i32,
            u64,
            i64,
            i64,
            i32,
            u64,
            u64,
            i32,
            SyscallInvoker,
            RegisterReader,
            RegisterWriter,
        ) -> i32 = reverie_dbt_runtime_process_clone_result;
        let _: unsafe extern "C" fn(i64, *const u64) -> i32 = reverie_dbt_runtime_copied_syscall;
        let _: unsafe extern "C" fn(
            *mut c_void,
            *mut c_void,
            i32,
            i32,
            u64,
            i64,
            *const u64,
            u64,
            *mut i64,
            *mut i64,
            *mut u64,
            SyscallInvoker,
            RegisterReader,
            RegisterWriter,
            MemoryReader,
            MemoryWriter,
            Emitter,
        ) -> i32 = reverie_dbt_runtime_pre_syscall;
        let _: unsafe extern "C" fn(*mut c_void) = reverie_dbt_runtime_background_init_v2;
        let _: extern "C" fn() = reverie_dbt_runtime_process_exit;
        let _: extern "C" fn(u64) -> i32 = reverie_dbt_runtime_ready;
        let _: extern "C" fn() -> *const libc::c_char = reverie_dbt_runtime_name;
        let _: extern "C" fn() -> u8 = reverie_dbt_runtime_kind_code;
        let _: unsafe extern "C" fn(*mut u64, *mut u64, *mut u64, *mut u64) =
            reverie_dbt_runtime_totals;
        let _: extern "C" fn() -> u32 = reverie_dbt_runtime_abi_version;
        let _: extern "C" fn() -> usize = reverie_dbt_runtime_callbacks_size;

        let _: unsafe extern "C" fn(*mut c_void) = reverie_dbt_runtime_background_init;
        let _: unsafe extern "C" fn(
            *mut c_void,
            *mut c_void,
            i32,
            i32,
            u64,
            i32,
            u64,
            u64,
            SyscallInvoker,
            RegisterReader,
            RegisterWriter,
        ) -> i32 = reverie_dbt_runtime_thread_created;
    }

    #[test]
    fn current_identity_uses_published_virtual_ids_not_host_ids() {
        let (scheduler_tid, process_pid) = RuntimeAbi::Current
            .runtime_identity(4, 3, 44_004, 44_003)
            .expect("positive virtual identities must be accepted");
        let scheduler_tid: i32 = scheduler_tid.into();
        let process_pid: i32 = process_pid.into();
        assert_eq!(scheduler_tid, 4);
        assert_eq!(process_pid, 3);
        assert_eq!(
            RuntimeAbi::Current.runtime_identity(0, 3, 44_004, 44_003),
            None
        );
        assert_eq!(
            RuntimeAbi::Current.runtime_identity(4, 0, 44_004, 44_003),
            None
        );
        assert_eq!(
            RuntimeAbi::Current.runtime_identity(-1, 3, 44_004, 44_003),
            None
        );
        assert_eq!(
            RuntimeAbi::Current.runtime_identity(4, -1, 44_004, 44_003),
            None
        );
    }

    #[test]
    fn version_one_identity_uses_physical_ids_when_virtual_fields_are_zero_or_different() {
        for (virtual_tid, virtual_pid) in [(0, 0), (4, 3), (-1, -1)] {
            let (scheduler_tid, process_pid) = RuntimeAbi::V1
                .runtime_identity(virtual_tid, virtual_pid, 44_004, 44_003)
                .expect("positive physical identities must be accepted for ABI v1");
            let scheduler_tid: i32 = scheduler_tid.into();
            let process_pid: i32 = process_pid.into();
            assert_eq!(scheduler_tid, 44_004);
            assert_eq!(process_pid, 44_003);
        }
        assert_eq!(RuntimeAbi::V1.runtime_identity(4, 3, 0, 44_003), None);
        assert_eq!(RuntimeAbi::V1.runtime_identity(4, 3, 44_004, 0), None);
    }

    #[test]
    fn version_one_open_file_creator_prefers_virtual_tid_with_physical_fallback() {
        let scheduler_tid: i32 = RuntimeAbi::V1.scheduler_tid(4, 44_004).unwrap().into();
        let virtual_creator = RuntimeAbi::V1.open_file_creator(4, 44_004).unwrap();
        let fallback_creator = RuntimeAbi::V1.open_file_creator(0, 44_004).unwrap();

        assert_eq!(scheduler_tid, 44_004);
        assert_eq!(virtual_creator.as_raw(), 4);
        assert_eq!(fallback_creator.as_raw(), 44_004);
        assert_eq!(RuntimeAbi::V1.open_file_creator(-1, 0), None);
    }

    #[test]
    fn canonical_dbt_log_records_have_timestamp_and_one_line() {
        let mut fields = String::new();
        push_escaped_record_text(&mut fields, "first\\second\nthird\rfourth");
        assert_eq!(fields, "first\\\\second\\nthird\\rfourth");
        assert_eq!(
            format_dbt_log_record("INFO", "detcore::scheduler", &fields, true),
            "1970-01-01T00:00:00.000000Z INFO detcore::scheduler: \
             first\\\\second\\nthird\\rfourth\n"
        );
        assert_eq!(
            format_dbt_log_record("INFO", "detcore::scheduler", "ready", false),
            "INFO detcore::scheduler: ready\n"
        );
    }

    #[test]
    fn protected_evidence_level_selects_the_expected_filter() {
        let info = dbt_log_level_from_code(3).expect("INFO code");
        assert!(info.enables(&tracing::Level::ERROR));
        assert!(info.enables(&tracing::Level::WARN));
        assert!(info.enables(&tracing::Level::INFO));
        assert!(!info.enables(&tracing::Level::DEBUG));
        assert!(!info.enables(&tracing::Level::TRACE));
        assert!(dbt_log_level_from_code(0).is_none());
        assert!(dbt_log_level_from_code(6).is_none());
    }

    #[test]
    fn requested_protected_evidence_requires_an_installed_subscriber() {
        assert!(protected_evidence_capture_ready(0, false));
        assert!(protected_evidence_capture_ready(3, true));
        assert!(!protected_evidence_capture_ready(3, false));
    }

    #[test]
    fn process_clone_result_invalidates_only_for_successful_clone_family_calls() {
        for sysnum in [
            libc::SYS_fork,
            libc::SYS_vfork,
            libc::SYS_clone,
            libc::SYS_clone3,
        ] {
            assert!(successful_process_clone_result(sysnum, 0));
            assert!(successful_process_clone_result(sysnum, 42));
            assert!(!successful_process_clone_result(
                sysnum,
                -libc::EINVAL as i64
            ));
        }

        for sysnum in [libc::SYS_getuid, libc::SYS_read, libc::SYS_execve] {
            assert!(!successful_process_clone_result(sysnum, 0));
            assert!(!successful_process_clone_result(sysnum, 42));
            assert!(!successful_process_clone_result(
                sysnum,
                -libc::EINVAL as i64
            ));
        }
    }

    #[test]
    fn process_child_registration_uses_only_successful_parent_results() {
        let fork = process_child_registration(libc::SYS_fork, 91_001, 4, 0, libc::SIGCHLD)
            .expect("successful parent-side fork must register the virtual child");
        assert_eq!(fork.0, Tid::from_raw(4));
        assert!(fork.1.is_empty());
        assert_eq!(fork.2, libc::SIGCHLD);

        let raw_flags = libc::CLONE_PARENT as u64;
        let clone =
            process_child_registration(libc::SYS_clone, 91_002, 5, raw_flags, libc::SIGUSR1)
                .expect("successful parent-side process clone must be registered");
        assert_eq!(clone.0, Tid::from_raw(5));
        assert!(clone.1.contains(CloneFlags::CLONE_PARENT));
        assert_eq!(clone.2, libc::SIGUSR1);

        assert_eq!(
            process_child_registration(libc::SYS_fork, 0, 4, 0, libc::SIGCHLD),
            None,
            "the child-side result must not register itself"
        );
        assert_eq!(
            process_child_registration(libc::SYS_clone, 91_003, 6, libc::CLONE_THREAD as u64, 0,),
            None,
            "thread clones keep the existing thread-created callback"
        );
        assert_eq!(
            process_child_registration(
                libc::SYS_vfork,
                91_004,
                7,
                libc::CLONE_VFORK as u64,
                libc::SIGCHLD,
            ),
            None,
            "vfork keeps the existing copied-child path"
        );
        assert!(
            process_child_registration(libc::SYS_clone3, 91_005, 8, 0, libc::SIGUSR2).is_some()
        );
    }

    #[test]
    fn process_clone_properties_cover_vfork_and_shared_files_forms() {
        let empty = [0; 6];
        assert_eq!(
            process_clone_properties(libc::SYS_fork, &empty, |_, _| {
                panic!("fork has no clone flags in guest memory")
            }),
            ProcessCloneProperties::default()
        );
        assert_eq!(
            process_clone_properties(libc::SYS_vfork, &empty, |_, _| {
                panic!("vfork has no clone flags in guest memory")
            }),
            ProcessCloneProperties {
                blocks_parent: true,
                shares_files_without_thread: false,
                shares_memory_without_thread: false,
            }
        );

        let mut clone = [0; 6];
        clone[0] = libc::CLONE_FILES as u64;
        assert!(
            process_clone_properties(libc::SYS_clone, &clone, |_, _| false)
                .shares_files_without_thread
        );
        clone[0] = (libc::CLONE_FILES | libc::CLONE_THREAD) as u64;
        assert!(
            !process_clone_properties(libc::SYS_clone, &clone, |_, _| false)
                .shares_files_without_thread
        );
        clone[0] = libc::CLONE_VM as u64;
        assert!(
            process_clone_properties(libc::SYS_clone, &clone, |_, _| false)
                .shares_memory_without_thread
        );
        clone[0] = (libc::CLONE_VM | libc::CLONE_THREAD) as u64;
        assert!(
            !process_clone_properties(libc::SYS_clone, &clone, |_, _| false)
                .shares_memory_without_thread
        );
        clone[0] = libc::CLONE_VFORK as u64;
        let vfork = process_clone_properties(libc::SYS_clone, &clone, |_, _| false);
        assert!(vfork.blocks_parent);
        assert!(!vfork.shares_memory_without_thread);

        let address = 0x2345;
        let mut clone3 = [0; 6];
        clone3[0] = address;
        clone3[1] = 64;
        assert!(
            process_clone_properties(libc::SYS_clone3, &clone3, |observed, bytes| {
                assert_eq!(observed, address as usize);
                bytes.copy_from_slice(&(libc::CLONE_FILES as u64).to_ne_bytes());
                true
            })
            .shares_files_without_thread
        );
        assert!(
            process_clone_properties(libc::SYS_clone3, &clone3, |_, bytes| {
                bytes.copy_from_slice(&(libc::CLONE_VFORK as u64).to_ne_bytes());
                true
            })
            .blocks_parent
        );
        clone3[1] = 63;
        assert_eq!(
            process_clone_properties(libc::SYS_clone3, &clone3, |_, _| {
                panic!("short clone3 input must not be read")
            }),
            ProcessCloneProperties::default()
        );
        clone3[1] = 64;
        assert_eq!(
            process_clone_properties(libc::SYS_clone3, &clone3, |_, _| false),
            ProcessCloneProperties::default()
        );
    }

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
    fn current_scheduler_identity_requires_a_positive_virtual_tid() {
        let scheduler_tid: i32 = RuntimeAbi::Current.scheduler_tid(4, 44_004).unwrap().into();
        assert_eq!(scheduler_tid, 4);
        assert_eq!(RuntimeAbi::Current.scheduler_tid(0, 44_004), None);
        assert_eq!(RuntimeAbi::Current.scheduler_tid(-1, 44_004), None);
    }

    #[test]
    fn prepare_exec_uses_scheduler_tid_and_physical_pid() {
        let scheduler_tid = Pid::from_raw(4);
        let physical_pid = 918_851;

        assert_eq!(
            prepare_exec_guest_identity(scheduler_tid, physical_pid),
            (scheduler_tid, Pid::from_raw(physical_pid))
        );
        assert!(should_send_dbt_prepare_exec(
            true,
            physical_pid + 1,
            physical_pid
        ));
        assert!(should_send_dbt_prepare_exec(
            true,
            physical_pid,
            physical_pid
        ));
        assert!(!should_send_dbt_prepare_exec(
            false,
            physical_pid + 1,
            physical_pid
        ));
    }

    #[test]
    fn lifecycle_syscalls_follow_thread_state_initialization() {
        let source = include_str!("lib.rs");
        let dispatch = source
            .split_once("pub unsafe extern \"C\" fn reverie_dbt_runtime_pre_syscall")
            .expect("pre-syscall callback")
            .1;
        let initialize = dispatch
            .find("if scratch.runtime_state.is_null()")
            .expect("lazy thread-state initialization");
        let lifecycle = dispatch
            .find("if requires_native_lifecycle(sysnum)")
            .expect("native lifecycle early return");
        let vfork_policy = dispatch
            .find("if clone_properties.blocks_parent")
            .expect("vfork flock policy");

        assert!(
            initialize < lifecycle,
            "fork/clone/exec must not return before lazy thread-state initialization"
        );
        assert!(
            vfork_policy < lifecycle,
            "vfork/CLONE_VFORK flock state must be checked before native execution"
        );
        assert!(
            !dispatch[..lifecycle].contains("if tid == pid && !scratch.runtime_state.is_null()"),
            "nonleader exec must send PrepareExec"
        );
    }

    #[test]
    fn version_one_child_handoff_matches_on_physical_identity() {
        let config = Config::default();
        let tool: Detcore = Detcore::new(Pid::from_raw(44_003), &config);
        let physical_child_tid = 44_005;
        let virtual_parent_pid = 3;
        let selected_child_tid = RuntimeAbi::V1
            .scheduler_tid(0, physical_child_tid)
            .expect("ABI v1 must select the physical child tid");
        let rng_entropy = dbt_child_rng_entropy(virtual_parent_pid, 1)
            .expect("ABI v1 child RNG still requires the published virtual process identity");
        assert_eq!(rng_entropy, (virtual_parent_pid as u128) << 64 | 1);
        assert_ne!(
            rng_entropy,
            dbt_child_rng_entropy(44_003, 1).expect("physical pid is positive")
        );
        let mut pending = HashMap::new();
        insert_pending_thread_parent(
            &mut pending,
            physical_child_tid,
            PendingThreadParent {
                parent_tid: Tid::from_raw(44_004),
                virtual_child_tid: selected_child_tid,
                rng_entropy,
                state: tool.init_thread_state(Tid::from_raw(44_004), None),
            },
        )
        .unwrap();

        let observed_child_tid = RuntimeAbi::V1
            .scheduler_tid(77, physical_child_tid)
            .expect("ABI v1 must ignore a differing virtual child tid");
        let parent = resolve_pending_thread_parent(
            &mut pending,
            physical_child_tid,
            observed_child_tid,
            |_| panic!("matching ABI v1 handoff emitted a mismatch diagnostic"),
        )
        .expect("ABI v1 physical child handoff must match subsequent thread init");
        assert_eq!(parent.virtual_child_tid, Tid::from_raw(physical_child_tid));
        assert!(pending.is_empty());
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#2348)
    /// THE WIRING BRACKET. The other identity tests exercise `RuntimeAbi` as a
    /// pure selector; this one reaches the assignment that delivers the selected
    /// process id and host thread ID into the thread state. It fails when either
    /// assignment is removed or fed the wrong identity.
    #[test]
    fn dbt_thread_state_publishes_scheduler_and_host_identities() {
        let config = Config::default();
        let tool: Detcore = Detcore::new(Pid::from_raw(3), &config);

        // Host ids deliberately unlike the virtual ones, and unlike each other,
        // so a state that published EITHER host value is distinguishable from
        // one that published the virtual pid. These are the real host tids
        // observed in the DETLOG this branch fixes.
        let (host_tid, host_pid) = (918_946, 918_851);
        let (virtual_tid, virtual_pid) = (4, 3);

        let (det_tid, det_pid) = RuntimeAbi::Current
            .runtime_identity(virtual_tid, virtual_pid, host_tid, host_pid)
            .expect("current callbacks publish positive virtual ids");
        let state = init_dbt_thread_state(
            &tool,
            det_tid,
            DetTid::from_raw(det_pid.into()),
            host_tid,
            None,
        );
        assert_eq!(
            state.detpid,
            Some(DetTid::from_raw(virtual_pid)),
            "current callbacks must publish the client's virtual pid into the thread state; this \
             is the value that becomes ResourceID::MemAddrSpace and the scheduler's \
             process identity"
        );
        assert_ne!(
            state.detpid,
            Some(DetTid::from_raw(host_pid)),
            "the host pid must not replace current callback path's published virtual process identity"
        );
        assert_eq!(state.physical_tid, Some(host_tid));

        // v1 selects the PHYSICAL pid, so the same helper must carry a different
        // value. Without this the test would pass against an implementation that
        // ignored the ABI and hard-coded the virtual id.
        let (det_tid_v1, det_pid_v1) = RuntimeAbi::V1
            .runtime_identity(virtual_tid, virtual_pid, host_tid, host_pid)
            .expect("v1 falls back to positive physical ids");
        let state_v1 = init_dbt_thread_state(
            &tool,
            det_tid_v1,
            DetTid::from_raw(det_pid_v1.into()),
            host_tid,
            None,
        );
        assert_eq!(
            state_v1.detpid,
            Some(DetTid::from_raw(host_pid)),
            "v1 keeps physical process identity"
        );
        assert_eq!(state_v1.physical_tid, Some(host_tid));
        assert_ne!(
            state.detpid, state_v1.detpid,
            "if the two ABIs published the same pid this bracket would prove nothing"
        );
    }

    #[test]
    fn pending_thread_parent_distinguishes_absent_mismatch_and_match() {
        let make_parent = |virtual_child_tid| {
            let config = Config::default();
            let tool: Detcore = Detcore::new(Pid::from_raw(3), &config);
            PendingThreadParent {
                parent_tid: Tid::from_raw(3),
                virtual_child_tid: Tid::from_raw(virtual_child_tid),
                rng_entropy: dbt_child_rng_entropy(3, 1).unwrap(),
                state: tool.init_thread_state(Tid::from_raw(3), None),
            }
        };

        let mut pending = HashMap::new();
        let mut diagnostic = None;
        assert!(matches!(
            resolve_pending_thread_parent(&mut pending, 42_001, Tid::from_raw(4), |message| {
                diagnostic = Some(message.to_owned());
            }),
            Err(1)
        ));
        assert!(diagnostic.is_none(), "an absent handoff is not an error");

        assert!(insert_pending_thread_parent(&mut pending, 42_001, make_parent(4)).is_ok());
        assert!(matches!(
            resolve_pending_thread_parent(&mut pending, 42_001, Tid::from_raw(9), |message| {
                diagnostic = Some(message.to_owned());
            }),
            Err(-1)
        ));
        assert!(pending.is_empty(), "a mismatched stale handoff is removed");

        assert!(insert_pending_thread_parent(&mut pending, 42_001, make_parent(4)).is_ok());
        diagnostic = None;
        let parent =
            resolve_pending_thread_parent(&mut pending, 42_001, Tid::from_raw(4), |message| {
                diagnostic = Some(message.to_owned())
            })
            .expect("matching physical and virtual identities must consume the handoff");
        assert!(diagnostic.is_none(), "a matching handoff is not an error");
        assert_eq!(parent.virtual_child_tid, Tid::from_raw(4));
        assert!(pending.is_empty());
    }

    #[test]
    fn duplicate_pending_thread_parent_is_refused_and_cleans_stale_entry() {
        let make_parent = |virtual_child_tid| {
            let config = Config::default();
            let tool: Detcore = Detcore::new(Pid::from_raw(3), &config);
            PendingThreadParent {
                parent_tid: Tid::from_raw(3),
                virtual_child_tid: Tid::from_raw(virtual_child_tid),
                rng_entropy: dbt_child_rng_entropy(3, 1).unwrap(),
                state: tool.init_thread_state(Tid::from_raw(3), None),
            }
        };

        let mut pending = HashMap::new();
        assert!(insert_pending_thread_parent(&mut pending, 42_001, make_parent(4)).is_ok());
        assert_eq!(
            insert_pending_thread_parent(&mut pending, 42_001, make_parent(9)),
            Err(Tid::from_raw(4))
        );
        assert!(pending.is_empty(), "a duplicate leaves no stale handoff");
    }

    #[test]
    fn self_identity_translation_uses_virtual_targets_even_with_v1_physical_scheduler_ids() {
        let (v1_scheduler_tid, v1_process_pid) = RuntimeAbi::V1
            .runtime_identity(4, 3, 10_004, 10_003)
            .expect("ABI v1 physical callback identities are valid");
        assert_eq!(i32::from(v1_scheduler_tid), 10_004);
        assert_eq!(i32::from(v1_process_pid), 10_003);

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
    ///
    /// STATED REASON, `chown` / `fchown` / `fchownat` / `lchown` (#1851). These
    /// four were reclassified from `PassThrough` to `Determinized` so the
    /// ptrace path answers the ownership-permission question as the fixed
    /// virtual root rather than as whatever host identity the backend happens
    /// to run under. They are added here rather than fail-closed because in a
    /// copied child there is no virtual root to be consistent with: every
    /// syscall that establishes that identity already escapes this gate — the
    /// query family (`getuid`, `geteuid`, `getgid`, `getegid`, `getresuid`,
    /// `getresgid`) and the setter family (`setuid`, `setgid`, `setreuid`,
    /// `setresuid`, `setresgid`) are all rows above. Refusing only the
    /// ownership MUTATION while the identity it is checked against is still
    /// the host's would not restore the contract; it would just deny an
    /// operation that natively succeeds, in a window (fork to exec) where the
    /// ptrace path's emulation is unavailable anyway. `utimensat` is the
    /// existing precedent: a `Determinized` metadata mutator acknowledged as
    /// running natively here. This row shrinks when the copied-child ABI can
    /// carry an emulated identity, not before.
    // `flock` remains in this syscall-level register because the zero-argument
    // census represents a malformed operation, which intentionally reaches the
    // kernel for `EINVAL`. Dedicated argument tests prove blocking modes refuse.
    const ACKNOWLEDGED_STRICT_COPIED_CHILD_ESCAPES: &[&str] = &[
        "accept",
        "accept4",
        "adjtimex",
        "alarm",
        "arch_prctl",
        "bind",
        "chown",
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
        "fchown",
        "fchownat",
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
        "lchown",
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

        let mut scratch = NativeThreadScratch {
            branches: 0,
            observed_syscalls: 0,
            rewritten_syscalls: 0,
            runtime_state: std::ptr::null_mut(),
            pending_thread_clone: 0,
            thread_clone_flags: 0,
            thread_clone_ctid: 0,
            pending_thread_start: 0,
            virtual_pid: 3,
            virtual_ppid: 1,
            virtual_tid: 3,
            pending_virtual_child: 4,
            pending_clone_flags: libc::CLONE_THREAD as u64,
        };
        let status = unsafe {
            reverie_dbt_runtime_thread_init(
                std::ptr::from_mut(&mut scratch).cast(),
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
        assert_eq!(scratch.branches, 99);
        assert_eq!(scratch.observed_syscalls, 0);
        assert_eq!(scratch.rewritten_syscalls, 0);
        assert!(scratch.runtime_state.is_null());
        assert_eq!(scratch.pending_thread_clone, 0);
        assert_eq!(scratch.thread_clone_flags, 0);
        assert_eq!(scratch.thread_clone_ctid, 0);
        assert_eq!(scratch.pending_thread_start, 0);
        assert_eq!(scratch.virtual_pid, 3);
        assert_eq!(scratch.virtual_ppid, 1);
        assert_eq!(scratch.virtual_tid, 3);
        assert_eq!(scratch.pending_virtual_child, 4);
        assert_eq!(scratch.pending_clone_flags, libc::CLONE_THREAD as u64);
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

        for operation in [libc::LOCK_SH, libc::LOCK_EX] {
            let mut args = [0; 6];
            args[1] = operation as u64;
            assert_eq!(
                copied_child_action_with_args(libc::SYS_flock, args),
                -libc::ENOLCK,
                "a copied child must not enter a blocking flock operation",
            );
        }
        for operation in [
            0,
            libc::LOCK_SH | libc::LOCK_EX,
            libc::LOCK_SH | libc::LOCK_NB,
            libc::LOCK_EX | libc::LOCK_NB,
            libc::LOCK_UN,
            libc::LOCK_UN | libc::LOCK_NB,
        ] {
            let mut args = [0; 6];
            args[1] = operation as u64;
            assert_eq!(
                copied_child_action_with_args(libc::SYS_flock, args),
                0,
                "operation {operation} must retain kernel validation or safe nonblocking behavior",
            );
        }
        assert_eq!(
            unsafe { reverie_dbt_runtime_copied_syscall(libc::SYS_flock, std::ptr::null()) },
            -libc::ENOLCK,
            "missing copied-child flock arguments must fail closed",
        );

        COPIED_PANIC_ON_UNSUPPORTED.store(previous, Ordering::Release);
    }
}
