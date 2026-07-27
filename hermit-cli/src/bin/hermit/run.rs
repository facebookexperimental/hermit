/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::hash_map::DefaultHasher;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::fs::File;
use std::hash::Hash;
use std::hash::Hasher;
use std::io::Read;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::LazyLock;

use ::tracing::metadata::LevelFilter;
use clap::Parser;
use colored::Colorize;
use hermit::Backend;
use hermit::Context;
use hermit::DetConfig;
use hermit::Error;
use reverie::process::Bind;
use reverie::process::Command;
use reverie::process::Container;
use reverie::process::ExitStatus;
use reverie::process::Mount;
use reverie::process::MountFlags;
use reverie::process::Namespace;
use reverie::process::Output;

use super::container::IdentityGuard;
use super::container::apply_affinity;
use super::container::default_container;
use super::container::identity_hardening_mounts;
use super::container::with_container;
use super::global_opts::GlobalOpts;
use super::tracing::init_file_tracing;
use super::verify::ComparedRun;
use super::verify::ComparisonOptions;
use super::verify::compare_two_runs;
use super::verify::temp_log_files;

const TMP_DIR: &str = "/tmp";
const FAIL_CLOSED_ENV: &str = "HERMIT_FAIL_CLOSED";
struct PreparedMounts {
    mounts: Vec<Mount>,
    identity_sources: IdentityGuard,
}

#[derive(Debug, Clone)]
struct E9patchOverlay {
    source: PathBuf,
    target: PathBuf,
}

#[derive(Debug, Clone, Eq, PartialEq)]
enum GuestPathMapping {
    Mapped(PathBuf),
    Hidden,
    Unchanged,
}

// Just a place to put the clap(flatten) directive..
#[derive(Debug, Parser, Clone)]
pub(crate) struct DetOptions {
    /// detcore configuration
    #[clap(flatten)]
    pub det_config: DetConfig,
}

/// Command-line options for the "run" subcommand.
#[derive(Debug, Parser, Clone)]
pub struct RunOpts {
    /// Select the process instrumentation backend.
    #[clap(long, value_enum)]
    backend: Option<Backend>,

    /// Program to run. Bare names are resolved using the guest PATH. Paths under host `/tmp` are
    /// hidden by Hermit's isolated `/tmp` unless `--tmp=/tmp` or an explicit mount exposes them.
    #[clap(value_name = "PROGRAM")]
    program: PathBuf,

    /// Arguments for the program.
    #[clap(value_name = "ARGS")]
    args: Vec<String>,

    #[clap(flatten)]
    pub(crate) det_opts: DetOptions,

    /// Enable fail-closed strict deterministic mode. Deterministic scheduling and I/O are the
    /// default; this explicit flag additionally rejects unsupported syscalls immediately.
    #[clap(
        long,
        conflicts_with_all = ["no_sequentialize_threads", "no_deterministic_io"]
    )]
    strict: bool,

    /// Disable deterministic sequential thread execution.
    #[clap(long)]
    pub(crate) no_sequentialize_threads: bool,

    /// Disable deterministic I/O behavior.
    #[clap(long)]
    no_deterministic_io: bool,

    /// Pin all guest threads to one or more cores, so that they do not migrate
    /// during execution. This is off by default, but it is implied by setting
    /// `max_timeslice` which requires stable RCB counters. RCB counters are
    /// not maintained consistently when Linux migrates a thread between cores.
    #[clap(long)]
    pin_threads: bool,

    /// Mount a file or directory. This uses the same syntax as Docker's `--mount` option. The
    /// source must exist on the host. For simple bind mounts into guest `/tmp`, use `--bind`.
    #[clap(long, value_name = "path")]
    mount: Vec<Mount>,

    /// Bind-mount a host file or directory into guest `/tmp`. Use `SOURCE` to preserve its path or
    /// `SOURCE:TARGET` to choose a target under `/tmp`; the source must already exist.
    #[clap(long, value_name = "path")]
    pub(crate) bind: Vec<Bind>,

    /// Select guest networking. `local` creates an isolated loopback interface; `host` exposes the
    /// host network and compromises isolation and deterministic reproducibility.
    #[clap(
        long,
        alias = "net",
        value_name = "local|host",
        default_value = "local"
    )]
    network: NetworkingMode,

    /// Run with namespaces but without ptrace, seccomp interception, or determinization. This is a
    /// useful smoke test when diagnosing ptrace/seccomp policy failures; PID and `/tmp` isolation
    /// still apply.
    #[clap(
        long,
        alias = "lite",
        conflicts_with = "chaos",
        conflicts_with = "verify",
        conflicts_with = "backend"
    )]
    namespace_only: bool,

    /// Run syscall interception directly on the host without creating Linux namespaces or
    /// mounting an isolated `/tmp`. This is not a sandbox and must only be used with trusted
    /// guests. Host process, filesystem, and network state are shared, reducing determinism.
    /// Schedule and preemption replay require stable namespace PIDs and are not supported.
    #[clap(
        long,
        visible_alias = "core-only",
        conflicts_with_all = [
            "mount",
            "bind",
            "network",
            "tmp",
            "namespace_only",
            "analyze_networking",
            "replay_schedule_from",
            "replay_preemptions_from"
        ]
    )]
    no_namespace: bool,

    /// Run in a minimally invasive syscall-interception mode. Combine with `hermit --log=info` to
    /// print intercepted syscalls.
    ///
    /// This does not determinize execution. It is shorthand for `--tmp=/tmp --network=host
    /// --no-virtualize-cpuid --no-virtualize-time --no-virtualize-metadata
    /// --no-sequentialize-threads --no-deterministic-io --no-rcb-time`.
    #[clap(
        long,
        conflicts_with = "chaos",
        conflicts_with = "namespace_only",
        conflicts_with = "seed",
        conflicts_with = "seed_from",
        conflicts_with = "analyze_networking"
    )]
    strace_only: bool,

    /// Specifies the directory to use as `/tmp`. This path gets bind-mounted
    /// over `/tmp` and the guest program does not see the real `/tmp` directory.
    /// If this path does not exist, it is created.
    ///
    /// If this option is not specified, a temporary directory is created,
    /// mounted over `/tmp`, and deleted when the guest has exited.
    #[clap(long, value_name = "dirpath")]
    tmp: Option<PathBuf>,

    /// Exactly like "seed" but we generate a seed for you. This is useful if multiple
    /// hermit runs execute in parallel and rand based collisions exist.  "Args" generates
    /// the seed from the other arguments passed to hermit, "SystemRandom" uses system
    /// randomness to generate a seed, and creates a log message recording it.
    #[clap(long, value_name = "'Args'|'SystemRandom'")]
    seed_from: Option<SeedFrom>,

    /// After running, immediately run a SECOND time, and compare the two
    /// executions. This will exit with an error if the guest process does OR if
    /// the executions do not match. In order to match, they must have the same
    /// observed output (e.g. stdout/stderr), and the same log of internal
    /// scheduler steps.
    ///
    /// It's on the user to ensure that the command run is idempotent, and thus
    /// that the first run will not have any side effects that affect the
    /// execution of the second run.
    #[clap(long)]
    verify: bool,

    /// Compare complete, unnormalized TRACE logs and show detailed differences.
    /// This detects internal timing and other trace-only divergence at the cost
    /// of substantially larger logs and stricter comparison.
    #[clap(long, requires = "verify")]
    verify_verbose: bool,

    /// If --verify is specified, indicates what guest exit status is required for
    /// hermit to consider the verification successful.  Both runs must satisfy this criteria,
    /// and hermit does not perform the second run if the first does not.
    #[clap(long, value_name = "success|failure|both", default_value = "success")]
    verify_allow: VerifyAllow,

    /// If --verify is specified, echo the FIRST run's `--log` output to stderr,
    /// the same way a normal (non-verify) run does. During --verify the log is
    /// otherwise diverted to a temporary file for comparison, so the user never
    /// sees it. This restores observability of `--log` output while still
    /// performing the two-run determinism check.
    #[clap(long, requires = "verify")]
    verify_logs: bool,

    /// Print a summary of the process tree's execution to stderr before exiting.
    #[clap(long, short = 'u')]
    pub(crate) summary: bool,

    /// Print a machine readable version of --summary to a file.
    #[clap(long)]
    pub(crate) summary_json: Option<PathBuf>,

    /// Diagnose non-zero network binds. Implies an isolated network namespace and conflicts with
    /// `--network=host`.
    #[clap(long)]
    analyze_networking: bool,

    /// The base environment that is presented to the guest. "Empty" is completely empty, and "Host"
    /// allows through all the environment variables in hermit's own environment.
    /// "Minimal" provides a minimal deterministic environment, setting only PATH, HOSTNAME, and HOME.
    #[clap(long, default_value = "host", value_name = "str")]
    base_env: BaseEnv,

    /// Additionally append one or more environment variables to the container environment. If a
    /// name is provided without a value, pass that variable through from the host.
    #[clap(short = 'e', long, value_parser = parse_assignment, value_name="name[=val]")]
    env: Vec<(String, Option<String>)>,

    /// Set the guest working directory. The path is resolved after guest mounts are applied, so an
    /// isolated path such as `/tmp` refers to the guest view.
    #[clap(long, value_name = "path")]
    workdir: Option<String>,

    /// For debugging, save the details of this final run config: printed to a file in a human
    /// readable format.
    #[clap(long, value_name = "path")]
    pub save_config: Option<PathBuf>,

    /// Read-only overlay that exposes the rewritten ELF at its original guest path.
    #[clap(skip)]
    e9patch_overlay: Option<E9patchOverlay>,

    /// Resolved guest executable path used after e9patch preprocessing.
    #[clap(skip)]
    e9patch_program: Option<PathBuf>,
}

fn parse_assignment(src: &str) -> Result<(String, Option<String>), Error> {
    static ENV_RE: LazyLock<regex::Regex> = LazyLock::new(||
        // Here we are extremely permissive, allowing all charecters in the "Portable Character
        // Set", ISO/IEC 6429:1992 standard:
        regex::Regex::new("^([\x07-<>-~]+)=([\x07-~]*)$").unwrap());
    static VAR_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new("^([\x07-<>-~]+)$").unwrap());

    if let Some(capture) = ENV_RE.captures(src) {
        if let (Some(name), Some(value)) = (capture.get(1), capture.get(2)) {
            Ok((name.as_str().to_owned(), Some(value.as_str().to_owned())))
        } else {
            anyhow::bail!("unable to parse name=value from '{}'", src)
        }
    } else if VAR_RE.is_match(src) {
        let var: String = src.to_owned();
        Ok((var, None))
    } else {
        anyhow::bail!("unable to parse env var name or name=value from '{}'", src)
    }
}

#[derive(Debug, Default, Clone, Copy, Parser, Eq, PartialEq)]
pub enum NetworkingMode {
    /// Create a local loopback device and allow local, intra-container network communication only.
    // WARNING: written in two places, here and in the #[clap(default_value)] above.
    #[default]
    Local,
    /// Allow through all network access via the host's network interface.
    Host,
    // None, // TODO: no network interface at all
    // Record, // TODO: record network traffic only, not other syscalls.
}

// Upper case will work, but prefer lower case.
impl fmt::Display for NetworkingMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match &self {
            NetworkingMode::Local => "local",
            NetworkingMode::Host => "host",
        };
        write!(f, "{}", s)
    }
}

impl FromStr for NetworkingMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "local" => Ok(NetworkingMode::Local),
            "host" => Ok(NetworkingMode::Host),
            _ => Err(format!("Could not parse: {:?}", s)),
        }
    }
}

#[derive(Debug, Clone, Copy, Parser, Eq, PartialEq)]
pub enum VerifyAllow {
    Success,
    Failure,
    Both,
}

impl FromStr for VerifyAllow {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "success" => Ok(VerifyAllow::Success),
            "failure" => Ok(VerifyAllow::Failure),
            "both" => Ok(VerifyAllow::Both),
            _ => Err(format!("Could not parse: {:?}", s)),
        }
    }
}

impl VerifyAllow {
    fn satisfies(&self, status: ExitStatus) -> bool {
        match self {
            VerifyAllow::Success => status == ExitStatus::SUCCESS,
            VerifyAllow::Failure => status != ExitStatus::SUCCESS,
            VerifyAllow::Both => true,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
enum BaseEnv {
    Empty,
    Minimal,
    Host,
}

impl FromStr for BaseEnv {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "empty" => Ok(BaseEnv::Empty),
            "minimal" => Ok(BaseEnv::Minimal),
            "host" => Ok(BaseEnv::Host),
            _ => Err(format!(
                "Expected Empty | Minimal | Host, could not parse: {:?}",
                s
            )),
        }
    }
}

/// Where to generate the random seed from.
#[derive(Debug, Clone)]
pub enum SeedFrom {
    Args,
    SystemRandom,
}

// Error boilerplate.
#[derive(Debug, Clone)]
pub struct ParseSeedFromError {
    details: String,
}

impl fmt::Display for ParseSeedFromError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.details)
    }
}

impl std::error::Error for ParseSeedFromError {
    fn description(&self) -> &str {
        &self.details
    }
}

impl FromStr for SeedFrom {
    type Err = ParseSeedFromError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "args" => Ok(SeedFrom::Args),
            "systemrandom" => Ok(SeedFrom::SystemRandom),
            _ => Err(ParseSeedFromError {
                details: format!("Expected Args | SystemRandom, could not parse: {:?}", s),
            }),
        }
    }
}

/// Displays as a string which needs only to be prepended with "hermit " to be a runnable command.
impl fmt::Display for RunOpts {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let dop = &self.det_opts.det_config;

        if let Some(backend) = self.backend {
            write!(f, " --backend={}", backend.as_str())?;
        }
        if self.no_sequentialize_threads {
            write!(f, " --no-sequentialize-threads")?;
        }
        if self.no_deterministic_io {
            write!(f, " --no-deterministic-io")?;
            assert!(!dop.deterministic_io)
        } else {
            assert!(dop.deterministic_io)
        }
        if self.network != Default::default() {
            write!(f, " --network={}", self.network)?;
        }
        if self.namespace_only {
            write!(f, " --namespace-only")?;
        }
        if self.no_namespace {
            write!(f, " --no-namespace")?;
        }
        if self.summary {
            write!(f, " --summary")?;
        }
        if let Some(p) = &self.summary_json {
            let s = p.to_str().expect("valid unicode path");
            write!(f, " --summary-json={}", shell_words::quote(s))?;
        }
        if self.analyze_networking {
            write!(f, " --analyze-networking")?;
        }
        if self.verify {
            write!(f, " --verify")?;
        }
        if self.verify_verbose {
            write!(f, " --verify-verbose")?;
        }
        if let Some(p) = &self.tmp {
            let s = p.to_str().expect("valid unicode path");
            write!(f, " --tmp={}", shell_words::quote(s))?;
        }
        match &self.verify_allow {
            VerifyAllow::Success => {} // default
            VerifyAllow::Failure => {
                write!(f, " --verify-allow=failure")?;
            }
            VerifyAllow::Both => {
                write!(f, " --verify-allow=both")?;
            }
        }
        match &self.base_env {
            BaseEnv::Empty => {
                write!(f, " --base-env=empty")?;
            }
            BaseEnv::Minimal => {
                write!(f, " --base-env=minimal")?;
            }
            BaseEnv::Host => {} // default
        }
        for (key, m_val) in &self.env {
            if let Some(val) = m_val {
                write!(f, " --env={}={}", key, shell_words::quote(val))?;
            } else {
                write!(f, " --env={}", key)?;
            }
        }
        if let Some(p) = &self.workdir {
            write!(f, " --workdir={}", shell_words::quote(p))?;
        }
        if let Some(p) = &self.save_config {
            let s = p.to_str().expect("valid string provided to --save-config");
            write!(f, " --save-config={}", shell_words::quote(s))?;
        }

        for mount in &self.mount {
            let mut acc = Vec::new();
            if let Some(s) = &mount.get_source() {
                acc.push(format!("source={}", s.display()));
            }
            acc.push(format!("target={}", mount.get_target().display()));
            write!(f, "--mount={}", shell_words::quote(&acc.join(",")),)?;
        }
        for bind in &self.bind {
            let src = bind.source.to_str().expect("valid unicode bind source");
            let tar = bind.target.to_str().expect("valid unicode target");
            if bind.source == bind.target {
                write!(f, " --bind={}", shell_words::quote(src))?;
            } else {
                write!(
                    f,
                    " --bind={}:{}",
                    shell_words::quote(src),
                    shell_words::quote(tar)
                )?;
            }
        }

        // Write the rest of the flags from the Config itself:
        write!(f, "{}", dop)?;

        write!(
            f,
            " -- {}",
            shell_words::quote(self.program.to_str().expect("valid unicode path"))
        )?;
        if !self.args.is_empty() {
            write!(f, " {}", shell_words::join(&self.args))?;
        }
        Ok(())
    }
}

/// Returns true if `program` names a hardware emulator / virtual machine
/// monitor whose emulated guest runs its own clock calibration. Such programs
/// (notably the `qemu-system-*` family) are sensitive to Hermit's host-time
/// virtualization. This is a filename heuristic used only to surface an advisory
/// warning; it never changes Hermit's behavior.
fn is_vmm_program(program: &Path) -> bool {
    program
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("qemu-system-"))
}

/// Advisory warning for running a VMM under Hermit's host-time virtualization.
///
/// QEMU and similar emulators derive the emulated PIT, PM timer, APIC timer,
/// RTC, and TSC from several different host clocks. Hermit virtualizes RDTSC and
/// `clock_gettime` from separate logical-time bases that are not mutually
/// coherent (especially under `--no-sequentialize-threads`), so the nested guest
/// observes inconsistent clock domains and its calibration breaks. See issue #6
/// and `docs/QEMU_BOOT.md`. Returns the message to print, or `None` when no
/// warning applies.
fn vmm_time_virtualization_warning(program: &Path, virtualize_time: bool) -> Option<String> {
    if virtualize_time && is_vmm_program(program) {
        Some(format!(
            "WARNING: {} looks like a hardware emulator (VMM). Hermit's host-time \
             virtualization exposes mutually inconsistent clock sources (a synthetic RDTSC \
             versus a virtualized clock_gettime) to the emulated guest, which can corrupt its \
             clock calibration (for example \"Unable to calibrate against PIT\", TSC marked \
             unstable, or \"No current clocksource\") and stall boot. If the nested guest \
             misbehaves, either disable Hermit's virtual clock with \
             --no-virtualize-time --no-virtualize-metadata, or make the emulator use a single \
             instruction-derived clock (for QEMU: -icount shift=0,sleep=off). \
             See docs/QEMU_BOOT.md.",
            program.display()
        ))
    } else {
        None
    }
}

#[test]
fn vmm_time_warning_fires_for_qemu_with_virtual_time() {
    // A qemu-system-* emulator under virtual time gets the advisory.
    for program in [
        "qemu-system-x86_64",
        "/usr/bin/qemu-system-x86_64",
        "qemu-system-aarch64",
    ] {
        let warning = vmm_time_virtualization_warning(Path::new(program), true);
        let message = warning
            .unwrap_or_else(|| panic!("expected a warning for {program} under virtual time"));
        assert!(message.contains("--no-virtualize-time"));
        assert!(message.contains("-icount"));
    }
}

#[test]
fn vmm_time_warning_silent_without_virtual_time() {
    // The workaround (disabling virtual time) must not itself warn.
    assert!(
        vmm_time_virtualization_warning(Path::new("qemu-system-x86_64"), false).is_none(),
        "no warning is expected once virtual time is disabled"
    );
}

#[test]
fn vmm_time_warning_silent_for_non_vmm_programs() {
    for program in ["ls", "/bin/echo", "qemu-img", "my-qemu-wrapper"] {
        assert!(
            vmm_time_virtualization_warning(Path::new(program), true).is_none(),
            "unexpected VMM warning for {program}"
        );
    }
}

#[test]
fn display_runopts1() {
    let vec: Vec<&str> = vec!["fakehermit", "fakeprog", "arg1", "arg2"];
    let mut ro = RunOpts::parse_from(vec.iter());
    ro.validate_args_with_perf_support(true).unwrap();
    assert_eq!(format!("{}", ro), " -- fakeprog arg1 arg2");
}

#[test]
fn backend_defaults_to_ptrace() {
    let mut ro = RunOpts::parse_from(["fakehermit", "fakeprog"]);
    ro.validate_args_with_perf_support(true).unwrap();
    assert_eq!(ro.backend, None);
    assert_eq!(ro.selected_backend(), Backend::Ptrace);
    assert_eq!(format!("{}", ro), " -- fakeprog");
}

#[test]
fn backend_values_parse_and_round_trip() {
    for (value, expected) in [
        ("ptrace", Backend::Ptrace),
        ("dbi", Backend::Dbi),
        ("liteinst", Backend::Liteinst),
        ("sabre", Backend::Sabre),
        ("kvm", Backend::Kvm),
        ("e9patch", Backend::E9patch),
    ] {
        let mut ro = RunOpts::parse_from(["fakehermit", "--backend", value, "fakeprog"]);
        ro.validate_args_with_perf_support(true).unwrap();
        assert_eq!(ro.backend, Some(expected));
        assert_eq!(ro.selected_backend(), expected);
        let normalized = if expected == Backend::Liteinst {
            format!(" --backend={value} --max-timeslice=disabled -- fakeprog")
        } else {
            format!(" --backend={value} -- fakeprog")
        };
        assert_eq!(format!("{}", ro), normalized);
    }
}

#[test]
fn e9patch_preserves_executable_identity_and_uses_ptrace_runtime() {
    let mut ro = RunOpts::parse_from(["fakehermit", "--backend", "e9patch", "/bin/echo", "hello"]);
    ro.e9patch_overlay = Some(E9patchOverlay {
        source: PathBuf::from("/cache/patched-echo"),
        target: PathBuf::from("/bin/echo"),
    });
    let command = ro.guest_command().unwrap();
    assert_eq!(command.get_program(), "/bin/echo");
    assert_eq!(command.get_arg0(), "/bin/echo");
    assert_eq!(ro.runtime_backend(), Backend::Ptrace);

    let tmpfs = tempfile::tempdir().unwrap();
    let mounts = ro.mounts(tmpfs.path()).unwrap();
    let overlay = mounts
        .mounts
        .iter()
        .find(|mount| mount.get_source() == Some(Path::new("/cache/patched-echo")))
        .unwrap();
    assert_eq!(overlay.get_target(), Path::new("/bin/echo"));
}

#[test]
fn mapped_guest_path_is_resolved_before_host_validation() {
    let tmp = tempfile::tempdir().unwrap();
    let tool = tmp.path().join("tool");
    fs::write(&tool, b"fixture").unwrap();
    let mut permissions = fs::metadata(&tool).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&tool, permissions).unwrap();

    let tmp_arg = format!("--tmp={}", tmp.path().display());
    let mut ro = RunOpts::parse_from([
        "fakehermit",
        "--backend",
        "e9patch",
        &tmp_arg,
        "-e",
        "PATH=/tmp",
        "tool",
    ]);
    assert_eq!(ro.tmp.as_deref(), Some(tmp.path()));
    assert_eq!(
        ro.guest_command()
            .unwrap()
            .get_captured_envs()
            .get(OsStr::new("PATH")),
        Some(&OsStr::new("/tmp").to_os_string())
    );
    assert_eq!(
        ro.mapped_host_program(Path::new("/tmp/tool")),
        GuestPathMapping::Mapped(tool.clone())
    );
    let (guest, host) = ro.resolve_guest_and_host_program().unwrap();
    assert_eq!(guest, Path::new("/tmp/tool"));
    assert_eq!(host, tool);
    ro.e9patch_program = Some(guest);
    let command = ro.guest_command().unwrap();
    assert_eq!(command.get_program(), "/tmp/tool");
    assert_eq!(command.get_arg0(), "tool");
}

#[test]
fn non_e9patch_validation_preserves_parent_component_paths() {
    let ro = RunOpts::parse_from(["fakehermit", "--backend", "ptrace", "/usr/bin/../bin/echo"]);
    ro.validate_program().unwrap();
}

#[test]
fn guest_path_normalization_rejects_parent_components() {
    let error = normalize_guest_path(Path::new("/mnt/../tool")).unwrap_err();
    assert!(error.to_string().contains("parent components"));
}

#[test]
fn e9patch_mount_target_rejects_parent_components() {
    let ro = RunOpts::parse_from([
        "fakehermit",
        "--backend",
        "e9patch",
        "--mount=type=tmpfs,target=/tmp/../bin",
        "/bin/echo",
    ]);
    let error = ro.validate_e9patch_mount_targets().unwrap_err().to_string();
    assert!(error.contains("mount target cannot contain parent components"));
}

#[test]
fn e9patch_mount_target_rejects_symlink_components() {
    let directory = tempfile::tempdir().unwrap();
    let link = directory.path().join("link");
    std::os::unix::fs::symlink("/tmp", &link).unwrap();
    let mount = format!(
        "--mount=type=tmpfs,target={}",
        link.join("target").display()
    );
    let ro = RunOpts::parse_from(["fakehermit", "--backend", "e9patch", &mount, "/bin/echo"]);
    let error = ro.validate_e9patch_mount_targets().unwrap_err();
    assert!(error.to_string().contains("mount target traverses symlink"));
}

#[test]
fn source_less_mount_hides_program_from_resolution() {
    let ro = RunOpts::parse_from(["fakehermit", "--mount=type=tmpfs,target=/bin", "/bin/echo"]);
    assert_eq!(
        ro.mapped_host_program(Path::new("/bin/echo")),
        GuestPathMapping::Hidden
    );
    let error = ro.resolve_guest_and_host_program().unwrap_err().to_string();
    assert!(error.contains("not visible through the configured guest mounts"));
}

#[test]
fn non_elf_entrypoints_skip_e9patch_preprocessing() {
    let directory = tempfile::tempdir().unwrap();
    let script = directory.path().join("script");
    fs::write(&script, b"#!/bin/sh\nexit 0\n").unwrap();
    let mut permissions = fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script, permissions).unwrap();
    assert!(!is_elf_file(&script).unwrap());
    assert!(is_elf_file(Path::new("/bin/sh")).unwrap());

    let mount_link = directory.path().join("mount-link");
    std::os::unix::fs::symlink("/var/run", &mount_link).unwrap();
    let unrelated_mount = format!(
        "--mount=type=tmpfs,target={}",
        mount_link.join("target").display()
    );
    let tmp = format!("--tmp={}", directory.path().display());
    let mut ro = RunOpts::parse_from([
        "fakehermit",
        "--backend",
        "e9patch",
        &unrelated_mount,
        &tmp,
        "/tmp/script",
    ]);
    ro.prepare_e9patch_program().unwrap();
    assert!(ro.e9patch_overlay.is_none());
}

#[test]
fn e9patch_overlay_uses_canonical_target_without_custom_mounts() {
    let ro = RunOpts::parse_from(["fakehermit", "--backend", "e9patch", "/bin/echo"]);
    assert_eq!(
        ro.resolve_e9patch_overlay_target(Path::new("/bin/echo"), Path::new("/bin/echo"))
            .unwrap(),
        fs::canonicalize("/bin/echo").unwrap()
    );
}

#[test]
fn e9patch_rejects_symlinked_executables_through_custom_mounts() {
    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join("executable");
    let link = directory.path().join("link");
    fs::write(&executable, b"fixture").unwrap();
    std::os::unix::fs::symlink(&executable, &link).unwrap();
    let mount = format!(
        "--mount=type=bind,source={},target=/e9patch-test",
        directory.path().display()
    );
    let ro = RunOpts::parse_from([
        "fakehermit",
        "--backend",
        "e9patch",
        &mount,
        "/e9patch-test/link",
    ]);
    let error = ro
        .resolve_e9patch_overlay_target(Path::new("/e9patch-test/link"), &link)
        .unwrap_err();
    assert!(error.to_string().contains("symlinked executable"));
}

#[test]
fn e9patch_rejects_mounts_that_change_a_symlink_target() {
    let directory = tempfile::tempdir().unwrap();
    let mount = format!(
        "--mount=type=bind,source={},target=/usr",
        directory.path().display()
    );
    let ro = RunOpts::parse_from(["fakehermit", "--backend", "e9patch", &mount, "/bin/echo"]);
    let error = ro
        .resolve_e9patch_overlay_target(Path::new("/bin/echo"), Path::new("/bin/echo"))
        .unwrap_err();
    assert!(error.to_string().contains("symlinked executable"));
}

#[test]
fn detects_symlink_resolution_through_implicit_mounts() {
    use std::os::fd::AsRawFd;

    let file = tempfile::NamedTempFile::new().unwrap();
    let proc_fd = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
    assert!(path_resolution_visits_prefix(&proc_fd, Path::new("/tmp")).unwrap());
    assert!(path_resolution_visits_prefix(&proc_fd, Path::new("/proc")).unwrap());
    assert!(!path_resolution_visits_prefix(Path::new("/bin/echo"), Path::new("/tmp")).unwrap());
}

#[test]
fn display_runopts2() {
    let vec: Vec<&str> = vec![
        "fakehermit",
        "--sequentialize-threads",
        "fakeprog",
        "arg1",
        "arg2",
    ];
    let mut ro = RunOpts::parse_from(vec.iter());
    ro.validate_args_with_perf_support(true).unwrap();
    assert_eq!(format!("{}", ro), " -- fakeprog arg1 arg2");
}

#[test]
fn display_runopts3() {
    let vec: Vec<&str> = vec![
        "fakehermit",
        "--no-sequentialize-threads",
        "--no-virtualize-metadata",
        "--epoch=2000-12-31T23:59:59+00:00",
        "fakeprog",
        "arg1",
        "arg2",
    ];
    let mut ro = RunOpts::parse_from(vec.iter());
    ro.validate_args_with_perf_support(true).unwrap();
    assert_eq!(
        format!("{}", ro),
        " --no-sequentialize-threads --no-virtualize-metadata --epoch=2000-12-31T23:59:59+00:00 -- fakeprog arg1 arg2"
    );
}

#[test]
fn display_runopts4() {
    let vec: Vec<&str> = vec!["fakehermit", "--sequentialize-threads", "fakeprog", "arg1"];
    let mut ro = RunOpts::parse_from(vec.iter());
    ro.validate_args_with_perf_support(true).unwrap();
    assert_eq!(format!("{}", ro), " -- fakeprog arg1");
}

#[test]
fn strict_flag_preserves_deterministic_defaults_and_rejects_unsupported_syscalls() {
    let mut normal = RunOpts::parse_from(["fakehermit", "fakeprog"]);
    normal.validate_args_with_perf_support(true).unwrap();
    assert!(!normal.det_opts.det_config.panic_on_unsupported_syscalls);

    let mut strict = RunOpts::parse_from(["fakehermit", "--strict", "fakeprog"]);
    strict.validate_args_with_perf_support(true).unwrap();

    assert!(strict.det_opts.det_config.sequentialize_threads);
    assert!(strict.det_opts.det_config.deterministic_io);
    assert!(!strict.det_opts.det_config.passthru_opt);
    assert!(strict.det_opts.det_config.panic_on_unsupported_syscalls);
    assert_eq!(
        format!("{}", strict),
        " --panic-on-unsupported-syscalls -- fakeprog"
    );
}

#[test]
fn panic_on_rbc_overshoot_flag_wires_to_detcore_config() {
    let default = RunOpts::parse_from(["fakehermit", "fakeprog"]);
    assert!(!default.det_opts.det_config.panic_on_rcb_overshoot);

    let mut opts = RunOpts::parse_from(["fakehermit", "--panic-on-rbc-overshoot", "fakeprog"]);
    opts.validate_args_with_perf_support(true).unwrap();
    assert!(opts.det_opts.det_config.panic_on_rcb_overshoot);
    assert_eq!(format!("{}", opts), " --panic-on-rbc-overshoot -- fakeprog");
}

#[test]
fn passthru_optimization_requires_explicit_opt_in() {
    let mut ro = RunOpts::parse_from(["fakehermit", "--passthru-opt", "fakeprog"]);
    ro.validate_args_with_perf_support(true).unwrap();

    assert!(ro.det_opts.det_config.passthru_opt);
    assert_eq!(format!("{}", ro), " --passthru-opt -- fakeprog");
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-644): Review rejecting optimization that bypasses fail-closed policy.
#[test]
fn passthru_optimization_rejects_fail_closed_modes() {
    for fail_closed in ["--strict", "--panic-on-unsupported-syscalls"] {
        let mut opts =
            RunOpts::parse_from(["fakehermit", "--passthru-opt", fail_closed, "fakeprog"]);
        let error = opts.validate_args_with_perf_support(true).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("--passthru-opt"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("fail-closed"),
            "unexpected error: {message}"
        );
    }
}

#[test]
fn timeslice_flags_parse_and_round_trip() {
    let mut ro = RunOpts::parse_from([
        "fakehermit",
        "--max-timeslice=100000",
        "--target-timeslice=20000",
        "fakeprog",
    ]);
    ro.validate_args_with_perf_support(true).unwrap();

    assert_eq!(
        ro.det_opts.det_config.max_timeslice,
        std::num::NonZeroU64::new(100_000)
    );
    assert_eq!(
        ro.det_opts.det_config.target_timeslice,
        std::num::NonZeroU64::new(20_000)
    );
    let rendered = format!("{}", ro);
    assert_eq!(
        rendered,
        " --max-timeslice=100000 --target-timeslice=20000 -- fakeprog"
    );

    let mut reparsed_args = vec!["fakehermit".to_owned()];
    reparsed_args.extend(shell_words::split(&rendered).unwrap());
    let mut reparsed = RunOpts::parse_from(reparsed_args);
    reparsed.validate_args_with_perf_support(true).unwrap();
    assert_eq!(
        reparsed.det_opts.det_config.max_timeslice,
        ro.det_opts.det_config.max_timeslice
    );
    assert_eq!(
        reparsed.det_opts.det_config.target_timeslice,
        ro.det_opts.det_config.target_timeslice
    );
}

#[test]
fn deprecated_preemption_timeout_alias_round_trips_canonically() {
    let mut ro = RunOpts::parse_from(["fakehermit", "--preemption-timeout=100000", "fakeprog"]);
    ro.validate_args_with_perf_support(true).unwrap();

    assert_eq!(
        ro.det_opts.det_config.max_timeslice,
        std::num::NonZeroU64::new(100_000)
    );
    assert_eq!(format!("{}", ro), " --max-timeslice=100000 -- fakeprog");
}

#[test]
fn deprecated_preemption_timeout_disabled_values_round_trip_canonically() {
    for value in ["disabled", "0"] {
        let flag = format!("--preemption-timeout={value}");
        let mut ro = RunOpts::parse_from(["fakehermit", &flag, "fakeprog"]);
        ro.validate_args_with_perf_support(true).unwrap();

        assert_eq!(ro.det_opts.det_config.max_timeslice, None);
        assert_eq!(format!("{}", ro), " --max-timeslice=disabled -- fakeprog");
    }
}

#[test]
fn max_timeslice_rejects_less_than_one_rcb() {
    let error =
        RunOpts::try_parse_from(["fakehermit", "--max-timeslice=9", "fakeprog"]).unwrap_err();

    assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
    assert!(error.to_string().contains("at least one RCB"));

    let mut ro = RunOpts::parse_from(["fakehermit", "--max-timeslice=10", "fakeprog"]);
    ro.validate_args_with_perf_support(true).unwrap();
    assert_eq!(
        ro.det_opts.det_config.max_timeslice,
        std::num::NonZeroU64::new(10)
    );

    let mut scaled = RunOpts::parse_from([
        "fakehermit",
        "--max-timeslice=10",
        "--clock-multiplier=2",
        "fakeprog",
    ]);
    let error = scaled.validate_args_with_perf_support(true).unwrap_err();
    assert!(error.to_string().contains("at least one RCB"));

    let mut zero = RunOpts::parse_from(["fakehermit", "--clock-multiplier=0", "fakeprog"]);
    assert!(
        zero.validate_args_with_perf_support(true)
            .unwrap_err()
            .to_string()
            .contains("finite and positive")
    );
}

#[test]
fn strict_flag_rejects_determinism_opt_outs() {
    for opt_out in ["--no-sequentialize-threads", "--no-deterministic-io"] {
        let error =
            RunOpts::try_parse_from(["fakehermit", "--strict", opt_out, "fakeprog"]).unwrap_err();

        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
        let message = error.to_string();
        assert!(message.contains("--strict"));
        assert!(message.contains(opt_out));
    }
}

#[test]
fn gdbserver_forces_host_networking() {
    // Without --gdbserver the default networking stays local.
    let mut plain = RunOpts::parse_from(["fakehermit", "fakeprog"]);
    plain.validate_args_with_perf_support(true).unwrap();
    assert_eq!(plain.network, NetworkingMode::Local);

    // With --gdbserver the isolated network namespace would hide the gdbserver
    // port from a host gdb client, so networking is forced to host.
    let mut opts = RunOpts::parse_from(["fakehermit", "--gdbserver", "fakeprog"]);
    assert_eq!(opts.network, NetworkingMode::Local);
    opts.validate_args_with_perf_support(true).unwrap();
    assert!(opts.det_opts.det_config.gdbserver);
    assert_eq!(opts.network, NetworkingMode::Host);
}

#[test]
fn gdbserver_respects_explicit_host_networking() {
    let mut opts = RunOpts::parse_from(["fakehermit", "--gdbserver", "--network=host", "fakeprog"]);
    opts.validate_args_with_perf_support(true).unwrap();
    assert_eq!(opts.network, NetworkingMode::Host);
}

#[test]
fn gdbserver_conflicts_with_analyze_networking() {
    let mut opts = RunOpts::parse_from([
        "fakehermit",
        "--gdbserver",
        "--analyze-networking",
        "fakeprog",
    ]);
    let error = opts.validate_args_with_perf_support(true).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("--gdbserver"), "message: {message}");
    assert!(
        message.contains("--analyze-networking"),
        "message: {message}"
    );
}

#[test]
fn no_namespace_uses_host_resources_and_disables_uts_assumption() {
    let mut opts = RunOpts::parse_from(["fakehermit", "--core-only", "fakeprog"]);
    opts.validate_args_with_perf_support(true).unwrap();

    assert!(opts.no_namespace);
    assert_eq!(opts.network, NetworkingMode::Host);
    assert_eq!(opts.tmp.as_deref(), Some(Path::new(TMP_DIR)));
    assert!(!opts.det_opts.det_config.has_uts_namespace);
    assert!(opts.pin_threads);
    assert_eq!(
        format!("{}", opts),
        " --network=host --no-namespace --tmp=/tmp -- fakeprog"
    );
}

#[test]
fn strict_help_describes_compatibility_and_opt_outs() {
    use clap::CommandFactory;

    let help = RunOpts::command().render_long_help().to_string();
    for expected in [
        "--strict",
        "fail-closed strict deterministic mode",
        "rejects unsupported syscalls immediately",
        "--no-sequentialize-threads",
        "Disable deterministic sequential thread execution",
        "--no-deterministic-io",
        "Disable deterministic I/O behavior",
        "--passthru-opt",
        "optimized partial syscall subscription set",
        "--panic-on-rbc-overshoot",
        "--max-timeslice",
        "--preemption-timeout",
        "--target-timeslice",
        "syscall boundaries",
        "--backend <BACKEND>",
        "Select the process instrumentation backend",
        "ptrace",
        "dbi",
        "kvm",
    ] {
        assert!(
            help.contains(expected),
            "missing {expected:?} in run help:\n{help}"
        );
    }
}

#[test]
fn display_runopts_without_perf_support() {
    let mut ro = RunOpts::parse_from(["fakehermit", "fakeprog", "arg1"]);
    ro.validate_args_with_perf_support(false).unwrap();
    assert_eq!(
        format!("{}", ro),
        " --max-timeslice=disabled -- fakeprog arg1"
    );
}

fn shebang_interpreter(path: &Path) -> Option<PathBuf> {
    let mut file = File::open(path).ok()?;
    let mut bytes = [0_u8; 256];
    let count = file.read(&mut bytes).ok()?;
    let bytes = &bytes[..count];
    if !bytes.starts_with(b"#!") {
        return None;
    }

    let start = bytes[2..]
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t'))?
        + 2;
    let end = bytes[start..]
        .iter()
        .position(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
        .map_or(bytes.len(), |offset| start + offset);
    Some(PathBuf::from(OsStr::from_bytes(&bytes[start..end])))
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-696): Review sharing ELF entrypoint detection with record.
pub(super) fn is_elf_file(path: &Path) -> Result<bool, Error> {
    let mut file = File::open(path)
        .with_context(|| format!("failed to open executable {}", path.display()))?;
    let mut magic = [0_u8; 4];
    match file.read_exact(&mut magic) {
        Ok(()) => Ok(magic == *b"\x7fELF"),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => Err(error)
            .with_context(|| format!("failed to inspect executable format for {}", path.display())),
    }
}

fn validate_executable(path: &Path, requested: &Path) -> Result<(), Error> {
    let metadata = fs::metadata(path).with_context(|| {
        format!(
            "Program {} does not exist or is not accessible. Check the path and any --mount or \
             --bind target.",
            requested.display()
        )
    })?;
    if metadata.is_dir() {
        anyhow::bail!(
            "Program {} is a directory; provide the path to an executable file",
            requested.display()
        );
    }
    if !metadata.is_file() {
        anyhow::bail!(
            "Program {} is not a regular executable file",
            requested.display()
        );
    }
    if metadata.permissions().mode() & 0o111 == 0 {
        anyhow::bail!(
            "Program {} is not executable. Add execute permission (for example, `chmod +x {}`) \
             or select another file.",
            requested.display(),
            requested.display()
        );
    }

    if let Some(interpreter) = shebang_interpreter(path) {
        if interpreter.as_os_str().is_empty() {
            anyhow::bail!(
                "Program {} has an empty shebang interpreter",
                requested.display()
            );
        }
        let interpreter_metadata = fs::metadata(&interpreter).with_context(|| {
            format!(
                "Program {} uses shebang interpreter {}, but that interpreter does not exist. \
                 Install it or update the script's #! line.",
                requested.display(),
                interpreter.display()
            )
        })?;
        if !interpreter_metadata.is_file() || interpreter_metadata.permissions().mode() & 0o111 == 0
        {
            anyhow::bail!(
                "Program {} uses shebang interpreter {}, but it is not an executable file",
                requested.display(),
                interpreter.display()
            );
        }
    }

    Ok(())
}

fn mapped_path(path: &Path, source: &Path, target: &Path) -> Option<PathBuf> {
    path.strip_prefix(target)
        .ok()
        .map(|suffix| source.join(suffix))
}

fn normalize_guest_path(path: &Path) -> Result<PathBuf, Error> {
    if !path.is_absolute() {
        anyhow::bail!("guest path must be absolute: {}", path.display());
    }
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            std::path::Component::RootDir | std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                anyhow::bail!(
                    "guest path cannot contain parent components: {}",
                    path.display()
                );
            }
            std::path::Component::Normal(part) => normalized.push(part),
            std::path::Component::Prefix(_) => unreachable!("Unix guest path has a prefix"),
        }
    }
    Ok(normalized)
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-696): Review sharing mount-boundary resolution with record.
pub(super) fn path_resolution_visits_prefix(path: &Path, prefix: &Path) -> Result<bool, Error> {
    let mut candidate = std::path::absolute(path)?;
    for _ in 0..40 {
        let components = candidate
            .components()
            .map(|component| component.as_os_str().to_os_string())
            .collect::<Vec<_>>();
        let mut current = PathBuf::from("/");
        let mut followed_symlink = false;
        for (index, component) in components.iter().enumerate() {
            if component == OsStr::new("/") || component == OsStr::new(".") {
                continue;
            }
            if component == OsStr::new("..") {
                current.pop();
            } else {
                current.push(component);
            }
            if current.starts_with(prefix) {
                return Ok(true);
            }
            let metadata = match fs::symlink_metadata(&current) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if !metadata.file_type().is_symlink() {
                continue;
            }
            let target = fs::read_link(&current)?;
            let mut next = if target.is_absolute() {
                target
            } else {
                current
                    .parent()
                    .ok_or_else(|| Error::msg("symlink has no parent"))?
                    .join(target)
            };
            for remaining in &components[index + 1..] {
                next.push(remaining);
            }
            candidate = next;
            followed_symlink = true;
            break;
        }
        if !followed_symlink {
            return Ok(false);
        }
    }
    anyhow::bail!("executable path exceeded Linux's symlink traversal limit")
}

fn validate_e9patch_mount_target(path: &Path) -> Result<(), Error> {
    if !path.is_absolute() {
        anyhow::bail!("e9patch mount target must be absolute: {}", path.display());
    }
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        anyhow::bail!(
            "e9patch mount target cannot contain parent components: {}",
            path.display()
        );
    }
    let mut current = PathBuf::from("/");
    for component in path.components() {
        let std::path::Component::Normal(part) = component else {
            continue;
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                anyhow::bail!(
                    "e9patch mount target traverses symlink {}",
                    current.display()
                );
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// Create two logging destinations and two global configs. Returns non-zero exit
/// status if there was a difference in any component of the output.
impl RunOpts {
    fn selected_backend(&self) -> Backend {
        self.backend.unwrap_or_default()
    }

    fn runtime_backend(&self) -> Backend {
        if self.selected_backend() == Backend::E9patch {
            Backend::Ptrace
        } else {
            self.selected_backend()
        }
    }

    pub fn main(&mut self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        // Set up an early tracing option before we're ready to set the global default:

        // The backend may be given in the preferred global position
        // (`hermit --backend X run ...`) or, for backwards compatibility, after the
        // subcommand (`hermit run --backend X ...`). An explicit subcommand-level
        // value wins; otherwise fall back to the global one.
        self.backend = self.backend.or(global.backend);
        if self.selected_backend() == Backend::Kvm {
            hermit::reserve_kvm_stdin(super::startup_stdin()?)?;
        }

        // TODO(T124429978): temporarily disabling this because it inexplicably clobbers our
        // subsequent tracing_subscriber::fmt::init() call.
        // tracing::subscriber::with_default(super::tracing::stderr_subscriber(global.log), || {
        self.validate_args()?;
        let backend = self.selected_backend();
        if backend == Backend::E9patch && self.no_namespace {
            anyhow::bail!(
                "--backend=e9patch requires mount namespaces to overlay the rewritten ELF at its \
                 original guest path"
            );
        }
        if self.namespace_only {
            if let Some(explicit_backend) = self.backend {
                anyhow::bail!(
                    "--backend={} cannot be used with --namespace-only because namespace-only mode \
                     bypasses instrumentation",
                    explicit_backend.as_str()
                );
            }
        } else if backend == Backend::E9patch {
            Backend::Ptrace.ensure_available()?;
        } else if backend != Backend::Kvm {
            backend.ensure_available()?;
        }
        // The KVM backend reaches real reverie-kvm code from its dispatch path
        // and reports an accurate, program-specific error there, so it is not
        // pre-empted by the generic availability probe above. E9patch is a CLI
        // preprocessor and probes its ptrace runtime and tool separately.
        self.validate_mount_sources()?;
        self.validate_program()?;
        if backend == Backend::E9patch {
            self.prepare_e9patch_program()?;
        }
        // });

        // DBI uses its dedicated CLI launch adapter. SaBRe, LiteInst, KVM,
        // e9patch, and ptrace use the common container and run/verify machinery.
        match backend {
            Backend::Ptrace
            | Backend::Liteinst
            | Backend::Sabre
            | Backend::Kvm
            | Backend::E9patch => {}
            Backend::Dbi => {
                return super::backends::run_dbi(
                    &self.program,
                    &self.args,
                    self.verify,
                    global.log,
                    &self.effective_det_config(),
                );
            }
        }

        if backend == Backend::Liteinst {
            eprintln!("hermit: [liteinst backend] Detcore Tool active");
        }

        if self.no_namespace {
            eprintln!(
                "WARNING: --no-namespace is not a sandbox; run trusted guests only. The guest \
                 inherits the caller UID/GID/capabilities and shares host /proc, filesystem, /tmp, \
                 localhost/network, ports, Unix sockets, and mutable state between runs. Unsupported \
                 syscalls can mutate host state; --verify may be less deterministic due to shared state."
            );
        }

        if self.namespace_only {
            self.run_with_namespace_only(global)
        } else if self.verify {
            self.verify(global)
        } else {
            let (status, _) = self.run(global, false)?;
            Ok(status)
        }
    }

    /// Some arguments imply others. This is the place where that validation occurs.
    /// Also this performs side effects like accessing system randomness to implement --seed-from=SystemArgs
    pub fn validate_args(&mut self) -> Result<(), Error> {
        let perf_supported = match self.selected_backend() {
            Backend::Ptrace | Backend::E9patch => reverie_ptrace::is_perf_supported(),
            Backend::Dbi | Backend::Sabre | Backend::Kvm => true,
            Backend::Liteinst => false,
        };
        self.validate_args_with_perf_support(perf_supported)
    }

    fn validate_args_with_perf_support(&mut self, perf_supported: bool) -> Result<(), Error> {
        let liteinst_backend = self.selected_backend() == Backend::Liteinst;
        let config = &mut self.det_opts.det_config;

        config.has_uts_namespace = !self.no_namespace;

        if self.no_namespace {
            self.network = NetworkingMode::Host;
            self.tmp = Some(PathBuf::from(TMP_DIR));
        }

        if self.analyze_networking {
            config.warn_non_zero_binds = true;
        }

        config.sequentialize_threads = self.strict || !self.no_sequentialize_threads;
        config.deterministic_io = self.strict || !self.no_deterministic_io;
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-644): Review explicit strict mode failing on unsupported syscalls.
        if self.strict {
            config.panic_on_unsupported_syscalls = true;
        }
        if config.passthru_opt && config.panic_on_unsupported_syscalls {
            anyhow::bail!(
                "--passthru-opt cannot be combined with fail-closed unsupported-syscall handling \
                 (--strict or --panic-on-unsupported-syscalls)"
            );
        }
        config.shutdown_on_unsupported_syscall = config.panic_on_unsupported_syscalls;

        // virtualize_metadata implies virtualize_time
        if config.virtualize_metadata && !config.virtualize_time {
            anyhow::bail!(
                "--no-virtualize-time also requires --no-virtualize-metadata; metadata timestamps \
                 cannot be virtualized without virtual time"
            );
        }
        if !(0.0..=1.0).contains(&config.sched_sticky_random_param) {
            anyhow::bail!(
                "--sched-sticky-random-param must be between 0 and 1 inclusive (received {})",
                config.sched_sticky_random_param
            );
        }
        if let Some(multiplier) = config.clock_multiplier
            && (!multiplier.is_finite() || multiplier <= 0.0)
        {
            anyhow::bail!(
                "--clock-multiplier must be finite and positive (received {})",
                multiplier
            );
        }
        let minimum_max_timeslice = config.minimum_max_timeslice_nanos();
        if let Some(max_timeslice) = config.max_timeslice
            && u64::from(max_timeslice) < minimum_max_timeslice
        {
            anyhow::bail!(
                "--max-timeslice must be at least one RCB ({} virtual nanoseconds at this clock multiplier)",
                minimum_max_timeslice
            );
        }

        // Perform internal validation on the Config args, before taking into account the
        // hermit run args. User-controlled panic conditions are checked above.
        config.validate();

        // This is a Detcore Config-internal matter, but relies on reverie_ptrace, which detcore is
        // allowed to depend on:
        if config.max_timeslice.is_some() && liteinst_backend {
            eprintln!(
                "WARNING: --backend=liteinst does not implement PMU/RCB timer delivery; continuing with --max-timeslice=disabled."
            );
            config.max_timeslice = None;
        } else if config.max_timeslice.is_some() && !perf_supported {
            // TODO(T124429978): this could change back to tracing::warn! when the bug is fixed:
            eprintln!(
                "WARNING: --max-timeslice requires user-space perf counters, but \
                 perf_event_open is unavailable; continuing with \
                 --max-timeslice=disabled. Check the host perf_event_paranoid value and \
                 container seccomp policy."
            );
            config.max_timeslice = None;
        }

        if let Some(sf) = &self.seed_from {
            let seed = match sf {
                SeedFrom::Args => {
                    let mut hasher = DefaultHasher::new();
                    self.args.hash(&mut hasher);
                    self.program.hash(&mut hasher);
                    hasher.finish()
                }
                SeedFrom::SystemRandom => rand::random::<u64>(),
            };
            // TODO(T124429978): this could change back to tracing::warn! when the bug is fixed:
            eprintln!(
                "[hermit] auto setting --seed {0:?} --sched-seed {0:?}",
                seed
            );
            config.seed = seed;
        }

        // Deterministic RCB counts requires thread pinning.  But this only matters if
        // we're expecting full determinstic execution (sequentialize_threads).
        if config.max_timeslice.is_some() && config.sequentialize_threads {
            self.pin_threads = true;
        }

        if self.strace_only {
            config.virtualize_cpuid = false;
            config.virtualize_metadata = false;
            config.virtualize_time = false;
            config.deterministic_io = false;
            self.network = NetworkingMode::Host;
            config.sequentialize_threads = false;
            config.no_rcb_time = true;
            if self.tmp.is_none() {
                self.tmp = Some(PathBuf::from("/tmp"));
            }
        }

        // The gdbserver listens on a TCP port that is bound inside the guest's
        // network namespace. With the default isolated (`local`) networking, that
        // port lives in the guest's unshared netns and is unreachable from a host
        // gdb client, so `hermit run --gdbserver` silently hangs waiting for a
        // connection that can never arrive. Fall back to host networking so the
        // debugger can attach. This mirrors how replay-mode gdbserver already
        // works: replay never unshares the network namespace, which is exactly why
        // its gdbserver is reachable from the host.
        if self.det_opts.det_config.gdbserver && self.network == NetworkingMode::Local {
            if self.analyze_networking {
                anyhow::bail!(
                    "--gdbserver requires host networking so a host gdb client can reach the \
                     gdbserver port, but --analyze-networking forces an isolated network \
                     namespace. Run these two modes separately."
                );
            }
            // TODO(T124429978): this could change back to tracing::warn! when the bug is fixed:
            eprintln!(
                "WARNING: --gdbserver requires host networking so a host gdb client can reach \
                 the gdbserver port; overriding --network=local with --network=host for this \
                 debug session. Network isolation and deterministic networking are disabled \
                 while the gdbserver is attached."
            );
            self.network = NetworkingMode::Host;
        }

        // Advise when running a VMM (e.g. QEMU) under host-time virtualization,
        // whose emulated guest clock calibration this corrupts (issue #6).
        // Checked last so it reflects any overrides above that disable virtual
        // time (e.g. --strace-only).
        let virtualize_time = self.det_opts.det_config.virtualize_time;
        if let Some(warning) = vmm_time_virtualization_warning(&self.program, virtualize_time) {
            // TODO(T124429978): this could change back to tracing::warn! when the bug is fixed:
            eprintln!("{warning}");
        }

        Ok(())
    }

    fn validate_mount_sources(&self) -> Result<(), Error> {
        for bind in &self.bind {
            let source = Path::new(OsStr::from_bytes(bind.source.to_bytes()));
            if !source.exists() {
                anyhow::bail!(
                    "--bind source {} does not exist. Create it or correct the source path before \
                     starting Hermit.",
                    source.display()
                );
            }
        }
        for mount in &self.mount {
            if let Some(source) = mount.get_source()
                && !source.exists()
            {
                anyhow::bail!(
                    "--mount source {} does not exist. Create it or correct the source path \
                     before starting Hermit.",
                    source.display()
                );
            }
        }
        Ok(())
    }

    fn validate_e9patch_mount_targets(&self) -> Result<(), Error> {
        for bind in &self.bind {
            let target = Path::new(OsStr::from_bytes(bind.target.to_bytes()));
            validate_e9patch_mount_target(target)?;
        }
        for mount in &self.mount {
            validate_e9patch_mount_target(mount.get_target())?;
        }
        Ok(())
    }

    fn resolve_e9patch_overlay_target(&self, guest: &Path, host: &Path) -> Result<PathBuf, Error> {
        let canonical = fs::canonicalize(host)
            .with_context(|| format!("failed to resolve executable {}", host.display()))?;
        match self.mapped_host_program(guest) {
            GuestPathMapping::Mapped(mapped) => {
                let mapped = std::path::absolute(mapped)?;
                if canonical != mapped {
                    anyhow::bail!(
                        "e9patch cannot safely overlay symlinked executable {} through a custom \
                         guest mount; use the resolved executable path or remove the mount",
                        guest.display()
                    );
                }
                Ok(guest.to_path_buf())
            }
            GuestPathMapping::Unchanged => {
                let host = std::path::absolute(host)?;
                let symlinked = canonical != host;
                let tmp_is_remapped =
                    self.tmp.as_deref() != Some(Path::new(TMP_DIR)) || !self.bind.is_empty();
                let crosses_implicit_mount = symlinked
                    && ((tmp_is_remapped
                        && path_resolution_visits_prefix(&host, Path::new(TMP_DIR))?)
                        || path_resolution_visits_prefix(&host, Path::new("/proc"))?);
                if symlinked && (!self.mount.is_empty() || crosses_implicit_mount) {
                    anyhow::bail!(
                        "e9patch cannot safely resolve symlinked executable {} across guest \
                         mounts; use its resolved guest path or remove the relevant mounts",
                        guest.display()
                    );
                }
                let canonical_guest = normalize_guest_path(&canonical)?;
                match self.mapped_host_program(&canonical_guest) {
                    GuestPathMapping::Mapped(mapped)
                        if std::path::absolute(&mapped)? != canonical =>
                    {
                        anyhow::bail!(
                            "e9patch cannot safely resolve executable {} because a custom guest \
                             mount changes its canonical target {}; use the resolved guest path",
                            guest.display(),
                            canonical_guest.display()
                        );
                    }
                    GuestPathMapping::Hidden => anyhow::bail!(
                        "Program {} is hidden by a mount after resolving symlinks",
                        guest.display()
                    ),
                    GuestPathMapping::Mapped(_) | GuestPathMapping::Unchanged => {}
                }
                Ok(canonical_guest)
            }
            GuestPathMapping::Hidden => anyhow::bail!(
                "Program {} is not visible through the configured guest mounts",
                guest.display()
            ),
        }
    }

    fn mapped_host_program(&self, program: &Path) -> GuestPathMapping {
        for bind in self.bind.iter().rev() {
            let source = Path::new(OsStr::from_bytes(bind.source.to_bytes()));
            let target = Path::new(OsStr::from_bytes(bind.target.to_bytes()));
            if !target.starts_with(TMP_DIR) {
                continue;
            }
            if let Some(path) = mapped_path(program, source, target) {
                return GuestPathMapping::Mapped(path);
            }
        }
        for mount in self.mount.iter().rev() {
            let target = mount.get_target();
            if let Ok(suffix) = program.strip_prefix(target) {
                return match mount.get_source() {
                    Some(source) => GuestPathMapping::Mapped(source.join(suffix)),
                    None => GuestPathMapping::Hidden,
                };
            }
        }
        if let Ok(suffix) = program.strip_prefix(TMP_DIR) {
            return self
                .tmp
                .as_ref()
                .map(|tmp| GuestPathMapping::Mapped(tmp.join(suffix)))
                .unwrap_or(GuestPathMapping::Hidden);
        }
        GuestPathMapping::Unchanged
    }

    fn guest_current_dir(&self, command: &Command) -> Result<PathBuf, Error> {
        let directory = command
            .get_current_dir()
            .map(Path::to_path_buf)
            .unwrap_or(std::env::current_dir()?);
        let absolute = if directory.is_absolute() {
            directory
        } else {
            std::path::absolute(directory)?
        };
        normalize_guest_path(&absolute)
    }

    fn mapped_or_visible_host_program(&self, guest: &Path) -> Option<PathBuf> {
        match self.mapped_host_program(guest) {
            GuestPathMapping::Mapped(host) => Some(host),
            GuestPathMapping::Hidden => None,
            GuestPathMapping::Unchanged => Some(guest.to_path_buf()),
        }
    }

    fn resolve_guest_and_host_program(&self) -> Result<(PathBuf, PathBuf), Error> {
        let command = self.guest_command()?;
        let requested = Path::new(command.get_program());

        if requested.is_absolute() {
            let requested = normalize_guest_path(requested)?;
            if let Some(host) = self.mapped_or_visible_host_program(&requested) {
                return Ok((requested, host));
            }
            if requested.starts_with(TMP_DIR) && requested.exists() {
                anyhow::bail!(
                    "Program {} is under host /tmp, but Hermit replaces guest /tmp with an \
                     isolated directory. Pass --tmp=/tmp to expose host /tmp or bind the program \
                     to a guest path under /tmp.",
                    requested.display()
                );
            }
            anyhow::bail!(
                "Program {} is not visible through the configured guest mounts",
                requested.display()
            );
        }

        let current_dir = self.guest_current_dir(&command)?;
        if requested.components().count() > 1 {
            let guest = normalize_guest_path(&current_dir.join(requested))?;
            let host = self.mapped_or_visible_host_program(&guest).ok_or_else(|| {
                Error::msg(format!(
                    "Program {} is not visible through the configured guest mounts",
                    requested.display()
                ))
            })?;
            return Ok((guest, host));
        }

        let environment = command.get_captured_envs();
        let path = environment
            .get(OsStr::new("PATH"))
            .cloned()
            .unwrap_or_default();
        for directory in path
            .as_bytes()
            .split(|byte| *byte == b':')
            .map(|bytes| Path::new(OsStr::from_bytes(bytes)))
        {
            let candidate = if directory.is_absolute() {
                directory.join(requested)
            } else {
                current_dir.join(directory).join(requested)
            };
            let guest = normalize_guest_path(&candidate)?;
            let Some(host) = self.mapped_or_visible_host_program(&guest) else {
                continue;
            };
            if fs::metadata(&host).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            }) {
                return Ok((guest, host));
            }
        }
        anyhow::bail!(
            "Could not resolve program {:?} in the guest PATH. Check PATH or use an absolute \
             executable path.",
            requested
        )
    }

    fn validate_program(&self) -> Result<(), Error> {
        if self.selected_backend() == Backend::E9patch {
            let (_, host) = self.resolve_guest_and_host_program()?;
            return validate_executable(&host, &self.program);
        }

        let command = self.guest_command()?;
        let requested = Path::new(command.get_program());
        if requested.is_absolute() {
            if let GuestPathMapping::Mapped(host) = self.mapped_host_program(requested) {
                return validate_executable(&host, requested);
            }
            if requested.starts_with(TMP_DIR) && self.tmp.is_none() && requested.exists() {
                anyhow::bail!(
                    "Program {} is under host /tmp, but Hermit replaces guest /tmp with an \
                     isolated directory. Pass --tmp=/tmp to expose host /tmp or bind the program \
                     into guest /tmp.",
                    requested.display()
                );
            }
            return validate_executable(requested, requested);
        }

        let resolved = command.find_program().with_context(|| {
            format!(
                "Could not resolve program {:?} in the guest PATH. Check PATH or use an absolute \
                 executable path.",
                requested
            )
        })?;
        validate_executable(&resolved, requested)
    }

    fn validate_e9patch_source_visibility(&self, source: &Path) -> Result<(), Error> {
        for mount in &self.mount {
            let target = mount.get_target();
            if !target.starts_with(TMP_DIR) && source.starts_with(target) {
                anyhow::bail!(
                    "--mount target {} would hide the cached e9patch artifact {}; choose a more \
                     specific mount target or a different instruction-map cache directory",
                    target.display(),
                    source.display()
                );
            }
        }
        Ok(())
    }

    fn prepare_e9patch_program(&mut self) -> Result<(), Error> {
        let (guest, host) = self.resolve_guest_and_host_program()?;
        self.e9patch_program = Some(guest.clone());
        let overlay_target = self.resolve_e9patch_overlay_target(&guest, &host)?;
        if !is_elf_file(&host)? {
            eprintln!(
                ":: Backend: e9patch preprocessing + ptrace runtime; mapped_sites=0; \
                 main_executable=non-ELF; preprocessing=not-applicable"
            );
            return Ok(());
        }
        if let Some(reason) = hermit::e9patch::unavailable_reason() {
            anyhow::bail!("backend `e9patch` is unavailable: {reason}");
        }
        let prepared = hermit::e9patch::prepare(&host)?;
        if prepared.patched_sites != 0 {
            self.validate_e9patch_mount_targets()?;
            self.validate_e9patch_source_visibility(&prepared.binary)?;
            self.e9patch_overlay = Some(E9patchOverlay {
                source: prepared.binary,
                target: overlay_target,
            });
        }
        let rewrite_cache = if prepared.patched_sites == 0 {
            "not-applicable"
        } else if prepared.rewrite_cache_hit {
            "hit"
        } else {
            "miss"
        };
        eprintln!(
            ":: Backend: e9patch preprocessing + ptrace runtime; candidate_sites={}; \
             mapped_sites={}; b0_sites={}; \
             instruction_map_cache={:?}; rewrite_cache={}; artifact_sha256={}",
            prepared.candidate_sites,
            prepared.patched_sites,
            prepared.b0_sites,
            prepared.instruction_map_cache_status,
            rewrite_cache,
            prepared.artifact_sha256.as_deref().unwrap_or("none"),
        );
        Ok(())
    }

    fn tmpfs(&self) -> Result<Tmpfs<'_>, Error> {
        match self.tmp.as_ref() {
            Some(path) => {
                let path = path.as_path();
                fs::create_dir_all(path)?;
                Ok(Tmpfs::Path(path))
            }
            None => Ok(Tmpfs::Temp(tempfile::TempDir::new()?)),
        }
    }

    pub fn run(
        &self,
        global: &GlobalOpts,
        capture_output: bool,
    ) -> Result<(ExitStatus, Option<Output>), Error> {
        if self.no_namespace {
            let mut process = Container::new();
            apply_affinity(&mut process, self.pin_threads);
            return with_container(&mut process, || {
                self.run_in_container(global, capture_output)
            });
        }

        let tmpfs = self.tmpfs()?;

        let (mut container, _identity_sources) = self.container(tmpfs.path())?;

        with_container(&mut container, || {
            self.run_in_container(global, capture_output)
        })
    }

    fn run_with_namespace_only(&self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        // TODO: Make this use detcore instead after detcore is capable of being
        // "lightweight".
        let _guard = global.init_tracing();

        let tmpfs = self.tmpfs()?;
        let PreparedMounts {
            mounts,
            identity_sources: _identity_sources,
        } = self.mounts(tmpfs.path())?;

        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            .unshare(Namespace::PID)
            .map_root()
            .hostname("hermetic-container.local")
            .domainname("local")
            .mount(Mount::proc())
            .mounts(mounts);

        match &self.network {
            NetworkingMode::Local => {
                command.local_networking_only();
            }
            NetworkingMode::Host => {}
        }

        let mut child = command.spawn()?;

        let exit_status = child.wait_blocking()?;

        Ok(exit_status)
    }

    // Execution mode corresponding to `run --verify`:
    fn verify(&self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        let (log1, log2) =
            temp_log_files("run1", "run2").context("Failed to create temporary log files")?;

        let (log1_file, log1_path) = log1.into_parts();
        let (log2_file, log2_path) = log2.into_parts();

        eprintln!(":: {}", "Run1...".yellow().bold());

        let out1: Output = self.run_verify(log1_file, global)?;

        // With --verify the first run's `--log` output was diverted to a
        // temporary file for later comparison rather than shown to the user.
        // When --verify-logs is set, echo that first run's log to stderr so the
        // user still sees `--log` output, matching a normal (non-verify) run.
        // The log file is fully flushed here because run_verify runs each
        // execution in a child process that has already exited.
        if self.verify_logs {
            match fs::read(&log1_path) {
                Ok(bytes) => std::io::stderr().write_all(&bytes)?,
                Err(err) => eprintln!(
                    "WARNING: --verify-logs could not read first-run log {}: {}",
                    log1_path.display(),
                    err
                ),
            }
        }

        if !self.verify_allow.satisfies(out1.status) {
            eprintln!(
                "First run errored during --verify, not continuing to a second. Stdout:\n{}\nStderr:\n{}",
                String::from_utf8_lossy(&out1.stdout),
                String::from_utf8_lossy(&out1.stderr),
            );
            return Err(Error::msg("First run during --verify exited in error"));
        }

        eprintln!(":: {}", "Run2...".yellow().bold());
        let out2 = self.run_verify(log2_file, global)?;

        let kvm_output_only = self.selected_backend() == Backend::Kvm;
        let status = compare_two_runs(
            ComparedRun {
                output: &out1,
                log: log1_path,
            },
            ComparedRun {
                output: &out2,
                log: log2_path,
            },
            ComparisonOptions {
                success_message: if kvm_output_only {
                    "Success: KVM guest output and exit status matched."
                } else {
                    "Success: deterministic. Determinism verified."
                },
                failure_message: "Failure: nondeterministic.",
                verbose: self.verify_verbose,
                compare_logs: !kvm_output_only,
            },
        )?;

        let backend_banner = match self.selected_backend() {
            Backend::Kvm => Some("KVM (reverie-kvm KvmGuest<Detcore>)"),
            Backend::Liteinst => Some("LiteInst (reverie-liteinst LiteinstGuest<Detcore>)"),
            _ => None,
        };
        if let Some(backend_banner) = backend_banner {
            eprintln!(":: Backend: {backend_banner}");
            std::io::stdout().write_all(&out1.stdout)?;
            std::io::stderr().write_all(&out1.stderr)?;
        }
        Ok(status)
    }

    /// Returns the mounts to be used with the container.
    fn mounts(&self, tmpfs: &Path) -> Result<PreparedMounts, Error> {
        let (mut mounts, identity_sources) = identity_hardening_mounts()?;

        for mount in &self.mount {
            if let Ok(path) = mount.get_target().strip_prefix(TMP_DIR) {
                // If the target is in /tmp, change it so it goes to our
                // temporary /tmp instead.
                mounts.push(mount.clone().target(tmpfs.join(path)).touch_target());
            } else {
                mounts.push(mount.clone());
            }
        }

        for bind in &self.bind {
            let mount = Mount::from(bind.clone()).rshared();

            // Bind mounts currently only make sense for things in `/tmp` since
            // that is the only directory we overlay.
            if let Ok(relative_path) = mount.get_target().strip_prefix(TMP_DIR) {
                let target = tmpfs.join(relative_path);
                mounts.push(mount.target(target).touch_target());
            } else {
                eprintln!(
                    "WARNING: --bind target {} is outside guest /tmp, so this option has no \
                     effect; files outside /tmp are already visible unless another mount hides them",
                    bind.target.to_string_lossy()
                );
            }
        }

        if let Some(overlay) = &self.e9patch_overlay {
            let target = if let Ok(relative_path) = overlay.target.strip_prefix(TMP_DIR) {
                tmpfs.join(relative_path)
            } else {
                overlay.target.clone()
            };
            mounts.push(
                Mount::bind(&overlay.source, &target)
                    .readonly()
                    .touch_target(),
            );
            mounts.push(
                Mount::new(target)
                    .flags(MountFlags::MS_BIND | MountFlags::MS_REMOUNT | MountFlags::MS_RDONLY),
            );
        }
        // Bind the /tmp/tmpXXXXXX tmpfs mount over /tmp to hide it. This way,
        // we still preserve the files or directories bind-mounted inside of it
        // while hiding the real /tmp.
        mounts.push(Mount::bind(tmpfs, TMP_DIR).rshared());

        Ok(PreparedMounts {
            mounts,
            identity_sources,
        })
    }

    /// Returns a configured container to run a function in.
    fn container(&self, tmpfs: &Path) -> Result<(Container, IdentityGuard), Error> {
        let mut container = default_container(self.pin_threads);

        match &self.network {
            NetworkingMode::Local => {
                container.local_networking_only();
            }
            NetworkingMode::Host => {
                // This conflict/invariant should could be resolved upstream:
                if self.analyze_networking {
                    container.local_networking_only();
                }
            }
        }

        let PreparedMounts {
            mounts,
            identity_sources,
        } = self.mounts(tmpfs)?;
        container.mounts(mounts);

        Ok((container, identity_sources))
    }

    pub fn run_verify(&self, log_file: fs::File, global: &GlobalOpts) -> Result<Output, Error> {
        if self.no_namespace {
            // Verify initializes a process-global tracing subscriber for each run. Keep a plain
            // child-process boundary between runs, but do not configure any namespaces or mounts.
            let mut process = Container::new();
            apply_affinity(&mut process, self.pin_threads);
            let mut log_file = Some(log_file);
            return with_container(&mut process, || {
                self.run_verify_in_container(&mut log_file, global)
            });
        }

        let tmpfs = self.tmpfs()?;

        let (mut container, _identity_sources) = self.container(tmpfs.path())?;

        let mut log_file = Some(log_file);
        with_container(&mut container, || {
            self.run_verify_in_container(&mut log_file, global)
        })
    }

    fn merge_from_env_settings(&self, command: &mut Command) -> anyhow::Result<()> {
        for (var, m_val) in &self.env {
            if let Some(val) = m_val {
                command.env(var, val);
            } else if let Ok(value) = std::env::var(var) {
                command.env(var, &value);
            } else {
                anyhow::bail!(
                    "Attempt to pass through env var {}, but it is not set in the host environment",
                    var
                )
            }
        }
        Ok(())
    }

    fn guest_command(&self) -> Result<Command, Error> {
        let program = self.e9patch_program.as_ref().unwrap_or(&self.program);
        let mut command = Command::new(program);
        command.args(&self.args);
        if self.e9patch_program.is_some() {
            command.arg0(&self.program);
        }
        if let Some(current_dir) = &self.workdir {
            command.current_dir(current_dir);
        }
        match self.base_env {
            BaseEnv::Empty => {
                command.env_clear();
                self.merge_from_env_settings(&mut command)?
            }
            BaseEnv::Minimal => {
                command.env_clear();
                command.env("HOSTNAME", "hermetic-container.local");
                command.env(
                    "PATH",
                    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                );
                command.env("HOME", "/root");
                self.merge_from_env_settings(&mut command)?
            }
            BaseEnv::Host => self.merge_from_env_settings(&mut command)?,
        }

        Ok(command)
    }

    fn save_config_to_disk(&self) -> Result<(), Error> {
        if let Some(path) = &self.save_config {
            let mut file = File::create(path)?;
            file.write_all(format!("{:#?}\n", self).as_bytes())?;
        }
        Ok(())
    }

    fn effective_det_config(&self) -> DetConfig {
        let mut config = self.det_opts.det_config.clone();
        if std::env::var(FAIL_CLOSED_ENV).is_ok_and(|value| value == "1") {
            config.panic_on_unsupported_syscalls = true;
        }
        config.shutdown_on_unsupported_syscall = config.panic_on_unsupported_syscalls;
        config
    }

    fn run_in_container(
        &self,
        global: &GlobalOpts,
        capture_output: bool,
    ) -> Result<(ExitStatus, Option<Output>), Error> {
        let _guard = global.init_tracing();

        let command = self.guest_command()?;

        let config = self.effective_det_config();
        self.save_config_to_disk()?;

        if capture_output {
            let out = hermit::run_with_output_backend(
                command,
                config,
                self.summary,
                &self.summary_json,
                self.runtime_backend(),
            )?;
            Ok((out.status, Some(out)))
        } else {
            let status = hermit::run_with_backend(
                command,
                config,
                self.summary,
                &self.summary_json,
                self.runtime_backend(),
            )?;
            Ok((status, None))
        }
    }

    fn run_verify_in_container(
        &self,
        log_file: &mut Option<fs::File>,
        global: &GlobalOpts,
    ) -> Result<Output, Error> {
        // HACK: Use interior mutability to workaround not being able to pass
        // `log_file` by value. Guaranteed by caller to never panic.
        let log_file = log_file.take().unwrap();

        let minimum_level = if self.verify_verbose {
            LevelFilter::TRACE
        } else {
            LevelFilter::DEBUG
        };
        let level = global.log.unwrap_or(minimum_level).max(minimum_level);

        let _guard = init_file_tracing(Some(level), log_file);

        let command = self.guest_command()?;

        let config = self.effective_det_config();
        self.save_config_to_disk()?;

        hermit::run_with_output_backend(
            command,
            config,
            self.summary,
            &self.summary_json,
            self.runtime_backend(),
        )
    }
}

/// Represents a tmpfs location. There are different ways to construct `/tmp` for
/// the container and this encapsulates all of them.
enum Tmpfs<'a> {
    /// Use an existing path as `/tmp`.
    Path(&'a Path),

    /// Use a new temporary directory as `/tmp`.
    Temp(tempfile::TempDir),
}

impl<'a> Tmpfs<'a> {
    /// Returns the path to `/tmp`.
    pub fn path(&self) -> &Path {
        match self {
            Self::Path(path) => path,
            Self::Temp(temp) => temp.path(),
        }
    }
}
