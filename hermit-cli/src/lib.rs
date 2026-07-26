/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Treat all Clippy warnings as errors.
#![deny(clippy::all)]
#![allow(clippy::uninlined_format_args)]

mod chroot;
mod consts;
mod desync;
// TODO-HUMAN-REVIEW(PR-594): Review the public e9patch preprocessing API.
pub mod e9patch;
mod error;
mod event;
mod event_stream;
mod fd;
mod id;
pub mod instruction_map;
mod interp;
mod metadata;
mod record;
mod recorder;
mod replay;
mod replayer;
mod sabre_ptrace;
mod script;

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::io::Write;
use std::os::fd::FromRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::anyhow;
use clap::ValueEnum;
use consts::METADATA_NAME;
pub use detcore::Config as DetConfig;
pub use detcore::Detcore;
pub use detcore::RecordOrReplay;
#[doc(hidden)]
pub use detcore_dbi::reverie_dbi_runtime_background_init;
#[doc(hidden)]
pub use detcore_dbi::reverie_dbi_runtime_name;
#[doc(hidden)]
pub use detcore_dbi::reverie_dbi_runtime_pre_syscall;
#[doc(hidden)]
pub use detcore_dbi::reverie_dbi_runtime_ready;
#[doc(hidden)]
pub use detcore_dbi::reverie_dbi_runtime_thread_exit;
#[doc(hidden)]
pub use detcore_dbi::reverie_dbi_runtime_thread_init;
#[doc(hidden)]
pub use detcore_dbi::reverie_dbi_runtime_totals;
pub use error::Context;
pub use error::Error;
pub use error::SerializableError;
pub use id::Id;
use metadata::Metadata;
use record::Record;
use replay::Replay;
pub use reverie::ExitStatus;
use reverie::GlobalTool;
pub use reverie::process;
pub use reverie::process::Command;
pub use reverie::process::Mount;
pub use reverie::process::Namespace;
pub use reverie::process::Output;
pub use reverie::process::Stdio;
pub use script::Shebang;
use serde::Deserialize;
use serde::Serialize;

enum KvmStdinReservation {
    Open(fs::File),
    Closed,
}

static KVM_STDIN_RESERVATION: Mutex<Option<KvmStdinReservation>> = Mutex::new(None);

/// Saves stdin captured before Rust's process startup can reuse a closed fd 0.
pub fn reserve_kvm_stdin(stdin: Option<fs::File>) -> io::Result<()> {
    let mut reservation = KVM_STDIN_RESERVATION
        .lock()
        .map_err(|_| io::Error::other("KVM stdin reservation lock is poisoned"))?;
    if reservation.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "KVM stdin is already reserved",
        ));
    }
    *reservation = Some(match stdin {
        Some(file) => KvmStdinReservation::Open(file),
        None => KvmStdinReservation::Closed,
    });
    Ok(())
}

fn duplicate_current_stdin() -> io::Result<Option<fs::File>> {
    // SAFETY: F_DUPFD_CLOEXEC duplicates fd 0 without taking ownership of it.
    let duplicate = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_DUPFD_CLOEXEC, 3) };
    if duplicate >= 0 {
        // SAFETY: F_DUPFD_CLOEXEC returned a new owned descriptor.
        return Ok(Some(unsafe { fs::File::from_raw_fd(duplicate) }));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EBADF) {
        Ok(None)
    } else {
        Err(error)
    }
}

fn ensure_kvm_stdin_reserved() -> io::Result<()> {
    let mut reservation = KVM_STDIN_RESERVATION
        .lock()
        .map_err(|_| io::Error::other("KVM stdin reservation lock is poisoned"))?;
    if reservation.is_none() {
        *reservation = Some(match duplicate_current_stdin()? {
            Some(file) => KvmStdinReservation::Open(file),
            None => KvmStdinReservation::Closed,
        });
    }
    Ok(())
}

fn reserved_kvm_stdin() -> Result<Option<fs::File>, Error> {
    ensure_kvm_stdin_reserved()?;
    let reservation = KVM_STDIN_RESERVATION
        .lock()
        .map_err(|_| io::Error::other("KVM stdin reservation lock is poisoned"))?;
    match reservation.as_ref() {
        Some(KvmStdinReservation::Open(file)) => Ok(Some(file.try_clone()?)),
        Some(KvmStdinReservation::Closed) => Ok(None),
        None => unreachable!("stdin reservation was initialized above"),
    }
}

/// The result of recording a command.
#[derive(Debug, Serialize, Deserialize)]
pub struct Recording {
    /// The unique ID of the recording.
    pub id: Id,

    /// The exit code of the command.
    pub exit_status: ExitStatus,
}

#[derive(Clone, Copy)]
enum CapabilityProbe {
    Namespaces,
    Ptrace,
    Seccomp,
}

fn run_capability_probe(probe: CapabilityProbe) -> Result<bool, Error> {
    // SAFETY: The child calls only async-signal-safe syscalls and exits immediately.
    let pid = unsafe { libc::fork() };
    if pid == -1 {
        return Err(std::io::Error::last_os_error()).context("Failed to fork capability probe");
    }
    if pid == 0 {
        let supported = match probe {
            CapabilityProbe::Namespaces => unsafe {
                libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWPID) == 0
            },
            CapabilityProbe::Ptrace => {
                // SAFETY: PTRACE_TRACEME ignores the pid and address arguments.
                unsafe {
                    libc::ptrace(
                        libc::PTRACE_TRACEME,
                        0,
                        std::ptr::null_mut::<libc::c_void>(),
                        std::ptr::null_mut::<libc::c_void>(),
                    ) != -1
                }
            }
            CapabilityProbe::Seccomp => {
                let mut filter = libc::sock_filter {
                    code: 0x06, // BPF_RET | BPF_K
                    jt: 0,
                    jf: 0,
                    k: 0x7fff0000, // SECCOMP_RET_ALLOW
                };
                let program = libc::sock_fprog {
                    len: 1,
                    filter: &mut filter,
                };
                // SAFETY: The filter is an allow-all program with a valid one-element lifetime.
                unsafe {
                    libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == 0
                        && libc::syscall(
                            libc::SYS_seccomp,
                            1, // SECCOMP_SET_MODE_FILTER
                            0,
                            &program,
                        ) == 0
                }
            }
        };
        // SAFETY: Avoid running Rust destructors after fork.
        unsafe { libc::_exit(i32::from(!supported)) }
    }

    let mut status = 0;
    loop {
        // SAFETY: pid is the child created above and status points to valid storage.
        let result = unsafe { libc::waitpid(pid, &mut status, 0) };
        if result == pid {
            return Ok(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
        }
        if result == -1 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("Failed to wait for capability probe");
        }
    }
}

fn validate_tracing_environment() -> Result<(), Error> {
    if !run_capability_probe(CapabilityProbe::Namespaces)? {
        anyhow::bail!(
            "Hermit cannot create its required user and PID namespaces: \
             unshare(CLONE_NEWUSER | CLONE_NEWPID) was denied. Allow unprivileged user namespaces \
             and the unshare syscall in the host/container policy."
        );
    }
    if !run_capability_probe(CapabilityProbe::Ptrace)? {
        anyhow::bail!(
            "Hermit cannot use ptrace in this environment: a child PTRACE_TRACEME probe was \
             denied. Allow same-UID parent-child ptrace in the container seccomp and host \
             Yama/LSM policy; CAP_SYS_PTRACE is normally not required. Use --namespace-only for \
             a sandbox smoke test without syscall interception."
        );
    }
    if !run_capability_probe(CapabilityProbe::Seccomp)? {
        anyhow::bail!(
            "Hermit cannot install its tracee seccomp filter: \
             seccomp(SECCOMP_SET_MODE_FILTER) was denied. Allow seccomp and \
             prctl(PR_SET_NO_NEW_PRIVS) in the container policy, or use --namespace-only for a \
             sandbox smoke test without syscall interception."
        );
    }
    Ok(())
}

fn is_dynamorio_sdk(path: &Path) -> bool {
    path.join("include/dr_api.h").is_file()
        || path.join("DynamoRIOConfig.cmake").is_file()
        || path.join("cmake/DynamoRIOConfig.cmake").is_file()
}

fn dynamorio_sdk_available() -> bool {
    if reverie_dbi::bundled_drrun_path().is_file() {
        return true;
    }
    const DEFAULT_ROOTS: [&str; 3] = [
        "/usr/lib/cmake/DynamoRIO",
        "/usr/local/lib/cmake/DynamoRIO",
        "/opt/dynamorio",
    ];

    ["DYNAMORIO_HOME", "DynamoRIO_DIR"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .chain(DEFAULT_ROOTS.into_iter().map(PathBuf::from))
        .any(|path| is_dynamorio_sdk(&path))
}

fn dbi_runtime_unavailable_reason() -> Option<String> {
    detcore_dbi::runtime_library_path().err().map(|error| {
        format!(
            "the Detcore DBI runtime is unavailable: {error}; build the hermit binary and \
             cdylib in the same target directory"
        )
    })
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#688): Review LiteInst runtime discovery.
/// Returns the LiteInst preload cdylib produced beside the Hermit binary.
#[doc(hidden)]
pub fn liteinst_runtime_library_path() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os("HERMIT_LITEINST_RUNTIME") {
        let path = PathBuf::from(path);
        return path.is_file().then_some(path).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "HERMIT_LITEINST_RUNTIME does not name a regular file",
            )
        });
    }

    let executable = std::env::current_exe()?;
    let directory = executable.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Hermit executable has no parent directory",
        )
    })?;
    [
        directory.join("libdetcore_liteinst.so"),
        directory.join("deps/libdetcore_liteinst.so"),
    ]
    .into_iter()
    .find(|path| path.is_file())
    .ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "libdetcore_liteinst.so was not built beside {}",
                executable.display()
            ),
        )
    })
}

fn liteinst_runtime_unavailable_reason() -> Option<String> {
    liteinst_runtime_library_path().err().map(|error| {
        format!(
            "the LiteInst preload runtime is unavailable: {error}; build detcore-liteinst and hermit in the same target directory"
        )
    })
}

fn kvm_device_unavailable_reason(path: &Path) -> Option<String> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .err()
        .map(|error| {
            format!(
                "cannot open {} read-write: {error}; grant access through the device owner/group \
                 or root",
                path.display()
            )
        })
}

/// Process instrumentation backend used to run a Hermit guest.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, ValueEnum)]
pub enum Backend {
    /// Use Reverie's ptrace backend.
    #[default]
    Ptrace,
    /// Use the DynamoRIO backend.
    Dbi,
    /// Use the LiteInst in-process backend with the Detcore Tool.
    Liteinst,
    /// Use the SaBRe static binary rewriting backend.
    Sabre,
    /// Use the KVM backend.
    Kvm,
    /// Preprocess the main ELF with e9patch, then use the ptrace runtime.
    // TODO-HUMAN-REVIEW(PR-594): Review the CLI-only hybrid backend selection.
    E9patch,
}

impl Backend {
    const ALL: [Self; 6] = [
        Self::Ptrace,
        Self::Dbi,
        Self::Liteinst,
        Self::Sabre,
        Self::Kvm,
        Self::E9patch,
    ];

    /// Returns the command-line spelling for this backend.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ptrace => "ptrace",
            Self::Dbi => "dbi",
            Self::Liteinst => "liteinst",
            Self::Sabre => "sabre",
            Self::Kvm => "kvm",
            Self::E9patch => "e9patch",
        }
    }

    /// Returns backends whose Hermit integration prerequisites are met.
    ///
    /// Some integrations use CLI launch adapters rather than direct
    /// [`run_with_backend`] dispatch.
    pub fn available() -> impl Iterator<Item = Self> {
        Self::ALL
            .into_iter()
            .filter(|backend| backend.is_available())
    }

    /// Returns whether this backend's integration prerequisites are met.
    pub fn is_available(self) -> bool {
        self.unavailable_reason().is_none()
    }

    /// Returns an actionable error when this backend's prerequisites are not met.
    pub fn ensure_available(self) -> Result<(), Error> {
        if let Some(reason) = self.unavailable_reason() {
            Err(anyhow!(
                "backend `{}` is unavailable: {reason}",
                self.as_str()
            ))
        } else {
            Ok(())
        }
    }

    fn unavailable_reason(self) -> Option<String> {
        match self {
            Self::Ptrace => validate_tracing_environment()
                .err()
                .map(|error| error.to_string()),
            Self::Dbi if !dynamorio_sdk_available() => Some(
                "the DynamoRIO SDK was not found; set DYNAMORIO_HOME or DynamoRIO_DIR to a valid SDK"
                    .to_owned(),
            ),
            Self::Dbi => dbi_runtime_unavailable_reason(),
            Self::Liteinst => liteinst_runtime_unavailable_reason(),
            // TODO-HUMAN-REVIEW(#589): Review SaBRe backend availability reporting.
            Self::Sabre => sabre_runtime_unavailable_reason(),
            Self::Kvm => kvm_device_unavailable_reason(Path::new("/dev/kvm")),
            Self::E9patch => validate_tracing_environment()
                .err()
                .map(|error| error.to_string())
                .or_else(e9patch::unavailable_reason),
        }
    }
}

const SABRE_BINARY_ENV: &str = "HERMIT_SABRE_BINARY";

fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

// TODO-HUMAN-REVIEW(PR-739): Review SaBRe loader discovery and executable validation.
fn resolve_sabre_binary_from(
    override_path: Option<&OsStr>,
    executable: &Path,
    path_env: &OsStr,
) -> Result<PathBuf, Error> {
    if let Some(requested) = override_path {
        if requested.is_empty() {
            return Err(anyhow!("{SABRE_BINARY_ENV} is empty"));
        }
        let path = PathBuf::from(requested);
        return is_executable_file(&path)
            .then_some(path.clone())
            .ok_or_else(|| {
                anyhow!(
                    "{SABRE_BINARY_ENV}={} is not an executable file",
                    path.display()
                )
            });
    }

    let directory = executable
        .parent()
        .ok_or_else(|| anyhow!("Hermit executable has no parent directory"))?;
    let sibling = directory.join("sabre");
    let target_build = directory.parent().map(|target| target.join("sabre/sabre"));

    if is_executable_file(&sibling) {
        return Ok(sibling);
    }
    if let Some(candidate) = &target_build
        && is_executable_file(candidate)
    {
        return Ok(candidate.clone());
    }
    if !path_env.is_empty()
        && let Some(candidate) = std::env::split_paths(path_env)
            .map(|directory| directory.join("sabre"))
            .find(|candidate| is_executable_file(candidate))
    {
        return Ok(candidate);
    }

    Err(anyhow!(
        "SaBRe executable was not found beside {} or in PATH; set {SABRE_BINARY_ENV} or build the pinned loader as target/sabre/sabre",
        executable.display()
    ))
}

fn resolve_sabre_binary() -> Result<PathBuf, Error> {
    let executable =
        std::env::current_exe().context("failed to locate running Hermit executable")?;
    let override_path = std::env::var_os(SABRE_BINARY_ENV);
    let path_env = std::env::var_os("PATH").unwrap_or_default();
    resolve_sabre_binary_from(override_path.as_deref(), &executable, &path_env)
}

const SABRE_RPC_SOCKET_ENV: &str = "REVERIE_SABRE_HERMIT_RPC_SOCKET";

// TODO-HUMAN-REVIEW(PR-738): Review controller/plugin artifact separation.
fn sabre_runtime_library_path() -> io::Result<PathBuf> {
    let executable = std::env::current_exe()?;
    let directory = executable.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Hermit executable has no parent directory",
        )
    })?;
    [
        directory.join("libdetcore_sabre.so"),
        directory.join("deps/libdetcore_sabre.so"),
    ]
    .into_iter()
    .find(|path| path.is_file())
    .ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "libdetcore_sabre.so was not built beside {}",
                executable.display()
            ),
        )
    })
}

fn sabre_runtime_unavailable_reason() -> Option<String> {
    if let Err(error) = resolve_sabre_binary() {
        return Some(error.to_string());
    }
    sabre_runtime_library_path().err().map(|error| {
        format!(
            "the Detcore SaBRe plugin is unavailable: {error}; build detcore-sabre and hermit in the same target directory"
        )
    })
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-774): Review the bounded SaBRe RPC disconnect drain.
const SABRE_RPC_DISCONNECT_TIMEOUT: Duration = Duration::from_secs(1);

async fn wait_for_sabre_rpc_disconnects<T>(
    global: &Arc<T>,
    timeout: Duration,
) -> Result<(), usize> {
    let disconnected = tokio::time::timeout(timeout, async {
        while Arc::strong_count(global) > 1 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await;

    if disconnected.is_ok() {
        return Ok(());
    }

    let live_references = Arc::strong_count(global).saturating_sub(1);
    if live_references == 0 {
        Ok(())
    } else {
        Err(live_references)
    }
}

fn ensure_backend_dispatch(backend: Backend) -> Result<(), Error> {
    // The CLI probes ptrace readiness before entering its container; repeating
    // the namespace probe here would test nested namespaces instead of the host.
    if backend == Backend::Ptrace {
        return Ok(());
    }
    if backend == Backend::E9patch {
        return Err(anyhow!(
            "backend `e9patch` requires CLI preprocessing; library callers must use \
             e9patch::prepare and then select `ptrace`"
        ));
    }
    // KVM and DBI have dedicated dispatches (`run_kvm` and `run_dbi`); neither
    // must reach this generic rejection path.
    backend.ensure_available()?;
    Err(anyhow!(
        "backend `{}` has no Hermit dispatch implementation",
        backend.as_str()
    ))
}

/// Run one command with the Detcore tool executing inside a SaBRe plugin and
/// the single GlobalState hosted by this Hermit coordinator process.
// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-738): Review SaBRe coordinator lifetime and artifact loading.
async fn run_sabre(
    mut command: Command,
    config: DetConfig,
    print_summary: bool,
    print_summary_to_json_file: &Option<PathBuf>,
    capture_output: bool,
) -> Result<Output, Error> {
    let sabre = resolve_sabre_binary()?;
    let plugin = sabre_runtime_library_path()
        .map_err(|error| anyhow!("failed to locate the Detcore SaBRe plugin: {error}"))?;
    let program = command.find_program().map_err(|error| {
        anyhow!(
            "failed to resolve SaBRe guest executable {:?}: {error}",
            command.get_program()
        )
    })?;

    let socket_dir = tempfile::Builder::new()
        .prefix("hermit-sabre-rpc-")
        .tempdir()?;
    let socket_path = socket_dir.path().join("coordinator.sock");
    let fallback_ready = Arc::new(AtomicBool::new(false));
    let global = Arc::new(detcore::GlobalState::init_global_state(&config).await);
    let server = reverie_rpc_transport::RpcServer::bind_with_readiness(
        &socket_path,
        global.clone(),
        config.clone(),
        fallback_ready.clone(),
    )
    .map_err(|error| anyhow!("failed to start SaBRe coordinator RPC: {error}"))?;
    let server_task = tokio::spawn(async move { server.serve().await });

    command.prepend_args([plugin.as_os_str(), OsStr::new("--"), program.as_os_str()]);
    command.program(&sabre);
    command.env(SABRE_RPC_SOCKET_ENV, &socket_path);
    command.env_remove("SABRE_BINARY");
    command.env_remove("SABRE_PLUGIN");

    tracing::info!(
        target: "hermit::sabre",
        guest = %program.display(),
        plugin = %plugin.display(),
        socket = %socket_path.display(),
        "launching Detcore guest through SaBRe with coordinator RPC",
    );

    let supervised = match sabre_ptrace::run(
        command.into_std_lossy(),
        PathBuf::from(&sabre),
        plugin.clone(),
        fallback_ready,
        capture_output,
    )
    .await
    {
        Ok(supervised) => supervised,
        Err(error) => {
            global.force_shutdown_with_error();
            server_task.abort();
            let _ = server_task.await;
            return Err(error);
        }
    };
    if !supervised.status.success() {
        global.force_shutdown_with_error();
    }
    tracing::info!(
        target: "hermit::sabre::fallback",
        patched_sites = supervised.patched_sites,
        "SaBRe ptrace fallback completed",
    );
    let output = Output {
        status: supervised.status.into(),
        stdout: supervised.stdout,
        stderr: supervised.stderr,
    };

    server_task.abort();
    let _ = server_task.await;
    wait_for_sabre_rpc_disconnects(&global, SABRE_RPC_DISCONNECT_TIMEOUT)
        .await
        .map_err(|live_references| {
            anyhow!("SaBRe coordinator stopped with {live_references} live RPC reference(s)")
        })?;
    let global = Arc::try_unwrap(global).map_err(|global| {
        anyhow!(
            "SaBRe coordinator stopped with {} live RPC reference(s)",
            Arc::strong_count(&global) - 1
        )
    })?;
    global
        .clean_up(print_summary, print_summary_to_json_file)
        .await;
    Ok(output)
}
/// Guest-physical memory available to the single-process KVM personality.
const KVM_GUEST_MEMORY_BYTES: usize = 256 * 1024 * 1024;

/// Maximum `#!` interpreter indirection levels, matching the Linux kernel's
/// `BINPRM_MAX_RECURSION` limit for chained script interpreters.
const MAX_SHEBANG_DEPTH: usize = 4;

/// Resolve `#!` interpreter scripts before the reverie-kvm ELF loader runs.
///
/// The KVM ELF loader can only map ELF images, so a guest program that is
/// actually a `#!`-script (for example `/usr/local/bin/file` -> `#!/bin/bash`,
/// or `/usr/bin/pkg-config` -> `#!/usr/bin/sh`) must be rewritten to launch its
/// interpreter, exactly as the kernel's `execve(2)` `binfmt_script` handler
/// does. On success the returned image is an ELF and `argv` has the interpreter,
/// its shebang arguments, and the script path prepended in kernel order:
/// `[interp, shebang_args.., script_path, <original argv[1..]>]`.
///
/// The interpreter line is parsed with hermit's shared [`Shebang`] so the KVM
/// backend matches how the ptrace backend and recorder treat `#!`-scripts.
fn resolve_kvm_shebang(
    resolved_program: &Path,
    mut argv: Vec<String>,
) -> Result<(PathBuf, Vec<String>, Vec<u8>), Error> {
    let mut load_path = resolved_program.to_path_buf();
    let mut image = fs::read(&load_path)
        .map_err(|error| anyhow!("failed to read KVM guest executable {load_path:?}: {error}"))?;

    let mut depth = 0;
    while image.starts_with(b"#!") {
        depth += 1;
        if depth > MAX_SHEBANG_DEPTH {
            return Err(anyhow!(
                "too many levels of `#!` interpreter indirection loading {resolved_program:?}"
            ));
        }
        let (interpreter, shebang_args) = Shebang::from_buf(&image)
            .ok_or_else(|| anyhow!("malformed `#!` interpreter line in {load_path:?}"))?
            .into_parts();
        let interpreter_str = interpreter
            .to_str()
            .ok_or_else(|| anyhow!("non-UTF-8 `#!` interpreter path in {load_path:?}"))?
            .to_owned();

        // Rewrite argv in kernel order. The prior argv[0] (the script's own
        // name) is dropped on the first level; on deeper levels the previous
        // interpreter path is preserved as a positional argument, matching
        // `binfmt_script`.
        let mut rewritten = Vec::with_capacity(argv.len() + shebang_args.len() + 2);
        rewritten.push(interpreter_str);
        for arg in &shebang_args {
            rewritten.push(
                arg.to_str()
                    .ok_or_else(|| anyhow!("non-UTF-8 `#!` interpreter argument in {load_path:?}"))?
                    .to_owned(),
            );
        }
        rewritten.push(load_path.to_string_lossy().into_owned());
        rewritten.extend_from_slice(&argv[1..]);
        argv = rewritten;

        load_path = interpreter;
        image = fs::read(&load_path).map_err(|error| {
            anyhow!(
                "failed to read `#!` interpreter {load_path:?} for {resolved_program:?}: {error}"
            )
        })?;
    }

    Ok((load_path, argv, image))
}

/// Dispatch a command onto the real reverie-kvm Tool runtime.
async fn run_kvm(
    command: &Command,
    mut config: DetConfig,
    print_summary: bool,
    print_summary_to_json_file: &Option<PathBuf>,
    capture_output: bool,
) -> Result<Output, Error> {
    let stdin = reserved_kvm_stdin()?;
    let requested_cwd = command
        .get_current_dir()
        .map(Path::to_owned)
        .unwrap_or(std::env::current_dir()?);
    let cwd = fs::canonicalize(&requested_cwd).map_err(|error| {
        anyhow!(
            "failed to resolve KVM guest working directory {:?}: {error}",
            requested_cwd
        )
    })?;
    let program = command
        .get_program()
        .to_str()
        .ok_or_else(|| anyhow!("KVM guest executable path is not valid UTF-8"))?
        .to_owned();
    if !cwd.is_dir() {
        return Err(anyhow!(
            "KVM guest working directory is not a directory: {:?}",
            cwd
        ));
    }
    let resolved_program = command.find_program().map_err(|error| {
        anyhow!("failed to resolve KVM guest executable {program:?} in the guest PATH: {error}")
    })?;
    let mut argv = Vec::with_capacity(1 + command.get_args().count());
    argv.push(program.clone());
    for argument in command.get_args() {
        argv.push(
            argument
                .to_str()
                .ok_or_else(|| anyhow!("KVM guest argument is not valid UTF-8"))?
                .to_owned(),
        );
    }

    // Rewrite `#!`-scripts to their interpreter before the ELF loader sees them.
    let (_interpreter_path, argv, image) = resolve_kvm_shebang(&resolved_program, argv)?;
    // After shebang resolution the executable is the interpreter (argv[0]).
    let program = argv.first().cloned().unwrap_or(program);
    let envp = command
        .get_captured_envs()
        .into_iter()
        .map(|(key, value)| {
            let key = key
                .to_str()
                .ok_or_else(|| anyhow!("KVM guest environment key is not valid UTF-8"))?;
            let value = value
                .to_str()
                .ok_or_else(|| anyhow!("KVM guest environment value is not valid UTF-8"))?;
            Ok(format!("{key}={value}"))
        })
        .collect::<Result<Vec<_>, Error>>()?;
    tracing::info!(
        target: "hermit::kvm",
        program = %program,
        argv = ?argv,
        cwd = %cwd.display(),
        env_count = envp.len(),
        "launching guest through reverie-kvm",
    );
    let argv = argv.iter().map(String::as_str).collect::<Vec<_>>();
    let envp = envp.iter().map(String::as_str).collect::<Vec<_>>();

    config.cpuid_virtualized_by_backend = true;
    config.backend_supports_madvise = false;
    let mut backend = reverie_kvm::KvmBackend::new_with_stdin(KVM_GUEST_MEMORY_BYTES, stdin)
        .map_err(|error| anyhow!("failed to initialize reverie-kvm: {error}"))?;
    backend
        .install_static_elf_with_context(&image, &argv, &envp, &cwd)
        .map_err(|error| anyhow!("failed to load KVM guest executable {program:?}: {error}"))?;

    let (global_state, code, stdout, stderr) = backend
        .run_static_elf_with_tool::<Detcore>(config, capture_output)
        .await
        .map_err(|error| anyhow!("KVM guest execution failed: {error}"))?;
    global_state
        .clean_up(print_summary, print_summary_to_json_file)
        .await;

    if !capture_output {
        std::io::stdout().write_all(&stdout)?;
        std::io::stderr().write_all(&stderr)?;
    }

    Ok(Output {
        status: ExitStatus::Exited(code),
        stdout,
        stderr,
    })
}

// TODO-HUMAN-REVIEW(PR-743): Review bounded relaunch before DBI guest execution.
fn dbi_client_thread_start_failed(status: &std::process::ExitStatus) -> bool {
    status.code() == Some(reverie_dbi::CLIENT_THREAD_START_FAILURE_EXIT_CODE)
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-737): Review public DBI dispatch and child environment ownership.
/// Dispatch a command onto the Detcore-linked reverie-dbi runtime.
async fn run_dbi(
    command: Command,
    config: DetConfig,
    print_summary: bool,
    capture_output: bool,
) -> Result<Output, Error> {
    if !config.sequentialize_threads {
        return Err(anyhow!(
            "the dbi backend requires sequentialized threads; remove \
             --no-sequentialize-threads (or --strace-only) to run under --backend dbi"
        ));
    }

    let config_json = serde_json::to_string(&config)
        .map_err(|error| anyhow!("failed to serialize the Detcore config for DBI: {error}"))?;
    let panic_on_unsupported_syscalls = config.panic_on_unsupported_syscalls;
    let (drrun, client) = detcore_dbi::prepare_native_client()
        .map_err(|error| anyhow!("failed to prepare the Detcore DynamoRIO client: {error}"))?;
    let mut runner = reverie_dbi::DbiRunner::new(&drrun, &client)
        .map_err(|error| {
            anyhow!(
                "failed to configure the DynamoRIO DBI runner (drrun={}, client={}): {error}",
                drrun.display(),
                client.display()
            )
        })?
        .summary(print_summary)
        .isolated_process_group(panic_on_unsupported_syscalls);
    if panic_on_unsupported_syscalls {
        runner = runner.client_argument("-panic-on-unsupported-syscalls");
    }

    let program = command.get_program().to_owned();
    let mut environment = command.get_captured_envs();
    environment.insert(detcore_dbi::DETCONFIG_ENV.into(), config_json.into());
    let guest = command.into_std_lossy();
    tracing::info!(
        target: "hermit::dbi",
        program = ?program,
        drrun = %drrun.display(),
        client = %client.display(),
        "launching guest through reverie-dbi with Detcore<DbiGuest>",
    );

    if capture_output {
        let launch = || {
            runner
                .output_with_environment(&guest, &environment)
                .map_err(|error| anyhow!("failed to launch drrun ({}): {error}", drrun.display()))
        };
        let mut output = launch()?;
        if dbi_client_thread_start_failed(&output.status) {
            tracing::warn!(
                target: "hermit::dbi",
                "DynamoRIO client thread failed before guest start; retrying once",
            );
            output = launch()?;
        }
        return Ok(Output {
            status: output.status.into(),
            stdout: output.stdout,
            stderr: output.stderr,
        });
    }

    let launch = || {
        runner
            .status_with_environment(&guest, &environment)
            .map_err(|error| anyhow!("failed to launch drrun ({}): {error}", drrun.display()))
    };
    let mut status = launch()?;
    if dbi_client_thread_start_failed(&status) {
        tracing::warn!(
            target: "hermit::dbi",
            "DynamoRIO client thread failed before guest start; retrying once",
        );
        status = launch()?;
    }
    Ok(Output {
        status: status.into(),
        stdout: Vec::new(),
        stderr: Vec::new(),
    })
}

// NOTE: A single-threaded executor is used here so that the tokio threads
// themselves wouldn't contribute non-determinism to the PID namespace. This
// could also be changed to a specific number of threads and that would be
// deterministic, but it shouldn't be based on the number of cores. When the
// thread count is based off of the number of cores in the machine, then two
// runs on different machines with a different number of cores will not be the
// same.
/// Run the given command as deterministically as possible.
pub fn run(
    command: Command,
    config: DetConfig,
    print_summary: bool,
    print_summary_to_json_file: &Option<PathBuf>,
) -> Result<ExitStatus, Error> {
    run_with_backend(
        command,
        config,
        print_summary,
        print_summary_to_json_file,
        Backend::Ptrace,
    )
}

/// Run the given command using the selected instrumentation backend.
pub fn run_with_backend(
    command: Command,
    config: DetConfig,
    print_summary: bool,
    print_summary_to_json_file: &Option<PathBuf>,
    backend: Backend,
) -> Result<ExitStatus, Error> {
    if backend == Backend::Kvm {
        ensure_kvm_stdin_reserved()?;
    }
    let config = prepare_backend_config(config, backend);
    run_with_backend_inner(
        command,
        config,
        print_summary,
        print_summary_to_json_file,
        backend,
    )
}

// TODO-HUMAN-REVIEW(PR-749): Review LiteInst backend configuration normalization.
fn prepare_backend_config(mut config: DetConfig, backend: Backend) -> DetConfig {
    if backend == Backend::Liteinst && config.max_timeslice.is_some() {
        eprintln!(
            "WARNING: --backend=liteinst does not implement PMU/RCB timer delivery; continuing with --max-timeslice=disabled."
        );
        config.max_timeslice = None;
    }
    config
}

// TODO-HUMAN-REVIEW(PR-736): Review reserved LiteInst runtime failure statuses.
fn liteinst_requires_forced_shutdown(status: ExitStatus) -> bool {
    matches!(
        status,
        ExitStatus::Exited(122..=127) | ExitStatus::Signaled(_, _)
    )
}

#[tokio::main(flavor = "current_thread")]
async fn run_with_backend_inner(
    command: Command,
    config: DetConfig,
    print_summary: bool,
    print_summary_to_json_file: &Option<PathBuf>,
    backend: Backend,
) -> Result<ExitStatus, Error> {
    if backend == Backend::Kvm {
        return Ok(run_kvm(
            &command,
            config,
            print_summary,
            print_summary_to_json_file,
            false,
        )
        .await?
        .status);
    }
    if backend == Backend::Dbi {
        return Ok(run_dbi(command, config, print_summary, false).await?.status);
    }
    if backend == Backend::Sabre {
        return Ok(run_sabre(
            command,
            config,
            print_summary,
            print_summary_to_json_file,
            false,
        )
        .await?
        .status);
    }
    if backend == Backend::Liteinst {
        let preload = liteinst_runtime_library_path()?;
        let (exit_status, mut global_state) =
            reverie_liteinst::LiteinstBackend::run_with_preload::<Detcore>(
                command, config, preload,
            )
            .await?;
        if liteinst_requires_forced_shutdown(exit_status) {
            global_state.force_shutdown_with_error();
            global_state.cancel_internal_scheduler().await;
        }
        global_state
            .clean_up(print_summary, print_summary_to_json_file)
            .await;
        return Ok(exit_status);
    }
    ensure_backend_dispatch(backend)?;

    let mut builder = reverie_ptrace::TracerBuilder::<Detcore>::new(command).config(config.clone());
    if config.gdbserver {
        builder = builder.gdbserver(config.gdbserver_port);
    }
    let (exit_status, global_state) = builder.spawn().await?.wait().await?;
    global_state
        .clean_up(print_summary, print_summary_to_json_file)
        .await; // Before it's dropped by this function.
    Ok(exit_status)
}

/// Variant of `run` that also captures stdout/stderr.
pub fn run_with_output(
    command: Command,
    config: DetConfig,
    print_summary: bool,
    print_summary_to_json_file: &Option<PathBuf>,
) -> Result<Output, Error> {
    run_with_output_backend(
        command,
        config,
        print_summary,
        print_summary_to_json_file,
        Backend::Ptrace,
    )
}

/// Variant of [`run_with_backend`] that also captures stdout/stderr.
pub fn run_with_output_backend(
    command: Command,
    config: DetConfig,
    print_summary: bool,
    print_summary_to_json_file: &Option<PathBuf>,
    backend: Backend,
) -> Result<Output, Error> {
    if backend == Backend::Kvm {
        ensure_kvm_stdin_reserved()?;
    }
    let config = prepare_backend_config(config, backend);
    run_with_output_backend_inner(
        command,
        config,
        print_summary,
        print_summary_to_json_file,
        backend,
    )
}

#[tokio::main(flavor = "current_thread")]
async fn run_with_output_backend_inner(
    mut command: Command,
    config: DetConfig,
    print_summary: bool,
    print_summary_to_json_file: &Option<PathBuf>,
    backend: Backend,
) -> Result<Output, Error> {
    if backend == Backend::Kvm {
        return run_kvm(
            &command,
            config,
            print_summary,
            print_summary_to_json_file,
            true,
        )
        .await;
    }
    if backend == Backend::Dbi {
        return run_dbi(command, config, print_summary, true).await;
    }
    if backend == Backend::Sabre {
        command.stdin(Stdio::null());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        return run_sabre(
            command,
            config,
            print_summary,
            print_summary_to_json_file,
            true,
        )
        .await;
    }
    if backend == Backend::Liteinst {
        command.stdin(Stdio::null());
        let preload = liteinst_runtime_library_path()?;
        let (output, mut global_state) =
            reverie_liteinst::LiteinstBackend::run_with_output_and_preload::<Detcore>(
                command, config, preload,
            )
            .await?;
        let status = output.status.into();
        if liteinst_requires_forced_shutdown(status) {
            global_state.force_shutdown_with_error();
            global_state.cancel_internal_scheduler().await;
        }
        global_state
            .clean_up(print_summary, print_summary_to_json_file)
            .await;
        return Ok(Output {
            status,
            stdout: output.stdout,
            stderr: output.stderr,
        });
    }
    ensure_backend_dispatch(backend)?;

    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let mut builder = reverie_ptrace::TracerBuilder::<Detcore>::new(command).config(config.clone());
    if config.gdbserver {
        builder = builder.gdbserver(config.gdbserver_port);
    }
    let (output, global_state) = builder.spawn().await?.wait_with_output().await?;
    global_state
        .clean_up(print_summary, print_summary_to_json_file)
        .await;
    Ok(output)
}

/// Holds the context necessary to run high-level hermit functions.
pub struct HermitData {
    // The data directory. Defaults to `~/.cache/hermit`. Note that we shouldn't
    // expect this to exist in any of the functions that are called.
    data_dir: PathBuf,
}

impl Default for HermitData {
    fn default() -> Self {
        Self::new()
    }
}

impl HermitData {
    /// Creates an instance of `HermitData` using `~/.cache/hermit` as the data
    /// directory.
    pub fn new() -> Self {
        Self::with_dir(
            dirs::cache_dir()
                .map_or_else(|| PathBuf::from("/tmp/hermit"), |dir| dir.join("hermit")),
        )
    }

    /// Creates a `HermitData` using the given directory as the base path for
    /// storing recording data.
    pub fn with_dir<P>(data_dir: P) -> Self
    where
        P: Into<PathBuf>,
    {
        Self {
            data_dir: data_dir.into(),
        }
    }

    /// Returns the path to the data directory where recordings are stored.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Records the execution of the given command, returning its `Recording`.
    ///
    /// If recording failed, then an error is returned. Note that if the command
    /// itself failed, then we still return a successful recording, but its exit
    /// status will be non-zero.
    pub fn record(&self, command: Command) -> Result<Recording, Error> {
        let data = self.create_recording_dir()?;
        let exit_status = record_to(command, data.path())?;
        self.commit_recording(data, exit_status)
    }

    /// Creates a temporary directory for a recording that has not been committed yet.
    pub fn create_recording_dir(&self) -> Result<tempfile::TempDir, Error> {
        let tmp_data_dir = self.data_dir.join("tmp");

        fs::create_dir_all(&tmp_data_dir).with_context(|| {
            format!(
                "Failed to create recording directory: {}",
                self.data_dir.display()
            )
        })?;

        Ok(tempfile::TempDir::new_in(tmp_data_dir)?)
    }

    /// Commits a completed temporary recording to the recording store.
    pub fn commit_recording(
        &self,
        data: tempfile::TempDir,
        exit_status: ExitStatus,
    ) -> Result<Recording, Error> {
        let id = Id::unique();

        // Atomically move the temporary recording to its final location.
        fs::rename(data.keep(), self.data_dir.join(id.to_string()))?;

        self.update_last_id(&id)
            .with_context(|| format!("Failed to update {:?}", self.data_dir.join("last")))?;

        Ok(Recording { id, exit_status })
    }

    /// Replays the given recording ID.
    pub fn replay(&self, id: Id) -> Result<ExitStatus, Error> {
        let data = self.data_dir.join(id.to_string());
        replay_from(&data)
    }

    /// Replays the given recording ID with a gdbserver available to attach to.
    pub fn replay_with_gdbserver(&self, id: Id, port: u16) -> Result<ExitStatus, Error> {
        let data = self.data_dir.join(id.to_string());
        replay_with_gdbserver(&data, port)
    }

    /// Returns an iterator over the recordings.
    ///
    /// Use [`Self::recording_metadata`] to get more information about a recording.
    pub fn recordings(&self) -> impl Iterator<Item = Id> + use<> {
        fs::read_dir(&self.data_dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                let entry = entry.ok()?;

                if entry.file_type().ok()?.is_dir() {
                    Some(entry.file_name().to_str()?.parse::<Id>().ok()?)
                } else {
                    None
                }
            })
    }

    /// Returns the metadata of a recording.
    pub fn recording_metadata(&self, id: Id) -> Result<Metadata, Error> {
        let mut metadata_path = self.data_dir.join(id.to_string());
        metadata_path.push(METADATA_NAME);

        let metadata: Metadata = serde_json::from_reader(
            fs::File::open(&metadata_path)
                .with_context(|| format!("Failed to open {:?}", metadata_path))?,
        )
        .with_context(|| format!("Failed to parse {:?}", metadata_path))?;

        Ok(metadata)
    }

    /// Deletes a recording.
    pub fn remove(&self, id: Id) -> Result<(), Error> {
        let path = self.data_dir.join(id.to_string());

        // Before deleting anything, make sure this file exists. This may not be a
        // recording if this file does not exist.
        let metadata_path = path.join(METADATA_NAME);
        let metadata = fs::metadata(&metadata_path)
            .with_context(|| format!("Failed to find {:?}", metadata_path))?;

        if !metadata.is_file() {
            return Err(anyhow!("{:?} is not a file", metadata_path));
        }

        // Do a recursive delete on the directory. Note that this does not follow
        // symlinks.
        fs::remove_dir_all(path)?;

        Ok(())
    }

    /// Returns the last recorded ID.
    pub fn last_id(&self) -> Result<Id, Error> {
        Ok(fs::read_to_string(self.data_dir.join("last"))?.parse()?)
    }

    /// Atomically updates the last recording ID.
    fn update_last_id(&self, id: &Id) -> Result<(), Error> {
        let mut file = tempfile::NamedTempFile::new_in(self.data_dir.join("tmp"))?;
        write!(file, "{}", id)?;
        file.persist(self.data_dir.join("last"))?;
        Ok(())
    }
}

impl<'a> From<Option<&'a PathBuf>> for HermitData {
    fn from(data_dir: Option<&'a PathBuf>) -> Self {
        data_dir.map_or_else(Self::new, Self::with_dir)
    }
}

/// Records to the specified directory, which must already exist.
#[tokio::main(flavor = "current_thread")]
pub async fn record_to(command: Command, dir: &Path) -> Result<ExitStatus, Error> {
    Ok(Record::spawn(command, dir).await?.wait().await?)
}

/// Records to the specified directory, which must already exist. The
/// stderr/stdout of the recording is captured in `Output`.
#[tokio::main(flavor = "current_thread")]
pub async fn record_with_output(mut command: Command, dir: &Path) -> Result<Output, Error> {
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    Ok(Record::spawn(command, dir)
        .await?
        .wait_with_output()
        .await?)
}

/// Replays from the specified directory.
#[tokio::main(flavor = "current_thread")]
pub async fn replay_from(dir: &Path) -> Result<ExitStatus, Error> {
    Ok(Replay::spawn(dir, false, None).await?.wait().await?)
}

/// Replays with a gdb server.
#[tokio::main(flavor = "current_thread")]
pub async fn replay_with_gdbserver(dir: &Path, port: u16) -> Result<ExitStatus, Error> {
    Ok(Replay::spawn(dir, false, Some(port)).await?.wait().await?)
}

/// Replays from the specified directory which must already exist. The
/// stderr/stdout of the replay is captured in `Output`.
#[tokio::main(flavor = "current_thread")]
pub async fn replay_with_output(dir: &Path) -> Result<Output, Error> {
    Ok(Replay::spawn(dir, true, None)
        .await?
        .wait_with_output()
        .await?)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use super::Backend;
    use super::ExitStatus;
    use super::SABRE_RPC_SOCKET_ENV;
    use super::dbi_runtime_unavailable_reason;
    use super::dynamorio_sdk_available;
    use super::ensure_backend_dispatch;
    use super::is_dynamorio_sdk;
    use super::kvm_device_unavailable_reason;
    use super::liteinst_requires_forced_shutdown;
    use super::liteinst_runtime_unavailable_reason;
    use super::prepare_backend_config;
    use super::resolve_kvm_shebang;
    use super::resolve_sabre_binary_from;
    use super::sabre_runtime_unavailable_reason;
    use super::wait_for_sabre_rpc_disconnects;

    #[test]
    fn liteinst_reserved_failures_require_scheduler_cancellation() {
        for status in 122..=127 {
            assert!(liteinst_requires_forced_shutdown(ExitStatus::Exited(
                status
            )));
        }
        assert!(!liteinst_requires_forced_shutdown(ExitStatus::Exited(121)));
        assert!(!liteinst_requires_forced_shutdown(ExitStatus::Exited(128)));
    }

    #[test]
    fn liteinst_backend_config_disables_unsupported_rcb_timeslices() {
        let config = super::DetConfig::default();
        assert!(config.max_timeslice.is_some());
        assert!(
            prepare_backend_config(config.clone(), Backend::Liteinst)
                .max_timeslice
                .is_none()
        );
        assert!(
            prepare_backend_config(config, Backend::Ptrace)
                .max_timeslice
                .is_some()
        );
    }

    #[test]
    fn liteinst_public_dispatch_runs_default_config_without_rcb_timers() {
        if Backend::Liteinst.ensure_available().is_err() {
            return;
        }

        let mut command = super::Command::new("/bin/echo");
        command.arg("hello");
        let output = super::run_with_output_backend(
            command,
            super::DetConfig::default(),
            false,
            &None,
            Backend::Liteinst,
        )
        .expect("run /bin/echo through LiteinstGuest<Detcore>");
        assert_eq!(output.status, super::ExitStatus::Exited(0));
        assert_eq!(output.stdout, b"hello\n");

        let status = super::run_with_backend(
            super::Command::new("/bin/true"),
            super::DetConfig::default(),
            false,
            &None,
            Backend::Liteinst,
        )
        .expect("run /bin/true through LiteinstGuest<Detcore>");
        assert_eq!(status, super::ExitStatus::Exited(0));
    }

    #[test]
    fn default_and_available_backends_reflect_host_probes() {
        assert_eq!(Backend::default(), Backend::Ptrace);
        let available = Backend::available().collect::<Vec<_>>();
        assert_eq!(
            available.contains(&Backend::Ptrace),
            Backend::Ptrace.is_available()
        );
        assert_eq!(
            available.contains(&Backend::Dbi),
            dynamorio_sdk_available() && dbi_runtime_unavailable_reason().is_none()
        );
        assert_eq!(
            available.contains(&Backend::Liteinst),
            liteinst_runtime_unavailable_reason().is_none()
        );
        assert_eq!(
            available.contains(&Backend::Sabre),
            sabre_runtime_unavailable_reason().is_none()
        );
        assert_eq!(
            available.contains(&Backend::Kvm),
            kvm_device_unavailable_reason(std::path::Path::new("/dev/kvm")).is_none(),
        );
        assert_eq!(
            available.contains(&Backend::E9patch),
            Backend::E9patch.is_available()
        );
    }

    #[test]
    fn dependency_probes_require_usable_paths() {
        let temp = tempfile::tempdir().unwrap();
        assert!(!is_dynamorio_sdk(temp.path()));
        fs::create_dir(temp.path().join("include")).unwrap();
        fs::write(temp.path().join("include/dr_api.h"), b"/* marker */").unwrap();
        assert!(is_dynamorio_sdk(temp.path()));

        let reason = kvm_device_unavailable_reason(temp.path())
            .expect("a directory must not pass the read-write KVM device probe");
        assert!(reason.contains("read-write"));
    }

    fn write_test_executable(path: &std::path::Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"test loader").unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn sabre_rpc_socket_uses_private_exec_environment() {
        assert!(SABRE_RPC_SOCKET_ENV.starts_with("REVERIE_SABRE_"));
    }

    #[tokio::test(start_paused = true)]
    async fn sabre_rpc_disconnect_wait_observes_delayed_release() {
        let global = Arc::new(());
        let connection = global.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            drop(connection);
        });

        assert_eq!(
            wait_for_sabre_rpc_disconnects(&global, Duration::from_millis(50)).await,
            Ok(())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn sabre_rpc_disconnect_wait_reports_stuck_connection() {
        let global = Arc::new(());
        let _connection = global.clone();

        assert_eq!(
            wait_for_sabre_rpc_disconnects(&global, Duration::from_millis(10)).await,
            Err(1)
        );
    }

    #[test]
    fn sabre_binary_resolver_finds_cargo_target_build() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("target/release/hermit");
        let loader = temp.path().join("target/sabre/sabre");
        write_test_executable(&loader);

        assert_eq!(
            resolve_sabre_binary_from(None, &executable, OsStr::new("")).unwrap(),
            loader
        );
    }

    #[test]
    fn sabre_binary_resolver_prefers_and_validates_override() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("target/release/hermit");
        let discovered = temp.path().join("target/sabre/sabre");
        let requested = temp.path().join("requested-sabre");
        write_test_executable(&discovered);
        write_test_executable(&requested);

        assert_eq!(
            resolve_sabre_binary_from(Some(requested.as_os_str()), &executable, OsStr::new(""))
                .unwrap(),
            requested
        );

        let mut permissions = fs::metadata(&requested).unwrap().permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(&requested, permissions).unwrap();
        let error =
            resolve_sabre_binary_from(Some(requested.as_os_str()), &executable, OsStr::new(""))
                .unwrap_err();
        assert!(error.to_string().contains("is not an executable file"));
    }

    #[test]
    fn optional_backends_report_accurate_availability() {
        match Backend::Dbi.ensure_available() {
            Ok(()) => assert!(
                dynamorio_sdk_available() && dbi_runtime_unavailable_reason().is_none(),
                "DBI reported available without its SDK and runtime"
            ),
            Err(dbi_error) => {
                let message = dbi_error.to_string();
                assert!(
                    message.contains("DynamoRIO SDK") || message.contains("Detcore DBI runtime"),
                    "unexpected DBI availability error: {message}"
                );
            }
        }
        assert_eq!(
            Backend::Liteinst.ensure_available().is_ok(),
            liteinst_runtime_unavailable_reason().is_none()
        );

        match Backend::Kvm.ensure_available() {
            Ok(()) => assert!(
                kvm_device_unavailable_reason(std::path::Path::new("/dev/kvm")).is_none(),
                "KVM reported available without a usable /dev/kvm",
            ),
            Err(kvm_error) => {
                let message = kvm_error.to_string();
                assert!(
                    message.contains("/dev/kvm"),
                    "unexpected KVM availability error: {message}",
                );
                assert!(!message.contains("requires root privileges"));
            }
        }
    }

    #[test]
    fn public_backend_dispatch_rejects_unprepared_e9patch() {
        let error = ensure_backend_dispatch(Backend::E9patch).unwrap_err();
        assert!(
            error.to_string().contains("requires CLI preprocessing"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn dbi_retries_only_the_pre_guest_bootstrap_failure() {
        use std::os::unix::process::ExitStatusExt as _;

        let failure = std::process::ExitStatus::from_raw(
            reverie_dbi::CLIENT_THREAD_START_FAILURE_EXIT_CODE << 8,
        );
        assert!(super::dbi_client_thread_start_failed(&failure));
        assert!(!super::dbi_client_thread_start_failed(
            &std::process::ExitStatus::from_raw(1 << 8)
        ));
    }

    #[test]
    fn dbi_public_dispatch_requires_sequentialized_threads() {
        let command = super::Command::new("/bin/true");
        let config = super::DetConfig {
            sequentialize_threads: false,
            ..Default::default()
        };

        let error = super::run_with_output_backend(command, config, false, &None, Backend::Dbi)
            .expect_err("DBI must reject non-sequentialized execution");
        assert!(
            error
                .to_string()
                .contains("dbi backend requires sequentialized threads"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn dbi_public_dispatch_runs_echo_through_detcore() {
        use clap::Parser;

        if Backend::Dbi.ensure_available().is_err() {
            return;
        }

        let mut command = super::Command::new("/bin/echo");
        command.arg("hello");
        let mut config = super::DetConfig::parse_from(["hermit-dbi-test"]);
        config.sequentialize_threads = true;
        config.validate();
        let output = super::run_with_output_backend(command, config, true, &None, Backend::Dbi)
            .expect("run /bin/echo through DbiGuest<Detcore>");

        assert_eq!(output.status, super::ExitStatus::Exited(0));
        assert_eq!(output.stdout, b"hello\n");
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .any(|line| line.starts_with("reverie-dbi: tool=Detcore ")),
            "DBI native summary did not prove Detcore dispatch: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn dbi_public_status_dispatch_runs_true_through_detcore() {
        use clap::Parser;

        if Backend::Dbi.ensure_available().is_err() {
            return;
        }

        let command = super::Command::new("/bin/true");
        let mut config = super::DetConfig::parse_from(["hermit-dbi-test"]);
        config.sequentialize_threads = true;
        config.validate();
        let status = super::run_with_backend(command, config, true, &None, Backend::Dbi)
            .expect("run /bin/true through DbiGuest<Detcore>");

        assert_eq!(status, super::ExitStatus::Exited(0));
    }

    #[test]
    fn kvm_runs_dynamic_echo_through_detcore() {
        use clap::Parser;

        if kvm_device_unavailable_reason(std::path::Path::new("/dev/kvm")).is_some() {
            return;
        }

        let mut command = super::Command::new("/bin/echo");
        command.arg("hello");
        let mut config = super::DetConfig::parse_from(["hermit-kvm-test"]);
        config.validate();
        let output = super::run_with_output_backend(command, config, false, &None, Backend::Kvm)
            .expect("run dynamic /bin/echo through KvmGuest<Detcore>");

        assert_eq!(output.status, super::ExitStatus::Exited(0));
        assert_eq!(output.stdout, b"hello\n");
        assert!(output.stderr.is_empty());
    }

    // Keep the low-level vmcall transport covered independently from the ELF
    // process personality. Requires /dev/kvm; skips cleanly otherwise.
    #[test]
    fn detcore_drives_kvm_guest_for_synthetic_syscall() {
        use clap::Parser;

        const MEMORY_SIZE: usize = 0x10_000;
        const ENTRY_POINT: u64 = 0x1000;
        const FRAME_ADDRESS: u64 = 0x2000;

        let mut backend = match reverie_kvm::KvmBackend::new(MEMORY_SIZE) {
            Ok(backend) => backend,
            Err(error) => {
                eprintln!("skipping KVM Detcore experiment: cannot init VM: {error}");
                return;
            }
        };

        // A guest that issues one `getpid` through the vmcall transport, then HLTs.
        backend
            .install_syscall(
                ENTRY_POINT,
                FRAME_ADDRESS,
                reverie_kvm::SyscallRequest::new(libc::SYS_getpid as u64, [0; 6]),
            )
            .expect("install synthetic getpid guest");

        // Minimal deterministic Detcore config with RCB preemption disabled.
        let mut config =
            super::DetConfig::parse_from(["hermit-kvm-test", "--max-timeslice=disabled"]);
        config.validate();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime");

        let outcome = runtime.block_on(async {
            backend
                .run_with_tool::<super::Detcore, _>(
                    config,
                    // Executor: forward anything Detcore injects to the host.
                    |request: &reverie_kvm::SyscallRequest, _memory: &reverie_kvm::GuestMemory| {
                        // SAFETY: forwarding a register-only syscall (getpid) to the
                        // host; no guest pointers are dereferenced by the kernel.
                        unsafe {
                            libc::syscall(
                                request.number() as libc::c_long,
                                request.args()[0],
                                request.args()[1],
                                request.args()[2],
                                request.args()[3],
                                request.args()[4],
                                request.args()[5],
                            ) as i64
                        }
                    },
                )
                .await
        });

        // The point of the experiment is to observe whether Detcore can be driven
        // to completion over KvmGuest at all; assert it did not error.
        outcome.expect("Detcore drove the synthetic KVM guest to completion");
    }

    // Minimal fake ELF payload: the loader only needs the image to NOT start
    // with `#!`, and a real ELF magic makes the intent obvious.
    const FAKE_ELF: &[u8] = b"\x7fELF\x02\x01\x01\x00 fake elf body";

    fn shebang_tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hermit-shebang-test-{}-{}",
            std::process::id(),
            tag
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn resolve_shebang_plain_elf_is_unchanged() {
        let dir = shebang_tmpdir("plain");
        let prog = dir.join("prog");
        fs::write(&prog, FAKE_ELF).unwrap();

        let argv = vec!["prog".to_owned(), "-a".to_owned()];
        let (path, out_argv, image) = resolve_kvm_shebang(&prog, argv).unwrap();
        assert_eq!(path, prog);
        assert_eq!(out_argv, vec!["prog".to_owned(), "-a".to_owned()]);
        assert_eq!(image, FAKE_ELF);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolve_shebang_single_level_kernel_order() {
        let dir = shebang_tmpdir("single");
        let interp = dir.join("fakebash");
        fs::write(&interp, FAKE_ELF).unwrap();
        let script = dir.join("script");
        // Interpreter with a single optional argument.
        fs::write(&script, format!("#!{} -x\necho hi\n", interp.display())).unwrap();

        let argv = vec!["script".to_owned(), "arg1".to_owned()];
        let (path, out_argv, image) = resolve_kvm_shebang(&script, argv).unwrap();
        assert_eq!(path, interp);
        // Kernel order: [interp, optarg, script_path, original args after argv[0]].
        assert_eq!(
            out_argv,
            vec![
                interp.to_string_lossy().into_owned(),
                "-x".to_owned(),
                script.to_string_lossy().into_owned(),
                "arg1".to_owned(),
            ]
        );
        assert_eq!(image, FAKE_ELF);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolve_shebang_nested_accumulates_like_binfmt_script() {
        let dir = shebang_tmpdir("nested");
        let interp = dir.join("fakebash");
        fs::write(&interp, FAKE_ELF).unwrap();
        let mid = dir.join("mid"); // a #!-interpreter that is itself a script
        fs::write(&mid, format!("#!{}\n", interp.display())).unwrap();
        let script = dir.join("script");
        fs::write(&script, format!("#!{} -e\n", mid.display())).unwrap();

        let argv = vec!["script".to_owned(), "arg1".to_owned()];
        let (path, out_argv, image) = resolve_kvm_shebang(&script, argv).unwrap();
        assert_eq!(path, interp);
        // Level 1: [mid, -e, script, arg1]; level 2 prepends [interp, mid].
        assert_eq!(
            out_argv,
            vec![
                interp.to_string_lossy().into_owned(),
                mid.to_string_lossy().into_owned(),
                "-e".to_owned(),
                script.to_string_lossy().into_owned(),
                "arg1".to_owned(),
            ]
        );
        assert_eq!(image, FAKE_ELF);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolve_shebang_rejects_infinite_recursion() {
        let dir = shebang_tmpdir("loop");
        let a = dir.join("a");
        let b = dir.join("b");
        fs::write(&a, format!("#!{}\n", b.display())).unwrap();
        fs::write(&b, format!("#!{}\n", a.display())).unwrap();

        let argv = vec!["a".to_owned()];
        assert!(resolve_kvm_shebang(&a, argv).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
