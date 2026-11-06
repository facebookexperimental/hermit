/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Admission for intercepted CPU profiles: v3 binds qualified host XSTATE
//! facts; host-no-rdrand requires CPUID faulting without the v3 qualification.
//!
//! Every class the v3 startup qualification cannot cover is refused by name.
//! Neither nondefault profile silently falls back to v2 or native CPUID.

use std::path::Path;
use std::path::PathBuf;

use detcore_model::cpu_profile::CpuProfile;
use detcore_model::cpu_profile::HostXstate;
use nix::sched::CpuSet;
use nix::sched::sched_getaffinity;
use nix::sched::sched_setaffinity;
use nix::unistd::Pid;

#[cfg(target_arch = "x86_64")]
use crate::xstate_probe;

/// What a host qualification yields: the XSTATE probe's report and the host
/// facts a v3 run binds.
#[cfg(target_arch = "x86_64")]
pub type Qualification = (xstate_probe::ProbeReport, HostXstate);
/// What a host qualification yields. The XSTATE probe runs x86 instructions,
/// so on other architectures no qualification exists.
#[cfg(not(target_arch = "x86_64"))]
pub type Qualification = (std::convert::Infallible, HostXstate);

/// Why a profile is refused, after `--cpu-profile <profile> refused: `.
pub type AdmissionRefusal = String;

/// A refused CPU profile, carried as an error so that it
/// crosses the container boundary as a policy refusal (exit 122).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuProfileRefusal {
    pub profile: CpuProfile,
    pub reason: AdmissionRefusal,
}

impl CpuProfileRefusal {
    pub fn new(profile: CpuProfile, reason: impl Into<String>) -> Self {
        Self {
            profile,
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for CpuProfileRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "--cpu-profile {} refused: {}", self.profile, self.reason)
    }
}

impl std::error::Error for CpuProfileRefusal {}

/// The admission request of a recording or replay, which always runs on the
/// ptrace backend with virtual CPUID.
pub fn record_or_replay_request(gdbserver: bool) -> AdmissionRequest<'static> {
    AdmissionRequest {
        backend: "ptrace",
        ptrace: true,
        virtualize_cpuid: true,
        virtualize_time: true,
        strace_only: false,
        gdbserver,
        namespace_only: false,
    }
}

/// The first host fact in which `replay` differs from `recorded`, by name, or
/// `None` when they are equal.
pub fn first_difference(recorded: &HostXstate, replay: &HostXstate) -> Option<&'static str> {
    if recorded.vendor != replay.vendor {
        Some("the CPU vendor")
    } else if recorded.signature != replay.signature {
        Some("the CPU family, model or stepping")
    } else if recorded.xcr0 != replay.xcr0 {
        Some("XCR0")
    } else if recorded.xsave_leaves != replay.xsave_leaves {
        Some("the XSAVE layout (CPUID leaf 0xD)")
    } else if (
        recorded.xcomp_supp,
        recorded.xcomp_perm,
        recorded.xcomp_guest_perm,
    ) != (
        replay.xcomp_supp,
        replay.xcomp_perm,
        replay.xcomp_guest_perm,
    ) {
        Some("the ARCH_GET_XCOMP masks")
    } else if recorded.xfeature_max != replay.xfeature_max {
        Some("XFEATURE_MAX")
    } else if (recorded.probe_cases, recorded.probe_forced_exit)
        != (replay.probe_cases, replay.probe_forced_exit)
    {
        Some("the qualification it passed")
    } else {
        None
    }
}

/// What the run asks for, as far as admission is concerned.
#[derive(Clone, Copy, Debug)]
pub struct AdmissionRequest<'a> {
    /// The backend's command-line name, and whether it is the ptrace backend.
    pub backend: &'a str,
    pub ptrace: bool,
    pub virtualize_cpuid: bool,
    pub virtualize_time: bool,
    pub strace_only: bool,
    pub gdbserver: bool,
    /// `--namespace-only`, which runs the guest with no Detcore at all.
    pub namespace_only: bool,
}

/// The refusals that need no host probe, in the order they are checked.
pub fn check_request(request: &AdmissionRequest<'_>) -> Result<(), AdmissionRefusal> {
    if !request.ptrace {
        let why = match request.backend {
            "kvm" => {
                "the KVM backend switches XCR0 between the guest's and the host's on every VM \
                 exit, a transition the startup qualification does not cover"
            }
            "dbt" => {
                "Detcore answers DBT's CPUID, but the DBT runtime's configuration carries no \
                 CPU profile, so it answers only the x86-64-v2 table"
            }
            "sabre" | "liteinst" | "in-guest-trap" => {
                "this backend does not trap CPUID, so the guest would see the host's"
            }
            _ => "only the ptrace backend is qualified",
        };
        return Err(format!("--backend={}: {why}", request.backend));
    }
    if request.namespace_only {
        return Err(
            "--namespace-only runs the guest without Detcore, so nothing answers its CPUID".into(),
        );
    }
    if !request.virtualize_cpuid || request.strace_only {
        return Err(
            "the profile is a virtual CPUID table, and this run does not virtualize CPUID".into(),
        );
    }
    if request.gdbserver {
        return Err(
            "--gdbserver writes the guest's FP registers, and the kernel's regset write forces \
             XSAVE header bits the qualification binds"
                .into(),
        );
    }
    Ok(())
}

/// The host profile must intercept CPUID and TSC on ptrace. Unlike v3, it
/// retains host XSAVE behavior and does not claim the XSTATE qualification.
pub fn check_host_no_rdrand_request(
    request: &AdmissionRequest<'_>,
) -> Result<(), AdmissionRefusal> {
    if !request.ptrace {
        return Err(format!(
            "--backend={} cannot provide this profile's required CPUID faulting; \
             use --backend=ptrace",
            request.backend
        ));
    }
    if request.namespace_only {
        return Err(
            "--namespace-only runs without CPUID interception; remove --namespace-only".into(),
        );
    }
    if request.strace_only {
        return Err(
            "--strace-only runs without CPUID and TSC interception; remove --strace-only".into(),
        );
    }
    if !request.virtualize_cpuid {
        return Err("CPUID must remain intercepted; remove --no-virtualize-cpuid".into());
    }
    if !request.virtualize_time {
        return Err(
            "the host's RDTSC/RDTSCP features require Hermit's virtual time; \
             remove --no-virtualize-time"
                .into(),
        );
    }
    Ok(())
}

#[cfg(target_arch = "x86_64")]
const CPUID_FAULTING_REMEDY: &str = "use a CPU and Linux kernel with CPUID faulting support (capable AMD CPUs need \
     Linux 6.17 or its CPUID-faulting backport)";

/// The source-tree script that loads a module turning on CPUID faulting in a
/// KVM guest whose hypervisor offers it but whose pre-6.17 kernel probes it
/// only on Intel vCPU models.
pub const CPUID_FAULTING_VM_SCRIPT: &str = "tools/cpuid-fault-kmod/enable-cpuid-faulting.sh";

/// A pointer to [`CPUID_FAULTING_VM_SCRIPT`] when `host` reports a hypervisor
/// (CPUID leaf 1 ECX bit 31) and `release` is older than Linux 6.17, the only
/// hosts the module can help; `None` otherwise.
pub fn cpuid_faulting_vm_pointer(host: &dyn HostCpuid, release: &str) -> Option<String> {
    let hypervisor = host.cpuid(1, 0)[2] & (1 << 31) != 0;
    let before_6_17 = kernel_version(release).is_some_and(|version| version < (6, 17));
    (hypervisor && before_6_17).then(|| {
        format!(
            "this host is a virtual machine on Linux {release}; if its hypervisor offers CPUID \
             faulting, {CPUID_FAULTING_VM_SCRIPT} in the Hermit source tree can enable it"
        )
    })
}

#[cfg(target_arch = "x86_64")]
fn native_cpuid_faulting_vm_pointer() -> Option<String> {
    let uname = nix::sys::utsname::uname().ok()?;
    cpuid_faulting_vm_pointer(&NativeCpuid, &uname.release().to_string_lossy())
}

/// The note to print after an error whose chain reports missing CPUID
/// faulting, on a host where the script may help, unless a message already
/// carries the pointer.
pub fn missing_cpuid_faulting_note(messages: &[String]) -> Option<String> {
    let missing = messages.iter().any(|message| {
        message.contains("ptrace-cpuid-faulting-unavailable")
            || message.contains("CPUID faulting is unavailable")
    });
    let pointed = messages
        .iter()
        .any(|message| message.contains(CPUID_FAULTING_VM_SCRIPT));
    if missing && !pointed {
        native_cpuid_faulting_vm_pointer()
    } else {
        None
    }
}

#[cfg(target_arch = "x86_64")]
fn check_host_no_rdrand_cpuid(host: &dyn HostCpuid) -> Result<(), AdmissionRefusal> {
    let intel_hybrid = host.cpuid(0, 0)[0] >= 7 && host.cpuid(7, 0)[3] & (1 << 15) != 0;
    // AMD advertises heterogeneous cores independently of Intel's hybrid bit:
    // Linux identifies X86_FEATURE_AMD_HTR_CORES through this extended leaf.
    let amd_heterogeneous = host.cpuid(0x8000_0000, 0)[0] >= 0x8000_0026
        && host.cpuid(0x8000_0026, 0)[0] & (1 << 30) != 0;
    if intel_hybrid || amd_heterogeneous {
        return Err(
            "this is a hybrid CPU whose CPUID features can change between core types; \
             use a homogeneous CPU host for --cpu-profile=host-no-rdrand"
                .into(),
        );
    }
    Ok(())
}

/// Probe the Linux CPUID-faulting API in a child. Disabling CPUID in the
/// tracer would also fault Rust's and libc's own feature detection, so only
/// the forked child changes its state. After fork the child uses only libc
/// syscalls and `_exit`, without allocation, locks or unwinding.
#[cfg(target_arch = "x86_64")]
pub fn qualify_host_no_rdrand() -> Result<(), AdmissionRefusal> {
    const ARCH_GET_CPUID: libc::c_int = 0x1011;
    const ARCH_SET_CPUID: libc::c_int = 0x1012;
    // SAFETY: the child calls only async-signal-safe operations below.
    let child = unsafe { libc::fork() };
    if child < 0 {
        return Err(format!(
            "cannot probe CPUID faulting: fork failed ({}); resolve the process-resource \
             failure and retry",
            std::io::Error::last_os_error()
        ));
    }
    if child == 0 {
        // SAFETY: these arch_prctl operations take integer arguments only.
        unsafe {
            let initial = libc::syscall(libc::SYS_arch_prctl, ARCH_GET_CPUID, 0);
            if initial != 0 && initial != 1 {
                libc::_exit(1);
            }
            if libc::syscall(libc::SYS_arch_prctl, ARCH_SET_CPUID, 0) != 0 {
                libc::_exit(2);
            }
            if libc::syscall(libc::SYS_arch_prctl, ARCH_GET_CPUID, 0) != 0 {
                libc::_exit(3);
            }
            libc::_exit(0);
        }
    }
    let mut status = 0;
    loop {
        // SAFETY: child is our live child PID and status points to one int.
        let waited = unsafe { libc::waitpid(child, &mut status, 0) };
        if waited == child {
            break;
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(format!(
                "cannot probe CPUID faulting: waitpid failed ({error}); resolve the child \
                 process failure and retry"
            ));
        }
    }
    let failure = if libc::WIFEXITED(status) {
        match libc::WEXITSTATUS(status) {
            0 => return check_host_no_rdrand_cpuid(&NativeCpuid),
            1 => "ARCH_GET_CPUID did not return a valid initial state",
            2 => "ARCH_SET_CPUID(0) failed",
            3 => "ARCH_GET_CPUID did not confirm that CPUID was disabled",
            _ => "the CPUID-faulting probe exited unexpectedly",
        }
    } else {
        "the CPUID-faulting probe did not exit normally"
    };
    let vm_pointer = native_cpuid_faulting_vm_pointer()
        .map(|pointer| format!("; {pointer}"))
        .unwrap_or_default();
    Err(format!(
        "CPUID faulting is unavailable: {failure}; {CPUID_FAULTING_REMEDY}{vm_pointer}"
    ))
}

// Each bit records a source CPU whose faulting and host-profile checks passed.
// Atomics survive the CLI's fork without a possibly held mutex. A Config also
// carries its chosen CPU, so concurrent library admissions cannot overwrite
// another run's source or authorize an unpinned tracer.
static HOST_NO_RDRAND_CPUS: [std::sync::atomic::AtomicBool; libc::CPU_SETSIZE as usize] =
    [const { std::sync::atomic::AtomicBool::new(false) }; libc::CPU_SETSIZE as usize];

fn affinity_cpus(affinity: &CpuSet) -> Vec<usize> {
    (0..CpuSet::count())
        .filter(|cpu| affinity.is_set(*cpu).unwrap_or(false))
        .collect()
}

fn host_profile_affinity() -> Result<CpuSet, AdmissionRefusal> {
    sched_getaffinity(Pid::from_raw(0))
        .map_err(|error| format!("cannot read the CPUID source thread's CPU affinity: {error}"))
}

/// The actual tracer is pinned before it constructs the guest, including a
/// library's cached tracer worker. Guest/worker threads inherit this singleton.
/// Every leaf and arbitrarily high subleaf therefore has one native source.
fn pin_host_cpuid_source(cpu: usize) -> Result<(), AdmissionRefusal> {
    let mut selected = CpuSet::new();
    selected.set(cpu).map_err(|error| error.to_string())?;
    sched_setaffinity(Pid::from_raw(0), &selected)
        .map_err(|error| format!("cannot pin the CPUID source thread to CPU {cpu}: {error}"))?;
    check_host_cpuid_source(cpu)
}

fn check_host_cpuid_source(cpu: usize) -> Result<(), AdmissionRefusal> {
    if affinity_cpus(&host_profile_affinity()?) != [cpu] {
        return Err(format!(
            "the tracer's CPU affinity changed after host CPUID admission; \
             keep the tracer pinned to its admitted CPUID source CPU {cpu}"
        ));
    }
    Ok(())
}

/// Select the lowest allowed CPU before the CLI's container can randomly
/// narrow affinity. Probe that CPU, restore the caller's mask, and return the
/// source to carry in Config::cpu_profile_source. The launch guard pins the
/// actual tracer thread; this also works with a pre-existing library worker.
/// Repeat runs require the same allowed CPU set and host configuration.
pub fn admit_host_no_rdrand(request: &AdmissionRequest<'_>) -> Result<usize, AdmissionRefusal> {
    admit_host_no_rdrand_with(request, qualify_host_no_rdrand)
}

fn admit_host_no_rdrand_with(
    request: &AdmissionRequest<'_>,
    probe: impl FnOnce() -> Result<(), AdmissionRefusal>,
) -> Result<usize, AdmissionRefusal> {
    check_host_no_rdrand_request(request)?;
    let original = host_profile_affinity()?;
    let cpu = affinity_cpus(&original)
        .first()
        .copied()
        .ok_or("the CPUID source thread has no allowed CPU")?;
    let admission = pin_host_cpuid_source(cpu).and_then(|()| probe());
    // Restore on success as well as refusal: a library caller's other work
    // must not inherit a placement constraint intended for its tracer.
    sched_setaffinity(Pid::from_raw(0), &original).map_err(|error| {
        format!("cannot restore the caller's CPU affinity after CPUID admission: {error}; probe result: {admission:?}")
    })?;
    admission?;
    HOST_NO_RDRAND_CPUS[cpu].store(true, std::sync::atomic::Ordering::Release);
    Ok(cpu)
}

/// A recording currently binds only the v3 host facts, not a complete native
/// CPUID snapshot. Refuse host mode before writing metadata or launching a
/// guest rather than accidentally treating it as v3.
pub fn check_record_or_replay(profile: CpuProfile) -> Result<(), CpuProfileRefusal> {
    if profile == CpuProfile::Auto {
        // `record start` resolves auto before this check, so a recording
        // holds the profile that ran; one claiming auto was never written by
        // Hermit, and replaying it would derive the profile again.
        return Err(CpuProfileRefusal::new(
            profile,
            "a recording holds the named profile auto resolved to, never auto itself",
        ));
    }
    if profile == CpuProfile::HostNoRdrand {
        return Err(CpuProfileRefusal::new(
            profile,
            "record/replay is not supported because recording metadata does not bind \
             the host CPUID state; use `hermit run --cpu-profile=host-no-rdrand --verify` \
             for repeated runs, or record with --cpu-profile=x86-64-v2 or x86-64-v3",
        ));
    }
    Ok(())
}

/// How many script interpreters `--cpu-profile auto` follows before it
/// refuses a chain as too long: the kernel bounds nested script
/// interpretation too (`bprm` recursion in fs/exec.c), and an automatic
/// consumer must not chase an unbounded chain either.
const MAX_SCRIPT_HOPS: usize = 4;

/// The named profile `--cpu-profile auto` resolves to for a guest program
/// at host path `program`, with why ([`detcore::exec_record::auto_profile`]):
/// from the declarations of the image it execs first and of that image's
/// program interpreter, where `to_host` gives the host path of a path as
/// the guest names it, or `None` where a mount with no source hides it in
/// the launch view: a hidden interpreter refuses, rather than reading the
/// host file its mount target shadows. `program` is the program in the
/// launch's completed view (its mounts, binds and image rootfs already
/// applied by the caller), and every script hop is mapped through
/// `to_host` the same way. A script's interpreter is followed as the
/// kernel follows it
/// (Detcore's `script_interpreter`, the `load_script` rules: the line
/// and the path end where the kernel ends them, at a newline and at a
/// space, tab or NUL), through a bounded chain of scripts to the image
/// the kernel would exec. Evidence is read strictly: a declaration
/// beside a scan state that could not be decided refuses, rather than
/// selecting from a lower bound. This chooses the profile before
/// launch; Detcore's check of each executed image and interpreter
/// (`Detcore::isa_refusal`) is still what refuses a run whose actual
/// objects need more, so a pathname read here can never admit one.
///
/// On a host that is not x86-64 no CPU profile is presented: an ELF64 image
/// built for the host's own machine declares no x86-64 ISA level, so auto
/// names the default profile, which there serves nothing
/// ([`isa_needs_on`]). Every other read and refusal is the same as on an
/// x86-64 host.
pub fn resolve_auto(
    program: &Path,
    to_host: impl Fn(&Path) -> Option<PathBuf>,
) -> Result<(CpuProfile, String), String> {
    resolve_auto_on(host_machine(), program, to_host)
}

/// `EM_X86_64`.
const EM_X86_64: u16 = 62;
/// `EM_AARCH64`.
const EM_AARCH64: u16 = 183;

/// This host's ELF machine (`e_machine`), or 0 for one auto does not know.
fn host_machine() -> u16 {
    match std::env::consts::ARCH {
        "x86_64" => EM_X86_64,
        "aarch64" => EM_AARCH64,
        _ => 0,
    }
}

/// The `e_machine` of the little-endian ELF64 image at `path`, or `None`
/// for a file that is unreadable or not one.
fn elf64_machine(path: &Path) -> Option<u16> {
    use std::io::Read as _;
    let mut header = [0_u8; 20];
    std::fs::File::open(path)
        .ok()?
        .read_exact(&mut header)
        .ok()?;
    // ELFMAG, ELFCLASS64, ELFDATA2LSB.
    (header[..4] == *b"\x7fELF" && header[4] == 2 && header[5] == 1)
        .then(|| u16::from_le_bytes([header[18], header[19]]))
}

/// What the image at `path` declares for auto on a host of ELF machine
/// `host`: its x86-64 ISA declaration, read strictly; and on a host that is
/// not x86-64, where no CPU profile is presented, nothing (`Unmarked`) for
/// an ELF64 image built for the host's own machine, which no x86-64 level
/// applies to. Anything else unreadable or not x86-64 stays `None`, which
/// auto refuses.
fn isa_needs_on(host: u16, path: &Path) -> Option<detcore::exec_record::IsaNeeds> {
    let needs = detcore::exec_record::IsaNeeds::read_path_strict(path);
    if needs.is_none() && host != EM_X86_64 && host != 0 && elf64_machine(path) == Some(host) {
        return Some(detcore::exec_record::IsaNeeds::Unmarked);
    }
    needs
}

/// [`resolve_auto`] on a host of ELF machine `host`.
fn resolve_auto_on(
    host: u16,
    program: &Path,
    to_host: impl Fn(&Path) -> Option<PathBuf>,
) -> Result<(CpuProfile, String), String> {
    use detcore::exec_record::IsaNeeds;
    use detcore::exec_record::Script;
    let mut image = program.to_path_buf();
    for _ in 0..=MAX_SCRIPT_HOPS {
        match detcore::exec_record::script_interpreter(&image) {
            Script::Not => break,
            Script::Interpreter(interpreter) => {
                image = to_host(&interpreter)
                    .ok_or_else(|| format!("{} is hidden by a mount", interpreter.display()))?;
            }
            Script::Declined => {
                return Err(format!(
                    "{} names no interpreter the kernel would run",
                    image.display()
                ));
            }
            Script::Unread => {
                return Err(format!("{} is unreadable", image.display()));
            }
        }
    }
    if matches!(
        detcore::exec_record::script_interpreter(&image),
        Script::Interpreter(_)
    ) {
        return Err(format!(
            "{} is a script chain longer than {MAX_SCRIPT_HOPS} interpreters",
            program.display()
        ));
    }
    let needs = isa_needs_on(host, &image);
    let mut objects = vec![(format!("the executable {}", image.display()), needs)];
    if matches!(needs, None | Some(IsaNeeds::Malformed(_))) {
        // Refused as unreadable or malformed, before its interpreter.
        return auto_profile_on(host, &objects);
    }
    // An image built for this host's own machine, on a host that is not
    // x86-64, names its interpreter in the same `PT_INTERP`.
    let machine = elf64_machine(&image)
        .filter(|&machine| host != EM_X86_64 && machine == host)
        .unwrap_or(EM_X86_64);
    match detcore::exec_record::program_interpreter_of(&image, machine) {
        Ok(None) => {}
        Ok(Some(interpreter)) => {
            let Some(interpreter_on_host) = to_host(&interpreter) else {
                return Err(format!(
                    "its program interpreter {} is hidden by a mount",
                    interpreter.display()
                ));
            };
            objects.push((
                format!("its program interpreter {}", interpreter.display()),
                isa_needs_on(host, &interpreter_on_host),
            ));
        }
        Err(why) => return Err(format!("{} cannot be read: {why}", image.display())),
    }
    auto_profile_on(host, &objects)
}

/// [`detcore::exec_record::auto_profile`] of `objects` on a host of ELF
/// machine `host`, whose refusal of an object it could not read names what
/// that host runs: on a host that is not x86-64, an image of the host's own
/// machine or an x86-64 image, not an x86-64 image alone.
fn auto_profile_on(
    host: u16,
    objects: &[(String, Option<detcore::exec_record::IsaNeeds>)],
) -> Result<(CpuProfile, String), String> {
    use detcore::exec_record::IsaNeeds;
    if host != EM_X86_64 && host != 0 {
        // The first object auto would refuse, in the order it refuses them.
        let first = objects
            .iter()
            .find(|(_, needs)| matches!(needs, None | Some(IsaNeeds::Malformed(_))));
        if let Some((what, None)) = first {
            return Err(format!(
                "{what} is unreadable, or neither a little-endian ELF64 image for this \
                 {} host nor an x86-64 one",
                if host == EM_AARCH64 {
                    "aarch64"
                } else {
                    "non-x86-64"
                }
            ));
        }
    }
    detcore::exec_record::auto_profile(objects)
}

/// Resolve a configuration's auto profile for a library launch of `command`
/// ([`resolve_auto`] of the program it names, found as the launch finds it),
/// before [`refuse_unadmitted_launch`]. `hermit run` and `hermit record
/// start` resolve it earlier, before qualification; a configuration naming a
/// profile is left as it is. A resolved x86-64-v3 is then admitted only as
/// an explicit one is, so a library launch that never qualified the host
/// refuses it by name.
pub fn resolve_launch_auto(
    command: &crate::Command,
    config: &mut detcore::Config,
) -> Result<(), CpuProfileRefusal> {
    if config.cpu_profile != CpuProfile::Auto {
        return Ok(());
    }
    let refuse = |why: String| CpuProfileRefusal::new(CpuProfile::Auto, why);
    let Ok(program) = command.find_program() else {
        // No program to read, so none for this launch to exec: its own
        // error (or its backend's refusal) is the one to report, in its
        // place. x86-64-v2 is named only so that nothing unresolved reaches
        // the backend; should an exec succeed after all, Detcore's check of
        // the executed objects still refuses one that needs more.
        config.cpu_profile = CpuProfile::X86_64V2;
        return Ok(());
    };
    // The pinned Reverie `Command` exposes no mounts or chroot to read
    // back, so this library boundary cannot yet map the completed launch
    // view: paths resolve as host paths. Detcore's per-exec ISA check of
    // the objects actually executed still refuses one that needs more
    // than the profile chosen here; resolving the mounted/chrooted view
    // here waits on Reverie accessors for a command's mounts and chroot
    // (revcodex on PR 4709).
    let (profile, why) = resolve_auto(&program, |path| Some(path.to_path_buf())).map_err(refuse)?;
    tracing::debug!("--cpu-profile auto resolved to {profile}: {why}");
    config.cpu_profile = profile;
    Ok(())
}

/// Linux's version as (major, minor), from a `uname -r` release string.
pub fn kernel_version(release: &str) -> Option<(u32, u32)> {
    let mut parts = release.split(['.', '-']);
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// Linux 5.16 is the first with the `ARCH_*_XCOMP_*` queries the profile
/// answers from the host; before it they return EINVAL.
pub fn check_kernel(release: &str) -> Result<(), AdmissionRefusal> {
    match kernel_version(release) {
        Some(version) if version >= (5, 16) => Ok(()),
        Some((major, minor)) => Err(format!(
            "Linux {major}.{minor} predates the 5.16 xstate queries the profile answers from"
        )),
        None => Err(format!("cannot read the kernel version from {release:?}")),
    }
}

/// A host CPUID reader, so tests can supply a host.
pub trait HostCpuid {
    fn cpuid(&self, leaf: u32, subleaf: u32) -> [u32; 4];
}

/// The CPUID instruction of the host this process runs on.
pub struct NativeCpuid;

#[cfg(target_arch = "x86_64")]
impl HostCpuid for NativeCpuid {
    fn cpuid(&self, leaf: u32, subleaf: u32) -> [u32; 4] {
        let r = std::arch::x86_64::__cpuid_count(leaf, subleaf);
        [r.eax, r.ebx, r.ecx, r.edx]
    }
}

const REGISTER_NAMES: [&str; 4] = ["EAX", "EBX", "ECX", "EDX"];

/// The host checks that need only CPUID, in the order they are checked:
/// every advertised v3 feature bit natively, OSXSAVE, not a hybrid part, and
/// not an AMD part without XSAVEERPTR (where Linux runs `fnclex; emms; fildl`
/// before every FPU restore, a path no host here can exercise).
pub fn check_host_cpuid(
    host: &dyn HostCpuid,
    advertised: &[(u32, u32, usize, u32)],
) -> Result<(), AdmissionRefusal> {
    let max_basic = host.cpuid(0, 0)[0];
    let max_extended = host.cpuid(0x8000_0000, 0)[0];
    let mut missing = Vec::new();
    for &(leaf, subleaf, register, bits) in advertised {
        let supported = if leaf >= 0x8000_0000 {
            leaf <= max_extended
        } else {
            leaf <= max_basic
        };
        let native = if supported {
            host.cpuid(leaf, subleaf)[register]
        } else {
            0
        };
        let absent = bits & !native;
        for bit in 0..32 {
            if absent & (1 << bit) != 0 {
                missing.push(format!(
                    "leaf {leaf:#x}.{subleaf} {} bit {bit}",
                    REGISTER_NAMES[register]
                ));
            }
        }
    }
    if !missing.is_empty() {
        return Err(format!(
            "the host lacks feature bits the profile advertises: {}",
            missing.join(", ")
        ));
    }
    if host.cpuid(1, 0)[2] & (1 << 27) == 0 {
        return Err("the host kernel has not enabled XSAVE (OSXSAVE is clear)".into());
    }
    if max_basic >= 7 && host.cpuid(7, 0)[3] & (1 << 15) != 0 {
        return Err(
            "this is a hybrid part (leaf 7 EDX bit 15), whose core types the qualification \
             would each have to sample"
                .into(),
        );
    }
    let vendor = host.cpuid(0, 0);
    let amd = vendor[1] == 0x6874_7541;
    let xsaveerptr = max_extended >= 0x8000_0008 && host.cpuid(0x8000_0008, 0)[1] & (1 << 2) != 0;
    if amd && !xsaveerptr {
        return Err(
            "this AMD part lacks XSAVEERPTR, so Linux runs `fnclex; emms; fildl` before every \
             FPU restore, which the qualification cannot exercise"
                .into(),
        );
    }
    Ok(())
}

/// The user-dynamic xstate component (AMX TILEDATA): Linux enables it in XCR0
/// while withholding process permission, so a permission request changes
/// process state and the three XCOMP answers differ.
#[cfg(target_arch = "x86_64")]
const XFEATURE_TILEDATA: u64 = 1 << 18;

#[cfg(target_arch = "x86_64")]
const ARCH_GET_XCOMP_SUPP: libc::c_int = 0x1021;
#[cfg(target_arch = "x86_64")]
const ARCH_GET_XCOMP_PERM: libc::c_int = 0x1022;
#[cfg(target_arch = "x86_64")]
const ARCH_REQ_XCOMP_PERM: libc::c_int = 0x1023;
#[cfg(target_arch = "x86_64")]
const ARCH_GET_XCOMP_GUEST_PERM: libc::c_int = 0x1024;

#[cfg(target_arch = "x86_64")]
fn arch_prctl_mask(cmd: libc::c_int) -> Result<u64, AdmissionRefusal> {
    let mut mask = 0u64;
    // SAFETY: the GET commands write one u64 through the pointer.
    let rc = unsafe { libc::syscall(libc::SYS_arch_prctl, cmd, &mut mask as *mut u64) };
    if rc != 0 {
        return Err(format!(
            "arch_prctl({cmd:#x}) failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(mask)
}

/// Linux's XFEATURE_MAX as ARCH_REQ_XCOMP_PERM shows it: the first component
/// index it answers EINVAL for. Every index below must answer EOPNOTSUPP; an
/// index that is granted would mean a dynamic feature, which is refused
/// before this runs.
#[cfg(target_arch = "x86_64")]
fn xfeature_max() -> Result<u32, AdmissionRefusal> {
    for index in 0..64u64 {
        // SAFETY: the request takes the component index by value.
        let rc = unsafe { libc::syscall(libc::SYS_arch_prctl, ARCH_REQ_XCOMP_PERM, index) };
        let errno = std::io::Error::last_os_error().raw_os_error();
        match (rc, errno) {
            (-1, Some(libc::EINVAL)) => return Ok(index as u32),
            (-1, Some(libc::EOPNOTSUPP)) => {}
            _ => {
                return Err(format!(
                    "ARCH_REQ_XCOMP_PERM for component {index} answered {rc} ({errno:?}), \
                     not EOPNOTSUPP or EINVAL"
                ));
            }
        }
    }
    Err("ARCH_REQ_XCOMP_PERM never answered EINVAL for indices 0-63".into())
}

/// The host facts a qualified v3 run binds.
#[cfg(target_arch = "x86_64")]
fn capture(probe: &xstate_probe::ProbeReport) -> Result<HostXstate, AdmissionRefusal> {
    let host = host_facts(probe.cases as u32, probe.forced_exit)?;
    if host.xcr0 != probe.xcr0 {
        return Err(format!(
            "XCR0 changed during admission ({:#x}, then {:#x})",
            probe.xcr0, host.xcr0
        ));
    }
    Ok(host)
}

/// This host's facts, with the qualification fields given rather than probed.
/// The caller has checked OSXSAVE.
#[cfg(target_arch = "x86_64")]
fn host_facts(probe_cases: u32, probe_forced_exit: bool) -> Result<HostXstate, AdmissionRefusal> {
    let host = NativeCpuid;
    let leaf0 = host.cpuid(0, 0);
    // SAFETY: XGETBV with ECX 0 is available because OSXSAVE was checked.
    let xcr0 = unsafe { std::arch::x86_64::_xgetbv(0) };
    let mut xsave_leaves = Vec::new();
    for subleaf in std::iter::once(0).chain(2..64) {
        if subleaf == 0 || xcr0 & (1 << subleaf) != 0 {
            let [eax, ebx, ecx, edx] = host.cpuid(0xd, subleaf);
            xsave_leaves.push([subleaf, eax, ebx, ecx, edx]);
        }
    }
    let xcomp_supp = arch_prctl_mask(ARCH_GET_XCOMP_SUPP)?;
    refuse_dynamic_features(xcr0, xcomp_supp)?;
    Ok(HostXstate {
        vendor: [leaf0[1], leaf0[3], leaf0[2]],
        signature: host.cpuid(1, 0)[0],
        xcr0,
        xsave_leaves,
        xcomp_supp,
        xcomp_perm: arch_prctl_mask(ARCH_GET_XCOMP_PERM)?,
        xcomp_guest_perm: arch_prctl_mask(ARCH_GET_XCOMP_GUEST_PERM)?,
        xfeature_max: xfeature_max()?,
        probe_cases,
        probe_forced_exit,
    })
}

/// Refuses a v3 replay before the qualification probe runs when the recording
/// carries no host facts, when this host differs in one of the facts it bound
/// (the qualification itself aside), or when its CPUID table digest is not the
/// one this replayer computes from them. The probe costs seconds of CPU, and a
/// replay on a different CPU, or of an altered recording, needs none of it to
/// be refused. Returns the recorded facts.
#[cfg(target_arch = "x86_64")]
pub fn precheck_replay(
    profile: CpuProfile,
    recorded: Option<&HostXstate>,
    recorded_digest: Option<&str>,
) -> Result<HostXstate, CpuProfileRefusal> {
    check_record_or_replay(profile)?;
    let refuse = |why: String| CpuProfileRefusal::new(profile, why);
    let recorded = recorded.cloned().ok_or_else(|| {
        refuse("the recording names the profile but carries no host facts".into())
    })?;
    check_host_cpuid(&NativeCpuid, &detcore::v3_feature_words()).map_err(refuse)?;
    let host = host_facts(recorded.probe_cases, recorded.probe_forced_exit).map_err(refuse)?;
    if let Some(fact) = first_difference(&recorded, &host) {
        return Err(refuse(format!(
            "this host differs from the recording's in {fact}, so the guest would see a \
             different CPU"
        )));
    }
    let config = detcore::Config {
        cpu_profile: profile,
        cpu_profile_host: Some(recorded.clone()),
        ..Default::default()
    };
    let digest = crate::record::hex_digest(&detcore::virtual_cpuid_digest(&config));
    if recorded_digest != Some(digest.as_str()) {
        return Err(refuse(format!(
            "the recording's CPUID table digest {recorded_digest:?} is not this replayer's \
             {digest}"
        )));
    }
    Ok(recorded)
}

/// This process's successful qualification. A recording or replay qualifies
/// in the CLI process, before its container child is forked, so the child
/// reuses this result instead of probing again from inside the container,
/// whose CPU affinity can be one busy CPU.
static ADMITTED: std::sync::OnceLock<Qualification> = std::sync::OnceLock::new();

/// Admits a v3 run on this host, or names why not. On success the probe report
/// and the host facts to bind are returned, and this process's ptrace
/// floating-point register writes are refused from then on. The request's own
/// refusals are checked every time; the host qualification runs once per
/// process.
pub fn admit(request: &AdmissionRequest<'_>) -> Result<Qualification, AdmissionRefusal> {
    admit_with(request, qualify_host_cached)
}

/// [`admit`] with its host qualification as a parameter, so that a test can
/// admit without probing.
fn admit_with(
    request: &AdmissionRequest<'_>,
    qualify: impl FnOnce() -> Result<Qualification, AdmissionRefusal>,
) -> Result<Qualification, AdmissionRefusal> {
    check_request(request)?;
    if let Some(admitted) = ADMITTED.get() {
        return Ok(admitted.clone());
    }
    let qualified = qualify()?;
    // The qualification covers the XSTATE_BV a guest's own instructions
    // produce. A tracer write of its floating-point registers (PTRACE_SETFPREGS
    // or PTRACE_SETREGSET NT_X86_XSTATE) sets that header from the tracer's
    // buffer instead, which nothing checked, so it fails with EPERM. The flag
    // is process-wide and set before the container child is forked, so the
    // tracer inherits it. It is set before the admission becomes visible in
    // [`ADMITTED`], so an admitted launch always runs with it.
    safeptrace::forbid_fpu_register_writes();
    Ok(ADMITTED.get_or_init(|| qualified).clone())
}

/// Refuses a nondefault profile launch that this process did not admit.
/// Hermit's library launch functions call this before any backend starts.
/// Host mode requires its faulting proof and interception options; v3 also
/// requires exactly the host facts [`admit`] bound after qualification and
/// its floating-point register write ban.
pub fn refuse_unadmitted_launch(
    config: &detcore::Config,
    ptrace: bool,
) -> Result<(), CpuProfileRefusal> {
    if config.cpu_profile.is_default() {
        return Ok(());
    }
    let refuse = |why: &str| Err(CpuProfileRefusal::new(config.cpu_profile, why));
    if config.cpu_profile == CpuProfile::Auto {
        return refuse(
            "auto was not resolved to a named profile before launch (resolve_launch_auto)",
        );
    }
    if config.cpu_profile == CpuProfile::HostNoRdrand {
        // Library callers can configure the network engine directly, without
        // passing through RunOpts or its early artifact-creation guard.
        if config.network_trace.mode != detcore_model::network_trace::NetworkTraceMode::Off {
            check_record_or_replay(config.cpu_profile)?;
        }
        let request = AdmissionRequest {
            backend: if ptrace { "ptrace" } else { "non-ptrace" },
            ptrace,
            virtualize_cpuid: config.virtualize_cpuid,
            virtualize_time: config.virtualize_time,
            strace_only: false,
            gdbserver: config.gdbserver,
            namespace_only: false,
        };
        check_host_no_rdrand_request(&request)
            .map_err(|why| CpuProfileRefusal::new(config.cpu_profile, why))?;
        let cpu = config.cpu_profile_source.filter(|cpu| {
            HOST_NO_RDRAND_CPUS
                .get(*cpu)
                .is_some_and(|admitted| admitted.load(std::sync::atomic::Ordering::Acquire))
        });
        let Some(cpu) = cpu else {
            return refuse(
                "this process did not verify CPUID faulting on the configuration's source CPU; \
                 call admit_host_no_rdrand and retain its source in cpu_profile_source",
            );
        };
        return pin_host_cpuid_source(cpu)
            .map_err(|reason| CpuProfileRefusal::new(config.cpu_profile, reason));
    }
    if !ptrace {
        return refuse("only the ptrace backend is qualified");
    }
    if !config.virtualize_cpuid || config.gdbserver {
        return refuse("the launch does not virtualize CPUID, or serves a gdbserver");
    }
    match (&config.cpu_profile_host, ADMITTED.get()) {
        (None, _) => return refuse("the configuration carries no qualified host facts"),
        (Some(_), None) => return refuse("this process did not qualify the host"),
        (Some(facts), Some((_, admitted))) if facts != admitted => {
            return refuse(
                "the configuration's host facts are not the ones this process qualified",
            );
        }
        _ => {}
    }
    if !safeptrace::fpu_register_writes_forbidden() {
        return refuse("ptrace floating-point register writes are not forbidden");
    }
    Ok(())
}

/// The host half of [`admit`], run once per process: the kernel, the
/// every-bit CPUID check, the dynamic-feature refusal, the probe and the host
/// facts. `hermit host-capabilities --cpu-profile x86-64-v3` runs it alone.
/// It always measures and never reads or writes the cache [`admit`] uses.
#[cfg(target_arch = "x86_64")]
pub fn qualify_host() -> Result<Qualification, AdmissionRefusal> {
    qualify_host_with(|_| xstate_probe::qualify(xstate_probe::ProbeOptions::default()))
}

/// [`admit`]'s host half: the same qualification, with the XINUSE probe
/// through the per-host cache ([`cached_probe`]).
#[cfg(target_arch = "x86_64")]
fn qualify_host_cached() -> Result<Qualification, AdmissionRefusal> {
    qualify_host_with(cached_probe)
}

/// The name of the XINUSE probe's file in the host qualification cache.
#[cfg(target_arch = "x86_64")]
const PROBE_CACHE_NAME: &str = "cpu-profile-x86-64-v3-probe";

#[cfg(target_arch = "x86_64")]
fn probe_cache_path() -> Option<std::path::PathBuf> {
    crate::host_qualification::default_path(PROBE_CACHE_NAME)
}

/// The probe's inputs besides the host: its options. XCR0 is checked on the
/// report itself ([`cached_report_matches`]).
#[cfg(target_arch = "x86_64")]
fn probe_cache_inputs() -> String {
    format!("options={:?}", xstate_probe::ProbeOptions::default())
}

/// The XINUSE probe for [`admit`]: its passing report cached for this host
/// (`crate::host_qualification`), or else a fresh probe, announced on stderr
/// because it takes seconds, and cached if it passes. The probe certifies the
/// CPU's and kernel's XSAVE behavior, which no sandbox changes, so the host
/// key suffices; the checks that depend on the caller's sandbox run on every
/// run (<https://github.com/rrnewton/hermit/issues/4515>). Under a hypervisor
/// nothing is cached: a live migration keeps the boot, the kernel and CPUID
/// but moves the guest to a host the forced-exit arm never certified.
#[cfg(target_arch = "x86_64")]
fn cached_probe(xcr0: u64) -> Result<xstate_probe::ProbeReport, AdmissionRefusal> {
    let hypervisor = std::arch::x86_64::__cpuid(1).ecx & (1 << 31) != 0;
    cached_probe_at(
        cache_path_for(hypervisor).as_deref(),
        &crate::host_qualification::HostKey::current(),
        xcr0,
        &mut |line: &str| eprint!("{line}"),
        || xstate_probe::qualify(xstate_probe::ProbeOptions::default()),
    )
}

/// The probe's cache file, or `None` under a hypervisor, where nothing is
/// cached ([`cached_probe`]).
#[cfg(target_arch = "x86_64")]
fn cache_path_for(hypervisor: bool) -> Option<std::path::PathBuf> {
    if hypervisor {
        return None;
    }
    probe_cache_path()
}

/// Whether a cached report is one this host's probe would produce: it binds
/// `cases` and `forced_exit` into a recording (`HostXstate::probe_cases` and
/// `probe_forced_exit`), so they are derived again here, from XCR0 and from
/// the cache's use only off a hypervisor, and a report that disagrees is a
/// miss. Its repetitions and pair count must be the default probe's too.
#[cfg(target_arch = "x86_64")]
fn cached_report_matches(report: &xstate_probe::ProbeReport, xcr0: u64) -> bool {
    let options = xstate_probe::ProbeOptions::default();
    report.xcr0 == xcr0
        && report.cases == xstate_probe::expected_case_count(xcr0)
        && !report.forced_exit
        && report.repetitions == options.repetitions
        && report.certified_pairs == report.cells as u64 * u64::from(report.repetitions)
}

/// [`cached_probe`] with its cache path (`None`: no cache), host key, warning
/// writer and probe as parameters, for the tests.
#[cfg(target_arch = "x86_64")]
fn cached_probe_at(
    path: Option<&std::path::Path>,
    host: &crate::host_qualification::HostKey,
    xcr0: u64,
    warn: &mut dyn FnMut(&str),
    probe: impl FnOnce() -> Result<xstate_probe::ProbeReport, xstate_probe::ProbeRefusal>,
) -> Result<xstate_probe::ProbeReport, AdmissionRefusal> {
    let Some(path) = path else {
        warn(
            "WARNING: qualifying this host for --cpu-profile x86-64-v3, which takes seconds; \
             with no cache to use it is measured again on every run.\n",
        );
        return probe();
    };
    let inputs = probe_cache_inputs();
    let cached = || {
        crate::host_qualification::load::<xstate_probe::ProbeReport>(path, host, &inputs)
            .filter(|report| cached_report_matches(report, xcr0))
    };
    if let Some(report) = cached() {
        return Ok(report);
    }
    // Checked again under the lock: another process that missed at the same
    // time may have measured and stored a pass while this one waited.
    crate::host_qualification::locked(path, || {
        if let Some(report) = cached() {
            return Ok(report);
        }
        warn(&if crate::host_qualification::writable(path) {
            format!(
                "WARNING: qualifying this host for --cpu-profile x86-64-v3, which takes \
                 seconds; a pass is cached in {} until the kernel, CPU, microcode, boot or \
                 Hermit binary changes.\n",
                path.display()
            )
        } else {
            format!(
                "WARNING: qualifying this host for --cpu-profile x86-64-v3, which takes \
                 seconds; {} cannot be written, so it is measured again on every run.\n",
                path.display()
            )
        });
        let report = probe()?;
        crate::host_qualification::store(path, host, &inputs, &report);
        Ok(report)
    })
}

/// [`qualify_host`] with the XINUSE probe as a parameter, given XCR0: the
/// kernel, the every-bit CPUID check and the dynamic-feature refusal run every
/// time, and the host facts are captured from this host every time.
#[cfg(target_arch = "x86_64")]
fn qualify_host_with(
    probe: impl FnOnce(u64) -> Result<xstate_probe::ProbeReport, AdmissionRefusal>,
) -> Result<Qualification, AdmissionRefusal> {
    let release = nix::sys::utsname::uname()
        .map_err(|e| format!("uname failed: {e}"))?
        .release()
        .to_string_lossy()
        .into_owned();
    check_kernel(&release)?;
    check_host_cpuid(&NativeCpuid, &detcore::v3_feature_words())?;
    // SAFETY: OSXSAVE was checked above.
    let xcr0 = unsafe { std::arch::x86_64::_xgetbv(0) };
    refuse_dynamic_features(xcr0, 0)?;
    let probe = probe(xcr0)?;
    let host = capture(&probe)?;
    Ok((probe, host))
}

/// Refuses a host with a user-dynamic xstate feature (AMX TILEDATA, bit 18) in
/// XCR0 or in ARCH_GET_XCOMP_SUPP: a guest's permission request for it changes
/// process state, which the profile cannot bind.
#[cfg(target_arch = "x86_64")]
fn refuse_dynamic_features(xcr0: u64, xcomp_supp: u64) -> Result<(), AdmissionRefusal> {
    if (xcr0 | xcomp_supp) & XFEATURE_TILEDATA != 0 {
        return Err(
            "the host has a user-dynamic xstate feature (AMX TILEDATA), whose permission \
             requests change process state"
                .into(),
        );
    }
    Ok(())
}

/// CPUID faulting is an x86 feature, so other architectures refuse the host
/// profile.
#[cfg(not(target_arch = "x86_64"))]
pub fn qualify_host_no_rdrand() -> Result<(), AdmissionRefusal> {
    Err(non_x86_refusal())
}

#[cfg(not(target_arch = "x86_64"))]
fn native_cpuid_faulting_vm_pointer() -> Option<String> {
    None
}

/// The v3 profile binds x86 XSTATE facts, so other architectures refuse it.
#[cfg(not(target_arch = "x86_64"))]
pub fn precheck_replay(
    profile: CpuProfile,
    _recorded: Option<&HostXstate>,
    _recorded_digest: Option<&str>,
) -> Result<HostXstate, CpuProfileRefusal> {
    check_record_or_replay(profile)?;
    Err(CpuProfileRefusal::new(profile, non_x86_refusal()))
}

/// The v3 profile binds x86 XSTATE facts, so other architectures refuse it.
#[cfg(not(target_arch = "x86_64"))]
pub fn qualify_host() -> Result<Qualification, AdmissionRefusal> {
    Err(non_x86_refusal())
}

/// Why a nondefault profile is refused off x86: both describe x86 CPUs.
#[cfg(not(target_arch = "x86_64"))]
fn non_x86_refusal() -> AdmissionRefusal {
    format!(
        "the nondefault CPU profiles describe x86-64 CPUs, and this host is {}",
        std::env::consts::ARCH
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    /// A report the default probe could produce for `xcr0`.
    fn probe_report(xcr0: u64) -> xstate_probe::ProbeReport {
        let cases = xstate_probe::expected_case_count(xcr0);
        let repetitions = xstate_probe::ProbeOptions::default().repetitions;
        xstate_probe::ProbeReport {
            cpu: 3,
            xcr0,
            cases,
            cells: cases * 12,
            repetitions,
            certified_pairs: (cases * 12) as u64 * u64::from(repetitions),
            forced_exit: false,
            reload_bound: 0,
            cpuid_min_cycles: 0,
            calibration: "cached".into(),
            retried_references: 0,
            retried_reloads: 0,
            retried_exits: 0,
            counter_reopens: 0,
        }
    }

    fn cache_host() -> crate::host_qualification::HostKey {
        crate::host_qualification::HostKey {
            kernel_release: "7.1.3".into(),
            boot_id: "boot-a".into(),
            cpu: "AuthenticAMD family 0x1a model 0x11 stepping 0x0".into(),
            microcode: "0xb101054".into(),
            hermit_binary: "64768:123:456:789.0".into(),
        }
    }

    /// Runs [`cached_probe_at`] once and returns its result, whether the probe
    /// ran and what it warned.
    fn probe_through_cache(
        path: Option<&std::path::Path>,
        xcr0: u64,
        answer: Result<xstate_probe::ProbeReport, xstate_probe::ProbeRefusal>,
    ) -> (
        Result<xstate_probe::ProbeReport, AdmissionRefusal>,
        bool,
        Vec<String>,
    ) {
        let mut ran = false;
        let mut warnings = Vec::new();
        let result = cached_probe_at(
            path,
            &cache_host(),
            xcr0,
            &mut |line: &str| warnings.push(line.to_owned()),
            || {
                ran = true;
                answer
            },
        );
        (result, ran, warnings)
    }

    /// The first v3 admission on a host measures, warning once on stderr, and
    /// caches the passing report; later ones reuse it without measuring or
    /// warning (<https://github.com/rrnewton/hermit/issues/4515>).
    #[test]
    fn a_passing_probe_is_measured_once_and_announced() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("hermit").join("probe.json");
        let (result, ran, warnings) =
            probe_through_cache(Some(&path), 0x2e7, Ok(probe_report(0x2e7)));
        assert_eq!(result, Ok(probe_report(0x2e7)));
        assert!(ran);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].starts_with("WARNING: "), "{warnings:?}");
        assert!(
            warnings[0].contains(&path.display().to_string()),
            "{warnings:?}"
        );
        assert_eq!(warnings[0].matches('\n').count(), 1, "{warnings:?}");
        let (result, ran, warnings) =
            probe_through_cache(Some(&path), 0x2e7, Err("must not probe".into()));
        assert_eq!(result, Ok(probe_report(0x2e7)));
        assert!(!ran);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// A refusal is not cached, and a different XCR0 measures again.
    #[test]
    fn a_refusal_or_a_new_xcr0_measures_again() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("probe.json");
        let (result, ran, _) = probe_through_cache(Some(&path), 0x2e7, Err("refused".into()));
        assert_eq!(result, Err("refused".into()));
        assert!(ran);
        let (result, ran, _) = probe_through_cache(Some(&path), 0x2e7, Ok(probe_report(0x2e7)));
        assert!(ran && result.is_ok());
        let (result, ran, _) = probe_through_cache(Some(&path), 0x207, Ok(probe_report(0x207)));
        assert_eq!(result, Ok(probe_report(0x207)));
        assert!(ran, "XCR0 is part of the key");
    }

    /// A cached report whose recorded fields this host's probe would not
    /// produce is a miss: the case count and the forced-exit arm are bound
    /// into recordings, so an edited or foreign file never supplies them.
    #[test]
    fn a_cached_report_that_disagrees_with_this_host_is_a_miss() {
        type Edit = fn(&mut xstate_probe::ProbeReport);
        let edits: [(&str, Edit); 5] = [
            ("cases", |r| r.cases = 1),
            ("forced exit", |r| r.forced_exit = true),
            ("repetitions", |r| r.repetitions += 1),
            ("certified pairs", |r| r.certified_pairs += 1),
            ("xcr0", |r| r.xcr0 ^= 1 << 9),
        ];
        for (field, edit) in edits {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("probe.json");
            let mut edited = probe_report(0x2e7);
            edit(&mut edited);
            crate::host_qualification::store(&path, &cache_host(), &probe_cache_inputs(), &edited);
            let (result, ran, _) = probe_through_cache(Some(&path), 0x2e7, Ok(probe_report(0x2e7)));
            assert!(ran, "{field}: a disagreeing report was reused");
            assert_eq!(result, Ok(probe_report(0x2e7)), "{field}");
        }
    }

    /// Under a hypervisor nothing is cached; elsewhere the cache file is used.
    #[test]
    fn nothing_is_cached_under_a_hypervisor() {
        assert_eq!(cache_path_for(true), None);
        assert_eq!(cache_path_for(false), probe_cache_path());
    }

    /// When the cache cannot be written the warning says it is measured again,
    /// not that it is cached.
    #[test]
    fn an_unwritable_cache_is_announced_as_such() {
        let directory = tempfile::tempdir().unwrap();
        let blocker = directory.path().join("hermit");
        std::fs::write(&blocker, b"a file where the directory would go").unwrap();
        let path = blocker.join("probe.json");
        let (result, ran, warnings) =
            probe_through_cache(Some(&path), 0x2e7, Ok(probe_report(0x2e7)));
        assert!(ran && result.is_ok());
        assert!(warnings[0].contains("cannot be written"), "{warnings:?}");
        assert!(!warnings[0].contains("is cached"), "{warnings:?}");
    }

    /// A process that misses while another holds the probe lock waits, finds
    /// the other's verdict under the lock and does not measure: concurrent
    /// cold runs measure once.
    #[test]
    fn a_miss_waits_for_the_lock_and_reuses_the_verdict_stored_meanwhile() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("probe.json");
        let (holding_tx, holding_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = {
            let path = path.clone();
            std::thread::spawn(move || {
                crate::host_qualification::locked(&path, || {
                    holding_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    crate::host_qualification::store(
                        &path,
                        &cache_host(),
                        &probe_cache_inputs(),
                        &probe_report(0x2e7),
                    );
                })
            })
        };
        holding_rx.recv().unwrap();
        let waiter = {
            let path = path.clone();
            std::thread::spawn(move || {
                probe_through_cache(Some(&path), 0x2e7, Err("must not measure".into()))
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(200));
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        let (result, ran, warnings) = waiter.join().unwrap();
        assert_eq!(result, Ok(probe_report(0x2e7)));
        assert!(!ran, "the waiter measured instead of reusing the verdict");
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// Without a cache directory every admission measures, and says so.
    #[test]
    fn without_a_cache_directory_every_admission_measures() {
        for _ in 0..2 {
            let (result, ran, warnings) = probe_through_cache(None, 0x2e7, Ok(probe_report(0x2e7)));
            assert!(ran && result.is_ok());
            assert!(
                warnings[0].contains("measured again on every run"),
                "{warnings:?}"
            );
        }
    }

    /// A host with AMX TILEDATA in XCR0 or in the supported mask is refused by
    /// name; one without it is not.
    #[test]
    fn a_dynamic_xstate_feature_is_refused() {
        assert_eq!(super::refuse_dynamic_features(0x2e7, 0x2e7), Ok(()));
        for (xcr0, supp) in [(0x602e7, 0x2e7), (0x2e7, 0x602e7), (0x402e7, 0x402e7)] {
            assert!(
                super::refuse_dynamic_features(xcr0, supp)
                    .unwrap_err()
                    .contains("AMX TILEDATA"),
                "{xcr0:#x} {supp:#x}"
            );
        }
    }

    use super::*;

    fn request(backend: &str) -> AdmissionRequest<'_> {
        AdmissionRequest {
            backend,
            ptrace: backend == "ptrace",
            virtualize_cpuid: true,
            virtualize_time: true,
            strace_only: false,
            gdbserver: false,
            namespace_only: false,
        }
    }

    #[test]
    fn only_a_ptrace_run_with_virtual_cpuid_and_no_gdbserver_is_considered() {
        assert_eq!(check_request(&request("ptrace")), Ok(()));
        for backend in [
            "kvm",
            "dbt",
            "sabre",
            "liteinst",
            "in-guest-trap",
            "e9patch",
        ] {
            let refusal = check_request(&request(backend)).unwrap_err();
            assert!(
                refusal.starts_with(&format!("--backend={backend}")),
                "{refusal}"
            );
        }
        assert!(check_request(&request("kvm")).unwrap_err().contains("XCR0"));
        assert!(
            check_request(&request("dbt"))
                .unwrap_err()
                .contains("carries no CPU profile")
        );
        let mut r = request("ptrace");
        r.virtualize_cpuid = false;
        assert!(check_request(&r).is_err());
        let mut r = request("ptrace");
        r.strace_only = true;
        assert!(check_request(&r).is_err());
        let mut r = request("ptrace");
        r.gdbserver = true;
        assert!(check_request(&r).unwrap_err().contains("--gdbserver"));
        let mut r = request("ptrace");
        r.namespace_only = true;
        assert!(check_request(&r).unwrap_err().contains("--namespace-only"));
    }

    #[test]
    fn host_profile_requires_interception_and_virtual_time() {
        let mut host = qualifying_host();
        assert_eq!(check_host_no_rdrand_cpuid(&host), Ok(()));
        host.0.get_mut(&(7, 0)).unwrap()[3] |= 1 << 15;
        assert!(
            check_host_no_rdrand_cpuid(&host)
                .unwrap_err()
                .contains("use a homogeneous CPU host")
        );
        host.0.get_mut(&(7, 0)).unwrap()[3] &= !(1 << 15);
        host.0.insert((0x8000_0000, 0), [0x8000_0026, 0, 0, 0]);
        host.0.insert((0x8000_0026, 0), [1 << 30, 0, 0, 0]);
        assert!(
            check_host_no_rdrand_cpuid(&host)
                .unwrap_err()
                .contains("use a homogeneous CPU host")
        );
        host.0.get_mut(&(0x8000_0026, 0)).unwrap()[0] = 0;
        assert_eq!(check_host_no_rdrand_cpuid(&host), Ok(()));
        // An unsupported leaf can alias a supported leaf; honor the maximum.
        host.0.get_mut(&(0x8000_0000, 0)).unwrap()[0] = 0x8000_0025;
        host.0.get_mut(&(0x8000_0026, 0)).unwrap()[0] = 1 << 30;
        assert_eq!(check_host_no_rdrand_cpuid(&host), Ok(()));
        assert_eq!(check_host_no_rdrand_request(&request("ptrace")), Ok(()));
        for backend in [
            "kvm",
            "dbt",
            "sabre",
            "liteinst",
            "in-guest-trap",
            "e9patch",
        ] {
            let refusal = check_host_no_rdrand_request(&request(backend)).unwrap_err();
            assert!(
                refusal.contains(&format!("--backend={backend}")),
                "{refusal}"
            );
            assert!(refusal.contains("use --backend=ptrace"), "{refusal}");
        }
        for (r, option) in [
            (
                AdmissionRequest {
                    virtualize_cpuid: false,
                    ..request("ptrace")
                },
                "--no-virtualize-cpuid",
            ),
            (
                AdmissionRequest {
                    virtualize_time: false,
                    ..request("ptrace")
                },
                "--no-virtualize-time",
            ),
            (
                AdmissionRequest {
                    strace_only: true,
                    ..request("ptrace")
                },
                "--strace-only",
            ),
            (
                AdmissionRequest {
                    namespace_only: true,
                    ..request("ptrace")
                },
                "--namespace-only",
            ),
        ] {
            let refusal =
                admit_host_no_rdrand_with(&r, || panic!("invalid request probed")).unwrap_err();
            assert!(refusal.contains(&format!("remove {option}")), "{refusal}");
        }
        // Only v3 binds qualified FPU register state and forbids gdb writes.
        let mut gdb = request("ptrace");
        gdb.gdbserver = true;
        assert_eq!(check_host_no_rdrand_request(&gdb), Ok(()));
    }

    #[test]
    fn host_profile_refuses_absent_faulting_and_requires_admission_to_launch() {
        let original = host_profile_affinity().unwrap();
        let mut config = detcore::Config {
            cpu_profile: CpuProfile::HostNoRdrand,
            ..Default::default()
        };
        let unadmitted = refuse_unadmitted_launch(&config, true).unwrap_err();
        assert!(unadmitted.reason.contains("did not verify CPUID faulting"));
        let simulated = "CPUID faulting is unavailable: ARCH_SET_CPUID(0) failed";
        let refusal =
            admit_host_no_rdrand_with(&request("ptrace"), || Err(simulated.into())).unwrap_err();
        assert_eq!(refusal, simulated);
        assert_eq!(
            affinity_cpus(&host_profile_affinity().unwrap()),
            affinity_cpus(&original)
        );
        assert!(refuse_unadmitted_launch(&config, true).is_err());

        // The source is chosen from the original mask, before a container or
        // cached worker has narrowed it. Admission restores the caller's mask.
        let source = admit_host_no_rdrand_with(&request("ptrace"), || Ok(())).unwrap();
        assert_eq!(source, affinity_cpus(&original)[0]);
        assert_eq!(
            affinity_cpus(&host_profile_affinity().unwrap()),
            affinity_cpus(&original)
        );
        assert!(refuse_unadmitted_launch(&config, true).is_err());
        config.cpu_profile_source = Some(source);
        for mode in [
            detcore_model::network_trace::NetworkTraceMode::Record,
            detcore_model::network_trace::NetworkTraceMode::Replay,
        ] {
            config.network_trace.mode = mode;
            let refusal = refuse_unadmitted_launch(&config, true).unwrap_err();
            assert!(refusal.reason.contains("record/replay is not supported"));
        }
        config.network_trace.mode = detcore_model::network_trace::NetworkTraceMode::Off;
        let other_config = config.clone();
        let original_cpus = affinity_cpus(&original);
        std::thread::spawn(move || {
            // A previously created library worker can have a wider or different
            // mask than admission's caller. The actual launch pins that worker.
            let other_cpu = *original_cpus.last().unwrap();
            let mut other = CpuSet::new();
            other.set(other_cpu).unwrap();
            sched_setaffinity(Pid::from_raw(0), &other).unwrap();
            assert_eq!(refuse_unadmitted_launch(&other_config, true), Ok(()));
            assert_eq!(affinity_cpus(&host_profile_affinity().unwrap()), [source]);
        })
        .join()
        .unwrap();
        assert_eq!(refuse_unadmitted_launch(&config, true), Ok(()));
        assert_eq!(affinity_cpus(&host_profile_affinity().unwrap()), [source]);
        assert!(refuse_unadmitted_launch(&config, false).is_err());
        config.virtualize_cpuid = false;
        assert!(refuse_unadmitted_launch(&config, true).is_err());
        config.virtualize_cpuid = true;
        config.virtualize_time = false;
        assert!(refuse_unadmitted_launch(&config, true).is_err());
        sched_setaffinity(Pid::from_raw(0), &original).unwrap();
    }

    #[test]
    fn host_profile_recordings_are_refused_before_v3_host_checks() {
        for profile in [CpuProfile::X86_64V2, CpuProfile::X86_64V3] {
            assert_eq!(check_record_or_replay(profile), Ok(()));
        }
        let refused = check_record_or_replay(CpuProfile::HostNoRdrand).unwrap_err();
        assert_eq!(refused.profile, CpuProfile::HostNoRdrand);
        assert!(refused.reason.contains("record/replay is not supported"));
        assert!(
            refused
                .reason
                .contains("hermit run --cpu-profile=host-no-rdrand --verify")
        );
        assert_eq!(
            precheck_replay(CpuProfile::HostNoRdrand, None, None),
            Err(refused)
        );
        assert_eq!(
            CpuProfileRefusal::new(CpuProfile::HostNoRdrand, "no faulting").to_string(),
            "--cpu-profile host-no-rdrand refused: no faulting"
        );
    }

    /// The launch certificate, in one process (no other test admits v3): a v3
    /// configuration with facts but no admission is refused; a refused
    /// request neither probes nor forbids floating-point writes; an admission
    /// forbids them before it is visible, and then exactly the admitted facts
    /// launch, on ptrace only.
    #[test]
    fn only_the_admitted_facts_launch_and_the_write_ban_comes_first() {
        let host = HostXstate {
            vendor: [1, 2, 3],
            signature: 0x00b1_0f11,
            xcr0: 0x2e7,
            xsave_leaves: vec![[0, 0x2e7, 0x988, 0x988, 0]],
            xcomp_supp: 0x2e7,
            xcomp_perm: 0x2e7,
            xcomp_guest_perm: 0x2e7,
            xfeature_max: 20,
            probe_cases: 33,
            probe_forced_exit: false,
        };
        let mut config = detcore::Config {
            cpu_profile: detcore_model::cpu_profile::CpuProfile::X86_64V3,
            cpu_profile_host: Some(host.clone()),
            ..Default::default()
        };
        let unadmitted = refuse_unadmitted_launch(&config, true).unwrap_err();
        assert!(
            unadmitted.reason.contains("did not qualify"),
            "{unadmitted}"
        );
        let v2 = detcore::Config {
            cpu_profile: CpuProfile::X86_64V2,
            ..Default::default()
        };
        assert_eq!(refuse_unadmitted_launch(&v2, false), Ok(()));

        let refused = admit_with(&request("kvm"), || {
            panic!("a refused request probed the host")
        });
        assert!(refused.is_err());
        assert!(!safeptrace::fpu_register_writes_forbidden());
        assert!(ADMITTED.get().is_none());

        let report = xstate_probe::ProbeReport {
            cpu: 3,
            xcr0: 0x2e7,
            cases: 33,
            cells: 396,
            repetitions: 32,
            certified_pairs: 12672,
            forced_exit: false,
            reload_bound: 0,
            cpuid_min_cycles: 0,
            calibration: String::new(),
            retried_references: 0,
            retried_reloads: 0,
            retried_exits: 0,
            counter_reopens: 0,
        };
        let admitted = admit_with(&request("ptrace"), || {
            // The ban is not yet in force while the host qualifies.
            assert!(!safeptrace::fpu_register_writes_forbidden());
            Ok((report.clone(), host.clone()))
        });
        assert_eq!(admitted, Ok((report, host.clone())));
        assert!(safeptrace::fpu_register_writes_forbidden());
        assert_eq!(refuse_unadmitted_launch(&config, true), Ok(()));
        assert!(refuse_unadmitted_launch(&config, false).is_err());
        let mut other = host;
        other.probe_cases = 0;
        config.cpu_profile_host = Some(other);
        let forged = refuse_unadmitted_launch(&config, true).unwrap_err();
        assert!(
            forged
                .reason
                .contains("not the ones this process qualified"),
            "{forged}"
        );
        config.cpu_profile_host = None;
        assert!(refuse_unadmitted_launch(&config, true).is_err());
    }

    #[test]
    fn a_replay_host_is_compared_fact_by_fact() {
        let recorded = HostXstate {
            vendor: [1, 2, 3],
            signature: 0x00b1_0f11,
            xcr0: 0x2e7,
            xsave_leaves: vec![[0, 0x2e7, 0x988, 0x988, 0]],
            xcomp_supp: 0x2e7,
            xcomp_perm: 0x2e7,
            xcomp_guest_perm: 0x2e7,
            xfeature_max: 20,
            probe_cases: 33,
            probe_forced_exit: false,
        };
        assert_eq!(first_difference(&recorded, &recorded.clone()), None);
        let differs = |f: fn(&mut HostXstate)| {
            let mut other = recorded.clone();
            f(&mut other);
            first_difference(&recorded, &other)
        };
        assert_eq!(differs(|h| h.vendor[0] = 9), Some("the CPU vendor"));
        assert_eq!(
            differs(|h| h.signature = 0x00a1_0f11),
            Some("the CPU family, model or stepping")
        );
        assert_eq!(differs(|h| h.xcr0 = 0x207), Some("XCR0"));
        assert_eq!(
            differs(|h| h.xsave_leaves[0][2] = 0xa88),
            Some("the XSAVE layout (CPUID leaf 0xD)")
        );
        assert_eq!(
            differs(|h| h.xcomp_perm = 0x207),
            Some("the ARCH_GET_XCOMP masks")
        );
        assert_eq!(differs(|h| h.xfeature_max = 19), Some("XFEATURE_MAX"));
        assert_eq!(
            differs(|h| h.probe_forced_exit = true),
            Some("the qualification it passed")
        );
        assert_eq!(
            CpuProfileRefusal::new(CpuProfile::X86_64V3, "why").to_string(),
            "--cpu-profile x86-64-v3 refused: why"
        );
    }

    #[test]
    fn kernels_before_5_16_are_refused() {
        assert_eq!(kernel_version("7.1.3-0_fbk0_0_g295ad0959f34"), Some((7, 1)));
        assert_eq!(check_kernel("5.16.0"), Ok(()));
        assert_eq!(check_kernel("6.4.3-0_fbk20"), Ok(()));
        assert!(check_kernel("5.15.120").is_err());
        assert!(check_kernel("4.18.0-553.el8").is_err());
        assert!(check_kernel("garbage").is_err());
    }

    struct FakeHost(BTreeMap<(u32, u32), [u32; 4]>);

    impl HostCpuid for FakeHost {
        fn cpuid(&self, leaf: u32, subleaf: u32) -> [u32; 4] {
            self.0.get(&(leaf, subleaf)).copied().unwrap_or([0; 4])
        }
    }

    /// A bare-metal-shaped host with every v3 bit, AMD with XSAVEERPTR.
    fn qualifying_host() -> FakeHost {
        let mut regs = BTreeMap::new();
        regs.insert((0, 0), [0xd, 0x6874_7541, 0x444d_4163, 0x6974_6e65]);
        regs.insert((0x8000_0000, 0), [0x8000_0008, 0, 0, 0]);
        regs.insert((0x8000_0008, 0), [0, 1 << 2, 0, 0]);
        for &(leaf, subleaf, register, bits) in &detcore::v3_feature_words() {
            regs.entry((leaf, subleaf)).or_insert([0; 4])[register] |= bits;
        }
        regs.get_mut(&(1, 0)).unwrap()[2] |= 1 << 27;
        FakeHost(regs)
    }

    #[test]
    fn a_host_with_every_v3_bit_qualifies_and_removing_any_one_is_named() {
        let words = detcore::v3_feature_words();
        let host = qualifying_host();
        assert_eq!(check_host_cpuid(&host, &words), Ok(()));
        // The hypervisor bit is not required: v3 clears it.
        assert!(words.iter().all(|&(leaf, _, register, bits)| !(leaf == 1
            && register == 2
            && bits & (1 << 31) != 0)));
        let mut checked = 0;
        for &(leaf, subleaf, register, bits) in &words {
            for bit in (0..32).filter(|b| bits & (1 << b) != 0) {
                let mut host = qualifying_host();
                host.0.get_mut(&(leaf, subleaf)).unwrap()[register] &= !(1 << bit);
                if leaf == 1 && register == 2 && bit == 27 {
                    continue; // OSXSAVE: refused by its own check, below
                }
                let refusal = check_host_cpuid(&host, &words).unwrap_err();
                assert!(
                    refusal.contains(&format!(
                        "leaf {leaf:#x}.{subleaf} {} bit {bit}",
                        REGISTER_NAMES[register]
                    )),
                    "{refusal}"
                );
                checked += 1;
            }
        }
        let advertised: u32 = words.iter().map(|&(_, _, _, bits)| bits.count_ones()).sum();
        // Every advertised bit but OSXSAVE, which has its own check.
        assert_eq!(checked, advertised - 1);
    }

    /// The kernel-module pointer is given only on a VM before Linux 6.17, and
    /// only after an error that reports missing faulting and lacks it already.
    #[test]
    fn cpuid_faulting_vm_pointer_needs_a_hypervisor_and_a_kernel_before_6_17() {
        let mut vm = qualifying_host();
        vm.0.get_mut(&(1, 0)).unwrap()[2] |= 1 << 31;
        let pointer = cpuid_faulting_vm_pointer(&vm, "6.16.1-0_fbk5_0_g0e127f210adc").unwrap();
        assert!(pointer.contains(CPUID_FAULTING_VM_SCRIPT), "{pointer}");
        assert!(pointer.contains("Linux 6.16.1-0_fbk5"), "{pointer}");
        assert!(cpuid_faulting_vm_pointer(&vm, "5.19.0").is_some());
        assert_eq!(cpuid_faulting_vm_pointer(&vm, "6.17.0"), None);
        assert_eq!(cpuid_faulting_vm_pointer(&vm, "7.1.3-0_fbk0"), None);
        assert_eq!(cpuid_faulting_vm_pointer(&vm, "garbage"), None);
        let bare_metal = qualifying_host();
        assert_eq!(cpuid_faulting_vm_pointer(&bare_metal, "6.16.1"), None);

        // Host-independent cases of the note: no matching message, or a
        // message that already names the script.
        assert_eq!(
            missing_cpuid_faulting_note(&["permission denied".to_string()]),
            None
        );
        assert_eq!(
            missing_cpuid_faulting_note(&[format!(
                "CPUID faulting is unavailable: ARCH_SET_CPUID(0) failed; {pointer}"
            )]),
            None
        );
    }

    #[test]
    fn osxsave_hybrid_parts_and_old_amd_are_refused() {
        let words = detcore::v3_feature_words();
        let mut host = qualifying_host();
        host.0.get_mut(&(1, 0)).unwrap()[2] &= !(1 << 27);
        assert!(check_host_cpuid(&host, &words).is_err());
        let mut host = qualifying_host();
        host.0.get_mut(&(7, 0)).unwrap()[3] |= 1 << 15;
        assert!(
            check_host_cpuid(&host, &words)
                .unwrap_err()
                .contains("hybrid")
        );
        let mut host = qualifying_host();
        host.0.get_mut(&(0x8000_0008, 0)).unwrap()[1] = 0;
        assert!(
            check_host_cpuid(&host, &words)
                .unwrap_err()
                .contains("XSAVEERPTR")
        );
    }

    /// On a host that is not x86-64, `--cpu-profile auto` names the default
    /// profile for an ELF64 image built for that host's machine, and for its
    /// program interpreter, instead of refusing it as not x86-64. An x86-64
    /// host still refuses such an image, every host still refuses a file
    /// that is not an ELF64 image, and a missing interpreter is still
    /// refused by name.
    #[test]
    fn auto_names_the_default_for_an_image_of_a_host_that_is_not_x86_64() {
        let directory = tempfile::tempdir().unwrap();
        // A little-endian ELF64 executable for `machine`, naming `interpreter`
        // in a PT_INTERP right after its one program header, if any.
        let image = |name: &str, machine: u16, interpreter: Option<&std::path::Path>| {
            let mut bytes = vec![0_u8; 64];
            bytes[..4].copy_from_slice(b"\x7fELF");
            bytes[4] = 2; // ELFCLASS64
            bytes[5] = 1; // ELFDATA2LSB
            bytes[6] = 1; // EV_CURRENT
            bytes[16..18].copy_from_slice(&2_u16.to_le_bytes()); // ET_EXEC
            bytes[18..20].copy_from_slice(&machine.to_le_bytes());
            bytes[20..24].copy_from_slice(&1_u32.to_le_bytes());
            bytes[52..54].copy_from_slice(&64_u16.to_le_bytes()); // e_ehsize
            bytes[54..56].copy_from_slice(&56_u16.to_le_bytes()); // e_phentsize
            if let Some(interpreter) = interpreter {
                bytes[32..40].copy_from_slice(&64_u64.to_le_bytes()); // e_phoff
                bytes[56..58].copy_from_slice(&1_u16.to_le_bytes()); // e_phnum
                let mut name = interpreter.as_os_str().as_encoded_bytes().to_vec();
                name.push(0);
                let mut header = vec![0_u8; 56];
                header[..4].copy_from_slice(&3_u32.to_le_bytes()); // PT_INTERP
                header[8..16].copy_from_slice(&120_u64.to_le_bytes()); // p_offset
                header[32..40].copy_from_slice(&(name.len() as u64).to_le_bytes());
                bytes.extend(header);
                bytes.extend(name);
            }
            let path = directory.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        };
        let same = |path: &std::path::Path| Some(path.to_path_buf());
        let loader = image("loader", super::EM_AARCH64, None);
        let program = image("program", super::EM_AARCH64, Some(&loader));
        let (profile, _) = super::resolve_auto_on(super::EM_AARCH64, &program, same)
            .expect("an aarch64 image on an aarch64 host resolves");
        assert_eq!(profile, super::CpuProfile::X86_64V2);
        let why = super::resolve_auto_on(super::EM_X86_64, &program, same).unwrap_err();
        assert!(why.contains("not an x86-64 ELF64 image"), "{why}");
        let text = directory.path().join("text");
        std::fs::write(&text, b"not an image\n").unwrap();
        let why = super::resolve_auto_on(super::EM_X86_64, &text, same).unwrap_err();
        assert!(why.contains("not an x86-64 ELF64 image"), "{why}");
        let why = super::resolve_auto_on(super::EM_AARCH64, &text, same).unwrap_err();
        assert!(why.contains("neither a little-endian ELF64 image"), "{why}");
        let orphan = image(
            "orphan",
            super::EM_AARCH64,
            Some(&directory.path().join("no-such-loader")),
        );
        let why = super::resolve_auto_on(super::EM_AARCH64, &orphan, same).unwrap_err();
        assert!(why.contains("no-such-loader"), "{why}");
    }

    /// `--cpu-profile auto` follows a valid script chain (script to
    /// script to an ELF image) through every hop, and refuses a chain
    /// longer than the bound and a `#!` line naming no interpreter.
    #[test]
    fn auto_follows_a_script_chain_and_refuses_an_unbounded_one() {
        use std::io::Write as _;
        let directory = tempfile::tempdir().unwrap();
        let exe = std::env::current_exe().unwrap();
        let script = |name: &str, interpreter: &std::path::Path| {
            let path = directory.path().join(name);
            let mut file = std::fs::File::create(&path).unwrap();
            writeln!(file, "#!{}", interpreter.display()).unwrap();
            path
        };
        // A -> B -> the test binary (an ELF image): both hops followed.
        let b = script("b", &exe);
        let a = script("a", &b);
        let (profile, _) = super::resolve_auto(&a, |path| Some(path.to_path_buf()))
            .expect("a valid chain resolves");
        assert_eq!(profile, super::CpuProfile::X86_64V2);
        // Every hop mapped through the caller's view: the same chain with
        // its interpreter names mapped onto the files above (the image
        // linked into the mapped view).
        std::os::unix::fs::symlink(&exe, directory.path().join(exe.file_name().unwrap())).unwrap();
        let (mapped, _) = super::resolve_auto(&a, |guest| {
            // The view maps the chain's own names; system paths (the
            // image's program interpreter) stay as they are.
            if guest.starts_with("/tmp") || guest.parent() == exe.parent() {
                Some(directory.path().join(guest.file_name().unwrap()))
            } else {
                Some(guest.to_path_buf())
            }
        })
        .expect("a mapped chain resolves");
        assert_eq!(mapped, super::CpuProfile::X86_64V2);
        // A self-referring script is a chain past the bound, refused.
        let loop_path = directory.path().join("loop");
        std::fs::write(&loop_path, format!("#!{}\n", loop_path.display())).unwrap();
        let why = super::resolve_auto(&loop_path, |path| Some(path.to_path_buf())).unwrap_err();
        assert!(why.contains("script chain longer"), "{why}");
        // A `#!` line naming no interpreter (NUL first) is declined.
        let declined = directory.path().join("declined");
        std::fs::write(&declined, b"#!\0/bin/sh\n").unwrap();
        let why = super::resolve_auto(&declined, |path| Some(path.to_path_buf())).unwrap_err();
        assert!(why.contains("names no interpreter"), "{why}");
        // An interpreter hidden by a mount with no source refuses, rather
        // than classifying the host file its target shadows.
        let why = super::resolve_auto(&a, |_| None).unwrap_err();
        assert!(why.contains("is hidden by a mount"), "{why}");
    }
}
