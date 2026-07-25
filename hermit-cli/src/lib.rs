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
mod script;

use std::fs;
use std::io;
use std::io::Write;
use std::os::fd::FromRawFd;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;

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
    /// Use the experimental LiteInst preload compatibility backend.
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

fn sabre_runtime_unavailable_reason() -> Option<String> {
    for (variable, description) in [
        ("HERMIT_SABRE_RUNNER", "Reverie SaBRe runner"),
        ("HERMIT_SABRE_BINARY", "SaBRe executable"),
        ("HERMIT_SABRE_PLUGIN", "Reverie SaBRe plugin"),
    ] {
        let Some(value) = std::env::var_os(variable) else {
            return Some(format!("set {variable} to the {description} path"));
        };
        let path = Path::new(&value);
        match fs::metadata(path) {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => {
                return Some(format!(
                    "{variable}={} is not a regular file",
                    path.display()
                ));
            }
            Err(error) => {
                return Some(format!(
                    "cannot access {variable}={}: {error}",
                    path.display()
                ));
            }
        }
    }
    None
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
    // The KVM backend has its own dispatch (`run_kvm`); it must not reach here.
    backend.ensure_available()?;
    Err(anyhow!(
        "backend `{}` has no Hermit dispatch implementation",
        backend.as_str()
    ))
}

/// Guest-physical memory available to the single-process KVM personality.
const KVM_GUEST_MEMORY_BYTES: usize = 256 * 1024 * 1024;

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
    let image = fs::read(&resolved_program).map_err(|error| {
        anyhow!(
            "failed to read KVM guest executable {:?}: {error}",
            resolved_program
        )
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
    run_with_backend_inner(
        command,
        config,
        print_summary,
        print_summary_to_json_file,
        backend,
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
    use std::fs;

    use super::Backend;
    use super::dbi_runtime_unavailable_reason;
    use super::dynamorio_sdk_available;
    use super::ensure_backend_dispatch;
    use super::is_dynamorio_sdk;
    use super::kvm_device_unavailable_reason;
    use super::liteinst_runtime_unavailable_reason;
    use super::sabre_runtime_unavailable_reason;

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
}
