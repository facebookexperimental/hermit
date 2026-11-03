/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Detcore configuration and widely used types.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fmt;
use std::num::NonZeroU32;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::SystemTime;

use chrono::DateTime;
use chrono::Utc;
use clap::Parser;
use reverie::BackendCapabilities;
use serde::Deserialize;
use serde::Serialize;

use crate::happens_before::HappensBeforeProgram;
use crate::network_trace::NetworkTraceConfig;
use crate::pid::DetTid;
use crate::schedule::SigWrapper;
use crate::time::NANOS_PER_RCB;
use crate::time::NANOS_PER_SCHED;
use crate::time::RcbTimeMultiplier;

const fn default_backend_capabilities() -> BackendCapabilities {
    DEFAULT_BACKEND_CAPABILITIES
}

/// The capabilities a [`Config`] has before the host prepares it for the
/// backend that runs it: ptrace's, the backend Detcore's own tests use, except
/// that nine facts stay off (`supports_blocked_wait_signal_interruption` and
/// the four split out of it, which [`set_kernel_signal_facts`] clears
/// together, `delivers_preemption_timer_events`,
/// `runs_guest_code_between_injections`, `reports_sigreturns`,
/// `hardware_watchpoints`). Until
/// `supports_blocked_wait_signal_interruption`,
/// `delivers_preemption_timer_events` and
/// `runs_guest_code_between_injections` became backend capabilities they were
/// `Config` fields that only the host set, for the backend it had selected,
/// and were false otherwise; a configuration nobody prepared keeps that
/// behaviour, and the four readings of the first with it. A configuration
/// nobody prepared cannot know that sigreturns are reported, so that is off
/// too, and it sets no hardware watchpoints (the DBT runtime's standalone
/// fallback starts from these). `reports_kernel_child_exit_signals` keeps
/// ptrace's value, as `reports_child_exit_publication` does: it took over the
/// child-exit ledger's gates from that field, so such a configuration behaves
/// as before. The host copies the running backend's own answer over all of
/// these before a run.
pub const DEFAULT_BACKEND_CAPABILITIES: BackendCapabilities = {
    let mut backend = BackendCapabilities::PTRACE;
    set_kernel_signal_facts(&mut backend, false);
    backend.delivers_preemption_timer_events = false;
    backend.runs_guest_code_between_injections = false;
    backend.reports_sigreturns = false;
    backend.hardware_watchpoints = 0;
    backend
};

/// Sets `supports_blocked_wait_signal_interruption` and the four capabilities
/// Detcore read from it until A5 step 3c.0 split them out
/// (`kernel_signal_state_is_guests`, `kernel_signal_targets_are_predictable`,
/// `host_task_reads_reach_guest_threads`, `host_timed_signals_are_physical`)
/// to `on`, together. Every backend has the five equal, so a configuration
/// that set or cleared the one field before the split sets or clears all five
/// with this, and behaves as it did.
pub const fn set_kernel_signal_facts(backend: &mut BackendCapabilities, on: bool) {
    backend.supports_blocked_wait_signal_interruption = on;
    backend.kernel_signal_state_is_guests = on;
    backend.kernel_signal_targets_are_predictable = on;
    backend.host_task_reads_reach_guest_threads = on;
    backend.host_timed_signals_are_physical = on;
}

/// One mount row whose kernel-private root must be replaced before it becomes
/// guest-visible.
///
/// The CLI populates this only after proving, with held file descriptors in the
/// completed mount namespace, that the mount is one Hermit created.  The raw
/// mount ID is namespace-local, so these entries are valid only for the one
/// container run whose configuration carries them.
#[derive(Debug, Serialize, Deserialize, Clone, Eq, PartialEq)]
pub struct MountInfoRootRewrite {
    /// Mount ID read from the held target descriptor's `/proc/self/fdinfo`.
    pub raw_mount_id: u64,
    /// Stable guest-visible replacement for the row's root field.
    pub deterministic_root: Vec<u8>,
    /// Exact encoded kernel root prefix used for descendant mount rows.
    ///
    /// This is present only for a proven private `/tmp`. Mounts installed below
    /// that directory before it is bound over guest `/tmp` otherwise expose the
    /// randomly named backing directory in mountinfo field 5.
    #[serde(default)]
    pub raw_root_prefix: Option<Vec<u8>>,
    /// Guest-visible prefix replacing `raw_root_prefix`.
    #[serde(default)]
    pub deterministic_root_prefix: Option<Vec<u8>>,
    /// Exact encoded host path prefix used for descendant mountpoints.
    #[serde(default)]
    pub raw_mountpoint_prefix: Option<Vec<u8>>,
    /// Guest-visible prefix replacing `raw_mountpoint_prefix`.
    #[serde(default)]
    pub deterministic_mountpoint_prefix: Option<Vec<u8>>,
}

/// The seccomp state of the thread that starts the guest, which the guest
/// inherits: every filter that thread has applies to the guest's system calls,
/// and to the calls Detcore injects into it. Detcore refuses the guest's own
/// `seccomp(2)` and `prctl(PR_SET_SECCOMP)`, so these and the backend's own
/// filter are the only filters a guest has.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize
)]
pub enum InheritedSeccomp {
    /// Not measured, or not readable, so a filter may apply.
    #[default]
    Unknown,
    /// No seccomp (`Seccomp: 0`).
    Disabled,
    /// Strict mode (`Seccomp: 1`).
    Strict,
    /// Filter mode (`Seccomp: 2`), with the number of filters when
    /// `Seccomp_filters` gives it.
    Filter(Option<u32>),
}

impl InheritedSeccomp {
    /// The state of the calling thread, which a process it forks inherits,
    /// from `/proc/thread-self/status`.
    pub fn measure() -> Self {
        std::fs::read_to_string("/proc/thread-self/status")
            .map_or(Self::Unknown, |status| Self::parse(&status))
    }

    /// The state the `Seccomp` and `Seccomp_filters` lines of the text of a
    /// `/proc/<pid>/status` file give. A missing or unexpected `Seccomp` line
    /// is unknown.
    pub fn parse(status: &str) -> Self {
        let field = |name: &str| {
            status
                .lines()
                .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
                .map(str::trim)
        };
        match field("Seccomp") {
            Some("0") => Self::Disabled,
            Some("1") => Self::Strict,
            Some("2") => Self::Filter(field("Seccomp_filters").and_then(|n| n.parse().ok())),
            _ => Self::Unknown,
        }
    }
}

/// What a launch tells Detcore about the guest's first image
/// ([`Config::launch_image`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchImage {
    /// The first image as its `/proc/self/exe` link names it: the program's
    /// canonical path, or for a script the interpreter it runs
    /// (`detcore::exec_record::ExecRecord::resolve`). `None` where the launch
    /// could not resolve it, which leaves the first image without a record.
    pub exe: Option<PathBuf>,
    /// The image the kernel exec'd in the guest's place, where a launcher
    /// loads the guest itself (SaBRe's loader, DynamoRIO's library), so that
    /// the kernel's `/proc/<pid>/exe` names the launcher. Detcore keeps a recorded image
    /// only where the kernel's link names that image or this one
    /// (`detcore::exec_record`). `None` where the kernel execs the guest.
    pub launcher: Option<PathBuf>,
}

/// One mount namespace, as `DetConfig::mountinfo_mount_namespace` names it.
///
/// `id` is the kernel's 64-bit namespace ID (`ioctl(NS_GET_MNTNS_ID)` on a
/// `/proc/<pid>/ns/mnt` descriptor, on kernels that have it). Linux draws it from
/// one counter that only increases (`__ns_tree_gen_id` in kernel/nstree.c), so
/// it names one namespace for the life of the boot. `device` and `inode` are
/// the descriptor's nsfs device and inode numbers, which Linux recycles once a
/// namespace is gone; they alone identify the namespace only on a kernel
/// without the ioctl, where `id` is `None`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MountNamespaceIdentity {
    /// The kernel's namespace ID, where the kernel reports one.
    pub id: Option<u64>,
    /// The nsfs device number of the namespace's descriptor.
    pub device: u64,
    /// The nsfs inode number of the namespace's descriptor.
    pub inode: u64,
}

/// What `--detlog-anon` records for each anonymous mapping.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    clap::ValueEnum
)]
pub enum DetlogAnon {
    /// One digest per mapping.
    Mappings,
    /// One digest per mapping, and a short one for each of its 4 KiB pages that is not all zero.
    Pages,
}

/// A frozen copy of a directory whose files the guest maps read-only while
/// another process rewrites them in place. On Android that is the system
/// property area, `/dev/__properties__`: init writes every property change into
/// those shared files, and bionic (the dynamic linker's private copy as well as
/// libc's) rereads its cached values whenever their serial moves, so property
/// writes made outside the run during it change which code the guest runs.
///
/// An open of `guest_dir/<name>` for reading, by any guest thread from its
/// first system call, opens `copy_dir/<name>` instead when that copy exists. A
/// write open, or a name with no copy, is left as it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PropertySnapshot {
    /// The directory the guest names: `/dev/__properties__`.
    pub guest_dir: PathBuf,
    /// The directory holding the copies, one per file of `guest_dir` Hermit
    /// could read, made before the guest's first exec.
    pub copy_dir: PathBuf,
    /// Each live file copied: its host device and inode, by which an open
    /// that reaches it by any spelling (a symbolic link, a bind mount, a
    /// descriptor link) is recognized, and its name in `copy_dir`.
    #[serde(default)]
    pub live_files: Vec<PropertyFile>,
    /// Each live file the capture saw but could not copy, because it stopped
    /// being readable mid-capture. It has no copy, so an open that reaches it
    /// by any spelling reads the live area, which is refused under the
    /// default fail-closed policy.
    #[serde(default)]
    pub uncopied_files: Vec<PropertyFile>,
    /// Whether the copies are one consistent cut: `properties_serial` read
    /// the same before and after they were taken. Otherwise (properties kept
    /// changing, or the serial could not be read) each file is coherent on
    /// its own, but two files may come from different moments. Shown in the
    /// run summary; not stored in preemption or schedule recordings.
    #[serde(default)]
    pub consistent_cut: bool,
}

impl PropertySnapshot {
    /// The live file the snapshot copied with host device `dev` and inode
    /// `ino`, if any.
    pub fn live_file(&self, dev: u64, ino: u64) -> Option<&PropertyFile> {
        self.live_files
            .iter()
            .find(|file| (file.dev, file.ino) == (dev, ino))
    }

    /// The live property file with host device `dev` and inode `ino` the
    /// capture saw, copied or not: reading it reads the live area. Matching
    /// is by identity alone, here and in [`Self::live_file`]: if a captured
    /// file is deleted during the run and its inode reused by an unrelated
    /// file, a pure read of that file is falsely redirected to the copy (a
    /// copied identity) or treated as the live area and refused in a
    /// fail-closed run (an uncopied one). Either way no live property read
    /// is let through.
    pub fn known_live_file(&self, dev: u64, ino: u64) -> Option<&PropertyFile> {
        self.live_file(dev, ino).or_else(|| {
            self.uncopied_files
                .iter()
                .find(|file| (file.dev, file.ino) == (dev, ino))
        })
    }
}

/// One live property file a [`PropertySnapshot`] copied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PropertyFile {
    /// The live file's host `st_dev`.
    pub dev: u64,
    /// The live file's host `st_ino`.
    pub ino: u64,
    /// Its name, in the guest directory and in the copy directory.
    pub name: String,
}

/// Configuration options for detcore.
#[derive(Debug, Serialize, Deserialize, Clone, Parser)]
pub struct Config {
    /// Disable virtual/logical time. Note that virtual time is required for virtual metadata.
    #[clap(long = "no-virtualize-time", action = clap::ArgAction::SetFalse)]
    pub virtualize_time: bool,

    /// Disable virtual cpuid
    #[clap(long = "no-virtualize-cpuid", action = clap::ArgAction::SetFalse)]
    pub virtualize_cpuid: bool,

    /// The CPU the guest sees: virtual x86-64-v2/v3 or intercepted host
    /// features without hardware randomness. The long help states each
    /// profile's host requirements and determinism limits. Like
    /// HERMIT_EPOCH, HERMIT_CPU_PROFILE supplies the value when the flag is
    /// absent; an explicit flag wins, and a value that names no profile is
    /// refused. Unset means auto, for the command line and
    /// [`Config::default`] alike: `hermit run` and `hermit record start`
    /// resolve it to a named profile before launch, and the library's launch
    /// functions resolve it from the program they launch. A serialized
    /// configuration or recording without the field still means x86-64-v2
    /// (`CpuProfile`'s own default), never auto. Admission is the same either
    /// way.
    #[clap(long, value_enum, default_value = "auto", env = "HERMIT_CPU_PROFILE", long_help = crate::cpu_profile::CPU_PROFILE_LONG_HELP)]
    #[serde(default)]
    pub cpu_profile: crate::cpu_profile::CpuProfile,

    /// aarch64: rewrite the canonical load/store-exclusive retry loops of
    /// every executable file mapping in the guest's memory, as each is
    /// mapped. Set by the CLI for `--rewrite-exclusives` in a run that has no
    /// mount namespace to carry rewritten files; never read from a recording.
    #[clap(skip)]
    #[serde(default)]
    pub rewrite_exclusives_in_memory: bool,

    /// aarch64: count the store-exclusives the guest executes and fails, as
    /// a run that records or replays preemptions does. Set by the CLI for
    /// each run of `--verify` and `--verify-run`, so that a divergence a
    /// retried load/store-exclusive loop explains can be named; never read
    /// from a recording.
    #[clap(skip)]
    #[serde(default)]
    pub count_store_exclusives: bool,

    /// aarch64: the guest paths `--rewrite-exclusives` rewrote into
    /// bind-mounted copies, or found nothing to rewrite in: the executable
    /// and each `--rewrite-exclusives-also` file. Set by the CLI for a run
    /// that rewrites with that overlay, empty otherwise, so that the run can
    /// name the other mapped files whose load/store-exclusive loops it left;
    /// never read from a recording.
    #[clap(skip)]
    #[serde(default)]
    pub rewrite_exclusives_overlay: Vec<PathBuf>,

    /// aarch64: whether `--rewrite-exclusives` rewrites by overlaying
    /// rewritten files, as the run's flags request. Set by the CLI from
    /// the flags alone, so early replay admission can state the run's
    /// rewrite provenance while `rewrite_exclusives_overlay` -- the
    /// files the overlay was given, known only once they are prepared
    /// -- is still empty; never read from a recording.
    #[clap(skip)]
    #[serde(default)]
    pub rewrite_exclusives_overlay_requested: bool,

    /// The host facts a v3 guest's leaf 0xD and xstate answers come from, set
    /// by the CLI after the host qualifies; `None` under v2. Neither profile
    /// field enters the legacy form the DBT runtime reads (DBT refuses v3).
    #[clap(skip)]
    #[serde(default)]
    pub cpu_profile_host: Option<crate::cpu_profile::HostXstate>,

    /// Native CPUID source for host-no-rdrand, selected during admission from
    /// the caller's original allowed CPU set, before container placement.
    /// The ptrace launch pins its actual tracer to this CPU. Never restored
    /// from recording metadata: host-profile recording/replay is refused.
    #[clap(skip)]
    #[serde(default)]
    pub cpu_profile_source: Option<usize>,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-845): Review in-process backend descriptor discovery.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-845): Review host-clock futex deadline detection.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-845): Review backend-owned syscall-clobber determinism.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-845): Review backend-local exit-group RPC cancellation.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1058): Review process-signal identity translation.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1125): Review backend-owned capability-control prctls.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1152): Review deferred vfork child registration.
    /// How the execution backend runs the guest, as the backend reports it
    /// through Reverie (`reverie::Backend::capabilities`).
    ///
    /// Detcore reads facts about the backend only from here, never from the
    /// backend's name, so a new backend is modelled correctly by reporting its
    /// capabilities. The host copies the running backend's answer into this
    /// field before the run starts. The default,
    /// [`DEFAULT_BACKEND_CAPABILITIES`], describes a ptrace-hosted guest, the
    /// backend Detcore's own tests use, with the three host-set facts and
    /// `reports_sigreturns` off.
    ///
    /// JSON that the guest can see is written by [`to_legacy_backend_json`],
    /// which carries this field as the separate keys used before it existed;
    /// [`from_legacy_backend_json`] reads them back.
    #[serde(default = "default_backend_capabilities")]
    #[clap(skip = DEFAULT_BACKEND_CAPABILITIES)]
    pub backend: BackendCapabilities,

    /// The guest may start with a terminal: one of the launcher's standard
    /// descriptors is a terminal, the launcher has a controlling terminal that
    /// the guest inherits with its session, or the launcher could not tell.
    /// Such a terminal makes Linux send signals at moments the host sets:
    /// SIGHUP and SIGCONT when it hangs up, SIGINT, SIGQUIT and SIGTSTP for its
    /// interrupt, quit and suspend characters, SIGWINCH when it is resized, and
    /// SIGTTIN and SIGTTOU for a background process group's reads and writes.
    /// The guest makes no traced call that arms them, so where Detcore treats
    /// such signals as reaching the guest from the host
    /// (`BackendCapabilities::host_timed_signals_are_physical`)
    /// the scheduler records all eight as host-timed for every process before the
    /// guest's first instruction, and no gated wait ends for them
    /// (<https://github.com/rrnewton/hermit/issues/3146>).
    ///
    /// The host sets it while it prepares the backend configuration. It has no
    /// legacy key: [`to_legacy_backend_json`] leaves it out and
    /// [`from_legacy_backend_json`] reads it back as false.
    #[serde(default)]
    #[clap(skip)]
    pub guest_may_inherit_a_terminal: bool,

    /// The forwarding policy (`detcore::detlog::ForwardPolicy::encode`) every
    /// in-guest Tool must forward its DETLOG records with, on the socket Hermit
    /// passed it (`hermit run --verify` under SaBRe or in-guest LiteInst);
    /// `None` when no socket is passed.
    ///
    /// The host sets it, not a flag, and every Tool image receives it in the
    /// configuration handshake on its coordinator connection, which guest code
    /// cannot change. A Tool image whose forwarder is missing, or forwards by
    /// another policy (guest code that ran first changed its private
    /// forwarding variables, so some records would be dropped before they are
    /// counted), then reports every forwarded-record count as UNCOUNTED
    /// (`detcore::detlog::require_forwarding`), so verification refuses the run
    /// instead of accepting a log without those records. Like
    /// `guest_may_inherit_a_terminal`, it has no legacy key.
    #[serde(default)]
    #[clap(skip)]
    pub in_guest_detlog_forward_policy: Option<String>,

    /// Whether every in-guest Tool image must run with syscall site patching
    /// off and collect its exit statistics (`hermit --backend=in-guest-trap`).
    ///
    /// The host sets it, not a flag, and every Tool image receives it in the
    /// configuration handshake on its coordinator connection, which guest code
    /// cannot change. The in-guest LiteInst runtime takes both settings from
    /// the guest environment, which guest code running before or during its
    /// installation could change, so once installation returns a Tool image
    /// compares the settings the runtime actually captured with this one and
    /// refuses to run the program when they differ
    /// (`detcore::in_guest_site_patching`). Like
    /// `in_guest_detlog_forward_policy`, it has no legacy key.
    #[serde(default)]
    #[clap(skip)]
    pub in_guest_site_patching_off: bool,

    /// The signals an in-guest runtime keeps for itself, which a guest may
    /// not send to another process or arm as a notification signal: bit
    /// `n - 1` for signal `n`, as Detcore's other signal sets are kept.
    ///
    /// Such a signal sent from one process to another would run the target's
    /// runtime handler on a thread that does not hold the scheduler's turn
    /// (A5 step 3c, design section 16.30,
    /// <https://github.com/rrnewton/hermit/issues/4485>). So Detcore answers a
    /// send of one of these signals to a process other than the caller's own
    /// (`kill`, `tkill`, `tgkill`, `rt_sigqueueinfo`, `rt_tgsigqueueinfo`,
    /// `pidfd_send_signal`), and any arming of one (`fcntl(F_SETSIG)`,
    /// `timer_create`, `prctl(PR_SET_PDEATHSIG)`), with `EPERM` and a WARN
    /// ([`Config::reserves_in_guest_signal`]). A send to the caller's own
    /// process is unchanged. Empty (0), the default, changes nothing.
    ///
    /// The host sets it, not a flag, when it prepares an in-guest run on the
    /// default route: SIGSYS, SIGSEGV while the runtime subscribes
    /// instructions, and SIGTRAP while LiteInst's guard router may be
    /// installed: on `--backend=liteinst`, never with
    /// `in_guest_site_patching_off` (a liteinst image whose environment turns
    /// patching off has no router but keeps SIGTRAP reserved, a known
    /// over-refusal).
    /// Every other backend, and the in-guest loader route, leave it empty.
    /// Every Tool image receives it in the configuration handshake, which
    /// guest code cannot change. Like `in_guest_site_patching_off`, it has no
    /// legacy key.
    #[serde(default)]
    #[clap(skip)]
    pub in_guest_reserved_signals: u64,

    /// Whether Detcore refuses the guest calls that would let the kernel send
    /// a guest process a signal at a moment host timing decides (A5 step 3c,
    /// slice 0b; design sections 16.30, D28 and D28a,
    /// <https://github.com/rrnewton/hermit/issues/4485>).
    ///
    /// While the in-guest runtime runs its own code, ordinary signals are
    /// blocked, and Linux keeps a blocked signal pending even under
    /// `SIG_IGN`, so such a signal could become pending at a host-timed
    /// moment whatever the guest's action. So where this is set, Detcore
    /// answers with `EPERM` and a WARN the operation that completes such a
    /// producer: an owner for a socket or for a descriptor with `O_ASYNC`
    /// (`F_SETOWN`, `F_SETOWN_EX`, `FIOSETOWN`, `SIOCSPGRP`), `O_ASYNC` for a
    /// descriptor with an owner (`F_SETFL`, `FIOASYNC`), a lease
    /// (`F_SETLEASE`) and a directory notification (`F_NOTIFY`). It holds a
    /// parent-death signal it can send itself, as it does on ptrace, and
    /// refuses the run by name (`pdeathsig-would-fire`) if one would fire; it
    /// refuses with `EPERM` one it cannot hold. A SIGKILL parent-death signal
    /// keeps the kernel's path. False, the default, changes nothing.
    ///
    /// The host sets it, not a flag, when it prepares an in-guest run on the
    /// default route, where it also clears the physical `RLIMIT_CPU` soft
    /// limit and refuses an inherited descriptor that is already such a
    /// producer (D25). Every other backend, and the in-guest loader route,
    /// leave it false. Every Tool image receives it in the configuration
    /// handshake, which guest code cannot change. Like
    /// `in_guest_site_patching_off`, it has no legacy key.
    #[serde(default)]
    #[clap(skip)]
    pub in_guest_refuses_host_timed_producers: bool,

    /// The guest's first image, as its `/proc/self/exe` link names it, and
    /// the image the kernel exec'd in its place where a launcher loads the
    /// guest ([`LaunchImage`]). Detcore answers that link from it until the
    /// guest's next exec, which Detcore resolves itself. `None` leaves the
    /// first image's link to the kernel.
    ///
    /// The host sets it, not a flag, from the program it launches. Like
    /// `in_guest_detlog_forward_policy` it has no legacy key, so DBT's
    /// legacy form never carries it.
    #[serde(default)]
    #[clap(skip)]
    pub launch_image: Option<LaunchImage>,

    /// The mount IDs and filesystem types of the mount namespace hermit-cli
    /// was launched in, read in its outer process before it creates its
    /// container ([`crate::host_input::LaunchNamespaceMounts`]). A descriptor
    /// the guest inherits from outside the container (the user's redirected
    /// stdio, `/dev/null`, a `--verify` stdin snapshot) names one of these
    /// mounts in its `fdinfo`, which the container's own mountinfo does not
    /// list, so Detcore reads the guest's standard descriptor kinds with it
    /// (`crate::host_input::StdioKind::of_guest_descriptor`). Empty for a
    /// library caller, whose descriptors' mounts are in its own namespace.
    ///
    /// The host sets it, not a flag. In-process only, as `happens_before`
    /// is: never serialized, so neither a recording nor DBT's legacy form
    /// carries it, and a replay never reads another host's mount IDs.
    #[serde(skip)]
    #[clap(skip)]
    pub launch_namespace_mounts: crate::host_input::LaunchNamespaceMounts,

    /// aarch64: the files the run's `--rewrite-exclusives` left unchanged,
    /// wholly or in part, because no mapping symbol it accepts marks their
    /// code: none, or one that is neither `$x` nor `$d`. The CLI sets it; the
    /// run names them in its summary.
    #[serde(default)]
    #[clap(skip)]
    pub rewrite_exclusives_unmarked: Vec<std::path::PathBuf>,

    /// The seccomp state the guest inherits from the thread that starts it
    /// ([`InheritedSeccomp`]). The caller that starts the guest states it, as
    /// only the caller knows the filters its launch installs, such as those of
    /// a `pre_exec` callback; `hermit run` measures it. Not stated, it is
    /// unknown. Nothing reads it. Whether a guest may start under an inherited
    /// filter is decided by the `hermit` command, before it starts the guest,
    /// from its own reading of `/proc/self/status` (`host_seccomp::admit`).
    /// It has no legacy key:
    /// [`to_legacy_backend_json`] leaves it out and
    /// [`from_legacy_backend_json`] reads it back as unknown.
    #[serde(default)]
    #[clap(skip)]
    pub inherited_seccomp: InheritedSeccomp,

    /// Epoch of the logical time.
    ///
    /// This is the datetime from which all time and date modtimes begin and
    /// monotonically increase. It is in RFC3339 format such as `2026-01-01T00:00:00Z`.
    /// The stable default here is for library callers and wire-format fixtures;
    /// the `hermit run` and `hermit oci run` commands replace an omitted CLI
    /// default with one host wall-clock sample taken before backend dispatch.
    ///
    /// A guest that checks TLS certificates over the host network (`--network=host` or
    /// `--record-networking`) checks them against this clock, so an epoch outside a server
    /// certificate's validity fails the HTTPS connections to that server: curl exits 60 with "SSL
    /// certificate problem: certificate is not yet valid" (or "certificate has expired"), and a
    /// libcurl caller sees "SSL peer certificate or SSH remote key was not OK". For such a run omit
    /// `--epoch`: `hermit run` then starts the clock at the current time and saves it in the run's
    /// manifest, and `hermit run --from last-run` repeats the run with that epoch.
    #[clap(
        long,
        env = "HERMIT_EPOCH",
        value_name = "YYYY-MM-DDThh:mm:ssZ",
        default_value = DEFAULT_EPOCH_STR,
        hide_default_value = true
    )]
    pub epoch: DateTime<Utc>,

    /// Use this number to seed the PRNG randomness for both RNG and scheduler.
    /// This acts as a global fallback in case either `sched_seed` or `rng-seed`
    /// are not explicitly specified
    #[clap(
        long = "seed",
        env = "HERMIT_PRNG",
        default_value = "0",
        value_name = "uint64"
    )]
    pub seed: u64,

    /// Use this number to seed the PRNG that supplies randomness to the guest.
    /// This supplies guest system calls that expose randomness, as well as
    /// the `/dev/[u]random` files. It does not affect the `rdrand` instruction,
    /// which is disabled in the guest.
    #[clap(long, value_name = "uint64")]
    pub rng_seed: Option<u64>,

    /// Seeds the PRNG which drives syscall response fuzzing (i.e. chaotically exercising syscall
    /// nondeterminism).  Like other seeds, this is initialized from the `--seed` if not
    /// specifically provided.
    #[clap(long, value_name = "uint64")]
    pub fuzz_seed: Option<u64>,

    /// Logical clock multiplier. Values above one make time appear to go faster within the sandbox.
    #[clap(long, value_name = "float")]
    pub clock_multiplier: Option<f64>,

    /// Disable substitution of virtual (deterministic) file metadata in lieu
    /// of the real metadata returned by `stat`/`statx`. This also preserves raw
    /// mountinfo device numbers so those interfaces continue to agree. Raw
    /// device values are host/filesystem observations and are not promised to
    /// reproduce across machines. Virtual metadata implies `virtualize_time`.
    #[clap(long = "no-virtualize-metadata", action = clap::ArgAction::SetFalse)]
    pub virtualize_metadata: bool,

    /// Proven Hermit-owned mount roots to hide from `/proc/*/mountinfo`.
    ///
    /// This is runtime provenance, not a user option.  `serde(default)` keeps
    /// older serialized configurations compatible and makes backends which do
    /// not use the common container setup explicitly receive no rewrite claim.
    #[serde(default)]
    #[clap(skip)]
    pub mountinfo_root_rewrites: Vec<MountInfoRootRewrite>,

    /// Backend-proven pairs of (`mountinfo` raw device, `stat`/`statx` raw device).
    ///
    /// A backend may synthesize mountinfo independently from its pathname
    /// metadata implementation.  These pairs state that the two raw numbers
    /// describe the same filesystem, so Detcore can feed both surfaces through
    /// one device identity.  The pairs are runtime provenance, not a user
    /// option; an absent pair must never be inferred from numeric coincidence.
    #[serde(default)]
    #[clap(skip)]
    pub mountinfo_device_rewrites: Vec<(u64, u64)>,

    /// Recording/container namespace mount IDs in canonical row order.
    ///
    /// Detcore validates every `/proc/*/mountinfo` view against this order.
    /// It does not number mount IDs by position in it; see
    /// `mount_id_assignment_order`. It is runtime provenance rather than a
    /// user option; replay retains recording-time raw IDs because its read
    /// events contain recording-time kernel bytes.
    #[serde(default)]
    #[clap(skip)]
    pub mountinfo_mount_ids: Vec<u64>,

    /// Whether `mountinfo_mount_ids` is an exact producer-owned snapshot.
    ///
    /// The distinction matters for an empty mountinfo file: an absent snapshot
    /// asks Detcore to observe the completed guest namespace, while a captured
    /// empty snapshot must remain empty during replay.
    #[serde(default)]
    #[clap(skip)]
    pub mountinfo_mount_ids_captured: bool,

    /// The mount namespace `mountinfo_mount_ids` was captured in
    /// ([`MountNamespaceIdentity`]). A backend that checks its provenance
    /// (KVM) requires its own namespace to be this one, instead of
    /// recapturing the namespace's rows: a host can detach a copied host mount
    /// from a private namespace at any time by removing that mount's
    /// directory, so two captures of one namespace need not agree. `None`
    /// when nothing was captured, and in recordings. Runtime provenance, not a
    /// user option.
    #[serde(default)]
    #[clap(skip)]
    pub mountinfo_mount_namespace: Option<MountNamespaceIdentity>,

    /// Raw mount IDs in the order Detcore numbered them for the guest.
    ///
    /// Detcore numbers a raw mount ID when the guest first observes it through
    /// `/proc/*/mountinfo` or `/proc/*/fdinfo/*`; `mountinfo_mount_ids` only
    /// validates mountinfo views. A live run starts empty. Recording persists
    /// the completed order so replay gives every raw ID its recorded number
    /// rather than deriving one from its fresh namespace or launch descriptor
    /// shape.
    #[serde(default)]
    #[clap(skip)]
    pub mount_id_assignment_order: Vec<u64>,

    /// The guest's container mount table, which Detcore renders as every
    /// spelling of `/proc/<pid>/mountinfo` and `/proc/<pid>/mounts` on every
    /// backend (`crate::container_mounts`,
    /// <https://github.com/rrnewton/hermit/issues/4213>). hermit-cli builds it
    /// once, in the completed container namespace before the guest starts,
    /// or, for `--no-namespace` and a library caller on KVM that built no
    /// container, in the namespace the guest runs in (its launch mounts and
    /// base mountpoints); a recording carries it, so its replay renders the
    /// recorded table. `None` (a library caller on a backend other than KVM,
    /// whose command may chroot or mount where Hermit cannot see, and every
    /// older configuration) leaves both files to the kernel, sanitized as
    /// before. Runtime provenance, not a user option.
    #[serde(default)]
    #[clap(skip)]
    pub container_mount_table: Option<crate::container_mounts::ContainerMountTable>,

    /// The guest path of the empty, read-only directory hermit-cli mounted in
    /// the guest's container for Detcore's placeholder directories where the
    /// guest's procfs is not the kernel's (KVM: the backend provides process
    /// signal control, `procfs_inode::tool_can_name_guest_descriptors`): a
    /// tmpfs at `/proc/fs/nfsd`, procfs's own permanently empty mount point,
    /// which Detcore's `--proc` level table hides from the guest at every
    /// level, and which the guest's mount table leaves out. Each descriptor
    /// Detcore serves there for `/proc` itself or the caller's own
    /// `/proc/<pid>/fd` is open on this directory, and Detcore answers every
    /// call through it from its model; an open, stat or statx of the guest's
    /// that reaches the directory otherwise fails as `/proc/fs` does. `None`
    /// where the guest's procfs is the kernel's (ptrace, the in-guest
    /// backends: nothing is mounted, see [`Self::container_procfs`]), and
    /// where hermit-cli mounted none or a later mount covers it:
    /// `--no-namespace`, `--namespace-only`, record and replay, a user mount
    /// over the placeholder, every library caller, and every older
    /// configuration. On KVM, Detcore then serves no placeholder directory,
    /// and an open of `/proc` or a descriptor directory is refused (`EACCES`).
    /// Runtime provenance, not a user option.
    #[serde(default)]
    #[clap(skip)]
    pub directory_placeholder: Option<PathBuf>,

    /// Whether the guest runs in hermit-cli's container, whose procfs mount
    /// at `/proc` is its own (every container hermit-cli runs a guest in, not
    /// `--no-namespace`, not a library caller, not an older configuration).
    /// Where the guest's procfs is the kernel's (ptrace), Detcore serves
    /// `/proc` itself and the caller's own `/proc/<pid>/fd` then, each on a
    /// descriptor on that procfs's root, and serves neither without it.
    /// Runtime provenance, not a user option.
    #[serde(default)]
    #[clap(skip)]
    pub container_procfs: bool,

    /// Sequentialize thread execution deterministically.
    #[clap(long)]
    pub sequentialize_threads: bool,

    /// Choose which side of an ordinary fork/clone runs first after the child is registered.
    /// Random choices are deterministic under `--sched-seed`.
    #[serde(default)]
    #[clap(long, default_value = "child", value_name = "child|parent|random")]
    pub runs_post_fork: RunsPostFork,

    /// Use the optimized partial syscall subscription set instead of intercepting every syscall.
    /// This permits unlisted syscalls to bypass Detcore and therefore weakens deterministic
    /// accounting; leave it disabled for fail-closed execution.
    #[serde(default)]
    #[clap(long)]
    pub passthru_opt: bool,

    /// Leave to the kernel, without a stop, the few syscalls that Detcore always passes
    /// through unchanged and that never wait for another thread: getppid, umask, getgroups,
    /// capget, sched_get_priority_max, sched_get_priority_min and get_robust_list. Each such
    /// call then costs no tracer round trip, but it also charges no virtual time, logs no
    /// DETLOG line, is not counted in the run summary, and is not a point where the thread's
    /// timeslice can end, so the run's virtual clock, schedule and log differ from a run
    /// without this flag, the same in every run with it. Off by default; refused under
    /// --strict, which promises that Detcore observes every syscall, without RCB time, and
    /// with a preemption record to write or replay, whose callback history counts these calls.
    #[serde(default)]
    #[clap(long)]
    pub untrap_passthrough_syscalls: bool,

    /// In chaos mode, uses much cheaper approximate preemption timers.  Only makes sense
    /// when recording preemptions for later (precise) replay.
    #[clap(long)]
    pub imprecise_timers: bool,

    /// Stop the guest at every timeslice end, including one where no other
    /// guest thread could run. By default Hermit does not stop a thread that
    /// is the only one able to run before it next enters Detcore (every other
    /// thread waits on a futex without a timeout, on a child or in
    /// `rt_sigsuspend`; no timer, sleeper, poll deadline or host call is
    /// pending): it processes those timeslice ends when the thread next
    /// enters Detcore, at the same branch counts and in the same order, so the
    /// schedule, the virtual clock and the chaos draws are the same either
    /// way and only the stops are saved. This option turns that off, for
    /// comparison.
    #[serde(default)]
    #[clap(long)]
    pub eager_lone_preemptions: bool,

    /// Turn off Detcore's own copy of each guest thread's signal dispositions
    /// and masks (A5, docs/SIGNAL_STATE_CHECK.md). The copy is on by default
    /// where it can run (ptrace with serialized threads): it is checked
    /// against the kernel, and decisions A5 converted read it in place of the
    /// kernel's `/proc` view. With it off, those decisions read `/proc`, and
    /// no handler return is reported. A recorded schedule states the setting
    /// it ran under, and replaying it under the other setting is refused.
    #[serde(default = "signal_state_copy_default")]
    #[clap(long = "no-signal-state-copy", action = clap::ArgAction::SetFalse)]
    pub signal_state_copy: bool,

    /// Whether a signal delivery that the kernel contradicts, where Detcore's
    /// copy and the kernel are both certain (A5 step 3.2), ends the run as an
    /// internal failure (`HERMIT_INTERNAL_FAILURE class=signal-delivery-check`)
    /// rather than being logged as a divergence that drops the copy. It is a
    /// Detcore defect either way. `hermit run --strict` sets it; nothing else
    /// does, and it changes nothing while the copy matches the kernel.
    ///
    /// The host sets it. It has no legacy key: [`to_legacy_backend_json`]
    /// leaves it out and [`from_legacy_backend_json`] reads it back as false.
    #[serde(default)]
    #[clap(skip)]
    pub signal_delivery_check_fatal: bool,

    /// `--strict`'s binder policy: the run never holds a binder descriptor,
    /// in a fresh run and in a replay alike, as binder reply payloads are not
    /// recorded (<https://github.com/rrnewton/hermit/issues/5124>). One, or
    /// a descriptor whose identity cannot be read, is refused where it
    /// arrives (an open, a receipt, the run's start), and every binder
    /// request on a descriptor that may be one is refused.
    /// `hermit run --strict` sets it; nothing else does.
    ///
    /// The host sets it. It has no legacy key: [`to_legacy_backend_json`]
    /// leaves it out and [`from_legacy_backend_json`] reads it back as false.
    #[serde(default)]
    #[clap(skip)]
    pub strict_binder: bool,

    /// Schedule threads chaotically.
    ///
    /// The behavior of this flag is subject to change. Current behavior is to randomize thread
    /// priorities at every logical timeslice. Other randomization strategies are possible with
    /// `--sched-heuristic`.
    ///
    /// Thread scheduling remains deterministic, determined by the random seed.
    #[clap(long, long_help = chaos_long_help())]
    pub chaos: bool,

    /// Draw the mean of each chaos slice from MIN..MAX virtual nanoseconds (both included),
    /// log-uniformly, instead of using `--max-timeslice` as every slice's mean, and cap every
    /// slice at MAX. Slices stay exponentially distributed around the drawn mean, so some fall
    /// below MIN. Races differ in the slice scale they need: some
    /// need a preemption in a short window, others a long uninterrupted run of one thread, and a
    /// seed sweep under one timeslice explores only one scale. Under a range every run mixes all
    /// the scales in it. The draws come from the scheduler seed's chaos stream, so a seed
    /// reproduces them, and a preemption record pins every slice end as usual. Sets the run's
    /// `--max-timeslice` to MAX: requires `--chaos` and PMU-backed preemption, and conflicts
    /// with an explicit `--max-timeslice`, with `--target-timeslice` and with `hermit run
    /// --chaos-seeded-timeslice`. A small MIN makes runs slower. Example: `1000..200000000`.
    #[serde(default)]
    #[clap(
        long,
        value_name = "MIN..MAX",
        requires = "chaos",
        conflicts_with = "target_timeslice"
    )]
    pub chaos_timeslice_range: Option<TimesliceRange>,

    /// How `--chaos-timeslice-range` draws. `slice` (the default) draws each slice's mean from
    /// the range. `run` instead draws the run's `--max-timeslice` once, log-uniformly from the
    /// range as a pure function of the scheduler seed, and the run is then an ordinary chaos run
    /// under it: a sweep's seeds spread over the scales of the range, each seed at one scale.
    /// That reaches a race needing one scale throughout in more seeds, at a higher cost per run
    /// for the fine draws. The drawn value is printed in `--summary-json`
    /// (`preemption.max_timeslice_ns`) and stored in a `--record-preemptions-to` record, from
    /// which a replay takes it. Without an explicit range, `run` uses
    /// [`RECOMMENDED_CHAOS_TIMESLICE_RANGE`]; `slice` still requires a range.
    #[serde(default)]
    #[clap(
        long,
        value_enum,
        default_value_t,
        requires = "chaos",
        requires_if("slice", "chaos_timeslice_range"),
        conflicts_with = "target_timeslice",
        long_help = chaos_timeslice_draw_long_help()
    )]
    pub chaos_timeslice_draw: ChaosTimesliceDraw,

    /// The `--max-timeslice` this run drew under `--chaos-timeslice-draw=run`, or took from the
    /// preemption record it replays. `hermit run` sets it, and `max_timeslice` to the same
    /// value, before the run starts; the run's own preemption record stores it. When set, no
    /// slice draws its mean from `--chaos-timeslice-range`.
    #[serde(default)]
    #[clap(skip)]
    pub chaos_timeslice: Option<NonZeroU64>,

    /// Uses the `--fuzz-seed` to generate randomness and fuzz nondeterminism in the futex semantics.
    #[clap(long)]
    pub fuzz_futexes: bool,

    /// Targeted chaos: bias scheduling toward known concurrency race patterns
    /// instead of exploring interleavings uniformly. At the scheduler's existing
    /// nondeterminism points it uses `--fuzz-seed` to (a) deliver a
    /// process-directed signal to a randomly chosen thread in the group (signal
    /// timing races) and (b) randomize the requeue position of a force-unblocked
    /// thread (lock-ordering / wakeup races). Only takes effect with `--chaos`;
    /// like the rest of chaos mode it remains reproducible under a fixed seed.
    #[clap(long)]
    pub chaos_target_races: bool,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1149)
    // TODO-HUMAN-REVIEW(PR-1151)
    /// Reproducible per-thread slowdown factors for chaos mode. A factor greater
    /// than one makes each RCB consume proportionally more virtual time, while a
    /// factor below one makes it consume less. Thus scheduling deadlines and the
    /// guest-visible virtual clock describe the same slowed execution rather than
    /// applying an out-of-band scheduling bias. The factor is a pure function of
    /// scheduler seed, stable deterministic thread id, and chaos epoch. A fixed
    /// seed therefore reproduces both timing and interleavings.
    #[clap(long)]
    pub chaos_per_thread_slowdown: bool,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1149)
    // TODO-HUMAN-REVIEW(PR-1151)
    /// Maximum ratio between the slowest and fastest per-thread slowdown factor
    /// for `--chaos-per-thread-slowdown`. Each thread's factor is drawn
    /// log-uniformly from `[1/R, R]` where `R` is this value. Must fit the Q32
    /// virtual-time representation and be `>= 1.0`; `1.0` disables the spread.
    #[clap(long, default_value = "10.0", value_name = "double")]
    pub chaos_slowdown_max_factor: f64,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    /// Length of a deterministic slowdown epoch in elapsed per-thread logical
    /// nanoseconds. At the first scheduler commit at or after each boundary the
    /// factor is redrawn as `factor(seed, stable_dettid, epoch)`. This is never
    /// wall time. `0` means one epoch for the entire run, making constant slowdown
    /// the single-epoch special case. Recorded preemption artifacts carry exact
    /// epoch transitions and factors for replay. Inert without chaos slowdown.
    #[clap(long, default_value = "0", value_name = "nanos")]
    pub chaos_epoch_length_ns: u64,

    /// Let a chaos preemption land up to this many instructions past the
    /// conditional branch its timeslice ends on. `0` (the default) is off.
    ///
    /// A chaos timeslice ends after a retired conditional branch, so a race
    /// window with no conditional branch in it is never split. With this set,
    /// each timeslice that ends on its timer is delivered `k` single steps
    /// past that branch instead, `k` drawn uniformly from `0..=N` for each
    /// timeslice from the seed, so a preemption can land inside straight-line
    /// code. A preemption record keeps each `k` and its branch count, and its
    /// replay stops at the same instruction. Each step is one ptrace stop, so
    /// a preemption costs up to `N` more. Needs `--chaos` and the ptrace
    /// backend's precise timers on x86_64; at most 256.
    #[serde(default)]
    #[clap(long, default_value = "0", value_name = "instructions")]
    pub chaos_instruction_offset: u64,

    /// Record the timing of preemption events for future replay or experimentation.
    /// This is only useful in chaos modes.
    #[clap(long)]
    pub record_preemptions: bool,

    /// Record and replay the complete slice-boundary protocol used by strict
    /// trace verification. This mode refuses mismatches, extra observations,
    /// incomplete replay and legacy schedule files; it does not resynchronize.
    /// Ordinary recording/replay and schedule editing retain their legacy mode.
    #[serde(default)]
    #[clap(long)]
    pub strict_schedule_boundaries: bool,

    /// File to write the record of preemptions (in JSON).  Implies `--record-preemptions`.
    #[clap(long, value_name = "filepath")]
    pub record_preemptions_to: Option<PathBuf>,

    /// JSON file to read recorded preemptions from.  When `--chaos` mode is activated, these
    /// recorded preemption points take the place of randomized scheduling decisions.
    #[clap(long, value_name = "filepath", conflicts_with = "replay_schedule_from")]
    pub replay_preemptions_from: Option<PathBuf>,

    /// File to read recorded schedule trace from. This execution will replay the schedule verbatim
    /// from the file.
    #[clap(
        long,
        value_name = "filepath",
        conflicts_with = "replay_preemptions_from"
    )]
    pub replay_schedule_from: Option<PathBuf>,

    /// If we run out of events while replaying a schedule, treat that as a fatal event and panic,
    /// rather than continuing execution.
    #[clap(long)]
    pub replay_exhausted_panic: bool,

    /// When playing a schedule trace from disk, bail out on the first time we desynchronize from
    /// the event sequence specified in the trace.
    #[clap(long)]
    pub die_on_desync: bool,

    /// The preemption record `--replay-preemptions-from` reads is not the recording of this
    /// run's schedule but an edited one, for example one `hermit analyze` knocked preemptions
    /// out of. A thread then can reach recorded slices that have already ended, which a faithful
    /// replay never does: the replay skips them and goes on with the first recorded slice still
    /// ahead, at its priority, instead of stopping the run. (At a thread's start the record's
    /// first priority, which the scheduler has already applied, holds until that slice ends.)
    #[serde(default)]
    #[clap(long, requires = "replay_preemptions_from")]
    pub replay_edited_preemptions: bool,
    /// Once the schedule replayed from `--replay-schedule-from` runs out,
    /// order the threads queued in turn at each priority by the virtual time
    /// each has run since then, least first, and equal run times by least
    /// recent selection, instead of in queue order. A thread's run time is
    /// what its own clock has advanced since that point; a thread seen for
    /// the first time, or runnable again after it was not, starts at no less
    /// than the least run time of the runnable threads. That is all it
    /// changes: a selected thread still runs until its slice ends, there is no
    /// preemption on wakeup, and threads pushed to the front of the queue (a
    /// timed wake) and pollers keep their places. Without it threads of equal
    /// priority take turns, so a thread that only computes or reads the clock
    /// keeps the CPU for a whole timeslice at each committing call of another.
    /// It reads nothing from the recording and draws nothing at random, so
    /// the rest of the run is as neutral as without it: what happens there
    /// depends only on the replayed prefix.
    #[clap(
        long,
        requires = "replay_schedule_from",
        conflicts_with_all = ["chaos", "replay_exhausted_panic"]
    )]
    #[serde(default)]
    pub replay_continue_fairly: bool,

    /// Given schedule events traced on recording or replaying, print the stack trace at the moment
    /// after the Nth event in the trace. Optionally, provide an output file into which the stack
    /// trace will be printed, otherwise it goes to stderr. With a recorded or replayed schedule, N
    /// counts every traced event, polling retries and events the schedule does not hold included,
    /// as the desync and missed-stack-trace lines do; the event number a schedule release reports
    /// leaves polling retries out, so it is not such an index. Under
    /// `--strict-schedule-boundaries`, N is instead a position in the complete boundary stream,
    /// `TimesliceEnd` records included: a replay requires it to lie inside the stored stream,
    /// while a recording counts the stream as it writes it.
    #[clap(long,
           short = 's',
           value_name = "index[,path]",
           value_parser = parse_index_with_path)]
    pub stacktrace_event: Vec<(u64, Option<PathBuf>)>,

    /// Internal, for `hermit analyze`: under `--replay-schedule-from`, each
    /// `--stacktrace-event` index names a position of the replayed schedule,
    /// not a traced event count. The stack prints when the replay consumes the
    /// recorded event at that position, however many events the run observed
    /// before it: a polling retry the recording did not make, say. A position
    /// at or past the schedule's end names the events the run makes after the
    /// replay consumed its last recorded event. A position the replay skips to
    /// resync, or drops with the rest of a released schedule, never prints and
    /// is reported as missed.
    #[serde(default)]
    #[clap(
        long,
        hide = true,
        requires = "replay_schedule_from",
        conflicts_with = "strict_schedule_boundaries"
    )]
    pub stacktrace_at_schedule_positions: bool,

    /// Internal feature used to signal the guest with SIGINT at every `--stacktrace-event`, this is
    /// in-lieu of using hermit's internal stacktrace printing facility, to instead have an external
    /// debugger handle it.  Accepts either signal names or numbers.
    #[clap(long, value_name = "signame")]
    pub stacktrace_signal: Option<SigWrapper>,

    /// Write an access trace to this path as the run records its schedule; its end line is written
    /// when the run ends, and a trace without it was cut short. The trace is JSON Lines with one
    /// record per syscall (its prehook and posthook positions in the recorded schedule, the thread,
    /// the thread's syscall count, the arguments, the result, and the resources it used: descriptor
    /// slots and the open files behind them, file contents, pipes, sockets, futex words, memory
    /// ranges, timers, threads and processes), one per signal delivery (its code, fault address and
    /// sender), and the first 256 bytes of each write to fd 1 or 2. It records resource accesses
    /// beside the schedule for inspection and comparison. An observer: it adds no scheduler request and
    /// moves no virtual time, so the schedule and the guest's results are those of a run without
    /// it, except that Hermit's own command line, which a guest can read as PID 1's, shows the
    /// option, and that the trace file itself is inside the container: it exists from before the
    /// guest's first instruction and grows during the run, so a guest that lists or watches its
    /// directory sees it. Needs a recorded schedule (`--record-preemptions-to` or `--record-preemptions`),
    /// whose positions the records name, sequentialized threads and the ptrace backend. The format
    /// is `detcore_model::access_trace`.
    #[serde(default)]
    #[clap(long = "record-access-trace-to", value_name = "path")]
    pub access_trace: Option<PathBuf>,

    /// With `--record-access-trace-to`, leave out the records whose last schedule event comes
    /// before this position. 0, the default, keeps every record. Resource state from before
    /// the position, such as which open file a descriptor number names, is then only partly
    /// visible in the trace.
    #[serde(default)]
    #[clap(long, value_name = "event", default_value = "0")]
    pub access_trace_from: u64,

    /// With `--record-access-trace-to`, write at most this many records from
    /// `--access-trace-from` on. Past it the trace stops, without its end line, so a reader
    /// sees it cut short; a run whose records all fit still ends with it. Unlimited by
    /// default.
    #[serde(default)]
    #[clap(long, value_name = "records")]
    pub access_trace_max_records: Option<u64>,

    /// With `--record-access-trace-to`, write at most this many bytes of header and records.
    /// The record that would pass it is not written: the trace stops there, without its end
    /// line, so a reader sees it cut short, and Hermit says so once on stderr. Defaults to
    /// 1073741824 (1 GiB, `detcore_model::access_trace::ACCESS_TRACE_DEFAULT_MAX_BYTES`), about
    /// three million records at their typical 350 bytes, so a long run cannot fill the disk by
    /// default; pass a larger count for a longer trace.
    #[serde(default)]
    #[clap(long, value_name = "bytes")]
    pub access_trace_max_bytes: Option<u64>,

    /// Report each access to these guest bytes, through a hardware watchpoint, as a `watch_hit`
    /// record in the access trace (`--record-access-trace-to`): the thread, the instruction after
    /// the access, the event it follows and, for a thread's first 64 hits, its stack. Repeat it for
    /// up to the backend's watchpoint count (4 on x86-64). ADDR is a guest virtual address (hex
    /// with 0x, or decimal), LEN 1, 2, 4 or 8 bytes with ADDR aligned to it (8 by default), and w
    /// reports writes (the default), rw reads and writes. Every thread arms its watches at its
    /// first stop from `--watch-from` on at which it does not block SIGTRAP; the trace's
    /// `watch_summary` lines say which threads armed, when, and how many hits each had. A hit stops
    /// the thread in the tracer only: it adds no scheduler request and moves no virtual time, so
    /// the schedule and the guest's results are those of the run without it. Needs the ptrace
    /// backend's hardware watchpoints and a precise timer whose overflow records are mapped.
    #[serde(default)]
    #[clap(long = "watch", value_name = "addr[:len][:w|rw]")]
    pub watch: Vec<crate::watch::WatchArg>,

    /// With `--watch`, arm the watches only once the run has recorded this many schedule events
    /// (0, the default, arms them from the start).
    #[serde(default)]
    #[clap(long, value_name = "event", default_value = "0")]
    pub watch_from: u64,

    /// **Deprecated:** Print a stacktrace each time the program is preempted.  Only makes sense in `--chaos` mode
    /// and typically goes with preemption recording/replaying.
    #[clap(long)]
    pub preemption_stacktrace: bool,

    /// File to write preemption stacktraces to. Implies `--preemption-stacktrace`. If a
    /// log file is not specified, preemption stacktraces are printed to stderr by default.
    #[clap(long, value_name = "filepath")]
    pub preemption_stacktrace_log_file: Option<PathBuf>,

    /// Enable deterministic IO by reassuring we always read/write the maximum possible bytes
    /// from IO syscalls. There might be cases that read/write syscalls return less bytes than
    /// requests. Detcore, makes an effort to request additional bytes until we reach the ones
    /// requested or EOF.
    #[clap(long)]
    pub deterministic_io: bool,

    /// Fail immediately on unsupported syscalls instead of forwarding them.
    /// Ordinary `hermit run` enables this policy; compatibility requires the
    /// explicit `--allow-unsupported-syscalls` opt-out.
    #[clap(long)]
    pub panic_on_unsupported_syscalls: bool,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-644): Review backend-safe fail-closed termination.
    /// Return a typed Tool error instead of unwinding through a backend callback.
    #[serde(default)]
    #[clap(skip)]
    pub exit_on_unsupported_syscall: bool,
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-644): Review process-tree shutdown for ptrace fail-closed mode.
    /// Terminate the whole tracer when an unsupported syscall is observed.
    #[serde(default)]
    #[clap(skip)]
    pub shutdown_on_unsupported_syscall: bool,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-644): Review the internal cross-process warning report channel.
    /// Internal inherited file descriptor used to aggregate unsupported syscalls.
    #[serde(default)]
    #[clap(skip)]
    pub unsupported_syscall_report_fd: Option<i32>,

    /// Panic when a precise PMU timer overshoots its expected RCB target instead of logging an
    /// error and continuing through normal timer handling. Intended for Detcore debugging.
    #[serde(default)]
    #[clap(
        long = "panic-on-rbc-overshoot",
        visible_alias = "panic-on-rcb-overshoot"
    )]
    pub panic_on_rcb_overshoot: bool,

    /// **Internal:** Set to `true` if we're inside a UTS namespace.
    // FIXME: This can be removed once spawn_fn-based tests support namespaces.
    #[clap(skip)]
    pub has_uts_namespace: bool,

    /// **Internal:** Print a one-time notice on Hermit's standard error when the guest binds an
    /// abstract `AF_UNIX` name, or fails to with `EADDRINUSE`. `hermit run` sets it for a run that
    /// asked for `--network=host` on the ptrace backend: the guest then shares the host's network
    /// namespace, and with it the abstract names of every program on the host, other Hermit runs
    /// included, and Detcore runs in Hermit's own process, so the notice reaches Hermit's stderr
    /// and never the guest's. Nothing the guest observes depends on it. The DBT backend's runtime
    /// reads the frozen legacy form ([`to_legacy_backend_json`]), which leaves this field out.
    #[serde(default)]
    #[clap(skip)]
    pub notice_host_network_abstract_binds: bool,

    /// How the guest's process and thread IDs are numbered
    /// ([`GuestIds`](crate::pid::GuestIds)). The host sets it, not a flag:
    /// `hermit run` from `--no-namespace` and `--pid-base`. Recorded schedules
    /// store it, and a replay under another numbering is refused.
    #[serde(default)]
    #[clap(skip)]
    pub guest_ids: crate::pid::GuestIds,

    /// **Internal:** For each of the guest's standard streams 0, 1 and 2, the lowest of them that
    /// is one open file description with it, as `kcmp(KCMP_FILE)` answered for the descriptors
    /// the launcher hands the guest (Hermit's KVM launch, where the guest's output is not
    /// captured: the stdin it reserved, and its own standard output and error, which the
    /// executor passes through). `None` where that is not known (any other launch, a captured or
    /// replaced stream, or a kernel that refuses kcmp): each stream is then a description of its
    /// own. Detcore's lock ledger takes the streams of one group as the one description they are
    /// (package K slice K8-h). The backend's own answer, a provider query naming which guest
    /// descriptors are one host description, would also cover descriptors no launcher hands
    /// over; no backend offers one yet. The DBT backend's runtime reads the frozen legacy form
    /// ([`to_legacy_backend_json`]), which leaves this field out.
    #[serde(default)]
    #[clap(skip)]
    pub standard_stream_aliases: Option<[u8; 3]>,

    /// **Internal:** Path to the replay data folder.
    #[clap(skip)]
    pub replay_data: Option<PathBuf>,

    /// Kill all remaining tasks iff daemons are the only ones left.
    /// Disabled by default.
    #[clap(long)]
    pub kill_daemons: bool,

    /// Serve the guest to one gdb client on 127.0.0.1:`--gdbserver-port`, and
    /// wait for it before the guest runs; attach with
    /// `gdb -ex 'target remote 127.0.0.1:PORT' PROGRAM`. The port is bound in the
    /// host's network namespace, so the guest keeps the network `--network`
    /// gives it, and a `--strict` run can be debugged with every strict refusal
    /// in force. The debugger is an input of the run: breakpoints, writes to the
    /// guest's memory or registers and the signals it sends all change it. With
    /// a gdbserver, Detcore also takes no reports of the guest's signal-handler
    /// returns, so a multithreaded guest whose handlers take scheduler turns can
    /// be scheduled differently than without one (docs/SIGNAL_STATE_CHECK.md).
    /// Only the ptrace backend (the default, and `--backend=e9patch`, whose
    /// rewritten guest runs under it) serves gdb: `--backend=kvm`, `dbt`,
    /// `sabre`, `liteinst` and `in-guest-trap`, and `--namespace-only`, refuse
    /// `--gdbserver` and `--dap-listen`.
    #[clap(long)]
    pub gdbserver: bool,
    /// The loopback TCP port `--gdbserver` serves on; 0 lets the system pick a
    /// free one, which the run prints.
    #[clap(long, value_name = "uint16", default_value = "1234")]
    pub gdbserver_port: u16,

    /// Configure the maximum time a guest thread may run without returning to Detcore. This is
    /// measured in virtual nanoseconds and enforced with retired conditional branch (RCB)
    /// counting. `--preemption-timeout` is retained as a deprecated alias.
    ///
    /// Set this to `disabled` or `0` to disable PMU-backed preemption. Positive values must be at
    /// least one RCB (10 virtual nanoseconds at the default clock multiplier) and require
    /// user-space hardware performance counters.
    ///
    /// Small values are expensive. A thread that computes without returning to Detcore is stopped
    /// by a PMU timer event at the end of every timeslice, and on the ptrace backend a precise
    /// timer (the default) is placed by stepping the guest through up to the last skid margin of
    /// the slice: 100 to 10000 RCBs depending on the processor, or the `--skid-margin` override. A
    /// slice no longer than the margin is stepped through whole. A CPU-bound guest can run
    /// thousands of times slower than at the default (a pipeline test took 0.3 s at the default and
    /// 994 s at 1000), and a `--timeout` that expires may mean a slow run rather than a hung one.
    /// `hermit run`, `hermit analyze` and `hermit bisect` say so on stderr when a slice is at most
    /// ten skid margins long, so that at least a tenth of it is stepped (1000000 virtual
    /// nanoseconds at the default clock multiplier on a processor with a 10000-RCB margin); a
    /// preemption replay, whose recording sets the slices, and `--imprecise-timers`, which steps
    /// nothing, say nothing. `--summary` reports how many timeslices the run had.
    #[serde(alias = "preemption_timeout")]
    #[clap(
                long,
                visible_alias = "preemption-timeout",
                value_name = "uint64|'disabled'",
                default_value = "200000000",
                value_parser = parse_timeslice)]
    pub max_timeslice: MaybeTimeslice,

    /// Target logical timeslice, in virtual nanoseconds, checked at syscall boundaries and the
    /// other points where the guest returns control to Detcore: signals, timer events, and
    /// trapped `rdtsc` and `cpuid` instructions unless `--target-timeslice-syscalls-only` is given.
    /// This avoids PMU preemption for workloads that enter the kernel frequently. Omit this option
    /// to use only `--max-timeslice`.
    #[serde(default)]
    #[clap(long, value_name = "virtual-nanoseconds")]
    pub target_timeslice: Option<NonZeroU64>,

    /// Do not end an expired `--target-timeslice` at a trapped `rdtsc` or `cpuid` while a PMU
    /// maximum (`--max-timeslice`) is armed; end it at the next syscall, signal or timer event,
    /// or at the PMU maximum. A trapped instruction can sit inside a user-space spinlock: QEMU's
    /// multi-threaded TCG reads the host TSC while holding its `vm_clock_lock`, and a vCPU that
    /// yields there leaves the other vCPUs spinning for whole PMU slices. Has no effect without
    /// `--target-timeslice`, or with `--max-timeslice disabled`, where the trap is the only way
    /// out of a busy-wait on the TSC.
    #[serde(default)]
    #[clap(long, requires = "target_timeslice")]
    pub target_timeslice_syscalls_only: bool,

    /// After a `FUTEX_WAKE` or `FUTEX_WAKE_BITSET` that woke at least one waiter, move the waker to
    /// the back of its priority band and have it yield, so that woken threads of its priority run
    /// before it continues. Without
    /// it, a thread that releases a contended lock keeps running until its slice ends and can
    /// re-take the lock many times before the waiter is scheduled: QEMU's multi-threaded TCG vCPUs
    /// starve its main loop of the big QEMU lock this way. The yield is a scheduler turn of its
    /// own: the waker's timeslice keeps its deadlines. Each extra thread switch is charged
    /// scheduler time that the guest can observe (see `--scheduler-turn-cost`), and costs wall
    /// time: a two-vCPU QEMU boot took 5% to 48% longer with it. The waker keeps its priority, so a
    /// woken thread of a lower priority still runs after it; `--chaos` redraws threads' priorities
    /// from their chaos PRNG, which can put a woken thread in a lower band; and a random
    /// `--sched-heuristic` does not select first in, first out. In those cases a woken thread is not
    /// guaranteed to run first. Applies to the precise futex mode while threads are sequentialized,
    /// on the ptrace backend (including e9patch preprocessing, which runs on it); replay applies it
    /// as the recorded run did.
    #[serde(default)]
    #[clap(long)]
    pub futex_wake_yields: bool,

    /// Hand the turn to a thread polling a pipe or FIFO once another thread's action has readied it
    /// (<https://github.com/rrnewton/hermit/issues/4380>), within this many virtual nanoseconds of
    /// the acting thread's own time. A write that moves bytes into a scheduler-managed pipe, a read
    /// that makes room in one, an explicit close of one end, or an open of a FIFO end readies the
    /// threads polling for that: the scheduler restores their priority, and once the acting
    /// thread's time is this far past its first readying action, at the next point where it
    /// returns to Detcore (a syscall's entry or exit, or a signal or trapped instruction), it is
    /// moved behind them and yields, so they run before it continues. A poller is restored to its
    /// own priority and the acting thread moves only within its band, so where `--chaos` has drawn
    /// a poller a lower priority than the acting thread, that poller still runs after it and is
    /// not guaranteed to run first. `0` hands off at the end of
    /// the readying syscall. Unset (the default), nothing changes: a polled thread whose peer is
    /// ready waits for the acting thread's slice to end, as before. Each hand-off adds at least two
    /// charged scheduler turns that the guest can observe as time (see `--scheduler-turn-cost`).
    /// A thread that computes after its write without returning to Detcore delays the hand-off
    /// until it next does, or its slice ends; an implicit close (an exit, an `O_CLOEXEC` exec)
    /// readies nothing; and an end reopened by path (`/proc/self/fd/N`, as `/dev/fd/N` process
    /// substitution does) is a channel of its own, so the other side's actions do not ready it. Recorded
    /// schedules store the value, and a replay under another value is refused. Refused without
    /// sequentialized threads, on the DBT, KVM and LiteInst backends, with `--namespace-only`,
    /// `--strace-only` or `--debug-externalize-sockets`, and
    /// under a `--sched-heuristic` that does not select by priority; never applied in record mode.
    #[serde(default)]
    #[clap(long, value_name = "nanos")]
    pub pipe_wake_latency: Option<u64>,

    /// How a run of consecutive scheduler turns that each end an expired timed wait (a futex wait
    /// answered `ETIMEDOUT`, or a sleep already due) is charged while another thread is queued:
    /// `once`, one scheduler turn for the whole run, or `each`, a turn per wait. Unset, `each`
    /// under `--chaos` or a `--sched-heuristic` that does not select by priority, where the time
    /// each charge adds stretches short waits as race search relies on, and `once` otherwise, so
    /// that a thread polling with waits shorter than a turn charge blocks and lets other threads
    /// run. A replay of a schedule or of preemptions that leaves this unset takes the value its
    /// record stores (a record that stores none, written before the option existed or by an
    /// `each` run, charged each wait), and an explicit value that disagrees with the record is
    /// refused. `hermit analyze` and `hermit bisect` run every trial under the value of the
    /// recording they start from, or `each` when analyze records its own chaos target. Under
    /// `hermit record` and `hermit replay` every wait is charged, whatever the value.
    #[serde(default)]
    #[clap(long, value_enum, value_name = "each|once")]
    pub expired_wait_charge: Option<ExpiredWaitCharge>,

    /// When a queued plain sleep (a request for `SleepUntil` alone, not yet due) becomes a timed
    /// waiter: `first-pass`, at the first quiescent scheduler pass that finds it queued, whatever
    /// the priorities, as a Linux hrtimer is armed when the sleep starts; or `when-chosen`, only
    /// when its thread is chosen before its deadline, so a sleeper behind threads of better
    /// priority wakes later. Unset, `when-chosen` under `--chaos` or a `--sched-heuristic` that
    /// does not select by priority, where those late wakes are what real sleep-ordered races
    /// depend on, and on a backend whose sleepers' signal mask and dispositions the scheduler
    /// does not read (DBT, SaBRe and LiteInst without the in-guest delivery gate, KVM), and
    /// `first-pass` otherwise. On such a backend a sleep parked at the first pass would be ended
    /// by any signal the scheduler sends it, including one Linux ignores or the sleeper blocks,
    /// so `first-pass` there, asked for or taken from a record, is refused by name before the
    /// guest starts. A replay of a schedule or of preemptions that
    /// leaves this unset takes the value its record stores (a record that stores none, written
    /// before the option existed or by a `when-chosen` run, registered sleeps when chosen), and an
    /// explicit value that disagrees with the record is refused. `hermit analyze` and `hermit
    /// bisect` run every trial under the value of the recording they start from, or
    /// `when-chosen` when analyze records its own chaos target. Under `hermit record` and
    /// `hermit replay` a sleep is registered when chosen, as before, whatever the value.
    #[serde(default)]
    #[clap(long, value_enum, value_name = "first-pass|when-chosen")]
    pub sleep_registration: Option<SleepRegistration>,

    /// Write a work/span report of the run's schedule to this path as JSON when the run ends
    /// normally: work (the sum of every thread's guest CPU time, in virtual nanoseconds), a
    /// lower span over the scheduler's certain happens-before edges (program order, spawn, a
    /// child's exit to the wait that reaps it, conflicting resources), and an upper span that
    /// also orders a thread that waited after everything other threads did meanwhile. The
    /// report lists the dependencies it does not model: what a wait waited for and who sent a
    /// signal, so the lower span can be optimistic; and what passes without a wait (data, an end
    /// of file or a peer's close found on a channel, a pending signal taken by `sigtimedwait`,
    /// shared memory, files and devices), which neither span orders; and the CPU time of a call a
    /// thread was running outside the schedule when it was killed. An observer: no scheduling
    /// decision reads it, so the schedule
    /// and the guest's results are those of a run without it, except that Hermit's own command
    /// line, which a guest can read as PID 1's, shows the option. Needs sequentialized threads
    /// and the ptrace backend. Unset (the default), nothing is gathered.
    #[serde(default)]
    #[clap(long, value_name = "path")]
    pub work_span_report: Option<std::path::PathBuf>,

    /// Virtual nanoseconds that each committed scheduler turn adds to the global clock, before
    /// `--clock-multiplier` (default 500000). The charge keeps time moving while threads take
    /// turns without running. Every thread observes it as a jump in time across a thread switch,
    /// so a guest that times short intervals while another thread runs, such as a QEMU guest
    /// kernel calibrating its TSC against the emulated PIT, can see a gap far larger than any
    /// work done. A smaller charge makes those jumps smaller; a finite poll or futex timeout then
    /// takes more turns to expire. The charge, after the multiplier, must be between 1 and
    /// 1000000000. If a charged turn would pass the end of representable virtual time, the run
    /// stops with HERMIT_VIRTUAL_CLOCK_EXHAUSTED and exit status 125.
    #[serde(default)]
    #[clap(long, value_name = "virtual-nanoseconds")]
    pub scheduler_turn_cost: Option<NonZeroU64>,

    /// Charge a scheduler turn that ends a timeslice because its time ran out (a preemption)
    /// at most the virtual time its thread ran since its previous turn, instead of the full
    /// turn charge (`--scheduler-turn-cost`, default 500000 virtual nanoseconds). Without it,
    /// every preemption adds the full charge however short the slice was, so a small
    /// `--max-timeslice`, or a short `--chaos` draw, multiplies the guest-visible time of
    /// CPU-bound work by about `1 + charge / slice`: 501 times at `--max-timeslice=1000`,
    /// enough for a test's own timeouts to expire before it reaches the code under test. With
    /// it the factor is at most 2, what a slice one full charge long already costs. A
    /// preemption whose thread ran at least one full charge since its previous turn is
    /// charged as before; a thread's turns include its syscalls, so one that follows a syscall
    /// by less is charged only the time since that syscall, however long the slice. A capped
    /// turn is charged at least one virtual nanosecond, unless its full charge after
    /// `--clock-multiplier` is zero. An explicit `sched_yield`, and every
    /// turn that does not end a timeslice, keep the full charge. This changes what the guest
    /// reads from `clock_gettime(CLOCK_MONOTONIC)` and the other virtual clocks, and so when
    /// its timers and timeouts expire: recorded schedules store the setting, and a replay
    /// under the other is refused.
    /// Refused where no timeslice ends by expiry (`--max-timeslice=disabled` without
    /// `--target-timeslice`) or no Detcore scheduler orders the threads (`--namespace-only`,
    /// `--strace-only`, `--no-sequentialize-threads`), and with the DBT backend; never applied
    /// in record mode.
    #[serde(default)]
    #[clap(long)]
    pub cap_preemption_charge: bool,

    /// End the run in virtual time: once the run's virtual clock is more than this many virtual
    /// nanoseconds past the run's start, Hermit grants no further scheduler turn and the run ends
    /// with exit status 124 and the stderr line `HERMIT_RUN_TIMEOUT class=virtual-timeout
    /// bound_ns=N`. The clock is Detcore's global virtual time since the run began, the same
    /// virtual time `--summary` reports as elapsed (`virttime_elapsed` in `--summary-json`); it is
    /// checked each time the scheduler commits it at the start of a turn, before the turn's thread
    /// runs, and the check never moves it. A guest that has already exited, or whose remaining
    /// processes a committed exit, kill, in-flight exec or fatal signal has marked doomed, is not
    /// stopped by it. A failed exec clears its in-flight mark before the old image resumes.
    /// Process-group SIGKILL marks are conservative: if a marked process survives a failed group
    /// kill, the bound does not stop it. Unlike `--timeout`, a
    /// wall-clock bound in seconds that fires wherever the guest has got to when the host's clock
    /// runs out (`class=run-timeout`), for a guest whose input all comes from inside the
    /// container this bound fires at the same point of the same schedule on every host and in
    /// every run, so the output written before it, `--summary` and a `--verify` comparison are
    /// reproducible. A stopped run is not a guest result, so `--verify` compares it only under
    /// `--verify-allow=failure` or `both`; the default `success` refuses run 1, as it refuses a
    /// deadlock under Hermit's model. A thread that polls something outside the container, such
    /// as a pipe a host process feeds, advances virtual time with each retry, and the number of
    /// retries depends on the host, so such a run can stop at another turn; `--verify
    /// --verify-strict` reports that as a divergence. Virtual time does not pass while every thread is blocked in a call
    /// outside the container, so bound such a wait with `--timeout`. On the ptrace backend the
    /// run is then finished as for a guest that exited: its output is kept and `--summary` says
    /// the bound fired. On the kvm backend the run exits 124 without a summary, and `--verify`
    /// with the bound is refused there. Needs sequentialized threads and virtual time: refused
    /// with `--no-sequentialize-threads`, `--namespace-only`, `--strace-only` or
    /// `--no-virtualize-time`, and on the dbt, liteinst, in-guest-trap and sabre backends, where
    /// it has not been qualified. Also refused before guest or debugger startup with
    /// `--gdbserver` or `--dap-listen`: a debugger can suppress a fatal signal after Detcore
    /// marks its process doomed, leaving a survivor outside the bound. Remove the debugger
    /// option or the virtual bound, or use `--timeout` for a wall-clock bound instead.
    #[serde(default)]
    #[clap(long, value_name = "virtual-nanoseconds")]
    pub virtual_timeout: Option<NonZeroU64>,

    /// Shut down immediately upon SIGINT, rather than letting the guest handle it.
    #[clap(long)]
    pub sigint_instakill: bool,

    /// Warn if binds are non-zero.
    #[clap(long)]
    pub warn_non_zero_binds: bool,

    /// Apply a specialized scheduling heuristic which may help exercise certain bugs.
    #[clap(long, default_value = "none", value_name = "str")]
    // TODO: Rename this to scheduler_strategy?
    pub sched_heuristic: SchedHeuristic,

    /// Use this number to seed the PRNG that supplies randomness to the scheduler.
    #[clap(long, env = "HERMIT_SCHED_SEED", value_name = "uint64")]
    pub sched_seed: Option<u64>,

    /// External network record or replay, set by `hermit run --record-networking`
    /// or `--replay-networking`; `Off` otherwise, in which case Detcore never
    /// consults it. Its perturbation seed has no fallback to `seed` or
    /// `sched_seed`.
    #[serde(default)]
    #[clap(skip)]
    pub network_trace: NetworkTraceConfig,

    /// Configure the probability for the Sticky Random scheduler to stay in a thread.
    /// For value 0.0, we are behaving like Random.
    /// For value 1.0, we are behaving like a DFS, where the same thread is
    /// always picked as long as it is available in the Run queue. After
    /// this thread is exhausted, the next thread will be chosen randomly.
    /// For value 0.5, we have a 50/50 chance to pick the same thread.
    #[clap(long, default_value = "0.0", value_name = "double")]
    pub sched_sticky_random_param: f64,

    /// **Internal:** An internal flag for indicating to Detcore whether we are in `hermit record` or
    /// `hermit replay` mode.  This is necessary because there are DIFFERENT global
    /// invariants in record mode (e.g. files dont exist).  If we move to a chroot model
    /// and reproduce more, recording less, then this flag should become obsolete.
    #[clap(skip = false)]
    pub recordreplay_modes: bool,

    /// **Internal:** Set only by `hermit replay`, alongside `recordreplay_modes`. Replay serves
    /// socket reads, writes, accepts and connects, and `poll`, `ppoll`, `select` and
    /// `pselect6`, from the recording, so they cannot block. They still run as backgrounded
    /// blocking I/O, as during recording, but Detcore's scheduler commits no other turn until
    /// they finish. Otherwise the descriptor-table changes of different threads could
    /// interleave differently from one replay to the next.
    ///
    /// It never enters the legacy form ([`to_legacy_backend_json`]), so a standalone DBT
    /// run's `HERMIT_DBT_DETCONFIG` reads it as false. Record and replay run only on the
    /// ptrace backend.
    #[serde(default)]
    #[clap(skip = false)]
    pub replaying: bool,

    /// **Internal:** debugging option to stop execution after a specific scheduler commit, aka turn number
    /// (non-negative integer). This only makes sense if `--sequentialize-threads` is specified, as the scheduler is otherwise not engaged.
    #[clap(long, value_name = "turn_N")]
    pub stop_after_turn: Option<u64>,

    /// **Internal:** debugging option to stop execution after a scheduler loop iteration (non-negative integer).
    /// This only makes sense if `--sequentialize-threads` is specified, as the scheduler is otherwise not engaged.
    #[clap(long, value_name = "iter_N")]
    pub stop_after_iter: Option<u64>,

    /// Send SIGNAL to the run's root process once the scheduler has committed
    /// turn N, as a timer expiry is sent (`turn=N:SIGNAL`, SIGNAL a name such
    /// as `SIGUSR1` or a number, not SIGKILL or SIGSTOP). It is sent before
    /// the next turn is selected, after the first `COMMIT turn M` with M >= N:
    /// turn numbers also count the turns the scheduler skips, so if turn N
    /// was skipped, after the next commit. The turn is a deterministic point
    /// of the run, so a process in the run that waits for the signal acts at
    /// the same point every time: for example a controller that stops QEMU and
    /// saves a snapshot there. The run itself goes on. Only with
    /// `--sequentialize-threads`, where the scheduler numbers turns.
    #[clap(long, value_name = "turn=N:SIGNAL", value_parser = parse_signal_at)]
    #[serde(default)]
    pub signal_at: Option<SignalAt>,

    /// The slice bound, in virtual nanoseconds, once the guest has crashed:
    /// `off` (the default) keeps `--max-timeslice` to the end; `default` is
    /// the default `--max-timeslice` (200000000); a number is that bound.
    ///
    /// A crash is a fatal signal on its way to ending a process: SIGABRT
    /// however it was sent, or SIGSEGV, SIGBUS, SIGILL, SIGFPE or SIGSYS
    /// raised by the kernel for a fault. One whose delivery takes the default
    /// action switches at once. One the guest catches arms the switch, which
    /// is taken when a process the crashed one created meanwhile (or the
    /// crashed process itself) execs while the handler still runs (Android's
    /// crash handler runs `crash_dump` that way), and dropped when the handler
    /// returns (a runtime that turns a fault into an exception, as ART and
    /// JVMs do for null checks). A handler left with a plain `longjmp`, which
    /// keeps the signal blocked, counts as still running. From the switch on,
    /// the slices of the crashed process and of the processes it creates are
    /// bounded by this value instead of the run's, so a run whose race needs
    /// fine slices does not spend its time slicing the crash dumper; other
    /// processes (a harness running trials as children) keep the run's bound.
    /// The switch happens at the same point of every run and replay that
    /// reaches it, and a preemption record stores both the setting and where
    /// it switched; a replay takes the record's setting, and refuses another
    /// (it cannot turn a recorded switch off). A no-op unless the run's own
    /// bound is below this one. `--summary` reports it.
    #[clap(
        long,
        value_name = "uint64|'default'|'off'",
        default_value = "off",
        value_parser = parse_post_crash_timeslice
    )]
    #[serde(default)]
    pub post_crash_timeslice: MaybeTimeslice,

    /// **Internal:** Debugging option to treat all sockets as mysterious external, nondeterministic
    /// entities, rather than container-internal and determinstically scheduled.
    #[clap(long)]
    pub debug_externalize_sockets: bool,

    /// **Internal:** Debugging option to change how futexes are implemented, either precisely modeled
    /// by hermit, by polling the kernel with non-blocking futex operations, or treated as external
    /// (nondeterministic) operations which unblock at imprecise times.
    #[clap(
        long,
        value_name = "precise|polling|external",
        default_value = "precise"
    )]
    pub debug_futex_mode: BlockingMode,

    /// Do not count the retired conditional branches (RCBs) of each thread towards its logical
    /// time.  Instead, count each checkin with the scheduler as a fixed increment to logical time.
    /// Even when this option is set, HW RCB performance counters may still be enabled if a
    /// max-timeslice is specified.
    #[clap(long)]
    pub no_rcb_time: bool,

    /// An option to enable logging the hash of heap memory maps for the purpose of determinism checking
    #[clap(long)]
    pub detlog_heap: bool,

    /// An option to enable logging the hash of stack memory maps for the purpose of determinism checking
    ///
    /// THIS HASH COVERS argv AND THE ENVIRONMENT, which the kernel places at the
    /// top of the initial process stack. Two runs whose command lines differ by a
    /// single character therefore produce different stack hashes from the first
    /// sample, even when the command lines are the same LENGTH and every stack
    /// address matches. Measured: equal-length-but-different argv diverged the
    /// hash 14 records in, while byte-identical argv held it for 5023 records.
    ///
    /// Holding a run-directory name to a fixed WIDTH is a sufficient control when
    /// only addresses matter, and is NOT sufficient here. Comparing two runs
    /// under this flag requires byte-identical argv and environment; otherwise
    /// the first divergence you find is your own input.
    #[clap(long)]
    pub detlog_stack: bool,

    /// Log a hash of every private, writable anonymous mapping at each syscall, for memory
    /// determinism checking: allocator arenas (scudo's, jemalloc's, glibc's mmap chunks), thread
    /// stacks and TLS blocks, none of which `--detlog-heap` (the brk heap) or `--detlog-stack`
    /// (the main thread's stack) covers. A mapping named by `PR_SET_VMA_ANON_NAME`
    /// (`[anon:<name>]`) counts as anonymous. `--detlog-anon=pages` adds a short digest for each
    /// 4 KiB page that is not all zero, to find the page where two runs part.
    ///
    /// ptrace backend only (also with e9patch preprocessing): the mappings come from the guest
    /// task's own `/proc/<tid>/maps`, which an in-guest tool shares and a VMM's does not hold.
    /// Other backends refuse the flag. EXPENSIVE: every byte of every such mapping is read at
    /// every syscall, in 1 MiB pieces, so a guest with a large heap pays for its whole size each
    /// time; a mapping over 256 MiB (a sanitizer's shadow, a heap reserved up front) is recorded
    /// by its size alone. Reading an untouched page maps the shared zero page there, which
    /// `mincore` then reports resident, as with `--detlog-heap` and `--detlog-stack`. A thread
    /// stack holds thread IDs and, below its stack pointer, the scratch Hermit writes when it
    /// injects a call, so runs whose IDs differ differ there legitimately. Page indices count
    /// from the mapping's start, so a mapping that merges or moves renumbers its pages.
    #[clap(
        long,
        value_enum,
        value_name = "pages",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "mappings"
    )]
    #[serde(default)]
    pub detlog_anon: Option<DetlogAnon>,

    /// Log a hash of the guest REGISTER FILE at guest-logical-control points, for determinism
    /// checking. stdout, the INFO log, the stack and the heap are all hashed today; the register
    /// file is not, so two backends can differ in register state and every existing check still
    /// reports parity.
    ///
    /// SAMPLED ONLY AT GUEST-LOGICAL-CONTROL POINTS -- see `Detcore::detlog_registers`. Registers
    /// are NOT sampled inside a tool handler: a backend running its handler in-guest executes code
    /// the ptrace reference never executes, so a difference there is correct behaviour, not a
    /// determinism bug.
    #[clap(long)]
    pub detlog_regs: bool,

    /// Log a hash of each syscall's OUTPUT BUFFER, taken at the syscall boundary from the
    /// address and length in the syscall's own arguments.
    ///
    /// WHAT IT SEES THAT THE MAPPING HASHES DO NOT. `--detlog-heap` and `--detlog-stack` hash a
    /// whole named mapping, so their coverage is decided by where the guest happened to ALLOCATE
    /// a buffer. Measured, three runs per cell, same netlink exchange with only the receive
    /// buffer's home changed: a `[stack]` buffer is missed by `--detlog-heap`, a `[heap]` buffer
    /// is missed by `--detlog-stack`, and a BSS/static or anonymous-`mmap` buffer is missed by
    /// BOTH even with both enabled. Anonymous `mmap` is where glibc puts any `malloc` above the
    /// 128 KiB `M_MMAP_THRESHOLD`. Reading the extent out of the syscall arguments makes the
    /// buffer's home irrelevant.
    ///
    /// WHY IT IS NOT REDUNDANT WITH `--verify`. A syscall whose buffer is a bare pointer in
    /// Reverie prints the ADDRESS, not the contents, so a `recvmsg` returning a stable
    /// `Ok(1468)` whose payload varies produces a character-identical record and `--verify`
    /// reports `bitwise_parity: true`. 44.1% of the syscalls in a QEMU/Linux boot move bytes
    /// through such a buffer.
    ///
    /// COST is proportional to bytes actually moved, NOT to syscall count or mapping size:
    /// ~0.75 s per GB of guest I/O. A QEMU/Linux boot moves 139.1 MB through these buffers,
    /// against the 10.9 TB `--detlog-heap` hashes over the same run.
    ///
    /// NAME IS PROVISIONAL: `io-buffers` is the owner's candidate and is not settled.
    ///
    /// ON BY DEFAULT. It was opt-in until 2026-08-24, and opt-in made the
    /// determinism gate weaker than its name: with the hash absent, the netlink
    /// `recvmsg` above compares equal and `--verify` reports success. A check
    /// that must be requested is not a standard. The opt-out exists for the
    /// deliberate case (bulk I/O where the cost matters and content parity is
    /// not the question), not as the ordinary setting.
    ///
    /// COST OF THE DEFAULT, measured 2026-08-24 on a 316-core x86_64 Linux
    /// build host: a typical small test guest pays about ONE MILLISECOND
    /// (`/bin/true` 0.029s -> 0.030s, `/bin/ls` 0.041s -> 0.041s, 8 runs each).
    /// 64 MiB through `cat` costs +0.07-0.10s in a RELEASE build, which is the
    /// ~1.1-1.6 s/GB matching the figure quoted above. The same workload in a
    /// DEBUG build costs +3.4s, roughly 50x more, because the hash loop is
    /// unoptimized -- so a debug-built node moving tens of megabytes is the one
    /// place the default is felt.
    #[clap(long = "no-detlog-io-buffers", action = clap::ArgAction::SetFalse)]
    pub detlog_io_buffers: bool,

    /// Sampling cadence for `--detlog-regs`: hash every Nth guest-logical-control point.
    ///
    /// COST TIER. 1 (the default) is the FULL tier -- every control point hashed -- and is what a
    /// short test should use. Measured cost at this scale is within run-to-run noise: /bin/true
    /// (49 control points), `wc -l /etc/passwd` (135) and a 5-iteration shell loop (195) were
    /// 0.04-0.07s with the flag on and the same with it off. A larger N is the SPOT-CHECK tier for
    /// runs where full hashing is too expensive; it trades detection latency for cost, since a
    /// divergence is only seen at the next sampled point. Every emitted line records the tier it
    /// was produced under, so a cell can state which tier it met instead of leaving it implicit.
    #[clap(long, default_value = "1", value_name = "uint64")]
    pub detlog_regs_cadence: u64,

    /// Configure a time offset (in seconds) between a container OS considered booted and a guest is executed
    /// This primarily affects 'sysinfo' syscall's 'uptime' field reporting
    #[clap(long, default_value = "120", value_name = "uint64")]
    pub sysinfo_uptime_offset: u64,

    /// Configure memory available for the container.  Takes a number of bytes, or shorthand (e.g.
    /// "1GB"). Right now this doesn't enforce an upper bound, but does affect the amount of memory
    /// reported to the guest.
    #[clap(long, default_value = "1GB", value_parser = try_parse_memory, value_name = "bytesize")]
    pub memory: u64,

    /// Present a machine with this many CPUs, 1 to 128, so that `nproc`,
    /// `sysconf(_SC_NPROCESSORS_ONLN/CONF)` and thread pools size themselves
    /// to it. Threads still run one at a time. The affinity mask,
    /// `getcpu`, CPUID's APIC IDs, `/proc/cpuinfo`, `/proc/stat`, a task's
    /// `Cpus_allowed` and `/sys/devices/system/cpu` (its CPU lists, `cpuN`
    /// directories, topology and cache files) all describe the same N
    /// single-core sockets on one NUMA node. `sched_setaffinity` follows
    /// Linux: a mask with none of the N CPUs is EINVAL. The host must list
    /// at least N CPUs under `/sys/devices/system/cpu`. Without this flag the
    /// guest sees Hermit's historical single CPU, whose `sched_setaffinity`
    /// accepts any mask.
    #[serde(default)]
    #[clap(long, value_name = "count", value_parser = parse_virtual_cpus)]
    pub virtual_cpus: Option<NonZeroU32>,

    /// How much of `/proc` the guest sees. Every level is the same on every
    /// backend and host. `minimal` (the default) shows a small set, modelled
    /// on gVisor's; `standard` shows every system-wide name Detcore serves or
    /// plans to serve. A system-wide or `/proc/sys` name outside the chosen level
    /// does not exist: lookups give ENOENT and listings leave it out. So does
    /// a name in a process's or a thread's directory, except a few that Hermit
    /// still leaves to the kernel (`mem`, `uid_map`, `gid_map`, `setgroups`),
    /// as are whether a process exists and the entries of its `fd`,
    /// `fdinfo`, `ns` and `net`. A spelling through a symbolic link or `..` is
    /// checked only when it opens a file.
    #[serde(default)]
    #[clap(
        long = "proc",
        default_value = "minimal",
        value_name = "minimal|standard"
    )]
    pub proc_level: ProcLevel,

    /// Configure extra interrupt points based on thread id and rcb counter. Detcore will raise a precise
    /// timer for this RCB whenever it detects that current current thread timeslice intercects any of the
    /// interrupt points specified
    #[clap(long, value_name = "tid:rcbs", value_parser = try_parse_numbers_with_colon)]
    pub interrupt_at: Vec<(DetTid, u64)>,

    /// Resolved happens-before program: deterministic ordering edges between
    /// anchored events (see `detcore_model::happens_before`). This is populated
    /// programmatically by hermit-cli after loading and resolving a
    /// `--happens-before` spec against the guest binary; it is not a direct CLI
    /// flag and is not serialized (it is reconstructed from the spec file each
    /// run, so `#[serde(skip)]` avoids requiring serde on `Sysno`-bearing
    /// positions and keeps save-config output stable). The scheduler enforces
    /// these edges only when `sequentialize_threads` is set.
    #[serde(skip)]
    #[clap(skip)]
    pub happens_before: Option<HappensBeforeProgram>,

    /// Whether Detcore records the host identity of each file the guest
    /// opens, sending each record to the global state, which writes the run's
    /// records to [`Self::host_input_log`] when the run ends (see
    /// [`crate::host_input`]). `hermit run --verify` sets it, so that a
    /// divergence caused by a host file replaced during a run can be named.
    /// The records are never part of the compared log. Serialized, so that it
    /// reaches the Detcore tool on every backend, including one that runs
    /// inside the guest and receives this `Config` over RPC; never a CLI flag.
    #[serde(default)]
    #[clap(skip)]
    pub record_host_inputs: bool,

    /// Where the global state writes the run's host-input records, one
    /// [`crate::host_input::HostInputRecord`] JSON line each and a
    /// [`crate::host_input::HostInputLogEnd`] line, when the run ends.
    /// `hermit run --verify` sets a private file. A host path, read only by
    /// the global state, which every backend that hosts it builds from this
    /// `Config` in-process; so set programmatically, like `happens_before`,
    /// and never serialized.
    #[serde(skip)]
    #[clap(skip)]
    pub host_input_log: Option<PathBuf>,

    /// Captured inputs that the launcher copied into private container files.
    /// Only the host global state consumes these identities, just like
    /// `host_input_log`; guest-side tools still report the actual open's stat.
    /// Never serialized: every backend constructs that global state from this
    /// in-process config. Physical identities remain necessary for deferred
    /// content checks even when records publish the captured input's origin.
    #[serde(skip)]
    #[clap(skip)]
    pub host_input_snapshots: Vec<crate::host_input::HostInputSnapshot>,

    /// The frozen copy of the Android system property area that the guest
    /// reads in place of the live one ([`PropertySnapshot`]). hermit-cli makes
    /// the copy and sets this before the guest's first exec on a host that has
    /// `/dev/__properties__`; never a CLI flag. Serialized, because Detcore's
    /// open handler reads it in each guest process's tool, which receives this
    /// `Config` encoded; it came after the legacy form froze, so it never
    /// enters that form and reads as unset there.
    #[serde(default)]
    #[clap(skip)]
    pub property_snapshot: Option<PropertySnapshot>,

    /// A guest address range, `[start, end)`, holding code whose syscalls
    /// the backend lets through without Detcore seeing them, which the guest
    /// cannot otherwise use without faulting. With
    /// [`Self::record_host_inputs`] set, a guest call that may change the
    /// memory there (a fixed mapping over it, a protection or advice change
    /// or unmapping that covers it, an open of a process-memory file for
    /// writing) is reported as a change at `/`, so no host input change is
    /// named in that run (see [`crate::host_input`]). The backend that hosts
    /// Detcore in-process sets it, so set programmatically, like
    /// `happens_before`, and never serialized.
    #[serde(skip)]
    #[clap(skip)]
    pub untraced_code_range: Option<(u64, u64)>,

    /// Opt-in capture of a core file for a guest thread killed by a signal
    /// whose default action dumps core; `None`, the default, writes nothing.
    /// A host path and host-side limits, read only by the global state at a
    /// fatal exit stop, so set programmatically, like `host_input_log`, and
    /// never serialized.
    #[serde(skip)]
    #[clap(skip)]
    pub fatal_core_capture: Option<FatalCoreCapture>,

    /// Whether the in-guest runtime delivers the guest's signals through
    /// Detcore's schedule (A5 step 3c.1, gate D35 of the design,
    /// <https://github.com/rrnewton/hermit/issues/4485>): while its code runs
    /// it blocks every signal it does not reserve, and at each return to
    /// guest code it predicts the kernel's deliveries, reports them in two
    /// phases, and makes a predicted death a scheduled one. Detcore answers
    /// Reverie's `Tool::in_guest_signal_delivery` from it.
    ///
    /// The host sets it, not a flag, when it prepares an in-guest run on the
    /// default route, and only where every condition of the gate holds:
    /// site patching off, sequentialized threads, no configuration in which
    /// the two-phase protocol cannot be kept (`detcore::two_phase_refusal`),
    /// and a scheduler that runs as the init of the guest's PID namespace,
    /// which only `hermit run`'s container launch knows. Preparing any run
    /// clears it first, so a caller's configuration cannot carry it in; the
    /// in-guest loader route never sets it. Every Tool image receives it in
    /// the configuration handshake, which guest code cannot change. False,
    /// the default, changes nothing. Like `in_guest_reserved_signals`, it
    /// has no legacy key.
    #[serde(default)]
    #[clap(skip)]
    pub in_guest_signal_delivery: bool,

    /// Whether `--replay-preemptions-from` names a record that carries
    /// callback checkpoints, so priority-history replay ends each annotated
    /// row at its recorded callback endpoint instead of at the first stop that
    /// reaches the row's RCB count
    /// (<https://github.com/rrnewton/hermit/issues/4082>).
    ///
    /// The host sets it, not a flag, from the validated record before the
    /// scheduler or any Tool reads the configuration, and refuses a
    /// configuration it does not qualify for such a record
    /// (`preemption_admission::refuse_unqualified_annotated_replay`). Preparing
    /// any run sets it from the record, so a caller's configuration cannot
    /// carry it in; Detcore refuses a record whose checkpoints disagree with
    /// it. A record so annotated was made with `--record-preemptions`, so the
    /// replay observes schedule events as its recording did
    /// (`observes_schedevents`). False, the default, changes nothing. Like
    /// `in_guest_signal_delivery`, it has no legacy key.
    #[serde(default)]
    #[clap(skip)]
    pub annotated_priority_replay: bool,
}

/// Where and how much [`Config::fatal_core_capture`] may write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FatalCoreCapture {
    /// Directory receiving the compressed cores. Its regular files, whoever
    /// wrote them, count against `max_total_bytes`.
    pub dir: PathBuf,
    /// Prefix of every file name this run writes, so the run can find and
    /// remove its own cores.
    pub file_prefix: String,
    /// Most bytes one compressed core may occupy.
    pub max_core_bytes: u64,
    /// Most bytes all regular files in `dir` may occupy together.
    pub max_total_bytes: u64,
    /// Longest one capture may take before it gives up.
    pub time_limit: std::time::Duration,
}

fn try_parse_numbers_with_colon(from_str: &str) -> anyhow::Result<(DetTid, u64)> {
    if let Some((thread_id_str, time_str)) = from_str.split_once(':') {
        Ok((
            thread_id_str
                .parse::<DetTid>()
                .map_err(anyhow::Error::msg)?,
            time_str.parse::<u64>().map_err(anyhow::Error::msg)?,
        ))
    } else {
        anyhow::bail!(
            "unable to parse <thread_id>:<logical_time> from '{}'",
            from_str
        )
    }
}

fn try_parse_memory(from_str: &str) -> anyhow::Result<u64> {
    <bytesize::ByteSize as FromStr>::from_str(from_str)
        .map(|res| res.as_u64())
        .map_err(anyhow::Error::msg)
}

/// The most CPUs `--virtual-cpus` presents: the bits of the 128-bit mask
/// Detcore's `sched_getaffinity` answers with, whose highest CPU is the
/// `kernel_max` (127) it reports.
pub const MAX_VIRTUAL_CPUS: u32 = 128;

fn parse_virtual_cpus(value: &str) -> Result<NonZeroU32, String> {
    value
        .parse::<u32>()
        .ok()
        .filter(|count| (1..=MAX_VIRTUAL_CPUS).contains(count))
        .and_then(NonZeroU32::new)
        .ok_or_else(|| {
            format!(
                "expected a CPU count from 1 to {MAX_VIRTUAL_CPUS}, for example --virtual-cpus 4"
            )
        })
}

/// Why a run may not rewrite exclusive loops in the guest's memory
/// ([`Config::rewrite_exclusives_in_memory_refusal`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewriteExclusivesRefusal {
    /// The host does not execute the FEAT_LSE atomics the rewrite emits.
    HostLacksLseAtomics,
    /// Threads run concurrently, so one could execute code Detcore writes.
    ThreadsNotSequentialized,
}

impl std::fmt::Display for RewriteExclusivesRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HostLacksLseAtomics => write!(
                f,
                "rewriting exclusive loops in the guest's memory rewrites them into FEAT_LSE \
                 atomic instructions, which this host does not execute (no HWCAP_ATOMICS); \
                 the rewritten image would raise SIGILL"
            ),
            Self::ThreadsNotSequentialized => write!(
                f,
                "rewriting exclusive loops in the guest's memory writes code that another \
                 thread could be running; run with threads sequentialized"
            ),
        }
    }
}

impl Config {
    /// How many CPUs the guest sees: `--virtual-cpus`, or Hermit's
    /// historical one.
    pub fn virtual_cpu_count(&self) -> u32 {
        self.virtual_cpus.map_or(1, NonZeroU32::get)
    }

    /// Whether the scheduler models which thread the kernel gives a signal,
    /// from the signal masks it reads from the kernel, and keeps the
    /// per-process ledger of signals discarded at send time; the guest side
    /// publishes a waiting thread's mask and records handler-restored
    /// real-time signals for it (`publish_wait_mask`,
    /// `record_handler_restore_mask`). It needs serialized threads, so that a
    /// mask it reads cannot change before the kernel acts on it, and a backend
    /// whose kernel signal targets a tool can predict
    /// (`BackendCapabilities::kernel_signal_targets_are_predictable`).
    pub fn models_signal_targets(&self) -> bool {
        self.sequentialize_threads && self.backend.kernel_signal_targets_are_predictable
    }

    /// Whether the scheduler reads a sleeper's signal mask and dispositions
    /// (`Scheduler::signal_sleep`): where it models the kernel's signal targets
    /// (`models_signal_targets`), or under the in-guest delivery gate. Elsewhere
    /// (DBT, SaBRe and LiteInst without the gate, KVM) a sleep parked as a
    /// timed waiter is woken by any signal the scheduler sends it, including a
    /// default-ignored `SIGCHLD` or a signal the sleeper blocks, which Linux
    /// never lets end a sleep.
    pub fn reads_sleeper_signal_state(&self) -> bool {
        self.models_signal_targets() || self.in_guest_signal_delivery
    }

    /// Whether `signal` is one of [`Config::in_guest_reserved_signals`]. Never
    /// signal 0 (an existence check sends nothing), nor a number outside 1 to
    /// 64, which Linux refuses with EINVAL before any target is named. Always
    /// false while the set is empty, which it is on every backend but the
    /// in-guest ones' default route.
    pub fn reserves_in_guest_signal(&self, signal: i64) -> bool {
        (1..=64).contains(&signal) && self.in_guest_reserved_signals & (1_u64 << (signal - 1)) != 0
    }

    /// This configuration with `change` applied to its backend capabilities.
    /// [`BackendCapabilities`] cannot be built field by field outside Reverie,
    /// so a caller that needs to adjust individual facts starts from the value
    /// already here.
    pub fn with_backend(mut self, change: impl FnOnce(&mut BackendCapabilities)) -> Self {
        change(&mut self.backend);
        self
    }

    /// Whether the epoch is the stable library default omitted by `Display`.
    pub fn has_default_epoch(&self) -> bool {
        self.epoch == DEFAULT_EPOCH_STR.parse::<DateTime<Utc>>().unwrap()
    }

    /// Replace the stable library/test default with the host wall clock captured
    /// by the outer `hermit run` invocation. The caller owns the single
    /// host-clock read boundary; all guest clock and metadata observations
    /// consume the resulting concrete epoch.
    pub fn capture_epoch_from_host_time(&mut self, now: SystemTime) {
        self.epoch = epoch_from_host_time(now);
    }

    /// Virtual nanoseconds one RCB takes at this clock multiplier and, under a
    /// chaos per-thread slowdown, at its largest factor: the most any thread's
    /// RCB takes, so a timeslice of `n` virtual nanoseconds is at least
    /// `n / largest_nanos_per_rcb()` RCBs for every thread.
    ///
    /// The two multipliers are combined first and then applied to
    /// `NANOS_PER_RCB`, the order of the one-RCB PMU epsilon that Detcore's
    /// post-hook compares with the end of the maximum
    /// (`post_handler_hook_observed` and `callback_checkpoint_expiry_due` in
    /// `detcore/src/lib.rs`), which [`Self::minimum_max_timeslice_nanos`]
    /// bounds. Floating-point multiplication is not associative: at a clock
    /// multiplier of 2.5 and a largest factor of 4.4, `(10 * 2.5) * 4.4` is one
    /// unit in the last place above 110, which would raise that minimum to 111.
    pub fn largest_nanos_per_rcb(&self) -> f64 {
        let slowdown = if self.chaos && self.chaos_per_thread_slowdown {
            self.chaos_slowdown_max_factor
        } else {
            1.0
        };
        NANOS_PER_RCB * (self.clock_multiplier.unwrap_or(1.0) * slowdown)
    }

    /// Smallest PMU-backed maximum representable by one RCB at this clock multiplier.
    pub fn minimum_max_timeslice_nanos(&self) -> u64 {
        (self.largest_nanos_per_rcb().ceil() as u64).max(NANOS_PER_RCB as u64)
    }

    /// Check invariants that must hold at every execution boundary without mutating the config.
    pub fn validate_invariants(&self) {
        assert!(self.sched_sticky_random_param >= 0.0);
        assert!(self.sched_sticky_random_param <= 1.0);
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-1149)
        assert!(
            self.chaos_slowdown_max_factor.is_finite()
                && self.chaos_slowdown_max_factor >= 1.0
                && self.chaos_slowdown_max_factor <= RcbTimeMultiplier::MAX,
            "chaos_slowdown_max_factor must be finite and in [1.0, {}], got {}",
            RcbTimeMultiplier::MAX,
            self.chaos_slowdown_max_factor
        );
        if let Some(multiplier) = self.clock_multiplier {
            assert!(
                multiplier.is_finite() && multiplier > 0.0,
                "clock_multiplier must be finite and positive"
            );
        }
        let minimum_max_timeslice = self.minimum_max_timeslice_nanos();
        assert!(
            self.max_timeslice
                .is_none_or(|timeslice| u64::from(timeslice) >= minimum_max_timeslice),
            "max_timeslice must be at least one RCB ({} virtual nanoseconds)",
            minimum_max_timeslice
        );
    }

    /// Whether this run may rewrite exclusive loops in the guest's memory,
    /// given whether the host executes FEAT_LSE atomics. `None` admits the
    /// run. Every launch path checks this at Detcore's operational
    /// admission, so a library caller gets the CLI's refusal without
    /// running the CLI's validator: without LSE a rewritten site would
    /// raise SIGILL, and with threads running concurrently one could be
    /// inside the code while Detcore writes it.
    pub fn rewrite_exclusives_in_memory_refusal(
        &self,
        host_has_lse_atomics: bool,
    ) -> Option<RewriteExclusivesRefusal> {
        if !self.rewrite_exclusives_in_memory {
            return None;
        }
        if !self.sequentialize_threads {
            return Some(RewriteExclusivesRefusal::ThreadsNotSequentialized);
        }
        if !host_has_lse_atomics {
            return Some(RewriteExclusivesRefusal::HostLacksLseAtomics);
        }
        None
    }

    /// Sanity check the flags, and update any wherever flag B is implied by A.
    pub fn validate(&mut self) {
        self.validate_invariants();

        // TODO(T124429978) Restore the eprintln! calls below to tracing::warn! when the tracing
        // subscriber is set up early enough for these warnings to print.

        if self.record_preemptions_to.is_some() {
            self.record_preemptions = true;
        }
        // TODO: separate out recording flags: --record-preemptions vs --record-schedule-trace
        // if self.record_preemptions && !self.chaos {
        //     tracing::warn!(
        //         "Setting --record-preemptions when not in chaos mode doesn't do anything."
        //     );
        // }

        if self.replay_schedule_from.is_some() && self.replay_preemptions_from.is_some() {
            panic!("Cannot set both --replay-preemptions-from and --replay-schedule-from!!");
        }

        if self.chaos {
            self.sequentialize_threads = true;
        }

        if self.replay_preemptions_from.is_some() && self.imprecise_timers {
            eprintln!(
                "WARNING: Setting --imprecise timers with --replay-preemptions-from is probably not what you want. They won't replay precisely."
            );
        }

        if self.stop_after_turn.is_some() && !self.sequentialize_threads {
            eprintln!(
                "WARNING: --stop-after-turn will have no effect if --no-sequentialize-threads is enabled"
            );
            self.stop_after_turn = None;
        }
        if self.stop_after_iter.is_some() && !self.sequentialize_threads {
            eprintln!(
                "WARNING: --stop-after-iter will have no effect if --no-sequentialize-threads is enabled"
            );
            self.stop_after_iter = None;
        }
        if self.signal_at.is_some() && !self.sequentialize_threads {
            eprintln!(
                "WARNING: --signal-at will have no effect if --no-sequentialize-threads is enabled"
            );
            self.signal_at = None;
        }

        if self.debug_externalize_sockets && !self.sequentialize_threads {
            eprintln!(
                "WARNING: --debug-externalize-sockets will have no effect if --no-sequentialize-threads is enabled"
            );
            self.debug_externalize_sockets = false;
        }

        if !self.stacktrace_event.is_empty()
            && !self.record_preemptions
            && self.replay_schedule_from.is_none()
        {
            eprintln!(
                "WARNING: -s/--stacktrace-event has no effect if not recording/replaying events!"
            );
        }

        if self.preemption_stacktrace_log_file.is_some() {
            self.preemption_stacktrace = true;
        }
    }

    /// Whether configured preemption can use this backend's timer mechanism.
    ///
    /// This is independent of RCB accounting and does not certify a clock.
    pub fn use_preemption_timer(&self) -> bool {
        self.max_timeslice.is_some() && self.backend.delivers_preemption_timer_events
    }

    /// Whether a thread that is the only one able to run may skip its
    /// timeslice-end stops and have them processed when it next enters
    /// Detcore (see [`Config::eager_lone_preemptions`]). Only where a
    /// timeslice end reads nothing from the guest but its branch count, and
    /// nothing but the thread's next entry could act before the next one:
    /// precise PMU timers keyed to retired branches at one virtual-time rate,
    /// no slice-boundary protocol, no stack traces at preemptions or events,
    /// no turn-numbered triggers (`--stop-after-turn`, `--stop-after-iter`,
    /// `--signal-at`), no happens-before program, no manual interrupt points,
    /// no syscall-boundary deadline (`--target-timeslice`) and no instruction
    /// offsets ([`Config::slice_ends_need_a_stop`]: an offset is stepped from a
    /// real stop at its branch target, and processing a skipped end can draw
    /// one). Never with a gdbserver either: a debugger is served at the stops,
    /// and between skipped ends it would see Detcore's slice bookkeeping lag
    /// the guest's program counter. A recorded schedule writes such an end
    /// without the RIP it has no stop to read (`SchedEvent::stopless`), and a
    /// schedule replay takes without a stop only the ends its recording took
    /// that way. Detcore's `replay_allows_stopless_end` decides each end of a
    /// replay, including the per-thread rates and the instruction offsets a
    /// preemption record may carry.
    pub fn replays_lone_preemptions(&self) -> bool {
        !self.eager_lone_preemptions
            && self.use_preemption_timer()
            && self.use_rcb_time()
            && self.sequentialize_threads
            && !self.imprecise_timers
            && !self.chaos_per_thread_slowdown
            && !self.strict_schedule_boundaries
            && self.target_timeslice.is_none()
            && !self.preemption_stacktrace
            && self.stacktrace_event.is_empty()
            && self.stop_after_turn.is_none()
            && self.stop_after_iter.is_none()
            && self.signal_at.is_none()
            && self.happens_before.is_none()
            && self.interrupt_at.is_empty()
            && !self.slice_ends_need_a_stop()
            && !self.gdbserver
    }

    /// Should we use RCB in computing logical time?
    ///
    /// The answer is NO either if `--no-rcb-time` is specified or if HW counters are disabled by
    /// setting `--max-timeslice=disabled`.
    pub fn use_rcb_time(&self) -> bool {
        self.max_timeslice.is_some() && !self.no_rcb_time
    }

    /// Whether every chaos timeslice end needs the guest really stopped at
    /// its PMU branch target: `--chaos-instruction-offset` single-steps from
    /// that stop to where the timeslice ends. Any optimization that takes a
    /// slice end without stopping the guest there (for example a lone
    /// runnable thread's slice end processed at its branch count, which
    /// [`Config::replays_lone_preemptions`] turns off) must be off while this
    /// holds, or the offset is lost and a replay of the record no longer
    /// stops where the recorded run did.
    pub fn slice_ends_need_a_stop(&self) -> bool {
        self.chaos && self.chaos_instruction_offset > 0
    }

    /// Whether a `FUTEX_WAKE` that woke `woken` waiters makes the waker yield
    /// to them (`--futex-wake-yields`).
    ///
    /// A thread that releases a contended lock usually keeps running: the
    /// waiter it woke is queued behind it, and the waker can re-take the lock,
    /// uncontended, many times before that waiter runs.
    /// glibc and Rust mutexes both let a releasing thread barge back in, and on
    /// real hardware the woken thread wins on another CPU within the release
    /// window. Under sequentialized threads it never gets one, so QEMU's
    /// multi-threaded TCG vCPUs starved its main loop of the big QEMU lock for
    /// whole PMU slices at a time
    /// (<https://github.com/rrnewton/hermit/issues/3874>). When this holds, the
    /// scheduler moves the waker to the back of its priority band, and the
    /// waker takes a yield turn at the syscall's post-hook, so woken threads
    /// of its priority run before it continues (it keeps its band; `--chaos`
    /// can redraw priorities, and a random heuristic does not select first
    /// in, first out). The yield leaves the waker's timeslice alone.
    ///
    /// A thread switch is not free in virtual time: every scheduler decision
    /// is charged, so a policy that switches more often moves time that the
    /// guest observes. A QEMU guest kernel that calibrates its TSC against the
    /// emulated PIT failed that calibration with this policy, because the
    /// main loop now ran in the middle of it.
    ///
    /// The decision reads only the configuration and the wake's result, the
    /// number of waiters the scheduler took off the futex in the waker's turn,
    /// which is already deterministic. A wake that woke no one keeps the
    /// slice. Replay applies the policy exactly as the recorded run did: a
    /// preemption record holds slice ends but not this queue order, so the
    /// requeue and the yield must happen again for the replayed run to reach
    /// the same schedule. Because the yield consumes no slice end and draws
    /// nothing from the thread's chaos PRNG, making it again keeps the replay
    /// aligned with its record.
    pub fn yields_after_futex_wake(&self, woken: u64) -> bool {
        self.futex_wake_yields && woken > 0
    }

    /// The virtual nanoseconds each committed scheduler turn adds to the
    /// global clock: `--scheduler-turn-cost` (default [`NANOS_PER_SCHED`])
    /// times `--clock-multiplier`, truncated as it always was. A configured
    /// cost must give a charge of at least one nanosecond, because the charge
    /// is what advances the clock through turns that run no guest code, such
    /// as the retries of a finite poll; a scaled charge of zero would leave
    /// such a timeout unable to expire. A configured cost, or its scaled
    /// charge, above [`MAX_SCHEDULER_TURN_COST`] is refused too, so that a
    /// configured charge never exceeds one second per turn.
    pub fn scheduler_turn_charge(&self) -> Result<u64, String> {
        let multiplier = self.clock_multiplier.unwrap_or(1.0);
        let Some(cost) = self.scheduler_turn_cost else {
            return Ok((NANOS_PER_SCHED * multiplier) as u64);
        };
        let cost = u64::from(cost);
        if cost > MAX_SCHEDULER_TURN_COST {
            return Err(format!(
                "--scheduler-turn-cost must be at most {MAX_SCHEDULER_TURN_COST} virtual \
                 nanoseconds (received {cost})"
            ));
        }
        let charge = cost as f64 * multiplier;
        if !(1.0..=MAX_SCHEDULER_TURN_COST as f64).contains(&charge) {
            return Err(format!(
                "--scheduler-turn-cost {cost} at --clock-multiplier {multiplier} charges {charge} \
                 virtual nanoseconds per scheduler turn; the charge must be at least 1, so that \
                 timed waits keep advancing, and at most {MAX_SCHEDULER_TURN_COST}"
            ));
        }
        Ok(charge as u64)
    }

    /// Why `post_crash_timeslice`, which is set, would not be in effect under
    /// this configuration ([`Self::post_crash_timeslice_in_effect`]), or
    /// `None` when it is, or is not set.
    pub fn post_crash_timeslice_refusal(&self) -> Option<String> {
        let bound = self.post_crash_timeslice?;
        if self.post_crash_timeslice_in_effect().is_some() {
            return None;
        }
        Some(if !self.sequentialize_threads {
            "the switch needs sequentialized threads".to_owned()
        } else {
            match self.max_timeslice {
                None => "the switch needs a --max-timeslice".to_owned(),
                Some(max) => format!(
                    "a bound of {bound} is not above --max-timeslice={max}; replay with the \
                     recording's --max-timeslice"
                ),
            }
        })
    }

    /// The post-crash bound this run switches to (`post_crash_timeslice`), or
    /// `None` when the switch is off: unset, without sequentialized threads
    /// (there is no scheduler turn to switch at), without a `max_timeslice`,
    /// or not above it.
    pub fn post_crash_timeslice_in_effect(&self) -> Option<NonZeroU64> {
        let bound = self.post_crash_timeslice?;
        let run = self.max_timeslice?;
        (self.sequentialize_threads && bound > run).then_some(bound)
    }

    /// The `--pipe-wake-latency` in effect (<https://github.com/rrnewton/hermit/issues/4380>): the
    /// option's value in run mode, with sequentialized threads and a heuristic that selects by
    /// priority; `None` everywhere else, so nothing of the hand-off runs.
    pub fn pipe_wake_latency_in_effect(&self) -> Option<u64> {
        self.pipe_wake_latency.filter(|_| {
            self.use_nonblocking_sockets()
                && !self.recordreplay_modes
                && self.sched_heuristic.is_priority_ordered()
        })
    }

    /// Whether a run of consecutive turns that each end an expired timed wait is charged one
    /// scheduler turn (`--expired-wait-charge`): the explicit value, or by default outside race
    /// search (`--chaos`, or a `--sched-heuristic` that does not select by priority). Never under
    /// `hermit record` and `hermit replay`. A replay that leaves the option unset takes its
    /// record's value instead (`Scheduler::new`).
    pub fn expired_wait_runs_charged_once(&self) -> bool {
        !self.recordreplay_modes
            && match self.expired_wait_charge {
                Some(charge) => charge == ExpiredWaitCharge::Once,
                None => !self.chaos && self.sched_heuristic.is_priority_ordered(),
            }
    }

    /// Whether a queued plain sleep is registered as a timed waiter at the first quiescent pass
    /// that finds it queued (`--sleep-registration`): the explicit value, or by default outside
    /// race search (`--chaos`, or a `--sched-heuristic` that does not select by priority) where
    /// the scheduler reads a sleeper's signal state (`reads_sleeper_signal_state`). Never under
    /// `hermit record` and `hermit replay`, whose schedule must not change. A replay that leaves
    /// the option unset takes its record's value instead (`Scheduler::new`).
    ///
    /// A registered sleep is parked, and a signal the scheduler sends a parked sleep wakes it
    /// whatever the signal is unless the scheduler can read the sleeper's mask and dispositions
    /// (`Scheduler::signal_sleep`). Without them (DBT, SaBRe and LiteInst without the in-guest
    /// gate, KVM), a default-ignored `SIGCHLD` or a blocked timer signal would end every plain
    /// sleep with `EINTR`, where Linux never wakes it. So the default registers when chosen on
    /// those backends, as before (<https://github.com/rrnewton/hermit/pull/4682>); an explicit
    /// `first-pass` there, or one taken from a record, is refused before the guest starts, and
    /// the scheduler does not register there whatever this says, so a record states what it did.
    pub fn queued_sleeps_registered_at_first_pass(&self) -> bool {
        !self.recordreplay_modes
            && match self.sleep_registration {
                Some(registration) => registration == SleepRegistration::FirstPass,
                None => {
                    !self.chaos
                        && self.sched_heuristic.is_priority_ordered()
                        && self.reads_sleeper_signal_state()
                }
            }
    }

    /// The settings of this run under which Detcore can record a descriptor
    /// without its file type, by their option names: without metadata
    /// virtualization it keeps no stat of a descriptor it opens, which is what
    /// tells a FIFO from a plain file. An access trace names them in its header
    /// (`TraceHeader::untyped_descriptors`).
    pub fn untyped_descriptor_settings(&self) -> Vec<String> {
        let mut settings = Vec::new();
        if !self.virtualize_metadata {
            settings.push("--no-virtualize-metadata".to_string());
        }
        settings
    }

    /// Should we convert sockets to SOCK_NONBLOCK?
    pub fn use_nonblocking_sockets(&self) -> bool {
        self.sequentialize_threads && !self.debug_externalize_sockets
    }

    /// Should we call trace_schedevent to trace each SchedEvent?
    /// This applies to both record and replay for scheduled events.
    pub fn should_trace_schedevent(&self) -> bool {
        self.record_preemptions || self.replay_schedule_from.is_some()
    }

    /// Whether this run produces and traces schedule events: the decision
    /// `should_trace_schedevent` makes for recording and schedule replay, and
    /// also replay of a callback-annotated priority history
    /// (`annotated_priority_replay`). Such a record was made with
    /// `--record-preemptions`, so its replay makes every decision this gates
    /// as its recording did, whether or not it writes a record, and its
    /// callback checkpoints compare against the progress those events count.
    pub fn observes_schedevents(&self) -> bool {
        self.should_trace_schedevent() || self.annotated_priority_replay
    }

    /// Why this effective configuration cannot replay a callback-annotated
    /// priority history, or `None` when it can. Its row ends are actual
    /// callback endpoints at exact RCB counts, guarded one RCB past each: that
    /// needs the backend's own Detcore preemption timer events
    /// (`delivers_preemption_timer_events`), sequentialized threads (the
    /// scheduler that grants each history and acknowledges each row), an
    /// active maximum timeslice, precise timers and RCB time. The answer
    /// names the first unmet requirement. It reads capabilities, never a
    /// backend's name, and never changes the configuration.
    pub fn annotated_priority_replay_unqualified(&self) -> Option<&'static str> {
        if !self.backend.delivers_preemption_timer_events {
            Some("cannot deliver Detcore preemption timer events")
        } else if !self.sequentialize_threads {
            Some("does not sequentialize threads (--no-sequentialize-threads)")
        } else if self.max_timeslice.is_none() {
            Some("has its preemption timer disabled (--max-timeslice=disabled)")
        } else if self.imprecise_timers {
            Some("requests imprecise timers (--imprecise-timers)")
        } else if self.no_rcb_time {
            Some("requests logical time without RCBs (--no-rcb-time)")
        } else {
            None
        }
    }

    /// Returns manual interuption points for a given thread
    pub fn interrupts_for_thread(&self, thread_id: DetTid) -> BTreeSet<u64> {
        self.interrupt_at
            .iter()
            .filter_map(|(tid, time)| {
                if tid.eq(&thread_id) {
                    Some(*time)
                } else {
                    None
                }
            })
            .collect::<BTreeSet<u64>>()
    }
}

impl fmt::Display for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.virtualize_time {
            write!(f, " --no-virtualize-time")?;
        }
        if !self.virtualize_cpuid {
            write!(f, " --no-virtualize-cpuid")?;
        }
        // The command line's default is auto, so every named profile, v2
        // included, is written out: a rerun of this line runs that profile,
        // and never derives one again.
        if self.cpu_profile != crate::cpu_profile::CpuProfile::Auto {
            write!(f, " --cpu-profile={}", self.cpu_profile)?;
        }
        if !self.virtualize_metadata {
            write!(f, " --no-virtualize-metadata")?;
        }
        if self.passthru_opt {
            write!(f, " --passthru-opt")?;
        }
        if self.untrap_passthrough_syscalls {
            write!(f, " --untrap-passthrough-syscalls")?;
        }
        match self.runs_post_fork {
            RunsPostFork::Child => {}
            RunsPostFork::Parent => write!(f, " --runs-post-fork=parent")?,
            RunsPostFork::Random => write!(f, " --runs-post-fork=random")?,
        }
        if !self.has_default_epoch() {
            write!(f, " --epoch={}", self.epoch.to_rfc3339())?;
        }
        if self.seed != 0 {
            write!(f, " --seed={}", self.seed)?;
        }

        if let Some(rng_seed) = self.rng_seed {
            write!(f, " --rng-seed={}", rng_seed)?;
        }
        if let Some(fuzz_seed) = self.fuzz_seed {
            write!(f, " --fuzz-seed={}", fuzz_seed)?;
        }

        if self.fuzz_futexes {
            write!(f, " --fuzz-futexes")?;
        }
        if self.chaos_target_races {
            write!(f, " --chaos-target-races")?;
        }
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-1149)
        if self.chaos_per_thread_slowdown {
            write!(f, " --chaos-per-thread-slowdown")?;
            write!(
                f,
                " --chaos-slowdown-max-factor={}",
                self.chaos_slowdown_max_factor
            )?;
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-1151)
            if self.chaos_epoch_length_ns > 0 {
                write!(f, " --chaos-epoch-length-ns={}", self.chaos_epoch_length_ns)?;
            }
        }
        if self.chaos_instruction_offset > 0 {
            write!(
                f,
                " --chaos-instruction-offset={}",
                self.chaos_instruction_offset
            )?;
        }
        if let Some(m) = self.clock_multiplier {
            write!(f, " --clock-multiplier={}", m)?;
        }
        if self.imprecise_timers {
            write!(f, " --imprecise-timers")?;
        }
        if self.eager_lone_preemptions {
            write!(f, " --eager-lone-preemptions")?;
        }
        if !self.signal_state_copy {
            write!(f, " --no-signal-state-copy")?;
        }
        if self.chaos {
            write!(f, " --chaos")?;
        }
        if let Some(range) = self.chaos_timeslice_range {
            write!(f, " --chaos-timeslice-range={range}")?;
        }
        if self.chaos_timeslice_draw == ChaosTimesliceDraw::Run {
            write!(f, " --chaos-timeslice-draw=run")?;
        }
        if self.strict_schedule_boundaries {
            write!(f, " --strict-schedule-boundaries")?;
        }
        if self.record_preemptions {
            write!(f, " --record-preemptions")?;
        }

        if let Some(p) = &self.record_preemptions_to {
            let s = p.to_str().expect("valid unicode path");
            write!(f, " --record-preemptions-to={}", shell_words::quote(s))?;
        }
        if let Some(p) = &self.replay_preemptions_from {
            let s = p.to_str().expect("valid unicode path");
            write!(f, " --replay-preemptions-from={}", shell_words::quote(s))?;
        }
        if let Some(p) = &self.replay_schedule_from {
            let s = p.to_str().expect("valid unicode path");
            write!(f, " --replay-schedule-from={}", shell_words::quote(s))?;
        }
        if self.replay_exhausted_panic {
            write!(f, " --replay-exhausted-panic")?;
        }
        if self.die_on_desync {
            write!(f, " --die-on-desync")?;
        }
        if self.replay_edited_preemptions {
            write!(f, " --replay-edited-preemptions")?;
        }
        if self.replay_continue_fairly {
            write!(f, " --replay-continue-fairly")?;
        }
        for (index, path) in &self.stacktrace_event {
            write!(f, " --stacktrace-event={}", index)?;
            if let Some(p) = path {
                let s = p.to_str().expect("valid unicode path");
                write!(f, ",{}", shell_words::quote(s))?;
            }
        }
        if let Some(p) = &self.access_trace {
            let s = p.to_str().expect("valid unicode path");
            write!(f, " --record-access-trace-to={}", shell_words::quote(s))?;
        }
        if self.access_trace_from != 0 {
            write!(f, " --access-trace-from={}", self.access_trace_from)?;
        }
        if let Some(records) = self.access_trace_max_records {
            write!(f, " --access-trace-max-records={}", records)?;
        }
        if let Some(bytes) = self.access_trace_max_bytes {
            write!(f, " --access-trace-max-bytes={}", bytes)?;
        }
        if self.stacktrace_at_schedule_positions {
            write!(f, " --stacktrace-at-schedule-positions")?;
        }
        for watch in &self.watch {
            write!(f, " --watch={}", watch)?;
        }
        if self.watch_from != 0 {
            write!(f, " --watch-from={}", self.watch_from)?;
        }
        if self.preemption_stacktrace {
            write!(f, " --preemption-stacktrace")?;
        }
        if self.panic_on_unsupported_syscalls {
            write!(f, " --panic-on-unsupported-syscalls")?;
        }
        if self.panic_on_rcb_overshoot {
            write!(f, " --panic-on-rbc-overshoot")?;
        }
        if self.kill_daemons {
            write!(f, " --kill-daemons")?;
        }
        if self.gdbserver {
            write!(f, " --gdbserver")?;
        }
        if self.gdbserver_port != /* default */ 1234u16 {
            write!(f, " --gdbserver-port={}", self.gdbserver_port)?;
        }
        match &self.max_timeslice {
            // The range sets the maximum itself (its MAX, or the value it
            // drew or a replayed record gave), and refuses an explicit one.
            Some(x)
                if self.chaos_timeslice_range.is_some()
                    && (self.chaos_timeslice_range.map(|range| range.max) == Some(*x)
                        || self.chaos_timeslice == Some(*x)) => {}
            Some(x) => {
                if *x != NonZeroU64::new(200_000_000).unwrap() {
                    write!(f, " --max-timeslice={}", x)?;
                }
            }
            None => {
                write!(f, " --max-timeslice=disabled")?;
            }
        }
        if let Some(target_timeslice) = self.target_timeslice {
            write!(f, " --target-timeslice={}", target_timeslice)?;
        }
        if self.target_timeslice_syscalls_only {
            write!(f, " --target-timeslice-syscalls-only")?;
        }
        if self.futex_wake_yields {
            write!(f, " --futex-wake-yields")?;
        }
        if let Some(latency) = self.pipe_wake_latency {
            write!(f, " --pipe-wake-latency={}", latency)?;
        }
        if let Some(charge) = self.expired_wait_charge {
            write!(f, " --expired-wait-charge={}", charge.as_str())?;
        }
        if let Some(registration) = self.sleep_registration {
            write!(f, " --sleep-registration={}", registration.as_str())?;
        }
        if let Some(report) = &self.work_span_report {
            write!(f, " --work-span-report={}", report.display())?;
        }
        if let Some(cost) = self.scheduler_turn_cost {
            write!(f, " --scheduler-turn-cost={}", cost)?;
        }
        if self.cap_preemption_charge {
            write!(f, " --cap-preemption-charge")?;
        }
        if let Some(bound) = self.virtual_timeout {
            write!(f, " --virtual-timeout={}", bound)?;
        }
        if self.sigint_instakill {
            write!(f, " --sigint-instakill")?;
        }
        if self.warn_non_zero_binds {
            write!(f, " --warn-non-zero-binds")?;
        }
        match &self.sched_heuristic {
            SchedHeuristic::None => {}
            SchedHeuristic::ConnectBind => {
                write!(f, " --sched-heuristic=connectbind")?;
            }
            SchedHeuristic::Random => {
                write!(f, " --sched-heuristic=random")?;
            }
            SchedHeuristic::StickyRandom => {
                write!(f, " --sched-heuristic=stickyrandom")?;
            }
        }
        if let Some(s) = self.sched_seed {
            write!(f, " --sched-seed={}", s)?;
        }
        if self.sched_sticky_random_param != 0.0 {
            write!(
                f,
                " --sched-sticky-random-param={}",
                self.sched_sticky_random_param
            )?;
        }
        if let Some(t) = self.stop_after_turn {
            write!(f, " --stop-after-turn={}", t)?;
        }
        if let Some(i) = self.stop_after_iter {
            write!(f, " --stop-after-iter={}", i)?;
        }
        if let Some(at) = self.signal_at {
            write!(f, " --signal-at={}", at)?;
        }
        if let Some(bound) = self.post_crash_timeslice {
            write!(f, " --post-crash-timeslice={}", bound)?;
        }
        if self.debug_externalize_sockets {
            write!(f, " --debug-externalize-sockets")?;
        }
        match &self.debug_futex_mode {
            BlockingMode::External => {
                write!(f, " --debug-futex-mode=external")?;
            }
            BlockingMode::Polling => {
                write!(f, " --debug-futex-mode=polling")?;
            }
            BlockingMode::Precise => { /* default */ }
        }
        if self.no_rcb_time {
            write!(f, " --no-rcb-time")?;
        }
        if self.detlog_heap {
            write!(f, " --detlog-heap")?;
        }
        if self.detlog_stack {
            write!(f, " --detlog-stack")?;
        }
        match self.detlog_anon {
            Some(DetlogAnon::Mappings) => write!(f, " --detlog-anon")?,
            Some(DetlogAnon::Pages) => write!(f, " --detlog-anon=pages")?,
            None => {}
        }
        if self.detlog_regs {
            write!(f, " --detlog-regs")?;
        }
        if self.detlog_regs_cadence != /* default */ 1 {
            write!(f, " --detlog-regs-cadence={}", self.detlog_regs_cadence)?;
        }
        if !self.detlog_io_buffers {
            write!(f, " --no-detlog-io-buffers")?;
        }
        if self.sysinfo_uptime_offset != /* default */ 120 {
            write!(f, " --sysinfo-uptime-offset={}", self.sysinfo_uptime_offset)?;
        }
        if self.memory != 1_000_000_000 {
            write!(f, " --memory={}", self.memory)?;
        }
        if let Some(cpus) = self.virtual_cpus {
            write!(f, " --virtual-cpus={cpus}")?;
        }
        match self.proc_level {
            ProcLevel::Minimal => {}
            ProcLevel::Standard => write!(f, " --proc=standard")?,
        }
        for (tid, rcb) in &self.interrupt_at {
            write!(f, " --interrupt-at={}:{}", tid, rcb)?;
        }
        Ok(())
    }
}

/// How much of `/proc` the guest sees (`--proc`). Each level contains the
/// one before it.
#[derive(
    Debug,
    Default,
    Clone,
    Copy,
    Serialize,
    Deserialize,
    Parser,
    PartialEq,
    Eq,
    PartialOrd,
    Ord
)]
pub enum ProcLevel {
    /// The names most programs read, modelled on gVisor's `/proc`.
    #[default]
    Minimal,
    /// Every name Detcore serves or plans to serve.
    Standard,
}

impl FromStr for ProcLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "minimal" => Ok(Self::Minimal),
            "standard" => Ok(Self::Standard),
            _ => Err(format!(
                "Expected minimal|standard, could not parse: {:?}",
                s
            )),
        }
    }
}

/// Which side of an ordinary fork/clone receives the first post-registration turn.
#[derive(
    Debug,
    Default,
    Clone,
    Copy,
    Serialize,
    Deserialize,
    Parser,
    PartialEq,
    Eq
)]
pub enum RunsPostFork {
    /// Run the newly registered child before its parent resumes.
    #[default]
    Child,
    /// Allow the parent to resume before the newly registered child starts.
    Parent,
    /// Deterministically choose child-first or parent-first from the scheduler seed.
    Random,
}

impl FromStr for RunsPostFork {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "child" => Ok(Self::Child),
            "parent" => Ok(Self::Parent),
            "random" => Ok(Self::Random),
            _ => Err(format!(
                "Expected Child|Parent|Random, could not parse: {:?}",
                s
            )),
        }
    }
}

/// How should we handle syscalls which may block, but are internal to the hermit container?
/// These syscalls are determinizable, but there are multiple methods of doing so.
/// These choices *do not* apply to blocking syscalls that wait for external conditions outside the
/// container, such as network responses.
///
/// Mostly it helps to switch this as: (1) a debugging aid to figure out what is going wrong with a
/// given guest program, or (2) in order to find the more performant mode for a given guest program.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Parser, PartialEq, Eq)]
pub enum BlockingMode {
    /// Handle the internal blocking syscall as though it was external, and unblocks at an
    /// unpredictable nondeterministic time.  These blocked threads will be parked in the
    /// scheduler's BlockedPool.
    ///
    /// (TODO: In the future these scheduling decisions will be recorded, and this comment needs to
    /// be updated accordingly.)
    External,
    /// Transform each blocking syscall into non-blocking, and then the scheduler will use that
    /// non-blocking form to repeatedly poll for completion of the operation.  When polling occurs
    /// (and the backoff policy there on) is decided by the scheduler.
    /// See NOTE [Blocking Syscalls via Internal Polling] in this folder.
    Polling,
    /// Precisely model the blocking and unblocking behavior inside hermit.
    /// TODO: This work is not completed yet for all forms of blocking syscalls.
    Precise,
}

impl FromStr for BlockingMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "polling" => Ok(BlockingMode::Polling),
            "precise" => Ok(BlockingMode::Precise),
            "external" => Ok(BlockingMode::External),
            _ => Err(format!(
                "Expected Polling|Precise|External, could not parse: {:?}",
                s
            )),
        }
    }
}

/// How a run of consecutive scheduler turns that each end an expired timed wait is charged while
/// another thread is queued (`--expired-wait-charge`).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    clap::ValueEnum
)]
#[serde(rename_all = "lowercase")]
pub enum ExpiredWaitCharge {
    /// Every expired wait is charged a scheduler turn.
    Each,
    /// A run of expired waits is charged one scheduler turn.
    Once,
}

impl ExpiredWaitCharge {
    /// The rule a run follows when it charges a run of expired waits once
    /// (`once`) or not (`each`).
    pub fn from_charged_once(once: bool) -> Self {
        if once { Self::Once } else { Self::Each }
    }

    /// The option's value as `--expired-wait-charge` spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Each => "each",
            Self::Once => "once",
        }
    }
}

/// When a queued plain sleep becomes a timed waiter (`--sleep-registration`).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    clap::ValueEnum
)]
#[serde(rename_all = "kebab-case")]
pub enum SleepRegistration {
    /// At the first quiescent scheduler pass that finds the sleep queued.
    FirstPass,
    /// Only when the sleeper's thread is chosen before its deadline.
    WhenChosen,
}

impl SleepRegistration {
    /// The rule a run follows when it registers queued sleeps at the first
    /// pass (`first-pass`) or not (`when-chosen`).
    pub fn from_registered_at_first_pass(first_pass: bool) -> Self {
        if first_pass {
            Self::FirstPass
        } else {
            Self::WhenChosen
        }
    }

    /// The option's value as `--sleep-registration` spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FirstPass => "first-pass",
            Self::WhenChosen => "when-chosen",
        }
    }
}

#[derive(
    Debug,
    Default,
    Clone,
    Copy,
    Serialize,
    Deserialize,
    Parser,
    PartialEq,
    Eq
)]
/// Apply a specialized scheduling heuristic which may help exercise certain bugs.
pub enum SchedHeuristic {
    /// Don't modify the scheduling algorithm.
    // TODO: Is the default a round robin?
    #[default]
    None,
    /// Prioritize connect and deprioritize bind to exercise races
    ConnectBind,
    /// Random: Randomly pick any available thread to make progress.
    Random,
    /// Sticky Random: Randomly pick any available thread. On the next round,
    /// and after the thread is parked, randomly choose if we will continue
    /// executing on the same thread, or picking another one.
    StickyRandom,
    // TODO: make all sleeps "instant".
}

impl SchedHeuristic {
    /// Whether selection follows priority order: `None` and `ConnectBind` pop
    /// the first queued entry, so a backed-off poller can be starved; `Random`
    /// and `StickyRandom` draw a uniform index into the queue and never are.
    /// The scheduler's run queue and `hermit run`'s refusals share this one
    /// definition.
    pub fn is_priority_ordered(self) -> bool {
        matches!(self, SchedHeuristic::None | SchedHeuristic::ConnectBind)
    }
}

// Lame to not derive this, but even `derive_more` won't do enums.
impl FromStr for SchedHeuristic {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "none" | "roundrobin" => Ok(SchedHeuristic::None),
            "connectbind" => Ok(SchedHeuristic::ConnectBind),
            "random" => Ok(SchedHeuristic::Random),
            "stickyrandom" => Ok(SchedHeuristic::StickyRandom),
            _ => Err(format!(
                "Expected None|ConnectBind|Random|StickyRandom, could not parse: {:?}",
                s
            )),
        }
    }
}

/// An optional virtual-timeslice duration. `None` disables that preemption mechanism.
pub type MaybeTimeslice = Option<NonZeroU64>;

/// How `--chaos-timeslice-range` draws (`--chaos-timeslice-draw`).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    Serialize,
    Deserialize,
    clap::ValueEnum
)]
#[serde(rename_all = "kebab-case")]
pub enum ChaosTimesliceDraw {
    /// Each chaos slice draws its mean from the range.
    #[default]
    Slice,
    /// The run draws its `--max-timeslice` from the range once.
    Run,
}

/// The range used by `--chaos-timeslice-draw=run` when no range is given.
/// One microsecond to 200 milliseconds spans fine preemption windows and
/// the ordinary maximum. Plain `--chaos` keeps its fixed maximum: fine draws
/// can cost much more, so this discovery mode remains explicit.
pub const RECOMMENDED_CHAOS_TIMESLICE_RANGE: TimesliceRange = TimesliceRange {
    min: NonZeroU64::new(1_000).unwrap(),
    max: NonZeroU64::new(200_000_000).unwrap(),
};

/// Long help for `--chaos`, including the explicit discovery mode.
pub fn chaos_long_help() -> String {
    let range = RECOMMENDED_CHAOS_TIMESLICE_RANGE;
    format!(
        "Schedule threads chaotically.\n\n\
         The behavior of this flag is subject to change. Current behavior is to randomize thread \
         priorities at every logical timeslice. Other randomization strategies are possible with \
         `--sched-heuristic`.\n\n\
         Thread scheduling remains deterministic, determined by the random seed.\n\n\
         By default, every chaos slice's mean is the run's `--max-timeslice` (200000000 virtual \
         nanoseconds unless given), so one seed sweep explores mostly one slice scale. For race \
         discovery, sweep seeds with `--chaos-timeslice-draw=run`: each run draws its \
         `--max-timeslice` once, log-uniformly from the recommended range {range} virtual \
         nanoseconds (or from `--chaos-timeslice-range`), as a pure function of the scheduler \
         seed. Fine draws explore short preemption windows, but can take orders of magnitude \
         longer: size time limits for them. A replay adopts a recorded drawn maximum without \
         drawing again. See `--chaos-timeslice-draw`."
    )
}

/// Long help for the draw policy, sharing the recommended range with resolution.
pub fn chaos_timeslice_draw_long_help() -> String {
    let range = RECOMMENDED_CHAOS_TIMESLICE_RANGE;
    format!(
        "How a chaos run draws its timeslice. Requires `--chaos`.\n\n\
         `run` (the discovery mode) draws the run's `--max-timeslice` once, log-uniformly from \
         `--chaos-timeslice-range`, or from the recommended range {range} virtual nanoseconds \
         (1 us to 200 ms) without an explicit range. The draw is a pure function of the scheduler \
         seed; the run is then an ordinary chaos run under it, so a seed sweep spreads its seeds \
         over the range, each seed at one scale. Without a seed, every run uses seed 0 and draws \
         the same value. Fine draws can be much slower: size time limits for the fine end.\n\n\
         Fine draws also incur frequent scheduler-turn charges in guest-visible time, which can \
         trigger a guest's own watchdogs. See `--cap-preemption-charge` for the explicit option \
         that caps expired-timeslice charges. The range parser requires MIN to be at least \
         10 virtual nanoseconds and no larger than MAX.\n\n\
         `--summary-json` reports the drawn value (`preemption.max_timeslice_ns`), and a \
         `--record-preemptions-to` record stores it. A replay adopts a recorded drawn maximum \
         without drawing again; it needs no timeslice option. A legacy record with no drawn \
         value does not override the requested draw policy. `hermit analyze` and `hermit bisect` \
         take a recorded value and refuse `run` in their run arguments, since all trials must \
         share one maximum.\n\n\
         `slice` (the default) draws each slice's mean from an explicitly supplied range; it is \
         what a range alone does. Without a range or an explicit `run` draw, the ordinary fixed \
         maximum remains unchanged."
    )
}

/// The bounds of `--chaos-timeslice-range`, in virtual nanoseconds, both
/// included: `MIN..MAX`, with `MIN` at least one RCB and no larger than `MAX`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TimesliceRange {
    pub min: NonZeroU64,
    pub max: NonZeroU64,
}

impl TimesliceRange {
    /// The point of the range a uniform draw `u` in `[0, 1)` maps to under a
    /// log-uniform law: `exp(ln MIN + u (ln MAX - ln MIN))`, in virtual
    /// nanoseconds. Each decade of the range gets an equal share of draws.
    pub fn log_uniform(&self, u: f64) -> f64 {
        let (low, high) = ((self.min.get() as f64).ln(), (self.max.get() as f64).ln());
        (low + u * (high - low))
            .exp()
            .clamp(self.min.get() as f64, self.max.get() as f64)
    }
}

impl FromStr for TimesliceRange {
    type Err = String;
    fn from_str(src: &str) -> Result<Self, Self::Err> {
        let (min, max) = src
            .split_once("..")
            .ok_or_else(|| format!("expected MIN..MAX virtual nanoseconds, got {src:?}"))?;
        let bound = |text: &str| -> Result<NonZeroU64, String> {
            let value = text
                .parse::<u64>()
                .map_err(|error| format!("{text:?} is not a number of nanoseconds: {error}"))?;
            if value < NANOS_PER_RCB as u64 {
                return Err(format!(
                    "{value} ns is below one RCB ({} virtual nanoseconds), the shortest PMU-backed \
                     timeslice",
                    NANOS_PER_RCB as u64
                ));
            }
            Ok(NonZeroU64::new(value).expect("at least one RCB"))
        };
        let (min, max) = (bound(min)?, bound(max)?);
        if min > max {
            return Err(format!("the range {min}..{max} is empty: MIN exceeds MAX"));
        }
        Ok(Self { min, max })
    }
}

// A saved or legacy configuration is checked as the command line is.
impl TryFrom<String> for TimesliceRange {
    type Error = String;
    fn try_from(text: String) -> Result<Self, Self::Error> {
        text.parse()
    }
}

impl From<TimesliceRange> for String {
    fn from(range: TimesliceRange) -> Self {
        range.to_string()
    }
}

impl std::fmt::Display for TimesliceRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}..{}", self.min, self.max)
    }
}

/// Deprecated name for an optional PMU-backed virtual-timeslice duration.
#[deprecated(note = "use MaybeTimeslice")]
pub type MaybePreemptionTimeout = MaybeTimeslice;

/// A signal sent to the run's root process at a scheduler turn
/// (`Config::signal_at`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalAt {
    /// The scheduler turn after whose commit the signal is sent.
    pub turn: u64,
    /// The signal's number.
    pub signal: i32,
}

impl std::fmt::Display for SignalAt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "turn={}:{}", self.turn, self.signal)
    }
}

/// Parse `turn=N:SIGNAL` (`Config::signal_at`).
fn parse_signal_at(src: &str) -> Result<SignalAt, String> {
    let rest = src
        .strip_prefix("turn=")
        .ok_or_else(|| format!("expected turn=N:SIGNAL, got {src:?}"))?;
    let (turn, signal) = rest
        .split_once(':')
        .ok_or_else(|| format!("expected turn=N:SIGNAL, got {src:?}"))?;
    let turn = turn
        .parse::<u64>()
        .map_err(|e| format!("the turn in {src:?} is not a non-negative integer: {e}"))?;
    let signal = match signal.parse::<i32>() {
        Ok(number) => nix::sys::signal::Signal::try_from(number),
        Err(_) => signal.parse::<nix::sys::signal::Signal>(),
    }
    .map_err(|_| format!("{signal:?} in {src:?} names no signal"))?;
    // Neither can be caught, blocked or ignored, so Linux acts on them in the
    // kernel at once: SIGKILL retires the root outside the scheduler's
    // SIGKILL retirement protocol, and SIGSTOP stops it outside the schedule.
    if matches!(
        signal,
        nix::sys::signal::Signal::SIGKILL | nix::sys::signal::Signal::SIGSTOP
    ) {
        return Err(format!(
            "{signal} in {src:?} cannot be caught or blocked; --signal-at sends only \
             signals the guest can handle"
        ));
    }
    Ok(SignalAt {
        turn,
        signal: signal as i32,
    })
}

fn parse_timeslice(src: &str) -> Result<MaybeTimeslice, ParseTimesliceError> {
    if let Ok(n) = src.parse::<u64>() {
        if n != 0 && n < NANOS_PER_RCB as u64 {
            Err(ParseTimesliceError::new(
                "PMU-backed timeslices must be at least one RCB (10 virtual nanoseconds)",
            ))
        } else {
            Ok(NonZeroU64::new(n))
        }
    } else {
        match src {
            "disabled" => Ok(None),
            _ => Err(ParseTimesliceError::new(
                "Unable to parse timeslice, expected disabled or a non-negative integer",
            )),
        }
    }
}

/// The default `--max-timeslice`, in virtual nanoseconds, which
/// `--post-crash-timeslice=default` names.
pub const DEFAULT_MAX_TIMESLICE: u64 = 200_000_000;

fn parse_post_crash_timeslice(src: &str) -> Result<MaybeTimeslice, ParseTimesliceError> {
    match src {
        "off" => Ok(None),
        "default" => Ok(NonZeroU64::new(DEFAULT_MAX_TIMESLICE)),
        _ => match parse_timeslice(src) {
            Ok(Some(bound)) => Ok(Some(bound)),
            _ => Err(ParseTimesliceError::new(
                "expected off, default, or a timeslice of at least one RCB (10 virtual \
                 nanoseconds)",
            )),
        },
    }
}

fn parse_index_with_path(src: &str) -> Result<(u64, Option<PathBuf>), String> {
    let convert = |e| format!("Failed to parse int index before comma: {e}");
    if let Some((index_str, path)) = src.split_once(',') {
        let ix = index_str.parse::<u64>().map_err(convert)?;
        let pathbuf = PathBuf::from_str(path).map_err(|_| "the impossible happened")?;
        Ok((ix, Some(pathbuf)))
    } else {
        let ix = src.parse::<u64>().map_err(convert)?;
        Ok((ix, None))
    }
}

#[derive(Debug)]
struct ParseTimesliceError {
    details: String,
}

impl fmt::Display for ParseTimesliceError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.details)
    }
}

impl ParseTimesliceError {
    fn new(msg: &str) -> ParseTimesliceError {
        ParseTimesliceError {
            details: msg.to_string(),
        }
    }
}

impl std::error::Error for ParseTimesliceError {
    fn description(&self) -> &str {
        &self.details
    }
}

/// The default epoch used by DetCore for things like initial file modtimes.
///
/// N.B. Default to a reasonable date. Some programs (like zip) have trouble with the
/// original unix epoch (time zero).
pub static DEFAULT_EPOCH_STR: &str = "2026-01-01T00:00:00Z";

pub use crate::canonical_test_epoch::CANONICAL_TEST_EPOCH;
pub use crate::canonical_test_epoch::CANONICAL_TEST_EPOCH_ARG;

/// Convert one invocation's captured host instant without losing subsecond
/// precision. CLI and comparison orchestration share this input conversion;
/// guest clock progression never reads the host clock through it.
pub fn epoch_from_host_time(now: SystemTime) -> DateTime<Utc> {
    DateTime::<Utc>::from(now)
}

impl Config {
    /// Construct the config using environment variables only, not CLI args.
    pub fn from_env() -> Self {
        let args: [OsString; 2] = [
            OsString::from("CMD"), // Silly/unused.
            OsString::from(format!("--epoch={}", DEFAULT_EPOCH_STR)),
        ];
        Config::parse_from(args.iter())
    }

    /// Returns effective "rng-seed" parameter taking in account "seed"
    /// parameter if former isn't specified
    pub fn rng_seed(&self) -> u64 {
        self.rng_seed.unwrap_or(self.seed)
    }

    /// Returns the fuzz_seed, as specified by the user or defaulting to the primary seed if
    /// unspecified.
    pub fn fuzz_seed(&self) -> u64 {
        self.fuzz_seed.unwrap_or(self.seed)
    }

    /// Returns effective "sched-seed" parameter taking in account "seed"
    /// parameter if former isn't specified
    pub fn sched_seed(&self) -> u64 {
        self.sched_seed.unwrap_or(self.seed)
    }
}

/// The largest `--scheduler-turn-cost` accepted, in virtual nanoseconds, both
/// before and after the clock multiplier scales it: one second per scheduler
/// turn.
pub const MAX_SCHEDULER_TURN_COST: u64 = 1_000_000_000;

/// N.B. we don't want to specify two different notions of "default", so we use the
/// `Clap` instance above.
/// Environment variable carrying the coordinator's fingerprint to an
/// out-of-process plugin: `detcore::config_wire_fingerprint`, which extends
/// [`config_wire_fingerprint`] with Detcore's global RPC definitions.
///
/// Named alongside the other `REVERIE_SABRE_HERMIT_*` launch variables so the
/// two travel together and a reader finds them in one place.
pub const CONFIG_FINGERPRINT_ENV: &str = "REVERIE_SABRE_HERMIT_CONFIG_FINGERPRINT";

/// Environment variable carrying the coordinator's fingerprint
/// (`detcore::config_wire_fingerprint`, which extends
/// [`config_wire_fingerprint`] with Detcore's global RPC definitions) to the
/// in-guest LiteInst runtime (`libdetcore_liteinst.so`), which checks it
/// before it decodes the configuration the coordinator sends. Without the
/// check, a runtime built from another tree fails with a bare decoding error
/// that names neither build (<https://github.com/rrnewton/hermit/issues/3986>).
/// The runtime removes it once checked, as it removes its DETLOG variables:
/// the value is zeroed in the environment block and the variable is unset, so
/// the fingerprint never reaches the guest.
pub const LITEINST_CONFIG_FINGERPRINT_ENV: &str = "HERMIT_LITEINST_CONFIG_FINGERPRINT";

const CONFIG_DEFINITION_SOURCES: &[&[u8]] = &[
    include_bytes!("config.rs"),
    // `RawInode`, the host identity the inode RPCs (`DeterminizeInode`,
    // `TouchFile`, `SetFileMtime`) carry. Its layout changed from one `u64`
    // to a device and an inode number, which neither encoding of `Config`
    // shows.
    include_bytes!("fd.rs"),
    include_bytes!("happens_before.rs"),
    include_bytes!("network_trace.rs"),
    include_bytes!("pid.rs"),
    include_bytes!("schedule.rs"),
    include_bytes!("time.rs"),
    // `WatchArg`, the element of `Config::watch`. The encoded default holds
    // no watch, so neither encoding of `Config` shows its layout.
    include_bytes!("watch.rs"),
];

/// A fingerprint of this build's [`Config`] payload and the configuration and
/// clock RPC definitions shared by a plugin and its coordinator.
///
/// # Why this exists
///
/// An out-of-process plugin such as `libdetcore_sabre.so` is a separate Cargo
/// artifact that lands in the same target directory as `hermit`. Changing
/// `Config` or `DetTime` -- or merely switching branches -- leaves the plugin
/// stale while everything still *looks* built. `Config` is transferred during
/// the RPC handshake, and `DetTime` is the first field in every Detcore request.
/// A stale plugin decodes either against the wrong layout and the failure
/// surfaces as an opaque codec error: measured, one added `bool` field
/// produced `Decode(InvalidBooleanValue(20))` at connect, which points nowhere
/// near "your plugin is from a different build" and cost a long diagnosis while
/// blocking every SaBRe measurement.
///
/// # What it measures
///
/// Two encodings of `Config::default()` and the source definitions for the
/// configuration and clock RPC fields are fingerprinted with separate domains:
///
/// - the exact legacy-bincode bytes used by Reverie RPC, which detect changes
///   such as `u32` to `u64` even when both default to JSON number zero; and
/// - the JSON encoding, which carries every field name and makes a pure rename
///   visible even though bincode is positional; and
/// - the source files defining `Config`, its local serialized field types,
///   `DetTime`, and the `RawInode` the inode RPCs carry, which catch
///   wire-incompatible changes hidden by a default such as `Option<u64>::None`
///   to `Option<u32>::None`, or an added clock or inode field that leaves both
///   encodings of `Config` unchanged.
///
/// The source and JSON domains are deliberately stricter than the wire format
/// strictly requires. A documentation-only edit in one of these files can
/// require rebuilding the plugin; missing a wire-incompatible hidden variant
/// can make it decode the handshake or a subsequent request at the wrong offsets.
fn config_wire_default() -> Config {
    // `Config::default()` is intentionally environment-aware through Clap.
    // A coordinator may therefore inherit HERMIT_EPOCH/HERMIT_PRNG,
    // HERMIT_SCHED_SEED or HERMIT_CPU_PROFILE even though the separately loaded plugin receives a
    // minimal guest environment. Those invocation values are payload data,
    // not wire shape, and must not make two artifacts from the same source
    // reject one another. Explicit arguments take precedence over Clap's
    // environment provider; clear the scheduler override afterward to retain
    // the real environment-free default of `None` in the encoded shape.
    let mut config = Config::parse_from([
        "config-wire-fingerprint",
        &format!("--epoch={DEFAULT_EPOCH_STR}"),
        "--seed=0",
        "--sched-seed=0",
        "--cpu-profile=x86-64-v2",
    ]);
    config.sched_seed = None;
    // `BackendCapabilities` is defined in Reverie, outside
    // `CONFIG_DEFINITION_SOURCES`, so the source domain cannot see a change to
    // it; only its encoding can. Every field but the optional one encodes at
    // a fixed width (a bool or the `u8` width of `hardware_watchpoints` in
    // one byte, the `u64` signal set in eight), so any constant carries those
    // widths (see
    // `config_fingerprint_sees_the_width_of_every_backend_capability`). KVM's
    // is kept, as it was when one field was optional and only KVM's set it.
    // The optional `rcb_clock_contract` is set to ptrace's, the one contract
    // any backend states, because a `None` would encode as the same bare tag
    // whatever the contract's type became.
    config.backend = BackendCapabilities::KVM;
    config.backend.rcb_clock_contract = BackendCapabilities::PTRACE.rcb_clock_contract;
    config
}

pub fn config_wire_fingerprint() -> String {
    let config = config_wire_default();
    let wire = bincode::serde::encode_to_vec(&config, bincode::config::legacy())
        .expect("canonical Config wire default must encode with Reverie's bincode configuration");
    let named_shape = serde_json::to_string(&config)
        .expect("canonical Config wire default must encode as JSON for field-name checking");
    fingerprint_of_config_material(&wire, &named_shape, CONFIG_DEFINITION_SOURCES)
}

/// What differs when a coordinator announced `expected` as its
/// [`config_wire_fingerprint`] and this build's is another, or `None` when
/// they are equal. The in-guest LiteInst runtime reports this before it
/// decodes the coordinator's configuration
/// (<https://github.com/rrnewton/hermit/issues/3986>).
pub fn config_fingerprint_mismatch(expected: &std::ffi::OsStr) -> Option<String> {
    let ours = config_wire_fingerprint();
    (expected.to_str() != Some(ours.as_str())).then(|| {
        format!(
            "its configuration and clock RPC fingerprint is {ours}, Hermit's is {}",
            expected.to_string_lossy()
        )
    })
}

/// Domain-separated FNV-1a over wire bytes, named JSON, and defining source.
/// This is a mismatch detector, not a security boundary. Length-prefixing each
/// domain prevents two different source-file boundaries from hashing the same
/// concatenation.
fn fingerprint_of_config_material(
    wire: &[u8],
    named_shape: &str,
    definition_sources: &[&[u8]],
) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut update = |domain: u8, bytes: &[u8]| {
        for byte in std::iter::once(&domain)
            .chain((bytes.len() as u64).to_le_bytes().iter())
            .chain(bytes.iter())
        {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x1000_0000_01b3);
        }
    };
    update(0, wire);
    update(1, named_shape.as_bytes());
    for source in definition_sources {
        update(2, source);
    }
    format!("{hash:016x}")
}

/// `Config::signal_state_copy` for a serialized configuration written before
/// the field existed: on, the default.
fn signal_state_copy_default() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        let v: Vec<String> = vec![];
        Config::parse_from(v.iter())
    }
}

/// Serializes `config` as JSON in the form it had before [`Config::backend`]
/// existed.
///
/// [`Config::backend`] is left out. In its place, at the same position in
/// the object, are the fifteen
/// separate backend keys that a serialized configuration carried before,
/// under the names and in the order it carried them, each with a value
/// computed from `config`. [`Config::record_host_inputs`], which the legacy
/// form never had, is left out too, and reads back as false: this form serves
/// only DBT, whose launcher collects no host inputs. So is
/// [`Config::guest_may_inherit_a_terminal`], which is read only where
/// `BackendCapabilities::host_timed_signals_are_physical` holds, false for
/// DBT, and [`Config::in_guest_detlog_forward_policy`] and
/// [`Config::in_guest_site_patching_off`], which is unset for DBT, and
/// [`Config::launch_image`], which came after it. So is [`Config::target_timeslice_syscalls_only`],
/// added after this form froze; DBT runs without a PMU maximum, where the
/// option has no effect. So are [`Config::futex_wake_yields`],
/// [`Config::scheduler_turn_cost`], [`Config::cap_preemption_charge`] and
/// [`Config::virtual_timeout`], also added after this form froze, which
/// `hermit run` refuses with the DBT backend, and [`Config::replaying`], set
/// only by `hermit replay`, which runs only on the ptrace backend. So is
/// [`Config::inherited_seccomp`], added after this form froze, which reads
/// back as unknown, and [`Config::virtual_cpus`], which `hermit run`
/// refuses with the DBT backend and reads back as unset. So is
/// [`Config::in_guest_reserved_signals`], empty for DBT (it is set only for
/// in-guest runs), which reads back as empty, and
/// [`Config::in_guest_refuses_host_timed_producers`] and
/// [`Config::in_guest_signal_delivery`], false for DBT, which read back as
/// false. So is [`Config::notice_host_network_abstract_binds`], added after this form
/// froze, which reads back as false: DBT's runtime then prints no notice for
/// an abstract `AF_UNIX` bind. So is [`Config::eager_lone_preemptions`], added after this form froze,
/// which has no effect without a PMU timer and reads back as false. So is
/// [`Config::standard_stream_aliases`], added after this form froze, set only for KVM, which
/// reads back as unset. So is [`Config::stacktrace_at_schedule_positions`], also added later,
/// which reads back as false: the DBT runtime takes no stack traces. Every
/// other field is serialized exactly as `serde_json::to_string(config)`
/// serializes it.
///
/// A standalone DBT run (one with no coordinator) reads its configuration
/// from `HERMIT_DBT_DETCONFIG` in this form or in the versioned envelope of
/// [`encode_backend_config_json`]. `hermit run --backend=dbt` writes neither:
/// it launches DBT coordinated, and the runtime receives the typed `Config`
/// through the coordinator's first RPC frame.
///
/// Read the result with [`from_legacy_backend_json`], which recovers
/// [`Config::backend`] from the fifteen keys. A plain `Config`
/// deserialization ignores the keys and gives the field its serde default.
pub fn to_legacy_backend_json(config: &Config) -> serde_json::Result<String> {
    if config.strict_schedule_boundaries {
        return Err(serde::ser::Error::custom(
            "strict schedule boundaries cannot be represented by the legacy backend configuration",
        ));
    }
    let mut json = Vec::with_capacity(4096);
    let mut serializer = serde_json::Serializer::new(&mut json);
    config.serialize(legacy_backend_json::ConfigSerializer {
        inner: &mut serializer,
        config,
    })?;
    String::from_utf8(json).map_err(serde::ser::Error::custom)
}

/// The JSON keys, in order, under which a serialized [`Config`] carried the
/// backend facts before they moved into [`Config::backend`], each with the
/// value it has for `config`.
///
/// `backend_is_kvm` named the backend rather than a fact; its value is
/// `provides_process_signal_control`, a capability only KVM reports.
/// `kvm_shared_dequeue_timers` was a startup policy, not a fact: the
/// production launcher, hermit-cli, set it exactly when that capability and
/// [`Config::sequentialize_threads`] held. Its value is that conjunction, and
/// [`from_legacy_backend_json`] refuses any other value.
fn legacy_backend_keys(config: &Config) -> [(&'static str, bool); 15] {
    let backend = &config.backend;
    [
        // No backend installs a CPUID policy of its own any more, so the key
        // is always written false; see `from_legacy_backend_json`.
        ("cpuid_virtualized_by_backend", false),
        // Every backend now takes every madvise advice and refuses what it does
        // not implement itself, so the key is always written true, its value on
        // DBT, the form's only reader; see `from_legacy_backend_json`.
        ("backend_supports_madvise", true),
        (
            "discover_live_file_metadata",
            backend.tool_shares_guest_descriptor_table,
        ),
        (
            "detect_host_clock_futex_timeouts",
            backend.guest_clock_reads_bypass_backend,
        ),
        // Reverie's backend contract now requires every backend to leave the
        // syscall-clobbered registers as a native return would (with the
        // exceptions it names), so the key is always written false, its value
        // on DBT, the form's only reader; see `from_legacy_backend_json`.
        ("syscall_clobbers_virtualized_by_backend", false),
        (
            "cancel_killed_thread_rpcs",
            backend.needs_killed_thread_rpc_cancellation,
        ),
        (
            "backend_reports_physical_process_exits",
            backend.reports_physical_process_exits,
        ),
        (
            "backend_tracks_process_children",
            backend.tracks_process_children,
        ),
        (
            "backend_runs_exit_robust_list",
            backend.runs_exit_robust_list,
        ),
        (
            "backend_requires_thread_directed_process_signals",
            backend.requires_thread_directed_process_signals,
        ),
        ("backend_is_kvm", backend.provides_process_signal_control),
        (
            "kvm_shared_dequeue_timers",
            backend.provides_process_signal_control && config.sequentialize_threads,
        ),
        (
            "backend_supports_parked_write_signal_interruption",
            backend.supports_parked_write_signal_interruption,
        ),
        // No backend virtualizes the capability prctls any more; they reach
        // the kernel on every backend. Always false; see
        // `from_legacy_backend_json`.
        ("backend_virtualizes_capability_prctls", false),
        // A vfork child may register before or after the parent's call returns
        // on every backend now, and Detcore waits for it either way, so the key
        // is always written false, its value on DBT, the form's only reader;
        // see `from_legacy_backend_json`.
        ("backend_defers_vfork_child_registration", false),
    ]
}

/// Parses JSON written by [`to_legacy_backend_json`], or any JSON in the form
/// a serialized [`Config`] had before [`Config::backend`] existed, and recovers
/// [`Config::backend`] from the fifteen legacy backend keys.
///
/// The keys are read exactly as they were read while each was a `Config`
/// field: a key that is absent takes the default that field had, and a key of
/// the wrong type fails the parse. Each key then supplies every capability that
/// replaced a read of it, so a decoded configuration behaves as the same JSON
/// did before:
///
/// - `discover_live_file_metadata` supplies the three descriptor and loopback
///   facts that were each a read of it;
/// - `backend_is_kvm` supplies the two behaviours it selected that remain
///   capabilities, `provides_process_signal_control` and
///   `emulates_child_waits` (the other three are now answered by the backend
///   itself: refusing a non-leader exec is a typed refusal it reports per
///   exec, the `gettimeofday` repair runs on every backend through its
///   `time(2)` probe and `Guest::storable_memory_ranges`, and the user
///   address limit is each guest's own `Guest::user_address_limit` report);
/// - every other key supplies the one capability of the same meaning.
///
/// `backend_supports_madvise` supplies no capability any more: it said the
/// tool could forward every madvise advice to the backend, which every backend
/// now accepts, refusing an advice it does not implement itself. Since
/// `supports_madv_dontneed` was deleted, `MADV_DONTNEED` had been forwarded
/// whatever this key said.
///
/// This inverts [`to_legacy_backend_json`] for every capability value the keys
/// can express, which includes every backend's own constant except for
/// capabilities added after the legacy form froze: those never enter it, as
/// `record_host_inputs` does not, and read back as their default. There are
/// fifteen. `process_exits_complete_asynchronously` and
/// `virtualizes_guest_sigalrm` are true only for in-guest LiteInst, which
/// never travels through this DBT-only form.
/// `supports_blocked_wait_signal_interruption`,
/// `delivers_preemption_timer_events` and
/// `runs_guest_code_between_injections` read back as false, the value DBT
/// has and the value their earlier `Config` fields read back as; so do
/// `reports_sigreturns` and the four capabilities split out of
/// `supports_blocked_wait_signal_interruption` (`kernel_signal_state_is_guests`,
/// `kernel_signal_targets_are_predictable`,
/// `host_task_reads_reach_guest_threads`, `host_timed_signals_are_physical`),
/// false for DBT too. `provides_guest_backtraces` reads back as true, the
/// ptrace value it takes from the starting constant and DBT's value; the
/// two backends that report false (in-guest LiteInst and KVM) travel
/// through this form only inside the versioned envelope.
/// `places_orphans` reads back as false and `publishable_parent_death_signals`
/// as the empty set, DBT's values; KVM, the one backend that reports
/// otherwise, travels through this form only inside the versioned envelope
/// too. `rcb_clock_contract` reads back as `None`, DBT's value; ptrace, the
/// one backend that states a contract, travels through this form only
/// inside the versioned envelope as well.
/// `hardware_watchpoints` reads back as 0, DBT's.
///
/// `cpuid_virtualized_by_backend` supplies no capability either: it said the
/// backend installed a CPUID policy of its own, which no backend does any
/// more (KVM stopped with Reverie 0f4d8852). It is still written, as false,
/// so the legacy form keeps its bytes, and an input that
/// says `true` fails the parse rather than being read as a policy no backend
/// has.
///
/// `backend_defers_vfork_child_registration` supplies no capability either: it
/// said the backend registered a vfork child only after the parent's call
/// returned, which Reverie's backend contract now allows of every backend. It
/// is written as false, its value on DBT, so the legacy form keeps its bytes.
/// The reader still takes it as one of the fifteen keys, so it keeps its
/// position in the array form, must be a boolean and may appear once, and
/// ignores its value (KVM wrote true).
///
/// `backend_supports_madvise` is written as true, its value on DBT, so the
/// legacy form keeps its bytes. The reader still takes it as one of the fifteen
/// keys, so it keeps its position in the array form, must be a boolean and may
/// appear once, and ignores its value (KVM wrote false).
///
/// `syscall_clobbers_virtualized_by_backend` supplies no capability either: it
/// said the backend left the registers a `syscall` instruction clobbers as a
/// native return would, which Reverie's backend contract now requires of every
/// backend (with the exceptions that contract names). It is written as false,
/// its value on DBT, so the legacy form keeps its bytes. The reader still takes it as one of the fifteen keys, so
/// it keeps its position in the array form, must be a boolean and may appear
/// once, and ignores its value (SaBRe and KVM wrote true).
///
/// `backend_virtualizes_capability_prctls` supplies no capability either: it
/// said the backend answered the capability-set prctls itself, and every
/// backend now passes them to the kernel. It is written as false, so the
/// legacy form keeps its bytes, and `true` fails the parse.
///
/// `kvm_shared_dequeue_timers` supplies no capability. It was read into a
/// `Config` field that chose the controlled signal path, and that choice is
/// now made from the capabilities, [`Config::sequentialize_threads`] and the
/// control the backend offers. It is read as it was, a boolean that is false
/// when absent, and the parse then fails unless it equals `backend_is_kvm &&
/// sequentialize_threads`, the value the production launcher always wrote. An
/// accepted input whose capabilities are those of the backend that runs it
/// selects the path it selected before. An input that would select another,
/// such as `true` without `backend_is_kvm`, or absent beside `backend_is_kvm`
/// and sequentialized threads, fails the parse rather than being silently
/// reinterpreted. (`backend_is_kvm` given to a backend that offers no control,
/// such as DBT, already misdescribed that backend and stays outside this
/// guarantee; the production launcher never writes it.) A failed parse is
/// handled like any malformed value: the DBT standalone JSON reader falls
/// back to its strict default configuration and emits its
/// `could not parse HERMIT_DBT_DETCONFIG` warning.
///
/// Everything else is read as the legacy form was read, by `Config`'s own
/// derived deserializer with the legacy keys taken out before it sees them:
///
/// - In an object, a key the legacy form did not name is ignored, whatever its
///   value. `backend` is such a key, so it is ignored rather than read as the
///   field it is now; no production writer emits it. A key the legacy form
///   named fails the parse when it appears twice or holds the wrong type.
/// - A JSON array lists the fields by position, as serde reads any derived
///   struct. The positions are those of the legacy form, which are the key
///   order of [`to_legacy_backend_json`]: the fifteen legacy keys stand where
///   [`Config::backend`] stands. Too many elements fail the parse; too few
///   leave the fields
///   after them missing, each taking its default or failing the parse as the
///   derived deserializer decides for that field.
pub fn from_legacy_backend_json(json: &str) -> serde_json::Result<Config> {
    let mut deserializer = serde_json::Deserializer::from_str(json);
    let (mut config, legacy) = serde::Deserializer::deserialize_struct(
        &mut deserializer,
        "Config",
        &[],
        legacy_backend_json::LegacyConfigVisitor,
    )?;
    deserializer.end()?;
    if legacy.cpuid_virtualized_by_backend {
        return Err(<serde_json::Error as serde::de::Error>::custom(
            "cpuid_virtualized_by_backend is true, but no backend installs a CPUID \
             policy of its own; the key must be false or absent",
        ));
    }
    if legacy.backend_virtualizes_capability_prctls {
        return Err(<serde_json::Error as serde::de::Error>::custom(
            "backend_virtualizes_capability_prctls is true, but no backend answers the \
             capability prctls itself; the key must be false or absent",
        ));
    }
    let controlled = legacy.backend_is_kvm && config.sequentialize_threads;
    if legacy.kvm_shared_dequeue_timers != controlled {
        return Err(<serde_json::Error as serde::de::Error>::custom(
            format_args!(
                "kvm_shared_dequeue_timers is {} but backend_is_kvm && \
             sequentialize_threads is {controlled}; the key is no longer an \
             independent setting and must equal that conjunction",
                legacy.kvm_shared_dequeue_timers,
            ),
        ));
    }
    config.backend = legacy.capabilities();
    Ok(config)
}

/// Preserve the exact backend facts alongside an already encoded legacy config.
///
/// The legacy form couples some capabilities and omits others. When it can
/// express `backend` exactly, return `legacy_json` byte for byte, so a reader
/// of the legacy form reads it unchanged. Otherwise wrap it in a versioned
/// envelope whose typed backend replaces the legacy projection on decoding.
/// Non-backend fields keep the legacy form's semantics in either case.
pub fn encode_backend_config_json(
    legacy_json: String,
    backend: BackendCapabilities,
) -> serde_json::Result<String> {
    if from_legacy_backend_json(&legacy_json)?.backend == backend {
        return Ok(legacy_json);
    }
    serde_json::to_string(&VersionedBackendConfig {
        dbt_config_version: 1,
        config: serde_json::from_str(&legacy_json)?,
        backend,
    })
}

/// Read a legacy configuration or a versioned envelope with exact backend facts.
///
/// Any valid legacy object or positional array retains
/// [`from_legacy_backend_json`]'s behavior, including ignored unknown fields.
/// Otherwise the input must be an envelope: unknown versions, duplicate fields
/// and missing typed capabilities are parse errors.
/// This parser does not choose the caller's error policy. The admitted DBT
/// standalone JSON path retains its strict-default parse fallback; coordinated
/// startup receives its authoritative typed Config through the RPC handshake.
pub fn decode_backend_config_json(json: &str) -> serde_json::Result<Config> {
    // Legacy inputs take precedence: even dbt_config_version was an ignored
    // unknown key in that form, and its presence must not change a valid run.
    if let Ok(config) = from_legacy_backend_json(json) {
        return Ok(config);
    }
    // Deserialize the original bytes, not a Value map that would erase
    // duplicate keys before the typed deserializers can reject them.
    let envelope: VersionedBackendConfig<'_> = serde_json::from_str(json)?;
    if envelope.dbt_config_version != 1 {
        return Err(<serde_json::Error as serde::de::Error>::custom(
            format_args!(
                "unsupported DBT configuration version {}",
                envelope.dbt_config_version
            ),
        ));
    }
    // `BackendCapabilities` reads a capability Reverie added later (with
    // `#[serde(default)]`, such as `rcb_clock_contract`) as its default when
    // it is absent. The envelope is written whole by this build's
    // `encode_backend_config_json`, so every capability must be named: an
    // absent one is refused rather than defaulted.
    let named: EnvelopeBackendKeys = serde_json::from_str(json)?;
    let expected = serde_json::to_value(envelope.backend)?;
    if let Some(missing) = expected
        .as_object()
        .into_iter()
        .flat_map(|capabilities| capabilities.keys())
        .find(|capability| !named.backend.contains_key(capability.as_str()))
    {
        return Err(<serde_json::Error as serde::de::Error>::custom(
            format_args!("missing field `{missing}` in the envelope's backend"),
        ));
    }
    let mut config = from_legacy_backend_json(envelope.config.get())?;
    config.backend = envelope.backend;
    Ok(config)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionedBackendConfig<'a> {
    dbt_config_version: u32,
    #[serde(borrow)]
    config: &'a serde_json::value::RawValue,
    backend: BackendCapabilities,
}

/// The capability names a versioned envelope gives, read apart from the
/// typed [`VersionedBackendConfig`], which has already refused duplicate and
/// unknown keys.
#[derive(Deserialize)]
struct EnvelopeBackendKeys {
    backend: serde_json::Map<String, serde_json::Value>,
}

/// The names of the fifteen legacy backend keys, in the order
/// [`legacy_backend_keys`] gives them.
const LEGACY_BACKEND_KEY_NAMES: [&str; 15] = [
    "cpuid_virtualized_by_backend",
    "backend_supports_madvise",
    "discover_live_file_metadata",
    "detect_host_clock_futex_timeouts",
    "syscall_clobbers_virtualized_by_backend",
    "cancel_killed_thread_rpcs",
    "backend_reports_physical_process_exits",
    "backend_tracks_process_children",
    "backend_runs_exit_robust_list",
    "backend_requires_thread_directed_process_signals",
    "backend_is_kvm",
    "kvm_shared_dequeue_timers",
    "backend_supports_parked_write_signal_interruption",
    "backend_virtualizes_capability_prctls",
    "backend_defers_vfork_child_registration",
];

const fn legacy_backend_key_default_true() -> bool {
    true
}

/// The fifteen legacy backend keys with the types and serde defaults they had
/// as `Config` fields. It is deserialized from the keys
/// [`legacy_backend_json::LegacyConfigVisitor`] takes out of the input, so a
/// key given twice fails here as it failed as a `Config` field.
#[derive(Deserialize)]
struct LegacyBackendKeys {
    #[serde(default)]
    cpuid_virtualized_by_backend: bool,
    // Supplies no capability; declared so that the key, like the others,
    // fails the parse when it is given twice.
    #[expect(
        dead_code,
        reason = "read only to refuse a repeated key; its value is ignored"
    )]
    #[serde(default = "legacy_backend_key_default_true")]
    backend_supports_madvise: bool,
    #[serde(default)]
    discover_live_file_metadata: bool,
    #[serde(default)]
    detect_host_clock_futex_timeouts: bool,
    // Supplies no capability; declared so that the key, like the others,
    // fails the parse when it is given twice.
    #[expect(
        dead_code,
        reason = "read only to refuse a repeated key; its value is ignored"
    )]
    #[serde(default)]
    syscall_clobbers_virtualized_by_backend: bool,
    #[serde(default)]
    cancel_killed_thread_rpcs: bool,
    #[serde(default)]
    backend_reports_physical_process_exits: bool,
    #[serde(default = "legacy_backend_key_default_true")]
    backend_tracks_process_children: bool,
    #[serde(default = "legacy_backend_key_default_true")]
    backend_runs_exit_robust_list: bool,
    #[serde(default)]
    backend_requires_thread_directed_process_signals: bool,
    #[serde(default)]
    backend_is_kvm: bool,
    /// Read as it was while it was a `Config` field, then checked by
    /// [`from_legacy_backend_json`] against `backend_is_kvm` and
    /// [`Config::sequentialize_threads`]; it supplies no capability.
    #[serde(default)]
    kvm_shared_dequeue_timers: bool,
    #[serde(default = "legacy_backend_key_default_true")]
    backend_supports_parked_write_signal_interruption: bool,
    #[serde(default)]
    backend_virtualizes_capability_prctls: bool,
    // Supplies no capability; declared so that the key, like the others,
    // fails the parse when it is given twice.
    #[expect(
        dead_code,
        reason = "read only to refuse a repeated key; its value is ignored"
    )]
    #[serde(default)]
    backend_defers_vfork_child_registration: bool,
}

impl LegacyBackendKeys {
    /// The capabilities these keys describe; the inverse of
    /// [`legacy_backend_keys`]. Every field is assigned, so the starting
    /// constant contributes nothing a key decides.
    fn capabilities(&self) -> BackendCapabilities {
        let mut backend = BackendCapabilities::PTRACE;
        backend.tool_shares_guest_descriptor_table = self.discover_live_file_metadata;
        backend.rediscovers_descriptors_after_exec = self.discover_live_file_metadata;
        backend.loopback_pollers_yield_to_peers = self.discover_live_file_metadata;
        backend.guest_clock_reads_bypass_backend = self.detect_host_clock_futex_timeouts;
        backend.needs_killed_thread_rpc_cancellation = self.cancel_killed_thread_rpcs;
        // A legacy encoding predates this key. Only the ptrace tracer reports
        // child-exit publication, and it is the one backend that needs no
        // killed-thread RPC cancellation.
        backend.reports_child_exit_publication = !self.cancel_killed_thread_rpcs;
        // Newer than the legacy form. It took over the child-exit ledger's gates
        // from reports_child_exit_publication and has its value on every backend
        // the form can express (true for ptrace and the default, false for DBT,
        // the form's only reader), so it is read back the same way.
        backend.reports_kernel_child_exit_signals = backend.reports_child_exit_publication;
        backend.reports_physical_process_exits = self.backend_reports_physical_process_exits;
        backend.tracks_process_children = self.backend_tracks_process_children;
        backend.runs_exit_robust_list = self.backend_runs_exit_robust_list;
        backend.requires_thread_directed_process_signals =
            self.backend_requires_thread_directed_process_signals;
        backend.supports_parked_write_signal_interruption =
            self.backend_supports_parked_write_signal_interruption;
        backend.provides_process_signal_control = self.backend_is_kvm;
        backend.emulates_child_waits = self.backend_is_kvm;
        // A legacy encoding predates this key. DBT, the form's only reader,
        // promotes shared open-file descriptions before a native fork or exec
        // and alone requires thread-directed process signals (DBT's runtime
        // also states it from any source, `assert_dbt_execution_model`).
        backend.promotes_open_files_before_native_lifecycle =
            self.backend_requires_thread_directed_process_signals;
        // Added after the legacy form froze. Each was a `Config` field with no
        // legacy key that read back as false (or, for the four split out of
        // `supports_blocked_wait_signal_interruption`, a reading of one), and
        // each is false for DBT, the form's only reader, so a DBT
        // configuration stays exact.
        set_kernel_signal_facts(&mut backend, false);
        backend.reports_sigreturns = false;
        backend.delivers_preemption_timer_events = false;
        backend.runs_guest_code_between_injections = false;
        // Added after the legacy form froze, and false for DBT, the form's
        // only reader, whose guest threads are host tasks whose exits run
        // Linux's robust-list walk. KVM, the one backend that reports true,
        // is preserved exactly only by the versioned envelope of
        // `encode_backend_config_json`, never by these keys.
        backend.kernel_never_walks_robust_lists = false;
        // Added after the legacy form froze (Reverie 9443a6b2): it reads
        // back as the starting constant's true, which is also DBT's value,
        // the form's only reader, so a DBT configuration stays exact. The
        // two backends that report false (in-guest LiteInst and KVM) are
        // preserved exactly only by the versioned envelope of
        // `encode_backend_config_json`, never by these keys.
        backend.provides_guest_backtraces = true;
        // Added after the legacy form froze (Reverie f614c7c8): false and the
        // empty set, DBT's values, the form's only reader. KVM, the one
        // backend that reports otherwise, is preserved exactly only by the
        // versioned envelope of `encode_backend_config_json`, never by these
        // keys.
        backend.places_orphans = false;
        backend.publishable_parent_death_signals = 0;
        // Added after the legacy form froze (Reverie 1161 made it a `u8`
        // width). The legacy form carries no width for it: by absence it
        // says the bool false, and the minimal width that represents a
        // legacy bool false is 0, so it reads back as 0. A backend with a
        // nonzero width (ptrace reports 4 on x86_64) is preserved exactly
        // only by the versioned envelope of `encode_backend_config_json`,
        // never by these keys.
        backend.hardware_watchpoints = 0;
        // Added after the legacy form froze (Reverie 7535309b): `None`, no
        // qualified RCB clock, DBT's value, the form's only reader. Ptrace,
        // the one backend that states a contract, is preserved exactly only
        // by the versioned envelope of `encode_backend_config_json`, never by
        // these keys.
        backend.rcb_clock_contract = None;
        backend
    }
}

/// The serde adapters behind [`to_legacy_backend_json`] and
/// [`from_legacy_backend_json`].
///
/// The serializer adapter wraps the JSON serializer only at the top level,
/// where `Config`'s derived `Serialize` opens one struct and writes its fields
/// in declaration order. Nested values go straight to the wrapped serializer,
/// so they, and every field other than the two it replaces, are encoded
/// unchanged.
///
/// The deserializer adapters likewise wrap only the top-level object or array,
/// and hand every nested value to the wrapped JSON deserializer.
mod legacy_backend_json {
    use std::fmt;

    use serde::Deserialize;
    use serde::Serialize;
    use serde::de;
    use serde::de::DeserializeSeed;
    use serde::de::IgnoredAny;
    use serde::de::MapAccess;
    use serde::de::SeqAccess;
    use serde::de::Visitor;
    use serde::de::value::BoolDeserializer;
    use serde::de::value::MapAccessDeserializer;
    use serde::de::value::MapDeserializer;
    use serde::de::value::SeqAccessDeserializer;
    use serde::de::value::StringDeserializer;
    use serde::de::value::U64Deserializer;
    use serde::ser::Error as _;
    use serde::ser::Impossible;
    use serde::ser::SerializeStruct;
    use serde::ser::Serializer;

    use super::BackendCapabilities;
    use super::Config;
    use super::LEGACY_BACKEND_KEY_NAMES;
    use super::LegacyBackendKeys;
    use super::legacy_backend_keys;

    /// The keys `Config` has now that its legacy form did not. The legacy
    /// form ignored them, as it ignored every key it did not name.
    /// `record_host_inputs` has no legacy key and no legacy value: the legacy
    /// form serves only DBT, whose launcher collects no host inputs, so it is
    /// never written and always reads as false.
    /// So is `guest_may_inherit_a_terminal`, which is read only where
    /// `BackendCapabilities::host_timed_signals_are_physical` holds;
    /// that is false for DBT.
    /// `target_timeslice_syscalls_only` came after the legacy form froze; the
    /// legacy form never carried it, so it is never written and reads as false,
    /// the behaviour every legacy reader already has.
    /// `in_guest_detlog_forward_policy` is unset for DBT (its runtime forwards
    /// no records on a socket), so it is never written and reads as unset.
    /// `futex_wake_yields`, `pipe_wake_latency`, `scheduler_turn_cost` and
    /// `cap_preemption_charge` and `virtual_timeout` came after the legacy form
    /// froze too, and `hermit run` refuses them with the DBT backend.
    /// `replaying` is never written either: only `hermit replay` sets it, and
    /// replay runs only on the ptrace backend.
    /// `in_guest_site_patching_off` is false for DBT (it is set only for
    /// in-guest LiteInst), so it is never written and reads as false.
    /// `in_guest_reserved_signals` is empty for DBT (it is set only for
    /// in-guest runs), so it is never written and reads as empty.
    /// `in_guest_refuses_host_timed_producers` is false for DBT (it is set
    /// only for in-guest runs), so it is never written and reads as false.
    /// So is `in_guest_signal_delivery`, for the same reason.
    /// `launch_image` came after the legacy form froze: it is never written
    /// and reads as unset. DBT's runtime takes it from the coordinator's
    /// configuration instead, which declares DynamoRIO's library as the
    /// launcher.
    /// `inherited_seccomp` came after the legacy form froze too. It is never
    /// written and reads as unknown.
    /// `cpu_profile` and `cpu_profile_host` came after it as well. DBT refuses
    /// the x86-64-v3 profile, so they are never written and read as v2.
    /// `signal_state_copy` came after it too. The copy is never kept on DBT
    /// (its kernel signal state is not the guest's), so it is never written
    /// and reads as on, its serde default.
    /// And `container_mount_table`, which describes one run's container: the
    /// frozen form omits it, so it is never written and reads as absent. The
    /// coordinated startup's typed configuration, which is how Hermit
    /// launches DBT, carries the table.
    /// `mountinfo_mount_namespace` too, for the same reason: it is never
    /// written and reads as absent. So are `directory_placeholder` and
    /// `container_procfs`, which describe one run's container; the
    /// coordinated startup's typed configuration carries them.
    /// `signal_delivery_check_fatal` matters only where the copy is kept, so
    /// it is never written and reads as false.
    /// `strict_binder` is the host's `--strict` policy for binder, which the
    /// frozen form predates: never written, read as false.
    /// `work_span_report` came after it as well: the report is written by the
    /// scheduler, not the in-guest runtime, so it is never written and reads
    /// as unset.
    /// `signal_at` came after the form froze; the scheduler acts on it, not
    /// the runtime, so it is never written and reads as unset.
    /// `annotated_priority_replay` came after it too, and is false for every
    /// backend that reads the legacy form: priority replay needs a preemption
    /// timer, which none of them delivers, so it is never written and reads
    /// as false.
    /// `virtual_cpus` came after it too, and `hermit run` refuses it with the
    /// DBT backend: it is never written and reads as unset.
    /// `expired_wait_charge` too: the scheduler charges turns, not the
    /// runtime, so it is never written and reads as unset.
    /// `notice_host_network_abstract_binds` came after it too. It is never written and
    /// reads as false, so the DBT runtime prints no notice for an abstract
    /// `AF_UNIX` bind under host networking.
    /// `replay_edited_preemptions` came after it too. It is never written and
    /// reads as false; it cannot matter there, since a timerless backend
    /// such as DBT refuses `--replay-preemptions-from`, which it requires.
    /// `eager_lone_preemptions` came after it too. DBT has no PMU timer, so
    /// the option has no effect there; it is never written and reads as
    /// false.
    /// `access_trace`, `access_trace_from`, `access_trace_max_records` and
    /// `access_trace_max_bytes` came after it too; the scheduler writes the
    /// trace, and `hermit run` refuses it on DBT, so they are never written
    /// and read as unset, 0, unset and unset. So did `watch` and `watch_from`,
    /// which need the trace and the ptrace backend's watchpoints: never
    /// written, read as none and 0.
    // `chaos_timeslice_range` came after it too: the scheduler draws the
    // slices, not the runtime, so it is never written and reads as unset.
    // `chaos_timeslice_draw` and `chaos_timeslice` came with it, for the same
    // reason; they read as `slice` and unset.
    // `guest_ids` came after it too: DBT numbers its guest itself, so it
    // is never written and reads as `GuestIds::Namespace`, its serde
    // default.
    // `detlog_anon` came after it too: a coordinator-side ptrace-only
    // diagnostic the other backends refuse, so it is never written and
    // reads as absent, its serde default.
    /// `replay_continue_fairly` came after it too; only the scheduler reads
    /// it, so it is never written and reads as false.
    /// `standard_stream_aliases` came after it too, and is set only for KVM;
    /// it is never written and reads as unset.
    /// `sleep_registration` too: the scheduler registers sleeps, not the
    /// runtime, so it is never written and reads as unset.
    /// `rewrite_exclusives_unmarked` came after it as well: the CLI rewrites
    /// the files before the run and the run's global state reports them, not
    /// the runtime, so it is never written and reads as empty.
    /// `rewrite_exclusives_in_memory` came after it as well: the in-memory
    /// rewrite of aarch64 exclusive loops is done by the ptrace backend's
    /// global state, and no backend that reads the legacy form does it, so it
    /// is never written and reads as false.
    /// `rewrite_exclusives_overlay` came after it too: the CLI replaced the
    /// files before the run, and only the record states it, so it is never
    /// written and reads as false.
    /// `post_crash_timeslice` came after it too, and the scheduler acts on it
    /// as well, so it is never written and reads as off.
    /// `stacktrace_at_schedule_positions` came after it too. It is never
    /// written and reads as false: the DBT runtime takes no stack traces.
    /// `property_snapshot` came after it too, and only hermit-cli sets it, on
    /// Android, where DBT does not run; it is never written and reads as
    /// unset.
    /// `rewrite_exclusives_overlay` likewise: the run's global state names the
    /// files the overlay rewrite left, so it is never written and reads as
    /// empty.
    /// `chaos_instruction_offset` came after it as well: the ptrace backend's
    /// precise timer takes its single steps, and `hermit run` refuses it on
    /// every other backend, so it is never written and reads as 0.
    /// `rewrite_exclusives_unmarked` likewise: it is never written and reads
    /// as empty.
    /// `rewrite_exclusives_overlay_requested` came after it too: the CLI
    /// states the requested overlay from its flags, and only the record
    /// states its provenance, so it is never written and reads as false.
    /// `untrap_passthrough_syscalls` came after it too. `hermit run` refuses it
    /// on every backend but ptrace, so it is never written and reads as false.
    const FIELDS_WITHOUT_A_LEGACY_KEY: [&str; 60] = [
        "backend",
        "strict_schedule_boundaries",
        "record_host_inputs",
        "guest_may_inherit_a_terminal",
        "in_guest_detlog_forward_policy",
        "in_guest_site_patching_off",
        "in_guest_reserved_signals",
        "in_guest_refuses_host_timed_producers",
        "launch_image",
        "target_timeslice_syscalls_only",
        "futex_wake_yields",
        "pipe_wake_latency",
        "expired_wait_charge",
        "sleep_registration",
        "replaying",
        "scheduler_turn_cost",
        "cap_preemption_charge",
        "virtual_timeout",
        "inherited_seccomp",
        "cpu_profile",
        "cpu_profile_host",
        "cpu_profile_source",
        "signal_state_copy",
        "container_mount_table",
        "mountinfo_mount_namespace",
        "directory_placeholder",
        "container_procfs",
        "signal_delivery_check_fatal",
        "strict_binder",
        "work_span_report",
        "signal_at",
        "post_crash_timeslice",
        "in_guest_signal_delivery",
        "annotated_priority_replay",
        "virtual_cpus",
        "notice_host_network_abstract_binds",
        "replay_edited_preemptions",
        "eager_lone_preemptions",
        "chaos_timeslice_range",
        "chaos_timeslice_draw",
        "chaos_timeslice",
        "guest_ids",
        "detlog_anon",
        "access_trace",
        "access_trace_from",
        "access_trace_max_records",
        "access_trace_max_bytes",
        "replay_continue_fairly",
        "standard_stream_aliases",
        "rewrite_exclusives_unmarked",
        "rewrite_exclusives_in_memory",
        "rewrite_exclusives_overlay",
        "rewrite_exclusives_overlay_requested",
        "count_store_exclusives",
        "stacktrace_at_schedule_positions",
        "property_snapshot",
        "watch",
        "watch_from",
        "chaos_instruction_offset",
        "untrap_passthrough_syscalls",
    ];

    /// Reads the top-level object or array of a legacy configuration. The
    /// derived `Config` deserializer reads every field; this takes the legacy
    /// backend keys out of the input before it sees them and collects them for
    /// [`LegacyBackendKeys`].
    pub(super) struct LegacyConfigVisitor;

    impl<'de> Visitor<'de> for LegacyConfigVisitor {
        type Value = (Config, LegacyBackendKeys);

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("struct Config")
        }

        fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
            let mut legacy = Vec::new();
            let config = Config::deserialize(MapAccessDeserializer::new(LegacyKeys {
                inner: map,
                legacy: &mut legacy,
            }))?;
            Ok((config, legacy_backend_keys_from(legacy)?))
        }

        fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
            let fields = config_field_order().map_err(<A::Error as de::Error>::custom)?;
            let mut legacy = Vec::new();
            let config = Config::deserialize(SeqAccessDeserializer::new(LegacyPositions {
                inner: seq,
                fields: fields.iter(),
                legacy: &mut legacy,
            }))?;
            Ok((config, legacy_backend_keys_from(legacy)?))
        }
    }

    /// Reads the collected legacy keys with their old types and defaults. A
    /// key that appeared twice is collected twice, and fails here as a
    /// duplicate field.
    fn legacy_backend_keys_from<E: de::Error>(
        legacy: Vec<(&'static str, bool)>,
    ) -> Result<LegacyBackendKeys, E> {
        LegacyBackendKeys::deserialize(MapDeserializer::<_, E>::new(legacy.into_iter()))
    }

    /// The top-level object's entries, with each legacy backend key read as the
    /// boolean it was and collected rather than passed on, and each key of
    /// [`FIELDS_WITHOUT_A_LEGACY_KEY`] skipped as the unknown key it was.
    struct LegacyKeys<'l, A> {
        inner: A,
        legacy: &'l mut Vec<(&'static str, bool)>,
    }

    impl<'de, A: MapAccess<'de>> MapAccess<'de> for LegacyKeys<'_, A> {
        type Error = A::Error;

        fn next_key_seed<K: DeserializeSeed<'de>>(
            &mut self,
            seed: K,
        ) -> Result<Option<K::Value>, A::Error> {
            while let Some(key) = self.inner.next_key::<String>()? {
                if let Some(name) = LEGACY_BACKEND_KEY_NAMES
                    .into_iter()
                    .find(|name| *name == key)
                {
                    let value = self.inner.next_value::<bool>()?;
                    self.legacy.push((name, value));
                } else if FIELDS_WITHOUT_A_LEGACY_KEY.contains(&key.as_str()) {
                    self.inner.next_value::<IgnoredAny>()?;
                } else {
                    return seed.deserialize(StringDeserializer::new(key)).map(Some);
                }
            }
            Ok(None)
        }

        fn next_value_seed<V: DeserializeSeed<'de>>(
            &mut self,
            seed: V,
        ) -> Result<V::Value, A::Error> {
            self.inner.next_value_seed(seed)
        }
    }

    /// The top-level array's elements, in the legacy form's positions. The
    /// derived `Config` deserializer asks for one element per field in
    /// declaration order; at [`Config::backend`] this reads up to fifteen
    /// elements as the legacy backend keys, and neither the field it asks for
    /// there nor [`Config::record_host_inputs`],
    /// [`Config::guest_may_inherit_a_terminal`],
    /// [`Config::in_guest_detlog_forward_policy`],
    /// [`Config::in_guest_site_patching_off`],
    /// [`Config::in_guest_reserved_signals`],
    /// [`Config::in_guest_refuses_host_timed_producers`],
    /// [`Config::in_guest_signal_delivery`], [`Config::launch_image`],
    /// [`Config::target_timeslice_syscalls_only`],
    /// [`Config::futex_wake_yields`], [`Config::replaying`],
    /// [`Config::scheduler_turn_cost`], [`Config::cap_preemption_charge`],
    /// [`Config::virtual_timeout`], [`Config::inherited_seccomp`] or [`Config::replay_edited_preemptions`] takes
    /// an element. Each gets a
    /// placeholder; [`super::from_legacy_backend_json`] replaces the first.
    struct LegacyPositions<'f, 'l, A> {
        inner: A,
        fields: std::slice::Iter<'f, String>,
        legacy: &'l mut Vec<(&'static str, bool)>,
    }

    impl<'de, A: SeqAccess<'de>> SeqAccess<'de> for LegacyPositions<'_, '_, A> {
        type Error = A::Error;

        fn next_element_seed<T: DeserializeSeed<'de>>(
            &mut self,
            seed: T,
        ) -> Result<Option<T::Value>, A::Error> {
            match self.fields.next().map(String::as_str) {
                Some("backend") => {
                    for name in LEGACY_BACKEND_KEY_NAMES {
                        match self.inner.next_element::<bool>()? {
                            Some(value) => self.legacy.push((name, value)),
                            // A short array; the rest take their defaults.
                            None => break,
                        }
                    }
                    let placeholder = serde_json::to_value(BackendCapabilities::PTRACE)
                        .map_err(<A::Error as de::Error>::custom)?;
                    seed.deserialize(placeholder)
                        .map(Some)
                        .map_err(<A::Error as de::Error>::custom)
                }
                Some(
                    "strict_schedule_boundaries"
                    | "record_host_inputs"
                    | "guest_may_inherit_a_terminal"
                    | "in_guest_site_patching_off"
                    | "in_guest_refuses_host_timed_producers"
                    | "in_guest_signal_delivery"
                    | "annotated_priority_replay"
                    | "target_timeslice_syscalls_only"
                    | "futex_wake_yields"
                    | "cap_preemption_charge"
                    | "replaying"
                    | "signal_delivery_check_fatal"
                    | "container_procfs"
                    | "notice_host_network_abstract_binds"
                    | "replay_edited_preemptions"
                    | "eager_lone_preemptions"
                    | "replay_continue_fairly"
                    | "strict_binder"
                    | "rewrite_exclusives_in_memory"
                    | "rewrite_exclusives_overlay_requested"
                    | "count_store_exclusives"
                    | "stacktrace_at_schedule_positions"
                    | "untrap_passthrough_syscalls",
                ) => seed.deserialize(BoolDeserializer::new(false)).map(Some),
                // Empty, as its serde default is.
                Some("rewrite_exclusives_overlay" | "rewrite_exclusives_unmarked") => seed
                    .deserialize(serde_json::Value::Array(Vec::new()))
                    .map(Some)
                    .map_err(<A::Error as de::Error>::custom),
                // Empty, as its serde default is; and 0, the default.
                Some(
                    "in_guest_reserved_signals"
                    | "access_trace_from"
                    | "watch_from"
                    | "chaos_instruction_offset",
                ) => seed.deserialize(U64Deserializer::new(0)).map(Some),
                // None, as its serde default is.
                Some("watch") => seed
                    .deserialize(serde_json::Value::Array(Vec::new()))
                    .map(Some)
                    .map_err(<A::Error as de::Error>::custom),
                // On, as its serde default is.
                Some("signal_state_copy") => {
                    seed.deserialize(BoolDeserializer::new(true)).map(Some)
                }
                // Unset, as its serde default is.
                Some(
                    "in_guest_detlog_forward_policy"
                    | "launch_image"
                    | "detlog_anon"
                    | "scheduler_turn_cost"
                    | "virtual_timeout"
                    | "pipe_wake_latency"
                    | "expired_wait_charge"
                    | "sleep_registration"
                    | "container_mount_table"
                    | "mountinfo_mount_namespace"
                    | "directory_placeholder"
                    | "work_span_report"
                    | "signal_at"
                    | "virtual_cpus"
                    | "chaos_timeslice_range"
                    | "chaos_timeslice"
                    | "access_trace"
                    | "access_trace_max_records"
                    | "access_trace_max_bytes"
                    | "standard_stream_aliases"
                    | "post_crash_timeslice"
                    | "property_snapshot",
                ) => seed
                    .deserialize(serde_json::Value::Null)
                    .map(Some)
                    .map_err(<A::Error as de::Error>::custom),
                // Per-slice, as its serde default is.
                Some("chaos_timeslice_draw") => {
                    let slice = serde_json::to_value(super::ChaosTimesliceDraw::Slice)
                        .map_err(<A::Error as de::Error>::custom)?;
                    seed.deserialize(slice)
                        .map(Some)
                        .map_err(<A::Error as de::Error>::custom)
                }
                // x86-64-v2, as their serde defaults are.
                Some("cpu_profile") => {
                    let v2 = serde_json::to_value(crate::cpu_profile::CpuProfile::X86_64V2)
                        .map_err(<A::Error as de::Error>::custom)?;
                    seed.deserialize(v2)
                        .map(Some)
                        .map_err(<A::Error as de::Error>::custom)
                }
                Some("cpu_profile_host" | "cpu_profile_source") => seed
                    .deserialize(serde_json::Value::Null)
                    .map(Some)
                    .map_err(<A::Error as de::Error>::custom),
                // Numbered by a PID namespace, as its serde default is.
                Some("guest_ids") => {
                    let namespace = serde_json::to_value(crate::pid::GuestIds::Namespace)
                        .map_err(<A::Error as de::Error>::custom)?;
                    seed.deserialize(namespace)
                        .map(Some)
                        .map_err(<A::Error as de::Error>::custom)
                }
                // Unknown, as its serde default is.
                Some("inherited_seccomp") => {
                    let unknown = serde_json::to_value(super::InheritedSeccomp::Unknown)
                        .map_err(<A::Error as de::Error>::custom)?;
                    seed.deserialize(unknown)
                        .map(Some)
                        .map_err(<A::Error as de::Error>::custom)
                }
                _ => self.inner.next_element_seed(seed),
            }
        }
    }

    /// The names of `Config`'s serialized fields in declaration order, which
    /// is the order its derived deserializer reads an array in: no field is
    /// serialized without being deserialized or the reverse.
    ///
    /// The configuration serialized to learn them is built by Clap from no
    /// arguments, with every environment binding removed before parsing.
    /// Building `Config::command()` still reads the bound variables (Clap's
    /// `Arg::env` reads a variable's value when the binding is made), but
    /// removing the bindings discards those values, so no ambient value
    /// reaches the parse. `Config::default()` would instead parse
    /// HERMIT_EPOCH, HERMIT_PRNG, HERMIT_SCHED_SEED and HERMIT_CPU_PROFILE, and exit the process
    /// when one of them does not parse; the legacy decoder was not affected
    /// by any of them.
    fn config_field_order() -> serde_json::Result<Vec<String>> {
        let config = environment_free_config().map_err(<serde_json::Error as de::Error>::custom)?;
        let json = serde_json::to_string(&config)?;
        Ok(serde_json::from_str::<KeyOrder>(&json)?.0)
    }

    /// `Config` as Clap builds it from no arguments and no environment.
    fn environment_free_config() -> Result<Config, clap::Error> {
        use clap::CommandFactory;
        use clap::FromArgMatches;

        let matches = Config::command()
            .mut_args(|arg| arg.env(None::<&'static str>))
            .try_get_matches_from(["config-field-order"])?;
        Config::from_arg_matches(&matches)
    }

    /// The keys of a JSON object, in the order they appear.
    pub(super) struct KeyOrder(pub(super) Vec<String>);

    impl<'de> Deserialize<'de> for KeyOrder {
        fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct KeyOrderVisitor;

            impl<'de> Visitor<'de> for KeyOrderVisitor {
                type Value = KeyOrder;

                fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                    formatter.write_str("a JSON object")
                }

                fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<KeyOrder, A::Error> {
                    let mut keys = Vec::new();
                    while let Some(key) = map.next_key::<String>()? {
                        map.next_value::<IgnoredAny>()?;
                        keys.push(key);
                    }
                    Ok(KeyOrder(keys))
                }
            }

            deserializer.deserialize_map(KeyOrderVisitor)
        }
    }

    pub(super) struct ConfigSerializer<'c, S> {
        pub(super) inner: S,
        pub(super) config: &'c Config,
    }

    pub(super) struct ConfigFields<'c, S> {
        inner: S,
        config: &'c Config,
        wrote_backend_keys: bool,
    }

    fn not_a_config<E: serde::ser::Error>() -> E {
        E::custom("the legacy backend JSON encoding applies only to a Detcore Config")
    }

    impl<'c, S: Serializer> Serializer for ConfigSerializer<'c, S> {
        type Ok = S::Ok;
        type Error = S::Error;
        type SerializeSeq = Impossible<S::Ok, S::Error>;
        type SerializeTuple = Impossible<S::Ok, S::Error>;
        type SerializeTupleStruct = Impossible<S::Ok, S::Error>;
        type SerializeTupleVariant = Impossible<S::Ok, S::Error>;
        type SerializeMap = Impossible<S::Ok, S::Error>;
        type SerializeStruct = ConfigFields<'c, S::SerializeStruct>;
        type SerializeStructVariant = Impossible<S::Ok, S::Error>;

        fn is_human_readable(&self) -> bool {
            self.inner.is_human_readable()
        }

        fn serialize_struct(
            self,
            name: &'static str,
            len: usize,
        ) -> Result<Self::SerializeStruct, S::Error> {
            // Two fields are replaced by fifteen keys.
            let len = len + legacy_backend_keys(self.config).len() - 2;
            Ok(ConfigFields {
                inner: self.inner.serialize_struct(name, len)?,
                config: self.config,
                wrote_backend_keys: false,
            })
        }

        fn serialize_bool(self, _: bool) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_i8(self, _: i8) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_i16(self, _: i16) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_i32(self, _: i32) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_i64(self, _: i64) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_u8(self, _: u8) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_u16(self, _: u16) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_u32(self, _: u32) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_u64(self, _: u64) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_f32(self, _: f32) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_f64(self, _: f64) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_char(self, _: char) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_str(self, _: &str) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_bytes(self, _: &[u8]) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_none(self) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_some<T: ?Sized + Serialize>(self, _: &T) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_unit(self) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_unit_struct(self, _: &'static str) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_unit_variant(
            self,
            _: &'static str,
            _: u32,
            _: &'static str,
        ) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_newtype_struct<T: ?Sized + Serialize>(
            self,
            _: &'static str,
            _: &T,
        ) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_newtype_variant<T: ?Sized + Serialize>(
            self,
            _: &'static str,
            _: u32,
            _: &'static str,
            _: &T,
        ) -> Result<S::Ok, S::Error> {
            Err(not_a_config())
        }
        fn serialize_seq(self, _: Option<usize>) -> Result<Self::SerializeSeq, S::Error> {
            Err(not_a_config())
        }
        fn serialize_tuple(self, _: usize) -> Result<Self::SerializeTuple, S::Error> {
            Err(not_a_config())
        }
        fn serialize_tuple_struct(
            self,
            _: &'static str,
            _: usize,
        ) -> Result<Self::SerializeTupleStruct, S::Error> {
            Err(not_a_config())
        }
        fn serialize_tuple_variant(
            self,
            _: &'static str,
            _: u32,
            _: &'static str,
            _: usize,
        ) -> Result<Self::SerializeTupleVariant, S::Error> {
            Err(not_a_config())
        }
        fn serialize_map(self, _: Option<usize>) -> Result<Self::SerializeMap, S::Error> {
            Err(not_a_config())
        }
        fn serialize_struct_variant(
            self,
            _: &'static str,
            _: u32,
            _: &'static str,
            _: usize,
        ) -> Result<Self::SerializeStructVariant, S::Error> {
            Err(not_a_config())
        }
    }

    impl<S: SerializeStruct> SerializeStruct for ConfigFields<'_, S> {
        type Ok = S::Ok;
        type Error = S::Error;

        fn serialize_field<T: ?Sized + Serialize>(
            &mut self,
            key: &'static str,
            value: &T,
        ) -> Result<(), S::Error> {
            match key {
                "backend" => {
                    for (legacy_key, legacy_value) in legacy_backend_keys(self.config) {
                        self.inner.serialize_field(legacy_key, &legacy_value)?;
                    }
                    self.wrote_backend_keys = true;
                    Ok(())
                }
                // No legacy key; see FIELDS_WITHOUT_A_LEGACY_KEY.
                "strict_schedule_boundaries"
                | "record_host_inputs"
                | "guest_may_inherit_a_terminal"
                | "in_guest_detlog_forward_policy"
                | "in_guest_site_patching_off"
                | "in_guest_reserved_signals"
                | "in_guest_refuses_host_timed_producers"
                | "launch_image"
                | "target_timeslice_syscalls_only"
                | "futex_wake_yields"
                | "pipe_wake_latency"
                | "expired_wait_charge"
                | "sleep_registration"
                | "replaying"
                | "scheduler_turn_cost"
                | "cap_preemption_charge"
                | "virtual_timeout"
                | "inherited_seccomp"
                | "cpu_profile"
                | "cpu_profile_host"
                | "cpu_profile_source"
                | "signal_state_copy"
                | "container_mount_table"
                | "mountinfo_mount_namespace"
                | "directory_placeholder"
                | "container_procfs"
                | "signal_delivery_check_fatal"
                | "strict_binder"
                | "work_span_report"
                | "signal_at"
                | "post_crash_timeslice"
                | "in_guest_signal_delivery"
                | "annotated_priority_replay"
                | "virtual_cpus"
                | "notice_host_network_abstract_binds"
                | "replay_edited_preemptions"
                | "eager_lone_preemptions"
                | "chaos_timeslice_range"
                | "chaos_timeslice_draw"
                | "chaos_timeslice"
                | "guest_ids"
                | "detlog_anon"
                | "standard_stream_aliases"
                | "rewrite_exclusives_unmarked" => Ok(()),
                "access_trace"
                | "access_trace_from"
                | "access_trace_max_records"
                | "access_trace_max_bytes"
                | "rewrite_exclusives_in_memory"
                | "rewrite_exclusives_overlay"
                | "rewrite_exclusives_overlay_requested"
                | "count_store_exclusives"
                | "stacktrace_at_schedule_positions"
                | "property_snapshot"
                | "watch"
                | "watch_from"
                | "chaos_instruction_offset"
                | "untrap_passthrough_syscalls" => Ok(()),
                "replay_continue_fairly" => Ok(()),
                _ => self.inner.serialize_field(key, value),
            }
        }

        fn skip_field(&mut self, key: &'static str) -> Result<(), S::Error> {
            self.inner.skip_field(key)
        }

        fn end(self) -> Result<S::Ok, S::Error> {
            if !self.wrote_backend_keys {
                return Err(S::Error::custom(
                    "Config serialized no `backend` field to replace with the legacy backend keys",
                ));
            }
            self.inner.end()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_post_crash_timeslice;

    /// A set bound is refused, with the reason, wherever it would not be in
    /// effect: not above the run's bound, no bound, unsequentialized threads.
    #[test]
    fn a_post_crash_bound_is_refused_where_it_would_not_take_effect() {
        let nz = std::num::NonZeroU64::new;
        let config = |bound: u64, max: Option<u64>, sequentialize: bool| super::Config {
            post_crash_timeslice: nz(bound),
            max_timeslice: max.and_then(nz),
            sequentialize_threads: sequentialize,
            ..Default::default()
        };
        assert_eq!(
            config(200, Some(100), true).post_crash_timeslice_refusal(),
            None
        );
        assert_eq!(
            super::Config::default().post_crash_timeslice_refusal(),
            None,
            "not set"
        );
        for (max, sequentialize, why) in [
            (Some(200), true, "not above --max-timeslice=200"),
            (Some(300), true, "not above --max-timeslice=300"),
            (None, true, "needs a --max-timeslice"),
            (Some(100), false, "needs sequentialized threads"),
        ] {
            let refusal = config(200, max, sequentialize)
                .post_crash_timeslice_refusal()
                .unwrap();
            assert!(refusal.contains(why), "{refusal}");
        }
    }

    /// `--post-crash-timeslice` takes off, default (the default
    /// `--max-timeslice`) or a bound of at least one RCB.
    #[test]
    fn post_crash_timeslice_parses_off_default_and_bounds() {
        assert_eq!(parse_post_crash_timeslice("off").unwrap(), None);
        assert_eq!(
            parse_post_crash_timeslice("default").unwrap(),
            std::num::NonZeroU64::new(super::DEFAULT_MAX_TIMESLICE)
        );
        assert_eq!(
            parse_post_crash_timeslice("5000000").unwrap(),
            std::num::NonZeroU64::new(5_000_000)
        );
        for bad in ["0", "5", "disabled", "fast"] {
            assert!(parse_post_crash_timeslice(bad).is_err(), "{bad}");
        }
    }

    use super::*;

    /// `--chaos-timeslice-range` takes MIN..MAX virtual nanoseconds, both at
    /// least one RCB and MIN no larger than MAX, prints back as given, needs
    /// `--chaos`, and maps uniform draws log-uniformly onto the range.
    fn range_max(config: &Config) -> NonZeroU64 {
        config.chaos_timeslice_range.unwrap().max
    }

    #[test]
    fn a_chaos_timeslice_range_is_min_to_max_nanoseconds_drawn_log_uniformly() {
        let range: TimesliceRange = "1000..1000000000".parse().unwrap();
        assert_eq!((range.min.get(), range.max.get()), (1_000, 1_000_000_000));
        assert_eq!(range.to_string(), "1000..1000000000");
        assert_eq!("10..10".parse::<TimesliceRange>().unwrap().max.get(), 10);
        for refused in ["1000", "9..1000", "2000..1000", "a..b", "1000..", "..1000"] {
            assert!(refused.parse::<TimesliceRange>().is_err(), "{refused}");
        }
        assert!(
            Config::try_parse_from(["config", "--chaos-timeslice-range=1000..2000"]).is_err(),
            "a range without --chaos was accepted"
        );
        let config =
            Config::try_parse_from(["config", "--chaos", "--chaos-timeslice-range=1000..2000"])
                .unwrap();
        assert_eq!(
            config.chaos_timeslice_range,
            Some("1000..2000".parse().unwrap())
        );
        assert!(
            Config::try_parse_from([
                "config",
                "--chaos",
                "--chaos-timeslice-range=1000..2000",
                "--target-timeslice=1000",
            ])
            .is_err(),
            "a range beside --target-timeslice was accepted"
        );
        // The rendered command line carries the range, not the maximum it
        // sets, and parses back to the same range.
        let mut resolved = config.clone();
        resolved.max_timeslice = Some(range_max(&config));
        let rendered = resolved.to_string();
        assert!(
            rendered.contains(" --chaos-timeslice-range=1000..2000"),
            "{rendered}"
        );
        assert!(!rendered.contains("--max-timeslice"), "{rendered}");
        let reparsed = Config::parse_from(
            std::iter::once("detcore".to_string())
                .chain(rendered.split_whitespace().map(String::from)),
        );
        assert_eq!(reparsed.chaos_timeslice_range, config.chaos_timeslice_range);
        // A saved configuration is checked as the command line is.
        assert_eq!(
            serde_json::to_value(config.chaos_timeslice_range).unwrap(),
            "1000..2000"
        );
        assert!(serde_json::from_str::<TimesliceRange>("\"2000..1000\"").is_err());
        assert_eq!(config.chaos_timeslice_draw, ChaosTimesliceDraw::Slice);
        assert_eq!(config.chaos_timeslice, None);
        let config = Config::try_parse_from([
            "config",
            "--chaos",
            "--chaos-timeslice-range=1000..2000",
            "--chaos-timeslice-draw=run",
        ])
        .unwrap();
        assert_eq!(config.chaos_timeslice_draw, ChaosTimesliceDraw::Run);
        let bare =
            Config::try_parse_from(["config", "--chaos", "--chaos-timeslice-draw=run"]).unwrap();
        assert_eq!(bare.chaos_timeslice_draw, ChaosTimesliceDraw::Run);
        assert_eq!(
            bare.chaos_timeslice_range, None,
            "the CLI resolves the range"
        );
        assert_eq!(
            Config::default().chaos_timeslice_draw,
            ChaosTimesliceDraw::Slice
        );
        for args in [
            vec!["config", "--chaos-timeslice-draw=run"],
            vec!["config", "--chaos", "--chaos-timeslice-draw=slice"],
            vec!["config", "--chaos", "--chaos-timeslice-draw=fixed"],
            vec![
                "config",
                "--chaos",
                "--chaos-timeslice-draw=run",
                "--target-timeslice=1000",
            ],
        ] {
            assert!(Config::try_parse_from(&args).is_err(), "{args:?}");
        }
        let rendered = bare.to_string();
        assert!(
            rendered.contains(" --chaos-timeslice-draw=run"),
            "{rendered}"
        );
        assert!(!rendered.contains("--chaos-timeslice-range"), "{rendered}");
        let reparsed =
            Config::parse_from(std::iter::once("detcore").chain(rendered.split_whitespace()));
        assert_eq!(reparsed.chaos_timeslice_draw, ChaosTimesliceDraw::Run);
        assert_eq!(reparsed.chaos_timeslice_range, None);
        assert_eq!(
            RECOMMENDED_CHAOS_TIMESLICE_RANGE,
            "1000..200000000".parse().unwrap()
        );
        let command = <Config as clap::CommandFactory>::command();
        for id in ["chaos", "chaos_timeslice_draw"] {
            let text = command
                .get_arguments()
                .find(|arg| arg.get_id() == id)
                .unwrap()
                .get_long_help()
                .unwrap()
                .to_string();
            assert!(
                text.contains(&RECOMMENDED_CHAOS_TIMESLICE_RANGE.to_string()),
                "{text}"
            );
            assert!(text.contains("time limits"), "{text}");
        }
        // A drawn maximum renders as the range and the draw, which parse
        // back to the same request.
        let mut drawn = config.clone();
        drawn.max_timeslice = NonZeroU64::new(1234);
        drawn.chaos_timeslice = NonZeroU64::new(1234);
        let rendered = drawn.to_string();
        assert!(
            rendered.contains(" --chaos-timeslice-range=1000..2000 --chaos-timeslice-draw=run"),
            "{rendered}"
        );
        assert!(!rendered.contains("--max-timeslice"), "{rendered}");
        let reparsed = Config::parse_from(
            std::iter::once("detcore".to_string())
                .chain(rendered.split_whitespace().map(String::from)),
        );
        assert_eq!(reparsed.chaos_timeslice_range, config.chaos_timeslice_range);
        assert_eq!(reparsed.chaos_timeslice_draw, ChaosTimesliceDraw::Run);

        // Six decades, an equal share each: u = k/6 lands on 10^(3+k).
        for k in 0..=6 {
            let expected = 10f64.powi(3 + k);
            let point = range.log_uniform(f64::from(k) / 6.0);
            assert!((point / expected - 1.0).abs() < 1e-9, "{k}: {point}");
        }
        let point: TimesliceRange = "4321..4321".parse().unwrap();
        assert_eq!(point.log_uniform(0.7), 4321.0);
    }

    /// `--chaos-instruction-offset` needs a real stop at every chaos slice
    /// end; nothing else does, and the option is inert without `--chaos`.
    #[test]
    fn only_a_chaos_instruction_offset_needs_a_stop_at_every_slice_end() {
        let mut config = Config::default();
        assert!(!config.slice_ends_need_a_stop());
        config.chaos_instruction_offset = 8;
        assert!(!config.slice_ends_need_a_stop());
        config.chaos = true;
        assert!(config.slice_ends_need_a_stop());
        config.chaos_instruction_offset = 0;
        assert!(!config.slice_ends_need_a_stop());
    }

    /// The four capabilities split out of
    /// `supports_blocked_wait_signal_interruption` in A5 step 3c.0 equal it
    /// on every backend, as the readings they replace did, so no backend's
    /// behaviour moves; and `models_signal_targets` is serialized threads and
    /// `kernel_signal_targets_are_predictable`, under both thread modes.
    #[test]
    fn split_signal_facts_equal_their_former_bundle_on_every_backend() {
        // Only ptrace and e9patch set the bundle today.
        let backends = [
            ("ptrace", BackendCapabilities::PTRACE, true),
            ("e9patch", BackendCapabilities::E9PATCH, true),
            ("liteinst", BackendCapabilities::LITEINST_IN_GUEST, false),
            ("sabre", BackendCapabilities::SABRE, false),
            ("dbt", BackendCapabilities::DBT, false),
            ("kvm", BackendCapabilities::KVM, false),
            ("unprepared", DEFAULT_BACKEND_CAPABILITIES, false),
        ];
        for (name, backend, expected) in backends {
            assert_eq!(
                backend.supports_blocked_wait_signal_interruption, expected,
                "{name}"
            );
            assert_eq!(backend.kernel_signal_state_is_guests, expected, "{name}");
            assert_eq!(
                backend.kernel_signal_targets_are_predictable, expected,
                "{name}"
            );
            assert_eq!(
                backend.host_task_reads_reach_guest_threads, expected,
                "{name}"
            );
            assert_eq!(backend.host_timed_signals_are_physical, expected, "{name}");
            // It sets exactly those five: the whole struct after the call
            // equals `backend` with only them flipped by hand.
            let mut flipped = backend;
            flipped.supports_blocked_wait_signal_interruption = !expected;
            flipped.kernel_signal_state_is_guests = !expected;
            flipped.kernel_signal_targets_are_predictable = !expected;
            flipped.host_task_reads_reach_guest_threads = !expected;
            flipped.host_timed_signals_are_physical = !expected;
            let mut toggled = backend;
            set_kernel_signal_facts(&mut toggled, !expected);
            assert_eq!(toggled, flipped, "{name}");
            set_kernel_signal_facts(&mut toggled, expected);
            assert_eq!(toggled, backend, "{name}");
            for sequentialize_threads in [false, true] {
                let cfg = Config {
                    sequentialize_threads,
                    backend,
                    ..Config::default()
                };
                assert_eq!(
                    cfg.models_signal_targets(),
                    sequentialize_threads && expected,
                    "{name}, sequentialize_threads={sequentialize_threads}"
                );
            }
        }
    }

    #[test]
    fn default_epoch_is_2026() {
        assert_eq!(DEFAULT_EPOCH_STR, "2026-01-01T00:00:00Z");
        let epoch = DEFAULT_EPOCH_STR.parse::<DateTime<Utc>>().unwrap();
        assert_eq!(epoch.timestamp(), 1_767_225_600);
    }

    #[test]
    fn resolved_epoch_is_serialized_as_an_exact_config_input() {
        let epoch = "2000-12-31T23:59:59.123456789Z"
            .parse::<DateTime<Utc>>()
            .unwrap();
        let config = Config {
            epoch,
            ..Config::default()
        };
        let encoded = serde_json::to_value(&config).unwrap();
        assert_eq!(
            encoded["epoch"],
            serde_json::json!("2000-12-31T23:59:59.123456789Z")
        );
        let decoded: Config = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.epoch, epoch);
    }

    #[test]
    fn default_backend_capabilities_match_instrumented_backends() {
        let config = Config::default();
        assert_eq!(config.backend, DEFAULT_BACKEND_CAPABILITIES);
        assert!(!config.backend.reports_physical_process_exits);
        assert!(config.backend.tracks_process_children);
        assert!(config.backend.runs_exit_robust_list);
        assert!(!config.backend.requires_thread_directed_process_signals);
        assert!(config.backend.supports_parked_write_signal_interruption);
        assert!(!config.sequentialize_threads);
        assert!(
            legacy_backend_keys(&config).contains(&("kvm_shared_dequeue_timers", false)),
            "the default configuration must not write a controlled-run legacy key"
        );
        assert!(!config.backend.supports_blocked_wait_signal_interruption);
        assert!(!config.backend.kernel_signal_state_is_guests);
        assert!(!config.backend.kernel_signal_targets_are_predictable);
        assert!(!config.backend.host_task_reads_reach_guest_threads);
        assert!(!config.backend.host_timed_signals_are_physical);
        assert!(!config.backend.delivers_preemption_timer_events);
        assert!(!config.backend.runs_guest_code_between_injections);
        assert!(!config.guest_may_inherit_a_terminal);
        assert!(config.in_guest_detlog_forward_policy.is_none());
        assert!(!config.in_guest_site_patching_off);
        assert_eq!(config.in_guest_reserved_signals, 0);
        assert!(!config.reserves_in_guest_signal(libc::SIGSYS.into()));
        assert!(!config.in_guest_refuses_host_timed_producers);
        assert!(!config.in_guest_signal_delivery);
        assert!(config.launch_image.is_none());
    }

    #[test]
    fn a_config_without_backend_capabilities_deserializes_as_ptrace() {
        let mut encoded = serde_json::to_value(Config::default()).unwrap();
        let fields = encoded.as_object_mut().unwrap();
        assert!(fields.remove("backend").is_some());
        assert!(!fields.contains_key("shared_dequeue_timers"));
        let decoded: Config = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.backend, DEFAULT_BACKEND_CAPABILITIES);
    }

    /// `Config`'s own derived deserializer ignores a key it does not name, and
    /// `shared_dequeue_timers` is no longer one it names, so JSON saved while
    /// it was a field still reads and the key decides nothing, whatever it
    /// holds and however often it appears. This is the whole compatibility
    /// policy for the named form, and it is safe because no production code
    /// reads a `Config` by field name: the standalone DBT form is read by
    /// [`from_legacy_backend_json`], which checks the key, and the bincode form
    /// that crosses process boundaries is positional. Removing the field
    /// changed that positional form and [`config_wire_fingerprint`]. SaBRe's
    /// plugin compares the fingerprint and refuses a coordinator from another
    /// build; LiteInst's in-guest runtime does not, so a runtime library built
    /// before the removal and loaded by a newer coordinator can misdecode the
    /// Config and fail its initialization (exit 127). That mixed-build hazard
    /// is not new: every change to the positional form has it.
    #[test]
    fn a_saved_shared_dequeue_timers_field_is_ignored() {
        let baseline = serde_json::to_string(&Config::default()).unwrap();
        for extra in [
            r#""shared_dequeue_timers":false"#,
            r#""shared_dequeue_timers":true"#,
            r#""shared_dequeue_timers":"yes""#,
            r#""shared_dequeue_timers":true,"shared_dequeue_timers":false"#,
        ] {
            let edited = baseline.replacen('{', &format!("{{{extra},"), 1);
            let decoded: Config = serde_json::from_str(&edited).unwrap();
            assert_eq!(
                serde_json::to_string(&decoded).unwrap(),
                baseline,
                "{edited}"
            );
        }
    }

    /// Ptrace's capabilities with the robust-list transition and parked-write
    /// signal interruption both left to Detcore: the value the retired
    /// ptrace-hosted LiteInst runtime reported. No backend reports it now, but
    /// the legacy keys can still express it, so the round trips keep it.
    fn ptrace_without_exit_transitions() -> BackendCapabilities {
        let mut backend = BackendCapabilities::PTRACE;
        backend.runs_exit_robust_list = false;
        backend.supports_parked_write_signal_interruption = false;
        backend
    }

    /// `backend` as the legacy form can carry it: capabilities added after the
    /// form froze never enter it and read back as their default (see
    /// [`process_exits_complete_asynchronously_never_enters_the_legacy_form`]).
    fn legacy_expressible(mut backend: BackendCapabilities) -> BackendCapabilities {
        backend.process_exits_complete_asynchronously = false;
        backend.virtualizes_guest_sigalrm = false;
        set_kernel_signal_facts(&mut backend, false);
        backend.delivers_preemption_timer_events = false;
        backend.runs_guest_code_between_injections = false;
        backend.reports_sigreturns = false;
        // Added after the legacy form froze: it reads back as false (see
        // `LegacyBackendKeys::capabilities`).
        backend.kernel_never_walks_robust_lists = false;
        // Added after the legacy form froze: it reads back as true (see
        // `LegacyBackendKeys::capabilities`).
        backend.provides_guest_backtraces = true;
        // Added after the legacy form froze: they read back as false and the
        // empty set (see `LegacyBackendKeys::capabilities`).
        backend.places_orphans = false;
        backend.publishable_parent_death_signals = 0;
        backend.reports_kernel_child_exit_signals = backend.reports_child_exit_publication;
        // The legacy discover_live_file_metadata bit couples three facts.
        backend.rediscovers_descriptors_after_exec = backend.tool_shares_guest_descriptor_table;
        // The legacy form derives promotion from the thread-directed signal
        // bit, which only DBT, the form's only reader, sets.
        backend.promotes_open_files_before_native_lifecycle =
            backend.requires_thread_directed_process_signals;
        backend.loopback_pollers_yield_to_peers = backend.tool_shares_guest_descriptor_table;
        // The legacy form carries no width for `hardware_watchpoints`: it
        // reads back as 0, the minimal width (see `LegacyBackendKeys::capabilities`).
        backend.hardware_watchpoints = 0;
        // Added after the legacy form froze: it reads back as `None` (see
        // `LegacyBackendKeys::capabilities`).
        backend.rcb_clock_contract = None;
        backend
    }

    #[test]
    fn legacy_backend_json_round_trips_every_backend_capabilities_constant() {
        for (name, backend) in [
            ("PTRACE", BackendCapabilities::PTRACE),
            ("E9PATCH", BackendCapabilities::E9PATCH),
            (
                "ptrace without exit transitions",
                ptrace_without_exit_transitions(),
            ),
            ("LITEINST_IN_GUEST", BackendCapabilities::LITEINST_IN_GUEST),
            ("SABRE", BackendCapabilities::SABRE),
            ("DBT", BackendCapabilities::DBT),
            ("KVM", BackendCapabilities::KVM),
        ] {
            for sequentialize_threads in [false, true] {
                let sent = Config {
                    backend,
                    sequentialize_threads,
                    // A launch sends a resolved profile: the legacy form has
                    // no profile field and reads back as v2, so an unresolved
                    // auto would not round-trip.
                    cpu_profile: crate::cpu_profile::CpuProfile::X86_64V2,
                    ..Config::default()
                };
                let json = to_legacy_backend_json(&sent).unwrap();
                let received = from_legacy_backend_json(&json).unwrap();
                assert_eq!(received.backend, legacy_expressible(backend), "{name}");
                assert_eq!(
                    received.sequentialize_threads, sequentialize_threads,
                    "{name}"
                );
                assert_controlled_key(&json, &backend, sequentialize_threads, name);
                let expressible = Config {
                    backend: legacy_expressible(backend),
                    ..sent.clone()
                };
                assert_eq!(
                    serde_json::to_string(&received).unwrap(),
                    serde_json::to_string(&expressible).unwrap(),
                    "{name}"
                );
                assert_eq!(to_legacy_backend_json(&received).unwrap(), json, "{name}");
                assert_controlled_key_is_checked(&json, &sent, name);
            }
        }
    }

    #[test]
    fn backend_config_json_preserves_every_backend_and_representable_bytes() {
        for (name, backend, needs_envelope) in [
            ("PTRACE", BackendCapabilities::PTRACE, true),
            ("E9PATCH", BackendCapabilities::E9PATCH, true),
            (
                "LITEINST_IN_GUEST",
                BackendCapabilities::LITEINST_IN_GUEST,
                true,
            ),
            ("SABRE", BackendCapabilities::SABRE, true),
            ("DBT", BackendCapabilities::DBT, true),
            // KVM's `provides_guest_backtraces` is false, which the legacy
            // form cannot express (it reads back true), so KVM needs the
            // envelope like the others.
            ("KVM", BackendCapabilities::KVM, true),
        ] {
            for sequentialize_threads in [false, true] {
                let sent = Config {
                    backend,
                    sequentialize_threads,
                    // A launch sends a resolved profile: the legacy form has
                    // no profile field and reads back as v2, so an unresolved
                    // auto would not round-trip.
                    cpu_profile: crate::cpu_profile::CpuProfile::X86_64V2,
                    ..Config::default()
                };
                let legacy = to_legacy_backend_json(&sent).unwrap();
                let encoded = encode_backend_config_json(legacy.clone(), backend).unwrap();
                assert_eq!(encoded != legacy, needs_envelope, "{name}");
                assert_eq!(
                    from_legacy_backend_json(&encoded).is_err(),
                    needs_envelope,
                    "{name}: an envelope must not be accepted as a legacy config"
                );
                let received = decode_backend_config_json(&encoded).unwrap();
                assert_eq!(received.backend, backend, "{name}");
                assert_eq!(
                    serde_json::to_string(&received).unwrap(),
                    serde_json::to_string(&sent).unwrap(),
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn backend_config_json_round_trips_each_independent_capability() {
        let baseline = serde_json::to_value(BackendCapabilities::DBT).unwrap();
        for (name, value) in baseline.as_object().unwrap() {
            let mut edited = baseline.clone();
            // Every capability is a bool except `hardware_watchpoints`, a
            // `u8` width, `publishable_parent_death_signals`, a `u64` set of
            // signal bits, and `rcb_clock_contract`, an optional contract:
            // flip a bool, and move a number to another value (DBT's is 0
            // for both: width 1, and KVM's set), and DBT's absent contract
            // to ptrace's.
            edited[name] = match name.as_str() {
                "hardware_watchpoints" => serde_json::Value::from(1u8),
                "publishable_parent_death_signals" => serde_json::Value::from(
                    BackendCapabilities::KVM.publishable_parent_death_signals,
                ),
                "rcb_clock_contract" => {
                    assert!(value.is_null(), "DBT states no RCB clock contract");
                    serde_json::to_value(BackendCapabilities::PTRACE.rcb_clock_contract).unwrap()
                }
                _ => serde_json::Value::Bool(!value.as_bool().unwrap()),
            };
            let backend = serde_json::from_value(edited).unwrap();
            let sent = Config {
                backend,
                ..Config::default()
            };
            let legacy = to_legacy_backend_json(&sent).unwrap();
            let encoded = encode_backend_config_json(legacy, backend).unwrap();
            let received = decode_backend_config_json(&encoded).unwrap();
            assert_eq!(received.backend, backend, "{name}");
        }
    }

    #[test]
    fn backend_config_json_requires_each_typed_capability_once() {
        let sent = Config {
            backend: BackendCapabilities::LITEINST_IN_GUEST,
            ..Config::default()
        };
        let encoded =
            encode_backend_config_json(to_legacy_backend_json(&sent).unwrap(), sent.backend)
                .unwrap();
        let envelope: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        for (name, value) in envelope["backend"].as_object().unwrap() {
            let mut missing = envelope.clone();
            missing["backend"].as_object_mut().unwrap().remove(name);
            assert!(
                decode_backend_config_json(&missing.to_string()).is_err(),
                "missing {name}"
            );
            let backend = serde_json::to_string(&sent.backend).unwrap();
            let duplicate = backend.replacen('{', &format!("{{\"{name}\":{value},"), 1);
            let duplicate = encoded.replace(&backend, &duplicate);
            assert_ne!(duplicate, encoded);
            assert!(
                decode_backend_config_json(&duplicate).is_err(),
                "duplicate {name}"
            );
        }
    }

    #[test]
    fn backend_config_json_keeps_legacy_object_and_array_semantics() {
        let legacy = to_legacy_backend_json(&Config::default()).unwrap();
        let unknown_duplicates = legacy.replacen(
            '{',
            r#"{"unrecognized":1,"unrecognized":null,"backend":null,"backend":true,"dbt_config_version":null,"dbt_config_version":2,"#,
            1,
        );
        let known_duplicate = legacy.replacen('{', r#"{"seed":1,"#, 1);
        let mistyped = legacy.replacen("\"backend_is_kvm\":false", "\"backend_is_kvm\":null", 1);
        assert_ne!(mistyped, legacy);
        for json in [
            legacy.clone(),
            positional(&legacy),
            unknown_duplicates,
            known_duplicate,
            mistyped,
            format!("{legacy} trailing"),
            "[]".to_owned(),
        ] {
            let old = from_legacy_backend_json(&json);
            let new = decode_backend_config_json(&json);
            match (old, new) {
                (Ok(old), Ok(new)) => assert_eq!(
                    serde_json::to_string(&old).unwrap(),
                    serde_json::to_string(&new).unwrap()
                ),
                (Err(_), Err(_)) => {}
                _ => panic!("legacy parse outcome changed for {json}"),
            }
        }
    }

    /// `kvm_shared_dequeue_timers` is read independently of the other keys,
    /// absent, false or true, and the parse succeeds exactly when the value
    /// read (false when absent) equals `backend_is_kvm &&
    /// sequentialize_threads`. Under that condition the accepted value is the
    /// one the decoded capabilities and `sequentialize_threads` imply, so the
    /// decoded configuration is the one sent; otherwise the error names the
    /// key.
    fn assert_controlled_key_is_checked(json: &str, sent: &Config, name: &str) {
        let controlled = sent.backend.provides_process_signal_control && sent.sequentialize_threads;
        let written = format!("\"kvm_shared_dequeue_timers\":{controlled}");
        assert_eq!(json.matches(&written).count(), 1, "{name}");
        for value in [None, Some(false), Some(true)] {
            let edited = match value {
                None => json.replacen(&format!(",{written}"), "", 1),
                Some(value) => json.replacen(
                    &written,
                    &format!("\"kvm_shared_dequeue_timers\":{value}"),
                    1,
                ),
            };
            assert_eq!(
                edited.contains("kvm_shared_dequeue_timers"),
                value.is_some(),
                "{name} {value:?}"
            );
            let case = format!(
                "{name} sequentialize_threads={} key={value:?}",
                sent.sequentialize_threads
            );
            match from_legacy_backend_json(&edited) {
                Ok(received) => {
                    assert_eq!(value.unwrap_or(false), controlled, "{case} was accepted");
                    // Compared with what the legacy form can carry; see
                    // `legacy_expressible`.
                    let expressible = Config {
                        backend: legacy_expressible(sent.backend),
                        ..sent.clone()
                    };
                    assert_eq!(
                        serde_json::to_string(&received).unwrap(),
                        serde_json::to_string(&expressible).unwrap(),
                        "{case}"
                    );
                }
                Err(error) => {
                    assert_ne!(value.unwrap_or(false), controlled, "{case}: {error}");
                    assert!(
                        error.to_string().contains("kvm_shared_dequeue_timers"),
                        "{case}: {error}"
                    );
                }
            }
        }
    }

    /// The four controlled-key outcomes that differ from simply ignoring the
    /// key, spelled out: each would otherwise run a configuration on a path
    /// other than the one the same JSON selected before.
    #[test]
    fn a_controlled_key_that_contradicts_the_capabilities_fails_the_parse() {
        let kvm = Config {
            backend: BackendCapabilities::KVM,
            sequentialize_threads: true,
            ..Config::default()
        };
        let kvm_json = to_legacy_backend_json(&kvm).unwrap();
        assert!(from_legacy_backend_json(&kvm_json).is_ok());
        let dbt = Config {
            backend: BackendCapabilities::DBT,
            sequentialize_threads: true,
            ..Config::default()
        };
        let dbt_json = to_legacy_backend_json(&dbt).unwrap();
        assert!(from_legacy_backend_json(&dbt_json).is_ok());
        for edited in [
            // KVM, sequentialized, key false: the control was left uninstalled.
            kvm_json.replacen(
                r#""kvm_shared_dequeue_timers":true"#,
                r#""kvm_shared_dequeue_timers":false"#,
                1,
            ),
            // KVM, sequentialized, key absent: read as false, as before.
            kvm_json.replacen(r#","kvm_shared_dequeue_timers":true"#, "", 1),
            // No control capability, key true: the controlled loop was chosen.
            dbt_json.replacen(
                r#""kvm_shared_dequeue_timers":false"#,
                r#""kvm_shared_dequeue_timers":true"#,
                1,
            ),
            // KVM without sequentialized threads, key true.
            to_legacy_backend_json(&Config {
                sequentialize_threads: false,
                ..kvm.clone()
            })
            .unwrap()
            .replacen(
                r#""kvm_shared_dequeue_timers":false"#,
                r#""kvm_shared_dequeue_timers":true"#,
                1,
            ),
        ] {
            assert_ne!(edited, kvm_json);
            assert_ne!(edited, dbt_json);
            let error = from_legacy_backend_json(&edited).unwrap_err();
            assert!(
                error.to_string().contains("kvm_shared_dequeue_timers"),
                "{error}"
            );
        }
    }

    /// Each legacy key decodes as the `Config` field it was: absent, it takes
    /// that field's default, and set alone, it gives exactly the capabilities
    /// that replaced Detcore's reads of that field.
    #[test]
    fn legacy_backend_keys_decode_as_the_fields_they_were() {
        let ptrace_json = to_legacy_backend_json(&Config::default()).unwrap();
        let ptrace: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&ptrace_json).unwrap();
        let keys = legacy_backend_keys(&Config::default());

        let mut absent = ptrace.clone();
        for (key, _) in keys {
            assert!(absent.remove(key).is_some(), "{key}");
        }
        let decoded = from_legacy_backend_json(&serde_json::to_string(&absent).unwrap()).unwrap();
        // The capabilities added after the legacy form froze read back as
        // false (`legacy_expressible`), whatever ptrace reports.
        let legacy_ptrace = legacy_expressible(BackendCapabilities::PTRACE);
        assert_eq!(decoded.backend, legacy_ptrace);
        let unflipped =
            serde_json::to_string(&from_legacy_backend_json(&ptrace_json).unwrap()).unwrap();
        assert_eq!(serde_json::to_string(&decoded).unwrap(), unflipped);

        for (key, ptrace_value) in keys {
            let mut expected = legacy_ptrace;
            match key {
                "cpuid_virtualized_by_backend" => {
                    // No backend installs a CPUID policy of its own, so `true`
                    // fails the parse.
                    let mut flipped = ptrace.clone();
                    flipped.insert(key.to_owned(), serde_json::Value::Bool(!ptrace_value));
                    let error = from_legacy_backend_json(&serde_json::to_string(&flipped).unwrap())
                        .unwrap_err();
                    assert!(error.to_string().contains(key), "{error}");
                    continue;
                }
                "syscall_clobbers_virtualized_by_backend"
                | "backend_supports_madvise"
                | "backend_defers_vfork_child_registration" => {
                    // None supplies a capability: either value reads back the
                    // same configuration.
                    let mut flipped = ptrace.clone();
                    flipped.insert(key.to_owned(), serde_json::Value::Bool(!ptrace_value));
                    let decoded =
                        from_legacy_backend_json(&serde_json::to_string(&flipped).unwrap())
                            .unwrap();
                    assert_eq!(serde_json::to_string(&decoded).unwrap(), unflipped, "{key}");
                    continue;
                }
                "discover_live_file_metadata" => {
                    expected.tool_shares_guest_descriptor_table = true;
                    expected.rediscovers_descriptors_after_exec = true;
                    expected.loopback_pollers_yield_to_peers = true;
                }
                "detect_host_clock_futex_timeouts" => {
                    expected.guest_clock_reads_bypass_backend = true
                }
                "cancel_killed_thread_rpcs" => {
                    expected.needs_killed_thread_rpc_cancellation = true;
                    expected.reports_child_exit_publication = false;
                    expected.reports_kernel_child_exit_signals = false;
                }
                "backend_reports_physical_process_exits" => {
                    expected.reports_physical_process_exits = true
                }
                "backend_tracks_process_children" => expected.tracks_process_children = false,
                "backend_runs_exit_robust_list" => expected.runs_exit_robust_list = false,
                "backend_requires_thread_directed_process_signals" => {
                    expected.requires_thread_directed_process_signals = true;
                    expected.promotes_open_files_before_native_lifecycle = true;
                }
                "backend_is_kvm" => {
                    expected.provides_process_signal_control = true;
                    expected.emulates_child_waits = true;
                }
                "kvm_shared_dequeue_timers" => {
                    // Set alone it contradicts `backend_is_kvm`, so the parse
                    // fails; see
                    // `a_controlled_key_that_contradicts_the_capabilities_fails_the_parse`.
                    let mut flipped = ptrace.clone();
                    flipped.insert(key.to_owned(), serde_json::Value::Bool(!ptrace_value));
                    assert!(
                        from_legacy_backend_json(&serde_json::to_string(&flipped).unwrap())
                            .is_err(),
                        "{key}"
                    );
                    continue;
                }
                "backend_supports_parked_write_signal_interruption" => {
                    expected.supports_parked_write_signal_interruption = false
                }
                "backend_virtualizes_capability_prctls" => {
                    // No backend answers the capability prctls itself, so
                    // `true` fails the parse.
                    let mut flipped = ptrace.clone();
                    flipped.insert(key.to_owned(), serde_json::Value::Bool(!ptrace_value));
                    let error = from_legacy_backend_json(&serde_json::to_string(&flipped).unwrap())
                        .unwrap_err();
                    assert!(error.to_string().contains(key), "{error}");
                    continue;
                }
                other => panic!("no expectation for legacy key {other}"),
            }
            let mut flipped = ptrace.clone();
            flipped.insert(key.to_owned(), serde_json::Value::Bool(!ptrace_value));
            let decoded =
                from_legacy_backend_json(&serde_json::to_string(&flipped).unwrap()).unwrap();
            assert_eq!(decoded.backend, expected, "{key}");
            assert_ne!(serde_json::to_string(&decoded).unwrap(), unflipped, "{key}");
        }
    }

    /// The legacy `kvm_shared_dequeue_timers` key holds exactly when the
    /// backend provides process signal control and threads are sequentialized,
    /// the condition under which Detcore installs that control.
    fn assert_controlled_key(
        json: &str,
        backend: &BackendCapabilities,
        sequentialize_threads: bool,
        name: &str,
    ) {
        let object: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(json).unwrap();
        assert_eq!(
            object["kvm_shared_dequeue_timers"],
            serde_json::Value::Bool(
                backend.provides_process_signal_control && sequentialize_threads
            ),
            "{name} sequentialize_threads={sequentialize_threads}"
        );
    }

    /// A legacy key failed the parse when it held the wrong type, or appeared
    /// twice, while it was a `Config` field; it still does, so a reader falls
    /// back as it did.
    #[test]
    fn a_legacy_backend_key_of_the_wrong_type_fails_the_parse() {
        let json = to_legacy_backend_json(&Config::default()).unwrap();
        assert!(from_legacy_backend_json(&json).is_ok());
        for (key, value) in legacy_backend_keys(&Config::default()) {
            let field = format!("\"{key}\":{value}");
            assert_eq!(json.matches(&field).count(), 1, "{field}");
            let mistyped = json.replacen(&field, &format!("\"{key}\":0"), 1);
            assert!(from_legacy_backend_json(&mistyped).is_err(), "{mistyped}");
            let repeated = json.replacen(&field, &format!("{field},{field}"), 1);
            assert!(from_legacy_backend_json(&repeated).is_err(), "{repeated}");
        }
    }

    /// The entries of a JSON object, in the order they appear.
    fn ordered_entries(json: &str) -> Vec<(String, serde_json::Value)> {
        struct Entries(Vec<(String, serde_json::Value)>);

        impl<'de> Deserialize<'de> for Entries {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                struct EntriesVisitor;

                impl<'de> serde::de::Visitor<'de> for EntriesVisitor {
                    type Value = Entries;

                    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                        formatter.write_str("a JSON object")
                    }

                    fn visit_map<A: serde::de::MapAccess<'de>>(
                        self,
                        mut map: A,
                    ) -> Result<Entries, A::Error> {
                        let mut entries = Vec::new();
                        while let Some(entry) = map.next_entry()? {
                            entries.push(entry);
                        }
                        Ok(Entries(entries))
                    }
                }

                deserializer.deserialize_map(EntriesVisitor)
            }
        }

        serde_json::from_str::<Entries>(json).unwrap().0
    }

    /// The JSON array of `json`'s values in its key order: the same
    /// configuration, listed by position.
    fn positional(json: &str) -> String {
        let values: Vec<serde_json::Value> = ordered_entries(json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        serde_json::to_string(&values).unwrap()
    }

    /// The legacy form's positions are `Config`'s fields in declaration order
    /// with the fifteen legacy keys where `backend` stands and no
    /// `record_host_inputs`, `guest_may_inherit_a_terminal`,
    /// `in_guest_detlog_forward_policy`, `in_guest_site_patching_off`,
    /// `in_guest_reserved_signals`, `in_guest_refuses_host_timed_producers`,
    /// `in_guest_signal_delivery`,
    /// `launch_image`, `target_timeslice_syscalls_only`, `futex_wake_yields`,
    /// `replaying`,
    /// `scheduler_turn_cost`, `cap_preemption_charge`,
    /// `inherited_seccomp`, `replay_edited_preemptions` or `detlog_anon`,
    /// which is the key order the encoder writes.
    #[test]
    fn legacy_positions_are_the_encoded_key_order() {
        let names = legacy_backend_keys(&Config::default()).map(|(name, _)| name);
        assert_eq!(names, LEGACY_BACKEND_KEY_NAMES);

        let fields = serde_json::from_str::<legacy_backend_json::KeyOrder>(
            &serde_json::to_string(&Config::default()).unwrap(),
        )
        .unwrap()
        .0;
        let mut expected = Vec::new();
        for field in &fields {
            match field.as_str() {
                "backend" => expected.extend(names.map(str::to_owned)),
                "strict_schedule_boundaries"
                | "record_host_inputs"
                | "guest_may_inherit_a_terminal"
                | "in_guest_detlog_forward_policy"
                | "in_guest_site_patching_off"
                | "in_guest_reserved_signals"
                | "in_guest_refuses_host_timed_producers"
                | "launch_image"
                | "target_timeslice_syscalls_only"
                | "futex_wake_yields"
                | "pipe_wake_latency"
                | "expired_wait_charge"
                | "sleep_registration"
                | "replaying"
                | "scheduler_turn_cost"
                | "cap_preemption_charge"
                | "virtual_timeout"
                | "inherited_seccomp"
                | "cpu_profile"
                | "cpu_profile_host"
                | "cpu_profile_source"
                | "signal_state_copy"
                | "container_mount_table"
                | "mountinfo_mount_namespace"
                | "directory_placeholder"
                | "container_procfs"
                | "signal_delivery_check_fatal"
                | "strict_binder"
                | "work_span_report"
                | "signal_at"
                | "post_crash_timeslice"
                | "in_guest_signal_delivery"
                | "annotated_priority_replay"
                | "virtual_cpus"
                | "notice_host_network_abstract_binds"
                | "replay_edited_preemptions"
                | "eager_lone_preemptions"
                | "chaos_timeslice_range"
                | "chaos_timeslice_draw"
                | "chaos_timeslice"
                | "guest_ids"
                | "detlog_anon"
                | "standard_stream_aliases"
                | "rewrite_exclusives_unmarked" => {}
                "access_trace"
                | "access_trace_from"
                | "access_trace_max_records"
                | "access_trace_max_bytes"
                | "rewrite_exclusives_in_memory"
                | "rewrite_exclusives_overlay"
                | "rewrite_exclusives_overlay_requested"
                | "count_store_exclusives"
                | "stacktrace_at_schedule_positions"
                | "property_snapshot"
                | "watch"
                | "watch_from"
                | "chaos_instruction_offset"
                | "untrap_passthrough_syscalls" => {}
                "replay_continue_fairly" => {}
                other => expected.push(other.to_owned()),
            }
        }
        let encoded = to_legacy_backend_json(&Config::default()).unwrap();
        let keys: Vec<String> = ordered_entries(&encoded)
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        assert_eq!(keys, expected);
        // `backend` becomes fifteen keys; `record_host_inputs`,
        // `guest_may_inherit_a_terminal`,
        // `in_guest_detlog_forward_policy`, `in_guest_site_patching_off`,
        // `in_guest_reserved_signals`, `in_guest_refuses_host_timed_producers`,
        // `launch_image`, `target_timeslice_syscalls_only`, `futex_wake_yields`,
        // `pipe_wake_latency`, `expired_wait_charge`, `sleep_registration`, `replaying`,
        // `scheduler_turn_cost`, `cap_preemption_charge`, `virtual_timeout`,
        // `inherited_seccomp`, `cpu_profile`,
        // `cpu_profile_host`, `cpu_profile_source`,
        // `signal_state_copy`, `container_mount_table`,
        // `guest_may_inherit_a_terminal`, `in_guest_detlog_forward_policy`,
        // `in_guest_site_patching_off`, `in_guest_reserved_signals`,
        // `in_guest_refuses_host_timed_producers`, `launch_image`,
        // `target_timeslice_syscalls_only`, `futex_wake_yields`,
        // `pipe_wake_latency`, `replaying`, `scheduler_turn_cost`,
        // `inherited_seccomp`, `cpu_profile`, `cpu_profile_host`,
        // `cpu_profile_source`, `signal_state_copy`, `container_mount_table`,
        // `mountinfo_mount_namespace`, `directory_placeholder`,
        // `container_procfs`, `signal_delivery_check_fatal`,
        // `work_span_report`, `signal_at`, `in_guest_signal_delivery`,
        // `annotated_priority_replay`, `virtual_cpus`, `replay_edited_preemptions`,
        // `notice_host_network_abstract_binds`, `eager_lone_preemptions`,
        // `chaos_timeslice_range`, `chaos_timeslice_draw`, `chaos_timeslice`,
        // `guest_ids`, `virtual_timeout`, `detlog_anon`,
        // `access_trace`, `access_trace_from`, `access_trace_max_records`,
        // `access_trace_max_bytes`, `replay_continue_fairly`,
        // `standard_stream_aliases`, `sleep_registration`, `strict_binder`,
        // `post_crash_timeslice`, `rewrite_exclusives_in_memory`,
        // `stacktrace_at_schedule_positions`, `count_store_exclusives`,
        // `property_snapshot`, `watch`, `watch_from`, `rewrite_exclusives_overlay`,
        // `chaos_instruction_offset`, `rewrite_exclusives_overlay_requested`,
        // `rewrite_exclusives_unmarked`, `untrap_passthrough_syscalls`
        // and `strict_schedule_boundaries` none: fourteen more keys and
        // fifty-nine fewer, so forty-five keys fewer than fields.
        assert!(!fields.iter().any(|field| field == "shared_dequeue_timers"));
        assert_eq!(fields.len(), keys.len() + 45);
    }

    /// `record_host_inputs` never enters the legacy form, whatever its value:
    /// the legacy string keeps its bytes, and it reads back as
    /// false from both the object and the array form.
    #[test]
    fn record_host_inputs_never_enters_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            record_host_inputs: true,
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(!json.contains("record_host_inputs"), "{json}");
        assert!(!from_legacy_backend_json(&json).unwrap().record_host_inputs);
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert!(!from_legacy_backend_json(&array).unwrap().record_host_inputs);
    }

    /// The files the exclusive rewrite left unchanged never enter the legacy
    /// form, whatever they are: the legacy string keeps its bytes, and they
    /// read back as empty from both the object and the array form.
    #[test]
    fn the_unmarked_rewrite_files_never_enter_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            rewrite_exclusives_unmarked: vec!["/lib64/libstdc++.so.6".into()],
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(!json.contains("rewrite_exclusives"), "{json}");
        let back = from_legacy_backend_json(&json).unwrap();
        assert!(back.rewrite_exclusives_unmarked.is_empty());
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        let back = from_legacy_backend_json(&array).unwrap();
        assert!(back.rewrite_exclusives_unmarked.is_empty());
    }

    /// `process_exits_complete_asynchronously`, added after the legacy form
    /// froze, never enters it, whatever its value, exactly as
    /// `record_host_inputs` does not: the legacy string keeps its bytes, and
    /// it reads back as false from the object and the array form.
    #[test]
    fn process_exits_complete_asynchronously_never_enters_the_legacy_form() {
        let on = Config {
            backend: BackendCapabilities::LITEINST_IN_GUEST,
            ..Config::default()
        };
        assert!(on.backend.process_exits_complete_asynchronously);
        assert!(on.backend.virtualizes_guest_sigalrm);
        let off = Config {
            backend: legacy_expressible(on.backend),
            ..on.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(
            !json.contains("process_exits_complete_asynchronously"),
            "{json}"
        );
        assert!(!json.contains("virtualizes_guest_sigalrm"), "{json}");
        let decoded = from_legacy_backend_json(&json).unwrap();
        assert!(!decoded.backend.process_exits_complete_asynchronously);
        assert!(!decoded.backend.virtualizes_guest_sigalrm);
        let array = positional(&json);
        assert!(
            !from_legacy_backend_json(&array)
                .unwrap()
                .backend
                .process_exits_complete_asynchronously
        );
    }

    /// The capabilities added after the legacy form froze that a legacy
    /// configuration reads back as false
    /// (`supports_blocked_wait_signal_interruption` and the four split out of
    /// it, `delivers_preemption_timer_events`,
    /// `runs_guest_code_between_injections`, `reports_sigreturns`,
    /// `kernel_never_walks_robust_lists`, `places_orphans`,
    /// `publishable_parent_death_signals`, which reads back as the empty set,
    /// and `rcb_clock_contract`, which reads back as `None`)
    /// never enter it, whatever their
    /// value: the legacy string keeps its bytes, and they read
    /// back as false from both the object and the array form.
    #[test]
    fn post_freeze_capabilities_never_enter_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let mut on = off.clone();
        set_kernel_signal_facts(&mut on.backend, true);
        on.backend.delivers_preemption_timer_events = true;
        on.backend.runs_guest_code_between_injections = true;
        on.backend.reports_sigreturns = true;
        on.backend.kernel_never_walks_robust_lists = true;
        on.backend.places_orphans = true;
        on.backend.publishable_parent_death_signals =
            BackendCapabilities::KVM.publishable_parent_death_signals;
        on.backend.rcb_clock_contract = BackendCapabilities::PTRACE.rcb_clock_contract;
        assert!(on.backend.rcb_clock_contract.is_some());
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        for key in [
            "supports_blocked_wait_signal_interruption",
            "kernel_signal_state_is_guests",
            "kernel_signal_targets_are_predictable",
            "host_task_reads_reach_guest_threads",
            "host_timed_signals_are_physical",
            "delivers_preemption_timer_events",
            "runs_guest_code_between_injections",
            "reports_sigreturns",
            "kernel_never_walks_robust_lists",
            "places_orphans",
            "publishable_parent_death_signals",
            "rcb_clock_contract",
        ] {
            assert!(!json.contains(key), "{json}");
        }
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        for decoded in [&json, &array] {
            let backend = from_legacy_backend_json(decoded).unwrap().backend;
            assert!(!backend.supports_blocked_wait_signal_interruption);
            assert!(!backend.kernel_signal_state_is_guests);
            assert!(!backend.kernel_signal_targets_are_predictable);
            assert!(!backend.host_task_reads_reach_guest_threads);
            assert!(!backend.host_timed_signals_are_physical);
            assert!(!backend.delivers_preemption_timer_events);
            assert!(!backend.runs_guest_code_between_injections);
            assert!(!backend.reports_sigreturns);
            assert!(!backend.kernel_never_walks_robust_lists);
            assert!(!backend.places_orphans);
            assert_eq!(backend.publishable_parent_death_signals, 0);
            assert_eq!(backend.rcb_clock_contract, None);
            assert_eq!(backend, legacy_expressible(BackendCapabilities::DBT));
        }
    }

    /// `guest_may_inherit_a_terminal` never enters the legacy form, whatever
    /// its value: the legacy string keeps its bytes, and it reads
    /// back as false from both the object and the array form.
    /// `in_guest_detlog_forward_policy` is unset for DBT, so the legacy form
    /// never carries it, and it reads back as unset.
    #[test]
    fn in_guest_detlog_forward_policy_never_enters_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            in_guest_detlog_forward_policy: Some("1".to_owned()),
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(!json.contains("in_guest_detlog_forward_policy"), "{json}");
        assert!(
            from_legacy_backend_json(&json)
                .unwrap()
                .in_guest_detlog_forward_policy
                .is_none()
        );
    }

    /// `launch_namespace_mounts` is never serialized: neither DBT's legacy
    /// form nor the configuration's own (which recordings keep) carries it,
    /// both read back with an empty table, and the run-start log line shows
    /// only its size.
    #[test]
    fn launch_namespace_mounts_never_enters_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            launch_namespace_mounts: crate::host_input::LaunchNamespaceMounts(vec![
                (29, "ext4".to_owned()),
                (31, "devtmpfs".to_owned()),
            ]),
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(!json.contains("launch_namespace_mounts"), "{json}");
        assert!(
            from_legacy_backend_json(&json)
                .unwrap()
                .launch_namespace_mounts
                .0
                .is_empty()
        );
        let full = serde_json::to_string(&on).unwrap();
        assert!(!full.contains("launch_namespace_mounts"), "{full}");
        assert!(
            serde_json::from_str::<Config>(&full)
                .unwrap()
                .launch_namespace_mounts
                .0
                .is_empty()
        );
        assert_eq!(
            format!("{:?}", on.launch_namespace_mounts),
            "LaunchNamespaceMounts { mounts: 2 }"
        );
    }

    /// `launch_image` has no legacy key, so the legacy form never carries
    /// it, and it reads back as unset from both the object and the array
    /// form.
    #[test]
    fn launch_image_never_enters_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            launch_image: Some(LaunchImage {
                exe: Some(PathBuf::from("/usr/bin/true")),
                launcher: Some(PathBuf::from("/usr/bin/env")),
            }),
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(!json.contains("launch_image"), "{json}");
        assert!(
            from_legacy_backend_json(&json)
                .unwrap()
                .launch_image
                .is_none()
        );
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert!(
            from_legacy_backend_json(&array)
                .unwrap()
                .launch_image
                .is_none()
        );
    }

    /// `in_guest_site_patching_off` is false for DBT, so the legacy form never
    /// carries it, and it reads back as false from both the object and the
    /// array form.
    #[test]
    fn in_guest_site_patching_off_never_enters_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            in_guest_site_patching_off: true,
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(!json.contains("in_guest_site_patching_off"), "{json}");
        assert!(
            !from_legacy_backend_json(&json)
                .unwrap()
                .in_guest_site_patching_off
        );
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert!(
            !from_legacy_backend_json(&array)
                .unwrap()
                .in_guest_site_patching_off
        );
    }

    #[test]
    fn guest_may_inherit_a_terminal_never_enters_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            guest_may_inherit_a_terminal: true,
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(!json.contains("guest_may_inherit_a_terminal"), "{json}");
        assert!(
            !from_legacy_backend_json(&json)
                .unwrap()
                .guest_may_inherit_a_terminal
        );
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert!(
            !from_legacy_backend_json(&array)
                .unwrap()
                .guest_may_inherit_a_terminal
        );
    }

    /// `guest_ids` never enters the legacy form, whatever its value: the
    /// legacy string keeps its bytes, and it reads back as numbered by a PID
    /// namespace from both the object and the array form.
    #[test]
    fn the_guest_numbering_never_enters_the_legacy_form() {
        let namespace = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let chosen = Config {
            guest_ids: crate::pid::GuestIds::Chosen {
                base: 5000,
                refuse_host_ids: true,
            },
            ..namespace.clone()
        };
        let json = to_legacy_backend_json(&chosen).unwrap();
        assert_eq!(json, to_legacy_backend_json(&namespace).unwrap());
        assert!(!json.contains("guest_ids"), "{json}");
        assert_eq!(
            from_legacy_backend_json(&json).unwrap().guest_ids,
            crate::pid::GuestIds::Namespace
        );
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert_eq!(
            from_legacy_backend_json(&array).unwrap().guest_ids,
            crate::pid::GuestIds::Namespace
        );
    }

    /// `chaos_instruction_offset` never enters the legacy form, whatever its
    /// value: the legacy string keeps its bytes, and it reads back as 0 from
    /// both the object and the array form.
    #[test]
    fn chaos_instruction_offset_never_enters_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            chaos: true,
            chaos_instruction_offset: 16,
            ..off.clone()
        };
        let chaos = Config {
            chaos: true,
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&chaos).unwrap());
        assert!(!json.contains("chaos_instruction_offset"), "{json}");
        assert_eq!(
            from_legacy_backend_json(&json)
                .unwrap()
                .chaos_instruction_offset,
            0
        );
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert_eq!(
            from_legacy_backend_json(&array)
                .unwrap()
                .chaos_instruction_offset,
            0
        );
        assert!(
            !to_legacy_backend_json(&off)
                .unwrap()
                .contains("chaos_instruction_offset")
        );
    }

    /// `inherited_seccomp` never enters the legacy form, whatever its value:
    /// the legacy string keeps its bytes, and it reads back as
    /// unknown from both the object and the array form.
    #[test]
    fn the_cpu_profile_never_enters_the_legacy_form() {
        let v2 = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let v3 = Config {
            cpu_profile: crate::cpu_profile::CpuProfile::X86_64V3,
            cpu_profile_source: Some(17),
            ..v2.clone()
        };
        let json = to_legacy_backend_json(&v3).unwrap();
        assert_eq!(json, to_legacy_backend_json(&v2).unwrap());
        assert!(!json.contains("cpu_profile"), "{json}");
        assert_eq!(
            from_legacy_backend_json(&json).unwrap().cpu_profile,
            crate::cpu_profile::CpuProfile::X86_64V2
        );
        assert_eq!(
            from_legacy_backend_json(&json).unwrap().cpu_profile_source,
            None
        );
        // The bincode form the tool reads carries all profile fields.
        let wire = bincode::serde::encode_to_vec(&v3, bincode::config::legacy()).unwrap();
        let (decoded, _): (Config, usize) =
            bincode::serde::decode_from_slice(&wire, bincode::config::legacy()).unwrap();
        assert_eq!(
            decoded.cpu_profile,
            crate::cpu_profile::CpuProfile::X86_64V3
        );
        assert_eq!(decoded.cpu_profile_source, Some(17));
    }

    #[test]
    fn inherited_seccomp_never_enters_the_legacy_form() {
        use crate::config::InheritedSeccomp;
        let unknown = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        assert_eq!(unknown.inherited_seccomp, InheritedSeccomp::Unknown);
        let measured = Config {
            inherited_seccomp: InheritedSeccomp::Filter(Some(2)),
            ..unknown.clone()
        };
        let json = to_legacy_backend_json(&measured).unwrap();
        assert_eq!(json, to_legacy_backend_json(&unknown).unwrap());
        assert!(!json.contains("inherited_seccomp"), "{json}");
        assert_eq!(
            from_legacy_backend_json(&json).unwrap().inherited_seccomp,
            InheritedSeccomp::Unknown
        );
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert_eq!(
            from_legacy_backend_json(&array).unwrap().inherited_seccomp,
            InheritedSeccomp::Unknown
        );
    }

    /// `Config` crosses Reverie RPC as legacy bincode, which is positional, so
    /// `inherited_seccomp` must be encoded whatever its value, the number of
    /// filters included.
    #[test]
    fn inherited_seccomp_round_trips_through_reverie_bincode() {
        use crate::config::InheritedSeccomp;
        for inherited_seccomp in [
            InheritedSeccomp::Unknown,
            InheritedSeccomp::Disabled,
            InheritedSeccomp::Strict,
            InheritedSeccomp::Filter(None),
            InheritedSeccomp::Filter(Some(3)),
        ] {
            let config = Config {
                inherited_seccomp,
                ..Config::default()
            };
            let wire = bincode::serde::encode_to_vec(&config, bincode::config::legacy()).unwrap();
            let (decoded, read): (Config, usize) =
                bincode::serde::decode_from_slice(&wire, bincode::config::legacy()).unwrap();
            assert_eq!(read, wire.len());
            assert_eq!(decoded.inherited_seccomp, inherited_seccomp);
            assert_eq!(
                bincode::serde::encode_to_vec(&decoded, bincode::config::legacy()).unwrap(),
                wire
            );
        }
    }

    /// `Seccomp` gives the mode, and `Seccomp_filters` the number of filters in
    /// filter mode.
    #[test]
    fn inherited_seccomp_is_read_from_a_status_file() {
        use crate::config::InheritedSeccomp;
        let status = |lines: &str| {
            InheritedSeccomp::parse(&format!(
                "Name:\tguest\nNoNewPrivs:\t1\n{lines}Speculation_Store_Bypass:\tthread vulnerable\n"
            ))
        };
        assert_eq!(
            status("Seccomp:\t0\nSeccomp_filters:\t0\n"),
            InheritedSeccomp::Disabled
        );
        assert_eq!(
            status("Seccomp:\t1\nSeccomp_filters:\t0\n"),
            InheritedSeccomp::Strict
        );
        assert_eq!(
            status("Seccomp:\t2\nSeccomp_filters:\t3\n"),
            InheritedSeccomp::Filter(Some(3))
        );
        assert_eq!(status("Seccomp:\t2\n"), InheritedSeccomp::Filter(None));
        assert_eq!(status(""), InheritedSeccomp::Unknown);
        assert_eq!(status("Seccomp:\t7\n"), InheritedSeccomp::Unknown);
        // This thread's own state is readable.
        assert_ne!(InheritedSeccomp::measure(), InheritedSeccomp::Unknown);
    }

    /// `target_timeslice_syscalls_only` never enters the legacy form, whatever
    /// its value: the legacy string keeps its bytes, and it reads
    /// back as false from both the object and the array form.
    #[test]
    fn target_timeslice_syscalls_only_never_enters_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            target_timeslice: NonZeroU64::new(1_000_000),
            ..Config::default()
        };
        let on = Config {
            target_timeslice_syscalls_only: true,
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(!json.contains("target_timeslice_syscalls_only"), "{json}");
        assert!(
            !from_legacy_backend_json(&json)
                .unwrap()
                .target_timeslice_syscalls_only
        );
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert!(
            !from_legacy_backend_json(&array)
                .unwrap()
                .target_timeslice_syscalls_only
        );
    }

    #[test]
    fn a_futex_wake_yields_only_when_it_woke_a_waiter() {
        let config = Config {
            futex_wake_yields: true,
            ..Config::default()
        };
        assert!(config.yields_after_futex_wake(1));
        assert!(config.yields_after_futex_wake(3));
        // A wake that found no waiter hands nothing off and keeps the slice.
        assert!(!config.yields_after_futex_wake(0));
        // Off by default.
        assert!(!Config::default().futex_wake_yields);
        assert!(!Config::default().yields_after_futex_wake(1));
    }

    #[test]
    fn a_futex_wake_yields_under_replay_too() {
        // Neither record holds the queue order the policy creates, so replay
        // must make the same decision as the recorded run.
        let config = Config {
            futex_wake_yields: true,
            ..Config::default()
        };
        let preemption_replay = Config {
            replay_preemptions_from: Some(PathBuf::from("/nonexistent/preemptions.json")),
            ..config.clone()
        };
        assert!(preemption_replay.yields_after_futex_wake(1));
        assert!(!preemption_replay.yields_after_futex_wake(0));
        let schedule_replay = Config {
            replay_schedule_from: Some(PathBuf::from("/nonexistent/schedule.json")),
            ..config
        };
        assert!(schedule_replay.yields_after_futex_wake(1));
    }

    #[test]
    fn scheduler_turn_charge_refuses_a_scaled_charge_below_one_nanosecond() {
        let default = Config::default();
        assert_eq!(default.scheduler_turn_charge(), Ok(500_000));
        let scaled = Config {
            clock_multiplier: Some(0.1),
            ..Config::default()
        };
        assert_eq!(scaled.scheduler_turn_charge(), Ok(50_000));
        let cost = |cost: u64, multiplier: f64| Config {
            scheduler_turn_cost: NonZeroU64::new(cost),
            clock_multiplier: Some(multiplier),
            ..Config::default()
        };
        assert_eq!(cost(10_000, 0.1).scheduler_turn_charge(), Ok(1_000));
        assert_eq!(cost(10, 0.1).scheduler_turn_charge(), Ok(1));
        // Truncation would make these zero: a finite poll would never expire.
        assert!(cost(9, 0.1).scheduler_turn_charge().is_err());
        assert!(cost(1, 0.1).scheduler_turn_charge().is_err());
        assert_eq!(
            cost(MAX_SCHEDULER_TURN_COST, 1.0).scheduler_turn_charge(),
            Ok(MAX_SCHEDULER_TURN_COST)
        );
        assert!(
            cost(MAX_SCHEDULER_TURN_COST + 1, 1.0)
                .scheduler_turn_charge()
                .is_err()
        );
        assert!(cost(u64::MAX, 1.0).scheduler_turn_charge().is_err());
        assert!(
            cost(MAX_SCHEDULER_TURN_COST, 1e12)
                .scheduler_turn_charge()
                .is_err()
        );
        // The scaled charge has the same upper bound as the cost: one charge
        // of 9e18 ns would exhaust the clock in two turns.
        assert!(
            cost(MAX_SCHEDULER_TURN_COST, 9e9)
                .scheduler_turn_charge()
                .is_err()
        );
        assert!(cost(1_000_000, 1_001.0).scheduler_turn_charge().is_err());
        assert_eq!(
            cost(1_000_000, 1_000.0).scheduler_turn_charge(),
            Ok(MAX_SCHEDULER_TURN_COST)
        );
        let message = cost(MAX_SCHEDULER_TURN_COST, 9e9)
            .scheduler_turn_charge()
            .unwrap_err();
        assert!(message.contains("at least 1"), "{message}");
        assert!(message.contains("at most 1000000000"), "{message}");
    }

    /// The in-memory rewrite admission every launch path applies: a host
    /// without LSE and a run with concurrent threads are each refused,
    /// and the serialized run on an LSE host is admitted.
    #[test]
    fn rewrite_exclusives_in_memory_admission_refuses_what_the_cli_refuses() {
        let on = Config {
            rewrite_exclusives_in_memory: true,
            sequentialize_threads: true,
            ..Config::default()
        };
        assert_eq!(
            on.rewrite_exclusives_in_memory_refusal(false),
            Some(RewriteExclusivesRefusal::HostLacksLseAtomics)
        );
        let concurrent = Config {
            sequentialize_threads: false,
            ..on.clone()
        };
        assert_eq!(
            concurrent.rewrite_exclusives_in_memory_refusal(true),
            Some(RewriteExclusivesRefusal::ThreadsNotSequentialized)
        );
        assert_eq!(on.rewrite_exclusives_in_memory_refusal(true), None);
        assert_eq!(
            Config::default().rewrite_exclusives_in_memory_refusal(false),
            None
        );
    }

    /// `rewrite_exclusives_in_memory` never enters the legacy form, whatever its value: the
    /// legacy string keeps its bytes, and it reads back as false from both
    /// the object and the array form.
    #[test]
    fn rewrite_exclusives_in_memory_never_enters_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            rewrite_exclusives_in_memory: true,
            rewrite_exclusives_overlay: vec!["/bin/guest".into()],
            rewrite_exclusives_overlay_requested: true,
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(!json.contains("rewrite_exclusives"), "{json}");
        let back = from_legacy_backend_json(&json).unwrap();
        assert!(!back.rewrite_exclusives_in_memory);
        assert!(back.rewrite_exclusives_overlay.is_empty());
        assert!(!back.rewrite_exclusives_overlay_requested);
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        let back = from_legacy_backend_json(&array).unwrap();
        assert!(!back.rewrite_exclusives_in_memory);
        assert!(back.rewrite_exclusives_overlay.is_empty());
        assert!(!back.rewrite_exclusives_overlay_requested);
    }

    /// `rewrite_exclusives_overlay` never enters the legacy form, whatever
    /// its value: the legacy string keeps its bytes, and it reads back as
    /// empty from both the object and the array form.
    #[test]
    fn rewrite_exclusives_overlay_never_enters_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            rewrite_exclusives_overlay: vec!["/bin/guest".into(), "/lib/libatomic.so.1".into()],
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(!json.contains("rewrite_exclusives_overlay"), "{json}");
        assert!(
            from_legacy_backend_json(&json)
                .unwrap()
                .rewrite_exclusives_overlay
                .is_empty()
        );
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert!(
            from_legacy_backend_json(&array)
                .unwrap()
                .rewrite_exclusives_overlay
                .is_empty()
        );
    }

    /// `count_store_exclusives` never enters the legacy form, whatever its
    /// value: the legacy string keeps its bytes, and it reads back as false
    /// from both the object and the array form.
    #[test]
    fn count_store_exclusives_never_enters_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            count_store_exclusives: true,
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(!json.contains("count_store_exclusives"), "{json}");
        assert!(
            !from_legacy_backend_json(&json)
                .unwrap()
                .count_store_exclusives
        );
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert!(
            !from_legacy_backend_json(&array)
                .unwrap()
                .count_store_exclusives
        );
    }

    /// `untrap_passthrough_syscalls` never enters the legacy form, whatever its
    /// value: the legacy string keeps its bytes, and it reads
    /// back as false from both the object and the array form.
    #[test]
    fn untrap_passthrough_syscalls_never_enters_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            untrap_passthrough_syscalls: true,
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(!json.contains("untrap_passthrough_syscalls"), "{json}");
        assert!(
            !from_legacy_backend_json(&json)
                .unwrap()
                .untrap_passthrough_syscalls
        );
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert!(
            !from_legacy_backend_json(&array)
                .unwrap()
                .untrap_passthrough_syscalls
        );
    }

    /// `futex_wake_yields` never enters the legacy form, whatever its
    /// value: the legacy string keeps its bytes, and it reads
    /// back as false from both the object and the array form.
    #[test]
    fn futex_wake_yields_never_enters_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            futex_wake_yields: true,
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(!json.contains("futex_wake_yields"), "{json}");
        assert!(!from_legacy_backend_json(&json).unwrap().futex_wake_yields);
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert!(!from_legacy_backend_json(&array).unwrap().futex_wake_yields);
    }

    /// `scheduler_turn_cost` never enters the legacy form, whatever its value:
    /// the legacy string keeps its bytes, and it reads back as
    /// unset from both the object and the array form.
    #[test]
    fn scheduler_turn_cost_never_enters_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            scheduler_turn_cost: NonZeroU64::new(10_000),
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(!json.contains("scheduler_turn_cost"), "{json}");
        assert_eq!(
            from_legacy_backend_json(&json).unwrap().scheduler_turn_cost,
            None
        );
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert_eq!(
            from_legacy_backend_json(&array)
                .unwrap()
                .scheduler_turn_cost,
            None
        );
    }

    /// `virtual_timeout` crosses Reverie RPC in the positional bincode form,
    /// set or not, and is rendered into the reproduce command; it never enters
    /// DBT's legacy form, whose runtime `hermit run` refuses it for.
    #[test]
    fn virtual_timeout_round_trips_but_never_enters_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            virtual_timeout: NonZeroU64::new(3_500_000_000),
            ..off.clone()
        };
        assert!(
            on.to_string().contains(" --virtual-timeout=3500000000"),
            "{on}"
        );
        assert!(!off.to_string().contains("--virtual-timeout"), "{off}");
        for config in [&off, &on] {
            let wire = bincode::serde::encode_to_vec(config, bincode::config::legacy()).unwrap();
            let (decoded, read): (Config, usize) =
                bincode::serde::decode_from_slice(&wire, bincode::config::legacy()).unwrap();
            assert_eq!(read, wire.len());
            assert_eq!(decoded.virtual_timeout, config.virtual_timeout);
        }
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(!json.contains("virtual_timeout"), "{json}");
        assert_eq!(
            from_legacy_backend_json(&json).unwrap().virtual_timeout,
            None
        );
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert_eq!(
            from_legacy_backend_json(&array).unwrap().virtual_timeout,
            None
        );
    }

    /// `cap_preemption_charge` never enters the legacy form, whatever its
    /// value: the legacy string keeps its bytes, and it reads
    /// back as false from both the object and the array form.
    #[test]
    fn cap_preemption_charge_never_enters_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            cap_preemption_charge: true,
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(!json.contains("cap_preemption_charge"), "{json}");
        assert!(
            !from_legacy_backend_json(&json)
                .unwrap()
                .cap_preemption_charge
        );
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert!(
            !from_legacy_backend_json(&array)
                .unwrap()
                .cap_preemption_charge
        );
    }

    /// `--cap-preemption-charge` is off by default, parses, and survives the
    /// rendered command line and serde, so a recorded configuration replays
    /// with the rule it ran with.
    #[test]
    fn cap_preemption_charge_is_opt_in_and_round_trips() {
        let default = Config::default();
        assert!(!default.cap_preemption_charge);
        assert!(!default.to_string().contains("--cap-preemption-charge"));
        let config = Config::parse_from(["detcore", "--cap-preemption-charge"]);
        assert!(config.cap_preemption_charge);
        let rendered = config.to_string();
        assert!(rendered.contains(" --cap-preemption-charge"), "{rendered}");
        let reparsed =
            Config::parse_from(std::iter::once("detcore").chain(rendered.split_whitespace()));
        assert!(reparsed.cap_preemption_charge);
        let json = serde_json::to_string(&config).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        assert!(back.cap_preemption_charge);
        let mut legacy: serde_json::Value = serde_json::to_value(&default).unwrap();
        legacy
            .as_object_mut()
            .unwrap()
            .remove("cap_preemption_charge");
        let old: Config = serde_json::from_value(legacy).unwrap();
        assert!(
            !old.cap_preemption_charge,
            "a configuration from before the option reads as off"
        );
    }

    /// `replaying` never enters the legacy form, whatever its value: the
    /// legacy string keeps its bytes, and it reads back as false
    /// from both the object and the array form.
    #[test]
    fn replaying_never_enters_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            replaying: true,
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(!json.contains("replaying"), "{json}");
        assert!(!from_legacy_backend_json(&json).unwrap().replaying);
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert!(!from_legacy_backend_json(&array).unwrap().replaying);
    }

    /// `Config` crosses Reverie RPC as legacy bincode, which is positional, so
    /// `replaying` must be encoded whatever its value: a field left out when
    /// false would shift every later field for the decoder.
    #[test]
    fn replaying_round_trips_through_reverie_bincode() {
        for replaying in [false, true] {
            let config = Config {
                replaying,
                ..Config::default()
            };
            let wire = bincode::serde::encode_to_vec(&config, bincode::config::legacy()).unwrap();
            let (decoded, read): (Config, usize) =
                bincode::serde::decode_from_slice(&wire, bincode::config::legacy()).unwrap();
            assert_eq!(read, wire.len());
            assert_eq!(decoded.replaying, replaying);
            assert_eq!(
                bincode::serde::encode_to_vec(&decoded, bincode::config::legacy()).unwrap(),
                wire
            );
        }
    }

    /// `signal_at` came after the legacy form froze, so it never enters it,
    /// and both the object and the array form read it back as unset.
    #[test]
    fn signal_at_never_enters_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            signal_at: Some(SignalAt {
                turn: 500,
                signal: libc::SIGUSR1,
            }),
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(!json.contains("signal_at"), "{json}");
        assert_eq!(from_legacy_backend_json(&json).unwrap().signal_at, None);
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert_eq!(from_legacy_backend_json(&array).unwrap().signal_at, None);
    }

    /// `virtual_cpus` came after the legacy form froze, so it never enters
    /// it: the DBT runtime's environment keeps its bytes, and the option
    /// reads back as unset (`hermit run` refuses it with DBT).
    #[test]
    fn virtual_cpus_never_enter_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            virtual_cpus: NonZeroU32::new(4),
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(!json.contains("virtual_cpus"), "{json}");
        assert_eq!(from_legacy_backend_json(&json).unwrap().virtual_cpus, None);
    }

    /// `--virtual-cpus` takes a count from 1 to the 128 CPUs Detcore's mask
    /// holds, and names the range when refused; it reaches the reproducer
    /// command line.
    #[test]
    fn virtual_cpus_parse_from_1_to_128() {
        let parse = |count: &str| {
            Config::try_parse_from(["detcore".to_owned(), format!("--virtual-cpus={count}")])
        };
        assert_eq!(parse("1").unwrap().virtual_cpus, NonZeroU32::new(1));
        assert_eq!(parse("128").unwrap().virtual_cpus, NonZeroU32::new(128));
        for refused in ["0", "129", "-1", "four"] {
            let error = parse(refused).unwrap_err().to_string();
            assert!(error.contains("from 1 to 128"), "{refused}: {error}");
        }
        let config = parse("8").unwrap();
        assert_eq!(config.virtual_cpu_count(), 8);
        assert!(config.to_string().contains(" --virtual-cpus=8"));
        assert_eq!(Config::default().virtual_cpus, None);
        assert_eq!(Config::default().virtual_cpu_count(), 1);
        assert!(!Config::default().to_string().contains("virtual-cpus"));
    }

    /// `eager_lone_preemptions` came after the legacy form froze, so it never
    /// enters it (DBT has no PMU timer, where it would act), and both the
    /// object and the array form read it back as false.
    #[test]
    fn eager_lone_preemptions_never_enters_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            eager_lone_preemptions: true,
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(!json.contains("eager_lone_preemptions"), "{json}");
        assert!(
            !from_legacy_backend_json(&json)
                .unwrap()
                .eager_lone_preemptions
        );
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert!(
            !from_legacy_backend_json(&array)
                .unwrap()
                .eager_lone_preemptions
        );
    }

    /// `stacktrace_at_schedule_positions` came after the legacy form froze, so
    /// it never enters it, and both the object and the array form read it
    /// back as false.
    #[test]
    fn stacktrace_at_schedule_positions_never_enters_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            stacktrace_at_schedule_positions: true,
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(!json.contains("stacktrace_at_schedule_positions"), "{json}");
        assert!(
            !from_legacy_backend_json(&json)
                .unwrap()
                .stacktrace_at_schedule_positions
        );
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert!(
            !from_legacy_backend_json(&array)
                .unwrap()
                .stacktrace_at_schedule_positions
        );
    }

    /// `--stacktrace-at-schedule-positions` needs a replayed schedule, is
    /// refused with `--strict-schedule-boundaries` (whose indices are
    /// boundary-stream positions already), and round-trips through the
    /// configuration's command line.
    #[test]
    fn stacktrace_at_schedule_positions_needs_a_replayed_schedule_and_round_trips() {
        assert!(Config::try_parse_from(["detcore", "--stacktrace-at-schedule-positions"]).is_err());
        assert!(
            Config::try_parse_from([
                "detcore",
                "--replay-schedule-from=schedule.json",
                "--strict-schedule-boundaries",
                "--stacktrace-at-schedule-positions",
            ])
            .is_err()
        );
        let config = Config::try_parse_from([
            "detcore",
            "--replay-schedule-from=schedule.json",
            "--stacktrace-event=7",
            "--stacktrace-at-schedule-positions",
        ])
        .unwrap();
        assert!(config.stacktrace_at_schedule_positions);
        let rendered = config.to_string();
        assert!(
            rendered.contains(" --stacktrace-at-schedule-positions"),
            "{rendered}"
        );
        let words: Vec<String> = shell_words::split(&rendered)
            .unwrap()
            .into_iter()
            .filter(|word| word.starts_with("--replay-") || word.starts_with("--stacktrace-"))
            .collect();
        let reparsed =
            Config::try_parse_from(std::iter::once("detcore".to_owned()).chain(words)).unwrap();
        assert!(reparsed.stacktrace_at_schedule_positions, "{rendered}");
        assert_eq!(
            reparsed.stacktrace_event, config.stacktrace_event,
            "{rendered}"
        );
    }

    /// `Config` crosses Reverie RPC as positional legacy bincode, so
    /// `signal_at` is encoded whether set or not.
    #[test]
    fn signal_at_round_trips_through_reverie_bincode() {
        for signal_at in [
            None,
            Some(SignalAt {
                turn: 3000,
                signal: libc::SIGUSR1,
            }),
        ] {
            let config = Config {
                signal_at,
                ..Config::default()
            };
            let wire = bincode::serde::encode_to_vec(&config, bincode::config::legacy()).unwrap();
            let (decoded, read): (Config, usize) =
                bincode::serde::decode_from_slice(&wire, bincode::config::legacy()).unwrap();
            assert_eq!(read, wire.len());
            assert_eq!(decoded.signal_at, signal_at);
        }
    }

    /// `notice_host_network_abstract_binds` came after the legacy form froze, so it never
    /// enters it, and both the object and the array form read it back as
    /// false.
    #[test]
    fn notice_host_network_abstract_binds_never_enters_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            notice_host_network_abstract_binds: true,
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(
            !json.contains("notice_host_network_abstract_binds"),
            "{json}"
        );
        assert!(
            !from_legacy_backend_json(&json)
                .unwrap()
                .notice_host_network_abstract_binds
        );
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert!(
            !from_legacy_backend_json(&array)
                .unwrap()
                .notice_host_network_abstract_binds
        );
    }

    /// `Config` crosses Reverie RPC as positional legacy bincode, so
    /// `notice_host_network_abstract_binds` is encoded whether set or not.
    #[test]
    fn notice_host_network_abstract_binds_round_trips_through_reverie_bincode() {
        for notice_host_network_abstract_binds in [false, true] {
            let config = Config {
                notice_host_network_abstract_binds,
                ..Config::default()
            };
            let wire = bincode::serde::encode_to_vec(&config, bincode::config::legacy()).unwrap();
            let (decoded, read): (Config, usize) =
                bincode::serde::decode_from_slice(&wire, bincode::config::legacy()).unwrap();
            assert_eq!(read, wire.len());
            assert_eq!(
                decoded.notice_host_network_abstract_binds,
                notice_host_network_abstract_binds
            );
        }
    }

    /// The access trace's four fields came after the legacy form froze, so
    /// they never enter it, and both the object and the array form read them
    /// back as unset, 0, unset and unset. Set or not, they cross Reverie RPC's positional
    /// bincode and the CLI spelling (`Display`) unchanged.
    #[test]
    fn access_trace_never_enters_the_legacy_form_and_round_trips() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            access_trace: Some(PathBuf::from("/var/run 1.access")),
            access_trace_from: 7586,
            access_trace_max_records: Some(40),
            access_trace_max_bytes: Some(4096),
            watch: vec!["0x404010:4:rw".parse().unwrap()],
            watch_from: 12,
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(!json.contains("access_trace"), "{json}");
        let decoded = from_legacy_backend_json(&json).unwrap();
        assert_eq!((decoded.access_trace, decoded.access_trace_from), (None, 0));
        assert_eq!(decoded.access_trace_max_records, None);
        assert_eq!(decoded.access_trace_max_bytes, None);
        assert_eq!((decoded.watch, decoded.watch_from), (vec![], 0));
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        let decoded = from_legacy_backend_json(&array).unwrap();
        assert_eq!((decoded.access_trace, decoded.access_trace_from), (None, 0));
        assert_eq!(
            (
                decoded.access_trace_max_records,
                decoded.access_trace_max_bytes
            ),
            (None, None)
        );

        for config in [Config::default(), on.clone()] {
            let wire = bincode::serde::encode_to_vec(&config, bincode::config::legacy()).unwrap();
            let (decoded, read): (Config, usize) =
                bincode::serde::decode_from_slice(&wire, bincode::config::legacy()).unwrap();
            assert_eq!(read, wire.len());
            assert_eq!(decoded.access_trace, config.access_trace);
            assert_eq!(decoded.access_trace_from, config.access_trace_from);
            assert_eq!(
                decoded.access_trace_max_records,
                config.access_trace_max_records
            );
            assert_eq!(
                decoded.access_trace_max_bytes,
                config.access_trace_max_bytes
            );
            assert_eq!(
                (decoded.watch, decoded.watch_from),
                (config.watch.clone(), config.watch_from)
            );
        }

        let spelled = on.to_string();
        assert!(
            spelled.contains(
                " --record-access-trace-to='/var/run 1.access' --access-trace-from=7586 \
                 --access-trace-max-records=40 --access-trace-max-bytes=4096 \
                 --watch=0x404010:4:rw --watch-from=12"
            ),
            "{spelled}"
        );
        assert!(!Config::default().to_string().contains("access-trace"));
        let reparsed = Config::parse_from([
            "hermit",
            "--record-access-trace-to=/var/run 1.access",
            "--access-trace-from=7586",
            "--access-trace-max-records=40",
            "--access-trace-max-bytes=4096",
            "--watch=0x404010:4:rw",
            "--watch-from=12",
        ]);
        assert_eq!(reparsed.access_trace, on.access_trace);
        assert_eq!(reparsed.access_trace_from, 7586);
        assert_eq!(reparsed.access_trace_max_records, Some(40));
        assert_eq!(reparsed.access_trace_max_bytes, Some(4096));
        assert_eq!((reparsed.watch, reparsed.watch_from), (on.watch, 12));
    }

    /// Anonymous-memory logging is newer than the frozen DBT configuration.
    /// Omit every mode from its object and positional forms while preserving
    /// the exact mode in the current serde and bincode encodings.
    #[test]
    fn anonymous_mapping_logs_never_enter_the_legacy_form() {
        let off = Config {
            detlog_stack: true,
            detlog_regs: true,
            detlog_io_buffers: false,
            detlog_regs_cadence: 7,
            ..Config::default()
        };
        let legacy = to_legacy_backend_json(&off).unwrap();
        assert!(!legacy.contains("detlog_anon"), "{legacy}");
        for mode in [None, Some(DetlogAnon::Mappings), Some(DetlogAnon::Pages)] {
            let config = Config {
                detlog_anon: mode,
                ..off.clone()
            };
            let object = to_legacy_backend_json(&config).unwrap();
            assert_eq!(object, legacy);
            let values: Vec<serde_json::Value> = ordered_entries(&object)
                .into_iter()
                .map(|(_, value)| value)
                .collect();
            let array = serde_json::to_string(&values).unwrap();
            for encoded in [object, array] {
                let decoded = from_legacy_backend_json(&encoded).unwrap();
                assert_eq!(decoded.detlog_anon, None);
                assert_eq!(to_legacy_backend_json(&decoded).unwrap(), legacy);
            }

            let current = serde_json::to_value(&config).unwrap();
            assert_eq!(current["detlog_anon"], serde_json::to_value(mode).unwrap());
            let decoded: Config = serde_json::from_value(current).unwrap();
            assert_eq!(decoded.detlog_anon, mode);
            let wire = bincode::serde::encode_to_vec(&config, bincode::config::legacy()).unwrap();
            let (decoded, read): (Config, usize) =
                bincode::serde::decode_from_slice(&wire, bincode::config::legacy()).unwrap();
            assert_eq!(read, wire.len());
            assert_eq!(decoded.detlog_anon, mode);
        }
    }

    /// The in-guest reserved-signal set as an in-guest default-route run
    /// carries it: SIGSYS, SIGSEGV and SIGTRAP.
    fn in_guest_reserved_set() -> u64 {
        [libc::SIGSYS, libc::SIGSEGV, libc::SIGTRAP]
            .into_iter()
            .fold(0, |set, signal| set | (1_u64 << (signal - 1)))
    }

    /// `in_guest_reserved_signals` is empty for DBT, so the legacy form never
    /// carries it, and it reads back as empty from both the object and the
    /// array form.
    #[test]
    fn in_guest_reserved_signals_never_enters_the_legacy_form() {
        let empty = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let reserved = Config {
            in_guest_reserved_signals: in_guest_reserved_set(),
            ..empty.clone()
        };
        let json = to_legacy_backend_json(&reserved).unwrap();
        assert_eq!(json, to_legacy_backend_json(&empty).unwrap());
        assert!(!json.contains("in_guest_reserved_signals"), "{json}");
        assert_eq!(
            from_legacy_backend_json(&json)
                .unwrap()
                .in_guest_reserved_signals,
            0
        );
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert_eq!(
            from_legacy_backend_json(&array)
                .unwrap()
                .in_guest_reserved_signals,
            0
        );
    }

    /// The set reaches every Tool image through Reverie's bincode handshake
    /// unchanged, empty or not, and a JSON configuration without the key reads
    /// as empty (its serde default).
    #[test]
    fn in_guest_reserved_signals_round_trips_and_defaults_to_empty() {
        for set in [0, in_guest_reserved_set()] {
            let config = Config {
                in_guest_reserved_signals: set,
                ..Config::default()
            };
            let wire = bincode::serde::encode_to_vec(&config, bincode::config::legacy()).unwrap();
            let (decoded, read): (Config, usize) =
                bincode::serde::decode_from_slice(&wire, bincode::config::legacy()).unwrap();
            assert_eq!(read, wire.len());
            assert_eq!(decoded.in_guest_reserved_signals, set);
            let json: Config =
                serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();
            assert_eq!(json.in_guest_reserved_signals, set);
        }
        let mut encoded = serde_json::to_value(Config::default()).unwrap();
        encoded
            .as_object_mut()
            .unwrap()
            .remove("in_guest_reserved_signals")
            .expect("the field is serialized");
        let decoded: Config = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.in_guest_reserved_signals, 0);
    }

    /// `reserves_in_guest_signal` answers from the set alone: each reserved
    /// signal and no other, never signal 0 or a number outside 1 to 64, and
    /// nothing at all while the set is empty.
    #[test]
    fn reserves_in_guest_signal_reads_only_the_set() {
        let empty = Config::default();
        let reserved = Config {
            in_guest_reserved_signals: in_guest_reserved_set(),
            ..Config::default()
        };
        for signal in -1..=66 {
            assert!(!empty.reserves_in_guest_signal(signal), "{signal}");
            let expected = [libc::SIGSYS, libc::SIGSEGV, libc::SIGTRAP]
                .into_iter()
                .any(|reserved| i64::from(reserved) == signal);
            assert_eq!(
                reserved.reserves_in_guest_signal(signal),
                expected,
                "{signal}"
            );
        }
        // The top bit is signal 64, the last one Linux accepts.
        let top = Config {
            in_guest_reserved_signals: 1 << 63,
            ..Config::default()
        };
        assert!(top.reserves_in_guest_signal(64));
        assert!(!top.reserves_in_guest_signal(65));
        assert!(!top.reserves_in_guest_signal(0));
    }

    /// `in_guest_refuses_host_timed_producers` is false for DBT, so the legacy
    /// form never carries it, and it reads back as false from both the object
    /// and the array form.
    #[test]
    fn in_guest_refuses_host_timed_producers_never_enters_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            in_guest_refuses_host_timed_producers: true,
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(
            !json.contains("in_guest_refuses_host_timed_producers"),
            "{json}"
        );
        assert!(
            !from_legacy_backend_json(&json)
                .unwrap()
                .in_guest_refuses_host_timed_producers
        );
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert!(
            !from_legacy_backend_json(&array)
                .unwrap()
                .in_guest_refuses_host_timed_producers
        );
    }

    /// The setting reaches every Tool image through Reverie's bincode
    /// handshake unchanged, on or off, and a JSON configuration without the
    /// key reads as off (its serde default).
    #[test]
    fn in_guest_refuses_host_timed_producers_round_trips_and_defaults_to_off() {
        for refuses in [false, true] {
            let config = Config {
                in_guest_refuses_host_timed_producers: refuses,
                ..Config::default()
            };
            let wire = bincode::serde::encode_to_vec(&config, bincode::config::legacy()).unwrap();
            let (decoded, read): (Config, usize) =
                bincode::serde::decode_from_slice(&wire, bincode::config::legacy()).unwrap();
            assert_eq!(read, wire.len());
            assert_eq!(decoded.in_guest_refuses_host_timed_producers, refuses);
            let json: Config =
                serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();
            assert_eq!(json.in_guest_refuses_host_timed_producers, refuses);
        }
        let mut encoded = serde_json::to_value(Config::default()).unwrap();
        encoded
            .as_object_mut()
            .unwrap()
            .remove("in_guest_refuses_host_timed_producers")
            .expect("the field is serialized");
        let decoded: Config = serde_json::from_value(encoded).unwrap();
        assert!(!decoded.in_guest_refuses_host_timed_producers);
    }

    /// `in_guest_signal_delivery` is false for DBT, so the legacy form never
    /// carries it, and it reads back as false from both the object and the
    /// array form.
    #[test]
    fn in_guest_signal_delivery_never_enters_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            in_guest_signal_delivery: true,
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(!json.contains("in_guest_signal_delivery"), "{json}");
        assert!(
            !from_legacy_backend_json(&json)
                .unwrap()
                .in_guest_signal_delivery
        );
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert!(
            !from_legacy_backend_json(&array)
                .unwrap()
                .in_guest_signal_delivery
        );
    }

    /// `--replay-edited-preemptions` stays out of the legacy (DBT) form, and
    /// reads as false from it.
    #[test]
    fn replay_edited_preemptions_never_enters_the_legacy_form() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            replay_preemptions_from: Some("record.preempts".into()),
            ..Config::default()
        };
        let on = Config {
            replay_edited_preemptions: true,
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(!json.contains("replay_edited_preemptions"), "{json}");
        assert!(
            !from_legacy_backend_json(&json)
                .unwrap()
                .replay_edited_preemptions
        );
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert!(
            !from_legacy_backend_json(&array)
                .unwrap()
                .replay_edited_preemptions
        );
    }

    /// `--replay-edited-preemptions` describes the record
    /// `--replay-preemptions-from` reads, so it needs one. It round-trips
    /// through the rendered options and through Reverie's bincode handshake,
    /// and a JSON configuration without the key reads as off.
    #[test]
    fn replay_edited_preemptions_needs_a_replayed_record_and_round_trips() {
        assert!(Config::try_parse_from(["detcore", "--replay-edited-preemptions"]).is_err());
        let config = Config::try_parse_from([
            "detcore",
            "--replay-preemptions-from=record.preempts",
            "--replay-edited-preemptions",
        ])
        .unwrap();
        assert!(config.replay_edited_preemptions);
        // The rendered options parse back to the same replay settings.
        let rendered = config.to_string();
        let words: Vec<String> = shell_words::split(&rendered)
            .unwrap()
            .into_iter()
            .filter(|word| word.starts_with("--replay-"))
            .collect();
        let reparsed =
            Config::try_parse_from(std::iter::once("detcore".to_owned()).chain(words)).unwrap();
        assert!(reparsed.replay_edited_preemptions, "{rendered}");
        assert_eq!(
            reparsed.replay_preemptions_from, config.replay_preemptions_from,
            "{rendered}"
        );
        let strict =
            Config::try_parse_from(["detcore", "--replay-preemptions-from=record.preempts"])
                .unwrap();
        assert!(!strict.to_string().contains("--replay-edited-preemptions"));

        for config in [&config, &strict] {
            let wire = bincode::serde::encode_to_vec(config, bincode::config::legacy()).unwrap();
            let (decoded, read): (Config, usize) =
                bincode::serde::decode_from_slice(&wire, bincode::config::legacy()).unwrap();
            assert_eq!(read, wire.len());
            assert_eq!(
                decoded.replay_edited_preemptions,
                config.replay_edited_preemptions
            );
        }
        let mut encoded = serde_json::to_value(&config).unwrap();
        encoded
            .as_object_mut()
            .unwrap()
            .remove("replay_edited_preemptions")
            .expect("the field is serialized");
        let decoded: Config = serde_json::from_value(encoded).unwrap();
        assert!(!decoded.replay_edited_preemptions);
    }

    /// The gate reaches every Tool image through Reverie's bincode handshake
    /// unchanged, on or off, and a JSON configuration without the key reads
    /// as off (its serde default).
    #[test]
    fn in_guest_signal_delivery_round_trips_and_defaults_to_off() {
        for on in [false, true] {
            let config = Config {
                in_guest_signal_delivery: on,
                ..Config::default()
            };
            let wire = bincode::serde::encode_to_vec(&config, bincode::config::legacy()).unwrap();
            let (decoded, read): (Config, usize) =
                bincode::serde::decode_from_slice(&wire, bincode::config::legacy()).unwrap();
            assert_eq!(read, wire.len());
            assert_eq!(decoded.in_guest_signal_delivery, on);
            let json: Config =
                serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();
            assert_eq!(json.in_guest_signal_delivery, on);
        }
        let mut encoded = serde_json::to_value(Config::default()).unwrap();
        encoded
            .as_object_mut()
            .unwrap()
            .remove("in_guest_signal_delivery")
            .expect("the field is serialized");
        let decoded: Config = serde_json::from_value(encoded).unwrap();
        assert!(!decoded.in_guest_signal_delivery);
    }

    /// `annotated_priority_replay` never enters the legacy form, and reads
    /// back as false from both its object and array forms; it crosses
    /// Reverie's positional bincode and JSON unchanged, and a JSON
    /// configuration without the key reads as off.
    #[test]
    fn annotated_priority_replay_never_enters_the_legacy_form_and_round_trips() {
        let off = Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        };
        let on = Config {
            annotated_priority_replay: true,
            ..off.clone()
        };
        let json = to_legacy_backend_json(&on).unwrap();
        assert_eq!(json, to_legacy_backend_json(&off).unwrap());
        assert!(!json.contains("annotated_priority_replay"), "{json}");
        assert!(
            !from_legacy_backend_json(&json)
                .unwrap()
                .annotated_priority_replay
        );
        let values: Vec<serde_json::Value> = ordered_entries(&json)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let array = serde_json::to_string(&values).unwrap();
        assert!(
            !from_legacy_backend_json(&array)
                .unwrap()
                .annotated_priority_replay
        );
        for annotated in [false, true] {
            let config = Config {
                annotated_priority_replay: annotated,
                ..Config::default()
            };
            let wire = bincode::serde::encode_to_vec(&config, bincode::config::legacy()).unwrap();
            let (decoded, read): (Config, usize) =
                bincode::serde::decode_from_slice(&wire, bincode::config::legacy()).unwrap();
            assert_eq!(read, wire.len());
            assert_eq!(decoded.annotated_priority_replay, annotated);
            let json: Config =
                serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();
            assert_eq!(json.annotated_priority_replay, annotated);
        }
        let mut encoded = serde_json::to_value(Config::default()).unwrap();
        encoded
            .as_object_mut()
            .unwrap()
            .remove("annotated_priority_replay")
            .expect("the field is serialized");
        let decoded: Config = serde_json::from_value(encoded).unwrap();
        assert!(!decoded.annotated_priority_replay);
    }

    /// Schedule events are observed exactly where they are traced, and also
    /// in replay of a callback-annotated priority history, whose recording
    /// traced them, whether or not that replay records.
    #[test]
    fn annotated_priority_replay_observes_schedule_events() {
        let plain = Config::default();
        assert!(!plain.should_trace_schedevent() && !plain.observes_schedevents());
        let recording = Config {
            record_preemptions: true,
            ..Config::default()
        };
        assert!(recording.should_trace_schedevent() && recording.observes_schedevents());
        let replay = Config {
            replay_preemptions_from: Some("history.json".into()),
            ..Config::default()
        };
        assert!(!replay.observes_schedevents(), "an unannotated history");
        let annotated = Config {
            annotated_priority_replay: true,
            ..replay
        };
        assert!(!annotated.should_trace_schedevent());
        assert!(annotated.observes_schedevents());
    }

    /// An annotated priority history replays only on an effective
    /// configuration with the backend's own preemption timer events,
    /// sequentialized threads, an active maximum, precise timers and RCB time.
    /// A configuration nobody prepared for a backend has no timer events and
    /// is refused; each other unmet requirement is named in turn, and the
    /// answer never changes the configuration.
    #[test]
    fn annotated_priority_replay_requires_a_qualified_effective_configuration() {
        let unprepared = Config {
            annotated_priority_replay: true,
            replay_preemptions_from: Some("history.json".into()),
            ..Config::default()
        };
        assert_eq!(
            unprepared.annotated_priority_replay_unqualified(),
            Some("cannot deliver Detcore preemption timer events")
        );
        let qualified = Config {
            backend: BackendCapabilities::PTRACE,
            sequentialize_threads: true,
            max_timeslice: NonZeroU64::new(1_000_000),
            imprecise_timers: false,
            no_rcb_time: false,
            ..unprepared.clone()
        };
        assert_eq!(qualified.annotated_priority_replay_unqualified(), None);
        let before = serde_json::to_value(&qualified).unwrap();
        for (config, setting) in [
            (
                Config {
                    sequentialize_threads: false,
                    ..qualified.clone()
                },
                "does not sequentialize threads (--no-sequentialize-threads)",
            ),
            (
                Config {
                    max_timeslice: None,
                    ..qualified.clone()
                },
                "has its preemption timer disabled (--max-timeslice=disabled)",
            ),
            (
                Config {
                    imprecise_timers: true,
                    ..qualified.clone()
                },
                "requests imprecise timers (--imprecise-timers)",
            ),
            (
                Config {
                    no_rcb_time: true,
                    ..qualified.clone()
                },
                "requests logical time without RCBs (--no-rcb-time)",
            ),
        ] {
            assert_eq!(
                config.annotated_priority_replay_unqualified(),
                Some(setting)
            );
        }
        assert_eq!(serde_json::to_value(&qualified).unwrap(), before);
    }

    /// `--signal-at` takes `turn=N:SIGNAL` with SIGNAL a name or a number.
    /// SIGKILL and SIGSTOP are refused, by name or number: the kernel acts on
    /// them at once, outside the scheduler's protocols for them.
    #[test]
    fn signal_at_parses_a_turn_and_a_catchable_signal() {
        let usr1 = SignalAt {
            turn: 500,
            signal: libc::SIGUSR1,
        };
        assert_eq!(parse_signal_at("turn=500:SIGUSR1"), Ok(usr1));
        assert_eq!(parse_signal_at("turn=500:10"), Ok(usr1));
        for refused in [
            "turn=500:SIGKILL",
            "turn=500:9",
            "turn=500:SIGSTOP",
            "turn=500:19",
        ] {
            let error = parse_signal_at(refused).unwrap_err();
            assert!(
                error.contains("cannot be caught or blocked"),
                "{refused}: {error}"
            );
        }
        for malformed in [
            "turn=500",
            "500:SIGUSR1",
            "turn=-1:SIGUSR1",
            "turn=5:SIGNOPE",
        ] {
            assert!(parse_signal_at(malformed).is_err(), "{malformed}");
        }
    }

    /// Serde reads a derived struct from a JSON array by position, so the
    /// legacy form could be given as one. Each array reads as the object
    /// whose values it lists in order, every backend's capabilities included.
    #[test]
    fn a_legacy_positional_array_reads_as_the_object_it_lists() {
        for (name, backend) in [
            ("PTRACE", BackendCapabilities::PTRACE),
            ("E9PATCH", BackendCapabilities::E9PATCH),
            (
                "ptrace without exit transitions",
                ptrace_without_exit_transitions(),
            ),
            ("LITEINST_IN_GUEST", BackendCapabilities::LITEINST_IN_GUEST),
            ("SABRE", BackendCapabilities::SABRE),
            ("DBT", BackendCapabilities::DBT),
            ("KVM", BackendCapabilities::KVM),
        ] {
            for sequentialize_threads in [false, true] {
                let sent = Config {
                    backend,
                    sequentialize_threads,
                    seed: 7,
                    chaos: true,
                    ..Config::default()
                };
                let json = to_legacy_backend_json(&sent).unwrap();
                assert_controlled_key(&json, &backend, sequentialize_threads, name);
                let array = positional(&json);
                assert!(array.starts_with('['), "{array}");
                let received = from_legacy_backend_json(&array).unwrap();
                assert_eq!(received.backend, legacy_expressible(backend), "{name}");
                assert_eq!(
                    received.sequentialize_threads, sequentialize_threads,
                    "{name}"
                );
                assert_eq!(received.seed, 7, "{name}");
                assert!(received.chaos, "{name}");
                assert_eq!(to_legacy_backend_json(&received).unwrap(), json, "{name}");
            }
        }
    }

    /// An array fails the parse where the legacy form's array did: with an
    /// element past the last position, with an element of the wrong type, or
    /// short of a field that has no default. The last position,
    /// `interrupt_at`, has none, so every shorter array fails.
    #[test]
    fn a_malformed_legacy_positional_array_fails_the_parse() {
        let json = to_legacy_backend_json(&Config {
            backend: BackendCapabilities::DBT,
            ..Config::default()
        })
        .unwrap();
        let entries = ordered_entries(&json);
        let values: Vec<serde_json::Value> =
            entries.iter().map(|(_, value)| value.clone()).collect();
        let parse = |values: &[serde_json::Value]| {
            from_legacy_backend_json(&serde_json::to_string(values).unwrap())
        };
        assert!(parse(&values).is_ok());

        let mut longer = values.clone();
        longer.push(serde_json::Value::Bool(false));
        assert!(parse(&longer).is_err());

        assert_eq!(entries.last().unwrap().0, "interrupt_at");
        for len in 0..values.len() {
            assert!(parse(&values[..len]).is_err(), "{len} elements");
        }

        for (position, (key, _)) in entries.iter().enumerate() {
            if LEGACY_BACKEND_KEY_NAMES.contains(&key.as_str()) || key == "seed" {
                let mut mistyped = values.clone();
                mistyped[position] = serde_json::json!("0");
                assert!(parse(&mistyped).is_err(), "{key}");
            }
        }
    }

    /// Selects the child half of
    /// [`a_legacy_configuration_decodes_whatever_the_ambient_hermit_settings`] and
    /// carries the input it decodes.
    const LEGACY_INPUT_IN_CHILD: &str = "DETCORE_MODEL_TEST_LEGACY_INPUT";
    /// The legacy object the child's decoded configuration must re-encode as.
    const LEGACY_EXPECTED_IN_CHILD: &str = "DETCORE_MODEL_TEST_LEGACY_EXPECTED";
    /// Printed by a child that decoded its input, so that a parent can tell
    /// that run from one that selected no test.
    const LEGACY_DECODED_IN_CHILD: &str = "legacy configuration decoded under the ambient settings";

    /// Clap reads HERMIT_EPOCH, HERMIT_PRNG, HERMIT_SCHED_SEED and HERMIT_CPU_PROFILE when it
    /// parses a `Config`, and exits the process when one of them does not
    /// parse. The legacy decoder read none of them, so a valid legacy object
    /// or array decodes to the configuration it lists whatever they hold. Each
    /// decode runs in a child process given those variables unset, set to
    /// text that does not parse, or set to bytes that are not UTF-8. The child
    /// builds no `Config` before decoding, which would read them itself.
    #[test]
    fn a_legacy_configuration_decodes_whatever_the_ambient_hermit_settings() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        if let Some(input) = std::env::var_os(LEGACY_INPUT_IN_CHILD) {
            let expected = std::env::var(LEGACY_EXPECTED_IN_CHILD).unwrap();
            let received = from_legacy_backend_json(input.to_str().unwrap()).unwrap();
            assert_eq!(
                received.backend,
                legacy_expressible(BackendCapabilities::DBT)
            );
            assert_eq!(received.seed, 7);
            assert_eq!(received.sched_seed, Some(11));
            assert_eq!(to_legacy_backend_json(&received).unwrap(), expected);
            println!("{LEGACY_DECODED_IN_CHILD}");
            return;
        }

        const VARIABLES: [&str; 4] = [
            "HERMIT_EPOCH",
            "HERMIT_PRNG",
            "HERMIT_SCHED_SEED",
            "HERMIT_CPU_PROFILE",
        ];
        let text = OsStr::new("not-a-value");
        let bytes = OsStr::from_bytes(b"\xff");
        let mut settings: Vec<Vec<(&str, &OsStr)>> = vec![Vec::new()];
        for variable in VARIABLES {
            settings.push(vec![(variable, text)]);
            settings.push(vec![(variable, bytes)]);
        }
        settings.push(VARIABLES.map(|variable| (variable, text)).to_vec());
        settings.push(VARIABLES.map(|variable| (variable, bytes)).to_vec());

        let object = to_legacy_backend_json(&Config {
            backend: BackendCapabilities::DBT,
            seed: 7,
            sched_seed: Some(11),
            ..Config::default()
        })
        .unwrap();
        let array = positional(&object);
        assert!(array.starts_with('['), "{array}");
        let test = format!(
            "{}::a_legacy_configuration_decodes_whatever_the_ambient_hermit_settings",
            module_path!().split_once("::").unwrap().1
        );
        for input in [&object, &array] {
            for setting in &settings {
                let mut child = std::process::Command::new(std::env::current_exe().unwrap());
                child
                    .args(["--exact", &test, "--nocapture"])
                    .env(LEGACY_INPUT_IN_CHILD, input)
                    .env(LEGACY_EXPECTED_IN_CHILD, &object);
                for variable in VARIABLES {
                    child.env_remove(variable);
                }
                for (variable, value) in setting {
                    child.env(variable, value);
                }
                let output = child.output().unwrap();
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                assert!(
                    output.status.success()
                        && stdout.contains(LEGACY_DECODED_IN_CHILD)
                        && stdout.contains("test result: ok. 1 passed; 0 failed;"),
                    "decoding {} under {setting:?} did not succeed exactly once: {}\n{stdout}\n{stderr}",
                    if input.starts_with('[') {
                        "the array"
                    } else {
                        "the object"
                    },
                    output.status,
                );
            }
        }
    }

    /// Set in the child process of
    /// [`hermit_cpu_profile_supplies_the_profile_only_when_the_flag_is_absent`]:
    /// the arguments it parses, one per line.
    const CPU_PROFILE_ARGS_IN_CHILD: &str = "DETCORE_MODEL_TEST_CPU_PROFILE_ARGS";

    /// HERMIT_CPU_PROFILE gives `--cpu-profile` its value when the flag is
    /// absent: unset is x86-64-v2, an explicit flag wins over the variable,
    /// and a value that names no profile is refused. The wire default stays
    /// x86-64-v2 whatever the variable holds. Each parse runs in a child
    /// process given exactly the variable under test, because Clap reads the
    /// process environment.
    #[test]
    fn hermit_cpu_profile_supplies_the_profile_only_when_the_flag_is_absent() {
        use clap::Parser;

        if let Some(args) = std::env::var_os(CPU_PROFILE_ARGS_IN_CHILD) {
            let args = args.into_string().unwrap();
            let argv = std::iter::once("config").chain(args.lines());
            match Config::try_parse_from(argv) {
                Ok(config) => println!(
                    "cpu-profile={} wire={} default={}",
                    config.cpu_profile.name(),
                    config_wire_default().cpu_profile.name(),
                    // `Config::default` itself, where the environment it
                    // reads parses (it exits on an invalid value).
                    match Config::try_parse_from(["config"]) {
                        Ok(_) => Config::default().cpu_profile.name(),
                        Err(_) => "refused",
                    }
                ),
                Err(error) => println!("refused: {:?}", error.kind()),
            }
            return;
        }

        let test = format!(
            "{}::hermit_cpu_profile_supplies_the_profile_only_when_the_flag_is_absent",
            module_path!().split_once("::").unwrap().1
        );
        let parse = |variable: Option<&str>, args: &[&str]| -> String {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .args(["--exact", &test, "--nocapture"])
                .env(CPU_PROFILE_ARGS_IN_CHILD, args.join("\n"));
            for name in [
                "HERMIT_EPOCH",
                "HERMIT_PRNG",
                "HERMIT_SCHED_SEED",
                "HERMIT_CPU_PROFILE",
            ] {
                child.env_remove(name);
            }
            if let Some(value) = variable {
                child.env("HERMIT_CPU_PROFILE", value);
            }
            let output = child.output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            assert!(
                output.status.success() && stdout.contains("test result: ok. 1 passed; 0 failed;"),
                "the child for {variable:?} {args:?} did not run exactly once: {}\n{stdout}",
                output.status,
            );
            stdout
                .lines()
                .find(|line| line.starts_with("cpu-profile=") || line.starts_with("refused: "))
                .unwrap_or_else(|| panic!("the child printed no verdict:\n{stdout}"))
                .to_owned()
        };

        // Unset on the command line is auto; the wire default stays v2.
        assert_eq!(
            parse(None, &[]),
            "cpu-profile=auto wire=x86-64-v2 default=auto"
        );
        assert_eq!(
            parse(Some("auto"), &[]),
            "cpu-profile=auto wire=x86-64-v2 default=auto"
        );
        assert_eq!(
            parse(Some("auto"), &["--cpu-profile=x86-64-v2"]),
            "cpu-profile=x86-64-v2 wire=x86-64-v2 default=auto"
        );
        assert_eq!(
            parse(Some("x86-64-v3"), &[]),
            "cpu-profile=x86-64-v3 wire=x86-64-v2 default=x86-64-v3"
        );
        assert_eq!(
            parse(Some("host-no-rdrand"), &[]),
            "cpu-profile=host-no-rdrand wire=x86-64-v2 default=host-no-rdrand"
        );
        assert_eq!(
            parse(None, &["--cpu-profile=x86-64-v3"]),
            "cpu-profile=x86-64-v3 wire=x86-64-v2 default=auto"
        );
        assert_eq!(
            parse(Some("x86-64-v3"), &["--cpu-profile=x86-64-v2"]),
            "cpu-profile=x86-64-v2 wire=x86-64-v2 default=x86-64-v3"
        );
        assert_eq!(parse(Some("x86-64-v4"), &[]), "refused: InvalidValue");
        assert_eq!(
            parse(Some("x86-64-v4"), &["--cpu-profile=x86-64-v2"]),
            "cpu-profile=x86-64-v2 wire=x86-64-v2 default=refused"
        );
    }

    /// `backend` and `shared_dequeue_timers` were not keys of the legacy form,
    /// which ignored every key it did not name. They are still ignored,
    /// whatever they hold, however often they appear and wherever they stand,
    /// so the legacy keys alone decide the capabilities. A key the legacy form
    /// named still fails the parse when it appears twice.
    #[test]
    fn keys_the_legacy_form_did_not_name_are_ignored() {
        let sent = Config {
            backend: legacy_expressible(BackendCapabilities::DBT),
            seed: 7,
            // A launch sends a resolved profile: the legacy form has no
            // profile field and reads back as v2.
            cpu_profile: crate::cpu_profile::CpuProfile::X86_64V2,
            ..Config::default()
        };
        let json = to_legacy_backend_json(&sent).unwrap();
        let kvm = serde_json::to_string(&BackendCapabilities::KVM).unwrap();
        for extra in [
            r#""backend":null"#.to_owned(),
            r#""shared_dequeue_timers":null"#.to_owned(),
            r#""backend":null,"shared_dequeue_timers":null"#.to_owned(),
            format!(r#""backend":{kvm},"shared_dequeue_timers":true"#),
            r#""backend":7,"backend":{"tracks_process_children":false}"#.to_owned(),
            r#""shared_dequeue_timers":"yes","shared_dequeue_timers":true"#.to_owned(),
            r#""no_such_field":[1,{"x":null}]"#.to_owned(),
        ] {
            for edited in [
                json.replacen('{', &format!("{{{extra},"), 1),
                format!("{},{extra}}}", &json[..json.len() - 1]),
            ] {
                let received = from_legacy_backend_json(&edited).unwrap();
                assert_eq!(received.backend, sent.backend, "{edited}");
                assert_eq!(
                    serde_json::to_string(&received).unwrap(),
                    serde_json::to_string(&sent).unwrap(),
                    "{edited}"
                );
                assert_eq!(to_legacy_backend_json(&received).unwrap(), json, "{edited}");
            }
        }

        let repeated = format!("{},\"seed\":7}}", &json[..json.len() - 1]);
        assert!(from_legacy_backend_json(&repeated).is_err());
    }

    #[test]
    fn network_perturbation_seed_never_falls_back_to_scheduler_seeds() {
        let mut config = Config {
            seed: 41,
            sched_seed: Some(42),
            ..Config::default()
        };
        assert_eq!(config.network_trace.network_perturb_seed, None);

        config.network_trace = NetworkTraceConfig {
            mode: crate::network_trace::NetworkTraceMode::Replay,
            path: Some("network.trace".into()),
            network_perturb_seed: Some(43),
        };
        assert_eq!(config.seed, 41);
        assert_eq!(config.sched_seed(), 42);
        assert_eq!(config.network_trace.network_perturb_seed, Some(43));
    }

    #[test]
    fn missing_mountinfo_provenance_deserializes_as_empty() {
        let mut value = serde_json::to_value(Config::default()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("mountinfo_root_rewrites");
        value
            .as_object_mut()
            .unwrap()
            .remove("mountinfo_device_rewrites");
        value.as_object_mut().unwrap().remove("mountinfo_mount_ids");
        value
            .as_object_mut()
            .unwrap()
            .remove("mountinfo_mount_ids_captured");
        value
            .as_object_mut()
            .unwrap()
            .remove("mount_id_assignment_order");
        value
            .as_object_mut()
            .unwrap()
            .remove("container_mount_table");
        value
            .as_object_mut()
            .unwrap()
            .remove("mountinfo_mount_namespace");
        value
            .as_object_mut()
            .unwrap()
            .remove("directory_placeholder");
        value.as_object_mut().unwrap().remove("container_procfs");
        let restored: Config = serde_json::from_value(value).unwrap();
        assert!(restored.mountinfo_root_rewrites.is_empty());
        assert!(restored.mountinfo_device_rewrites.is_empty());
        assert!(restored.mountinfo_mount_ids.is_empty());
        assert!(!restored.mountinfo_mount_ids_captured);
        assert!(restored.mount_id_assignment_order.is_empty());
        assert!(restored.container_mount_table.is_none());
        assert!(restored.mountinfo_mount_namespace.is_none());
        assert!(restored.directory_placeholder.is_none());
        assert!(!restored.container_procfs);
    }

    /// Lone slice ends are processed without stops only where a slice end
    /// reads nothing from the guest but its branch count: each listed option
    /// turns it off.
    #[test]
    fn lone_preemptions_are_replayed_only_where_a_slice_end_reads_nothing_else() {
        let base = Config {
            sequentialize_threads: true,
            backend: reverie::BackendCapabilities::PTRACE,
            ..Config::parse_from(["detcore"])
        };
        assert!(base.replays_lone_preemptions());
        let off: Vec<(&str, Config)> = vec![
            (
                "--eager-lone-preemptions",
                Config {
                    eager_lone_preemptions: true,
                    ..base.clone()
                },
            ),
            (
                "no sequentialization",
                Config {
                    sequentialize_threads: false,
                    ..base.clone()
                },
            ),
            (
                "--max-timeslice=disabled",
                Config {
                    max_timeslice: None,
                    ..base.clone()
                },
            ),
            (
                "--no-rcb-time",
                Config {
                    no_rcb_time: true,
                    ..base.clone()
                },
            ),
            (
                "--imprecise-timers",
                Config {
                    imprecise_timers: true,
                    ..base.clone()
                },
            ),
            (
                "--chaos-per-thread-slowdown",
                Config {
                    chaos_per_thread_slowdown: true,
                    ..base.clone()
                },
            ),
            (
                "--strict-schedule-boundaries",
                Config {
                    strict_schedule_boundaries: true,
                    ..base.clone()
                },
            ),
            (
                "--target-timeslice",
                Config {
                    target_timeslice: NonZeroU64::new(1_000),
                    ..base.clone()
                },
            ),
            (
                "--preemption-stacktrace",
                Config {
                    preemption_stacktrace: true,
                    ..base.clone()
                },
            ),
            (
                "--stacktrace-event",
                Config {
                    stacktrace_event: vec![(1, None)],
                    ..base.clone()
                },
            ),
            (
                "--stop-after-turn",
                Config {
                    stop_after_turn: Some(1),
                    ..base.clone()
                },
            ),
            (
                "--stop-after-iter",
                Config {
                    stop_after_iter: Some(1),
                    ..base.clone()
                },
            ),
            (
                "--interrupt-at",
                Config {
                    interrupt_at: vec![(DetTid::from_raw(3), 1)],
                    ..base.clone()
                },
            ),
            (
                "--signal-at",
                Config {
                    signal_at: Some(parse_signal_at("turn=1:SIGUSR1").unwrap()),
                    ..base.clone()
                },
            ),
            (
                "--happens-before",
                Config {
                    happens_before: Some(crate::happens_before::HappensBeforeProgram {
                        version: 1,
                        anchors: Default::default(),
                        edges: Vec::new(),
                        max_hold_ns: None,
                    }),
                    ..base.clone()
                },
            ),
            (
                "--chaos --chaos-instruction-offset=8",
                Config {
                    chaos: true,
                    chaos_instruction_offset: 8,
                    ..base.clone()
                },
            ),
            (
                "--gdbserver",
                Config {
                    gdbserver: true,
                    ..base.clone()
                },
            ),
        ];
        for (option, config) in off {
            assert!(!config.replays_lone_preemptions(), "{option}");
        }
        // Recording and both replays keep them: a recording writes a lone
        // end's event without a RIP, and a replay takes without a stop only
        // the ends it may (`Detcore::replay_allows_stopless_end`).
        let on: Vec<(&str, Config)> = vec![
            (
                "--record-preemptions",
                Config {
                    record_preemptions: true,
                    ..base.clone()
                },
            ),
            (
                "--replay-preemptions-from",
                Config {
                    replay_preemptions_from: Some("p".into()),
                    ..base.clone()
                },
            ),
            (
                "--replay-schedule-from",
                Config {
                    replay_schedule_from: Some("s".into()),
                    record_preemptions: true,
                    ..base.clone()
                },
            ),
        ];
        for (option, config) in on {
            assert!(config.replays_lone_preemptions(), "{option}");
        }
        // Chaos alone draws no offsets: its slice ends need no stop.
        assert!(
            Config {
                chaos: true,
                ..base.clone()
            }
            .replays_lone_preemptions()
        );
    }

    #[test]
    fn runs_post_fork_parses_all_modes_and_defaults_to_child() {
        assert_eq!(Config::default().runs_post_fork, RunsPostFork::Child);
        assert_eq!(
            Config::parse_from(["detcore", "--runs-post-fork=parent"]).runs_post_fork,
            RunsPostFork::Parent
        );
        assert_eq!(
            Config::parse_from(["detcore", "--runs-post-fork=random"]).runs_post_fork,
            RunsPostFork::Random
        );
        assert!(Config::try_parse_from(["detcore", "--runs-post-fork=invalid"]).is_err());
    }

    #[test]
    fn proc_level_parses_both_levels_defaults_to_minimal_and_round_trips() {
        assert_eq!(Config::default().proc_level, ProcLevel::Minimal);
        assert!(!Config::default().to_string().contains("--proc"));
        let standard = Config::parse_from(["detcore", "--proc=standard"]);
        assert_eq!(standard.proc_level, ProcLevel::Standard);
        assert!(standard.to_string().contains(" --proc=standard"));
        assert_eq!(
            Config::parse_from(["detcore", "--proc=minimal"]).proc_level,
            ProcLevel::Minimal
        );
        assert!(Config::try_parse_from(["detcore", "--proc=host"]).is_err());
        assert!(ProcLevel::Minimal < ProcLevel::Standard);
    }

    #[test]
    fn untrap_passthrough_syscalls_is_opt_in_and_round_trips() {
        assert!(!Config::default().untrap_passthrough_syscalls);
        let config = Config::parse_from(["detcore", "--untrap-passthrough-syscalls"]);
        assert!(config.untrap_passthrough_syscalls);
        assert!(
            config
                .to_string()
                .contains(" --untrap-passthrough-syscalls")
        );
        assert!(!Config::default().to_string().contains("untrap"));
    }

    #[test]
    fn panic_on_rcb_overshoot_is_opt_in_and_round_trips() {
        assert!(!Config::default().panic_on_rcb_overshoot);

        let config = Config::parse_from(["detcore", "--panic-on-rbc-overshoot"]);
        assert!(config.panic_on_rcb_overshoot);
        assert!(config.to_string().contains(" --panic-on-rbc-overshoot"));

        let alias = Config::parse_from(["detcore", "--panic-on-rcb-overshoot"]);
        assert!(alias.panic_on_rcb_overshoot);
    }

    #[test]
    fn config_display_preserves_nondefault_post_fork_modes() {
        let mut config = Config {
            runs_post_fork: RunsPostFork::Parent,
            ..Config::default()
        };
        assert!(config.to_string().contains(" --runs-post-fork=parent"));

        config.runs_post_fork = RunsPostFork::Random;
        assert!(config.to_string().contains(" --runs-post-fork=random"));
    }

    /// `--expired-wait-charge` is unset by default, parses both values, is
    /// printed by `Display` (reproducer commands are built from it) only when
    /// set, round-trips through serde, and never enters the legacy form.
    #[test]
    fn the_expired_wait_charge_is_printed_when_set_and_never_enters_the_legacy_form() {
        assert_eq!(Config::default().expired_wait_charge, None);
        assert!(
            !Config::default()
                .to_string()
                .contains("--expired-wait-charge")
        );
        for (value, charge) in [
            ("each", ExpiredWaitCharge::Each),
            ("once", ExpiredWaitCharge::Once),
        ] {
            let set =
                Config::try_parse_from(["detcore", &format!("--expired-wait-charge={value}")])
                    .unwrap();
            assert_eq!(set.expired_wait_charge, Some(charge));
            assert!(
                set.to_string()
                    .contains(&format!(" --expired-wait-charge={value}")),
                "{set}"
            );
            let reparsed: Config =
                serde_json::from_str(&serde_json::to_string(&set).unwrap()).unwrap();
            assert_eq!(reparsed.expired_wait_charge, Some(charge));
            let legacy = to_legacy_backend_json(&set).unwrap();
            assert!(!legacy.contains("expired_wait_charge"), "{legacy}");
            assert_eq!(
                from_legacy_backend_json(&legacy)
                    .unwrap()
                    .expired_wait_charge,
                None
            );
        }
    }

    /// `--sleep-registration` is unset by default, parses both values, is
    /// printed by `Display` (reproducer commands are built from it) only when
    /// set, round-trips through serde, and never enters the legacy form.
    #[test]
    fn the_sleep_registration_is_printed_when_set_and_never_enters_the_legacy_form() {
        assert_eq!(Config::default().sleep_registration, None);
        assert!(
            !Config::default()
                .to_string()
                .contains("--sleep-registration")
        );
        for (value, registration) in [
            ("first-pass", SleepRegistration::FirstPass),
            ("when-chosen", SleepRegistration::WhenChosen),
        ] {
            let set = Config::try_parse_from(["detcore", &format!("--sleep-registration={value}")])
                .unwrap();
            assert_eq!(set.sleep_registration, Some(registration));
            assert!(
                set.to_string()
                    .contains(&format!(" --sleep-registration={value}")),
                "{set}"
            );
            let reparsed: Config =
                serde_json::from_str(&serde_json::to_string(&set).unwrap()).unwrap();
            assert_eq!(reparsed.sleep_registration, Some(registration));
            let legacy = to_legacy_backend_json(&set).unwrap();
            assert!(!legacy.contains("sleep_registration"), "{legacy}");
            assert_eq!(
                from_legacy_backend_json(&legacy)
                    .unwrap()
                    .sleep_registration,
                None
            );
        }
    }

    /// By default a queued sleep registers at the first pass only where the scheduler reads a
    /// sleeper's signal state: a backend whose kernel signal targets it predicts (ptrace), or any
    /// backend under the in-guest delivery gate. DBT, SaBRe, LiteInst and KVM register when
    /// chosen by default. An explicit value decides here; a `first-pass` the backend cannot honour
    /// is refused at launch. Race search and `hermit record`/`replay` keep their rule.
    #[test]
    fn first_pass_sleep_registration_is_the_default_only_where_signal_state_is_read() {
        let config = |backend: BackendCapabilities,
                      gate: bool,
                      sleep_registration: Option<SleepRegistration>| Config {
            backend,
            in_guest_signal_delivery: gate,
            sleep_registration,
            sequentialize_threads: true,
            ..Config::default()
        };
        for (name, backend) in [
            ("PTRACE", BackendCapabilities::PTRACE),
            ("E9PATCH", BackendCapabilities::E9PATCH),
        ] {
            let default = config(backend, false, None);
            assert!(default.reads_sleeper_signal_state(), "{name}");
            assert!(default.queued_sleeps_registered_at_first_pass(), "{name}");
        }
        for (name, backend) in [
            ("DBT", BackendCapabilities::DBT),
            ("SABRE", BackendCapabilities::SABRE),
            ("LITEINST_IN_GUEST", BackendCapabilities::LITEINST_IN_GUEST),
            ("KVM", BackendCapabilities::KVM),
        ] {
            let default = config(backend, false, None);
            assert!(!default.reads_sleeper_signal_state(), "{name}");
            assert!(!default.queued_sleeps_registered_at_first_pass(), "{name}");
            let gated = config(backend, true, None);
            assert!(gated.reads_sleeper_signal_state(), "{name} under the gate");
            assert!(
                gated.queued_sleeps_registered_at_first_pass(),
                "{name} under the gate"
            );
            assert!(
                config(backend, false, Some(SleepRegistration::FirstPass))
                    .queued_sleeps_registered_at_first_pass(),
                "{name}, asked for"
            );
        }
        let unserialized = Config {
            sequentialize_threads: false,
            ..config(BackendCapabilities::PTRACE, false, None)
        };
        assert!(!unserialized.reads_sleeper_signal_state());
        assert!(!unserialized.queued_sleeps_registered_at_first_pass());
        let chaos = Config {
            chaos: true,
            ..config(BackendCapabilities::PTRACE, false, None)
        };
        assert!(!chaos.queued_sleeps_registered_at_first_pass());
        let recorded = Config {
            recordreplay_modes: true,
            ..config(
                BackendCapabilities::PTRACE,
                false,
                Some(SleepRegistration::FirstPass),
            )
        };
        assert!(!recorded.queued_sleeps_registered_at_first_pass());
    }

    #[test]
    fn the_work_span_report_is_off_by_default_and_never_enters_the_legacy_form() {
        assert_eq!(Config::default().work_span_report, None);
        let on = Config::try_parse_from(["detcore", "--work-span-report=/tmp/ws.json"]).unwrap();
        assert_eq!(
            on.work_span_report.as_deref(),
            Some(std::path::Path::new("/tmp/ws.json"))
        );
        assert!(on.to_string().contains(" --work-span-report=/tmp/ws.json"));
        let reparsed: Config = serde_json::from_str(&serde_json::to_string(&on).unwrap()).unwrap();
        assert_eq!(reparsed.work_span_report, on.work_span_report);
        let mut old = serde_json::to_value(Config::default()).unwrap();
        old.as_object_mut().unwrap().remove("work_span_report");
        let old: Config = serde_json::from_value(old).unwrap();
        assert_eq!(old.work_span_report, None);
        let legacy = to_legacy_backend_json(&on).unwrap();
        assert!(!legacy.contains("work_span_report"), "{legacy}");
        assert_eq!(
            from_legacy_backend_json(&legacy).unwrap().work_span_report,
            None
        );
    }

    #[test]
    fn a_fatal_delivery_check_is_off_by_default_and_never_enters_the_legacy_form() {
        assert!(!Config::default().signal_delivery_check_fatal);
        let fatal = Config {
            signal_delivery_check_fatal: true,
            ..Config::default()
        };
        let json = serde_json::to_string(&fatal).unwrap();
        let reparsed: Config = serde_json::from_str(&json).unwrap();
        assert!(reparsed.signal_delivery_check_fatal);
        let mut old = serde_json::to_value(Config::default()).unwrap();
        old.as_object_mut()
            .unwrap()
            .remove("signal_delivery_check_fatal");
        let old: Config = serde_json::from_value(old).unwrap();
        assert!(!old.signal_delivery_check_fatal);
        let legacy = to_legacy_backend_json(&fatal).unwrap();
        assert!(!legacy.contains("signal_delivery_check_fatal"), "{legacy}");
        assert!(
            !from_legacy_backend_json(&legacy)
                .unwrap()
                .signal_delivery_check_fatal
        );
    }

    #[test]
    fn the_signal_state_copy_is_on_by_default_and_its_off_switch_round_trips() {
        let dflt = Config::default();
        assert!(dflt.signal_state_copy);
        assert!(!dflt.to_string().contains("signal-state-copy"));
        // A configuration serialized before the field existed reads as on.
        let mut json = serde_json::to_value(&dflt).unwrap();
        json.as_object_mut().unwrap().remove("signal_state_copy");
        let old: Config = serde_json::from_value(json).unwrap();
        assert!(old.signal_state_copy);
        // Off renders as the flag, so that a command rebuilt from a
        // configuration (analyze's reproduction commands) keeps it off.
        let off = Config::parse_from(["detcore", "--no-signal-state-copy"]);
        assert!(!off.signal_state_copy);
        let rendered = off.to_string();
        assert!(rendered.contains(" --no-signal-state-copy"), "{rendered}");
        let reparsed = Config::parse_from(
            std::iter::once("detcore".to_string())
                .chain(rendered.split_whitespace().map(String::from)),
        );
        assert!(!reparsed.signal_state_copy);
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1149)
    #[test]
    fn chaos_per_thread_slowdown_is_opt_in_and_round_trips() {
        // Off by default; the factor default is present but inert.
        let dflt = Config::default();
        assert!(!dflt.chaos_per_thread_slowdown);
        assert_eq!(dflt.chaos_slowdown_max_factor, 10.0);
        // Default (disabled) config does not emit the flags.
        assert!(!dflt.to_string().contains("--chaos-per-thread-slowdown"));

        let config = Config::parse_from([
            "detcore",
            "--chaos",
            "--chaos-per-thread-slowdown",
            "--chaos-slowdown-max-factor=4.5",
        ]);
        assert!(config.chaos_per_thread_slowdown);
        assert_eq!(config.chaos_slowdown_max_factor, 4.5);

        // The Display round-trips both flags into the recorded schedule artifact.
        let rendered = config.to_string();
        assert!(rendered.contains(" --chaos-per-thread-slowdown"));
        assert!(rendered.contains(" --chaos-slowdown-max-factor=4.5"));
        let reparsed = Config::parse_from(
            std::iter::once("detcore".to_string())
                .chain(rendered.split_whitespace().map(String::from)),
        );
        assert!(reparsed.chaos_per_thread_slowdown);
        assert_eq!(reparsed.chaos_slowdown_max_factor, 4.5);
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    #[test]
    fn chaos_epoch_length_is_opt_in_and_round_trips() {
        // Off by default (single stable factor == plain per-thread-slowdown).
        let dflt = Config::default();
        assert_eq!(dflt.chaos_epoch_length_ns, 0);
        assert!(!dflt.to_string().contains("--chaos-epoch-length-ns"));

        // Epochs are only emitted alongside per-thread-slowdown.
        let config = Config::parse_from([
            "detcore",
            "--chaos",
            "--chaos-per-thread-slowdown",
            "--chaos-epoch-length-ns=100000",
        ]);
        assert_eq!(config.chaos_epoch_length_ns, 100000);

        let rendered = config.to_string();
        assert!(rendered.contains(" --chaos-epoch-length-ns=100000"));
        let reparsed = Config::parse_from(
            std::iter::once("detcore".to_string())
                .chain(rendered.split_whitespace().map(String::from)),
        );
        assert_eq!(reparsed.chaos_epoch_length_ns, 100000);

        // Without per-thread-slowdown the epoch flag is inert and not rendered.
        let no_slowdown = Config::parse_from(["detcore", "--chaos", "--chaos-epoch-length-ns=100"]);
        assert_eq!(no_slowdown.chaos_epoch_length_ns, 100);
        assert!(!no_slowdown.to_string().contains("--chaos-epoch-length-ns"));
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1149)
    #[test]
    #[should_panic(expected = "chaos_slowdown_max_factor must be finite and in")]
    fn validate_rejects_chaos_slowdown_max_factor_below_one() {
        let mut config = Config {
            chaos_slowdown_max_factor: 0.5,
            ..Default::default()
        };
        config.validate();
    }

    #[test]
    #[should_panic(expected = "max_timeslice must be at least one RCB")]
    fn validate_rejects_max_timeslice_below_one_rcb() {
        let mut config = Config {
            max_timeslice: NonZeroU64::new(NANOS_PER_RCB as u64 - 1),
            ..Default::default()
        };

        config.validate();
    }

    #[test]
    fn validate_accepts_one_rcb_max_timeslice() {
        let mut config = Config {
            max_timeslice: NonZeroU64::new(NANOS_PER_RCB as u64),
            ..Default::default()
        };

        config.validate();
    }

    #[test]
    #[should_panic(expected = "clock_multiplier must be finite and positive")]
    fn validate_rejects_invalid_clock_multiplier() {
        let mut config = Config {
            clock_multiplier: Some(0.0),
            ..Default::default()
        };
        config.validate();
    }

    #[test]
    #[should_panic(expected = "max_timeslice must be at least one RCB")]
    fn validate_scales_one_rcb_minimum_with_clock_multiplier() {
        let mut config = Config {
            max_timeslice: NonZeroU64::new(10),
            clock_multiplier: Some(2.0),
            ..Default::default()
        };
        config.validate();
    }

    #[test]
    fn config_fingerprint_sees_the_width_of_every_backend_capability() {
        // `BackendCapabilities` lives in Reverie, outside the fingerprinted
        // sources, so only its encoding can reveal a change to it. Every
        // field encodes at a fixed width whatever its value (a bool, or the
        // `u8` width of `hardware_watchpoints`, in one byte; the `u64`
        // signal set of `publishable_parent_death_signals` in eight), so the
        // encoded default carries every field, and adding, removing or
        // widening one changes the bytes. A field that is optional would have
        // to be present in the default, or a change to its width would encode
        // as the same bare `None`; this fails first. The one optional field,
        // `rcb_clock_contract`, is present: `config_wire_default` sets
        // ptrace's contract.
        let config = config_wire_default();
        let fields = serde_json::to_value(config.backend).unwrap();
        let fields = fields.as_object().unwrap();
        // The true widths from the capability table: every field is a bool
        // except `hardware_watchpoints`, a `u8` count,
        // `publishable_parent_death_signals`, a `u64` set of signal bits, and
        // `rcb_clock_contract`, an optional unit-variant enum, present here.
        for (name, value) in fields {
            match name.as_str() {
                "hardware_watchpoints" => {
                    let width = value.as_u64().expect("a u8 count encodes as a number");
                    assert!(width <= u64::from(u8::MAX), "{name}={width} exceeds a u8");
                }
                "publishable_parent_death_signals" => {
                    value
                        .as_u64()
                        .expect("a u64 signal set encodes as a number");
                }
                "rcb_clock_contract" => {
                    value
                        .as_str()
                        .expect("the default's contract is present, a unit variant's name");
                }
                _ => assert!(value.is_boolean(), "{name}={value} is not a bool"),
            }
        }
        let encode = |config: &Config| {
            bincode::serde::encode_to_vec(config, bincode::config::legacy()).unwrap()
        };
        // One byte per field (the option's tag for `rcb_clock_contract`),
        // seven more for the `u64` signal set, and four for the contract's
        // `u32` variant index.
        assert_eq!(
            bincode::serde::encode_to_vec(config.backend, bincode::config::legacy())
                .unwrap()
                .len(),
            fields.len() + 7 + 4
        );
        // The backend's values are part of the encoded default.
        let flipped = config.clone().with_backend(|backend| {
            backend.tracks_process_children = !backend.tracks_process_children
        });
        assert_eq!(encode(&flipped).len(), encode(&config).len());
        assert_ne!(encode(&flipped), encode(&config));
        // The one numeric capability: another width within a `u8` changes
        // the bytes at the same length; a wider type would change the length
        // asserted above.
        let widened = config.clone().with_backend(|backend| {
            backend.hardware_watchpoints = backend.hardware_watchpoints.wrapping_add(1)
        });
        assert_eq!(encode(&widened).len(), encode(&config).len());
        assert_ne!(encode(&widened), encode(&config));
        // The signal set: another set changes the bytes at the same length.
        let resigned = config
            .clone()
            .with_backend(|backend| backend.publishable_parent_death_signals ^= 1 << 63);
        assert_eq!(encode(&resigned).len(), encode(&config).len());
        assert_ne!(encode(&resigned), encode(&config));
        // The contract: its absence drops the variant index.
        let uncontracted = config
            .clone()
            .with_backend(|backend| backend.rcb_clock_contract = None);
        assert_eq!(encode(&uncontracted).len() + 4, encode(&config).len());
    }

    #[test]
    fn config_fingerprint_includes_clock_rpc_definitions() {
        let config = config_wire_default();
        let wire = bincode::serde::encode_to_vec(&config, bincode::config::legacy()).unwrap();
        let named_shape = serde_json::to_string(&config).unwrap();
        let current = config_wire_fingerprint();
        let clock_source = include_bytes!("time.rs").as_slice();

        // The previous guard covered these same Config bytes and definitions,
        // but omitted DetTime. Adding a positional clock field could therefore
        // pass the handshake guard and corrupt the following request on decode.
        let without_clock: Vec<_> = CONFIG_DEFINITION_SOURCES
            .iter()
            .copied()
            .filter(|source| *source != clock_source)
            .collect();
        assert_ne!(
            fingerprint_of_config_material(&wire, &named_shape, &without_clock),
            current,
            "the published fingerprint must reject source inputs that omit the RPC clock"
        );

        // Hold all Config material fixed and remove only the added serialized
        // clock field from its definition. A future clock-only change must also
        // invalidate the existing artifact guard, independently of config.rs.
        let changed_clock =
            include_str!("time.rs").replacen("    inherited_nanos: LogicalDuration,", "", 1);
        assert_ne!(changed_clock.as_bytes(), clock_source);
        let changed_sources: Vec<_> = CONFIG_DEFINITION_SOURCES
            .iter()
            .map(|source| {
                if *source == clock_source {
                    changed_clock.as_bytes()
                } else {
                    *source
                }
            })
            .collect();
        assert_ne!(
            fingerprint_of_config_material(&wire, &named_shape, &changed_sources),
            current,
            "a clock-only serialized field change must invalidate the fingerprint"
        );
    }

    /// The inode RPCs carry `RawInode`, whose layout neither encoding of
    /// `Config` shows. A plugin built before `RawInode` gained its device
    /// sends one `u64` where the coordinator decodes two, so a change to its
    /// definition must invalidate the fingerprint.
    #[test]
    fn config_fingerprint_includes_the_inode_rpc_identity() {
        let config = config_wire_default();
        let wire = bincode::serde::encode_to_vec(&config, bincode::config::legacy()).unwrap();
        let named_shape = serde_json::to_string(&config).unwrap();
        let current = config_wire_fingerprint();
        let inode_source = include_bytes!("fd.rs").as_slice();
        assert!(CONFIG_DEFINITION_SOURCES.contains(&inode_source));
        let without_device = include_str!("fd.rs").replacen("    pub dev: u64,\n", "", 1);
        assert_ne!(without_device.as_bytes(), inode_source);
        let changed_sources: Vec<_> = CONFIG_DEFINITION_SOURCES
            .iter()
            .map(|source| {
                if *source == inode_source {
                    without_device.as_bytes()
                } else {
                    *source
                }
            })
            .collect();
        assert_ne!(
            fingerprint_of_config_material(&wire, &named_shape, &changed_sources),
            current,
            "an inode-identity layout change must invalidate the fingerprint"
        );
    }

    /// `Config::watch` is empty in the encoded default, so neither encoding
    /// shows the layout of its element, `WatchArg`: a plugin built before a
    /// change to it would decode a run's watches against another layout. A
    /// change to its definition must invalidate the fingerprint.
    #[test]
    fn config_fingerprint_includes_the_watch_element_layout() {
        let config = config_wire_default();
        assert!(config.watch.is_empty(), "the default hides the element");
        let wire = bincode::serde::encode_to_vec(&config, bincode::config::legacy()).unwrap();
        let named_shape = serde_json::to_string(&config).unwrap();
        let current = config_wire_fingerprint();
        let watch_source = include_bytes!("watch.rs").as_slice();
        assert!(CONFIG_DEFINITION_SOURCES.contains(&watch_source));
        let without_len = include_str!("watch.rs").replacen("    pub len: u8,\n", "", 1);
        assert_ne!(without_len.as_bytes(), watch_source);
        let changed_sources: Vec<_> = CONFIG_DEFINITION_SOURCES
            .iter()
            .map(|source| {
                if *source == watch_source {
                    without_len.as_bytes()
                } else {
                    *source
                }
            })
            .collect();
        assert_ne!(
            fingerprint_of_config_material(&wire, &named_shape, &changed_sources),
            current,
            "a watch element layout change must invalidate the fingerprint"
        );
    }

    #[test]
    fn a_config_fingerprint_mismatch_names_both_fingerprints() {
        let ours = config_wire_fingerprint();
        assert_eq!(
            config_fingerprint_mismatch(std::ffi::OsStr::new(&ours)),
            None
        );
        let other = config_fingerprint_mismatch(std::ffi::OsStr::new("0123456789abcdef"))
            .expect("another fingerprint is a mismatch");
        assert!(other.contains(&ours), "{other}");
        assert!(other.contains("0123456789abcdef"), "{other}");
        use std::os::unix::ffi::OsStrExt;
        assert!(config_fingerprint_mismatch(std::ffi::OsStr::from_bytes(b"\xff")).is_some());
    }

    #[test]
    fn config_fingerprint_is_stable_and_shape_sensitive() {
        // STABLE: a build must agree with itself, or the guard would reject a
        // MATCHED pair -- which would be worse than having no guard at all.
        assert_eq!(config_wire_fingerprint(), config_wire_fingerprint());
        assert_eq!(config_wire_fingerprint().len(), 16);

        let config = config_wire_default();
        let base = serde_json::to_string(&config).unwrap();
        let wire = bincode::serde::encode_to_vec(&config, bincode::config::legacy()).unwrap();
        assert_eq!(
            fingerprint_of_config_material(&wire, &base, CONFIG_DEFINITION_SOURCES),
            config_wire_fingerprint()
        );

        // SHAPE-SENSITIVE, checked on the same mechanism the real function uses.
        // One added field is exactly the change that caused the outage.
        let with_extra_field = format!("{},\"a_new_flag\":false}}", &base[..base.len() - 1]);
        assert_ne!(
            fingerprint_of_config_material(&wire, &with_extra_field, CONFIG_DEFINITION_SOURCES),
            config_wire_fingerprint()
        );
        // A removed field.
        let removed = base.replacen("\"virtualize_time\":true,", "", 1);
        assert_ne!(
            fingerprint_of_config_material(&wire, &removed, CONFIG_DEFINITION_SOURCES),
            config_wire_fingerprint()
        );
        // A pure rename, which bincode would tolerate but which we still refuse.
        let renamed = base.replacen("\"virtualize_time\"", "\"virtualise_time\"", 1);
        assert_ne!(
            fingerprint_of_config_material(&wire, &renamed, CONFIG_DEFINITION_SOURCES),
            config_wire_fingerprint()
        );

        // The counterexample the JSON-only fingerprint missed: serde_json emits
        // the same text for integer zero regardless of width, but legacy bincode
        // changes the payload width. A stale peer would decode every following
        // field at the wrong offset.
        #[derive(Serialize)]
        struct U32Field {
            field: u32,
        }
        #[derive(Serialize)]
        struct U64Field {
            field: u64,
        }
        let u32_value = U32Field { field: 0 };
        let u64_value = U64Field { field: 0 };
        let u32_json = serde_json::to_string(&u32_value).unwrap();
        let u64_json = serde_json::to_string(&u64_value).unwrap();
        assert_eq!(
            u32_json, u64_json,
            "the planted JSON collision must be real"
        );
        let u32_wire =
            bincode::serde::encode_to_vec(&u32_value, bincode::config::legacy()).unwrap();
        let u64_wire =
            bincode::serde::encode_to_vec(&u64_value, bincode::config::legacy()).unwrap();
        assert_ne!(u32_wire, u64_wire, "the planted wire retype must be real");
        assert_ne!(
            fingerprint_of_config_material(&u32_wire, &u32_json, &[b"struct S { field: u32 }"]),
            fingerprint_of_config_material(&u64_wire, &u64_json, &[b"struct S { field: u64 }"]),
            "a wire-incompatible integer retype must change the fingerprint"
        );

        // Defaults can hide an incompatible inner type in BOTH value encodings.
        // The definition source is therefore load-bearing, not decorative.
        #[derive(Serialize)]
        struct OptionalU32 {
            field: Option<u32>,
        }
        #[derive(Serialize)]
        struct OptionalU64 {
            field: Option<u64>,
        }
        let optional_u32 = OptionalU32 { field: None };
        let optional_u64 = OptionalU64 { field: None };
        let optional_u32_json = serde_json::to_string(&optional_u32).unwrap();
        let optional_u64_json = serde_json::to_string(&optional_u64).unwrap();
        assert_eq!(optional_u32_json, optional_u64_json);
        let optional_u32_wire =
            bincode::serde::encode_to_vec(&optional_u32, bincode::config::legacy()).unwrap();
        let optional_u64_wire =
            bincode::serde::encode_to_vec(&optional_u64, bincode::config::legacy()).unwrap();
        assert_eq!(
            optional_u32_wire, optional_u64_wire,
            "the planted default must be invisible in both value encodings"
        );
        assert_ne!(
            fingerprint_of_config_material(
                &optional_u32_wire,
                &optional_u32_json,
                &[b"struct S { field: Option<u32> }"]
            ),
            fingerprint_of_config_material(
                &optional_u64_wire,
                &optional_u64_json,
                &[b"struct S { field: Option<u64> }"]
            ),
            "a hidden wire-incompatible inner-type change must alter the fingerprint"
        );
    }

    #[test]
    fn config_fingerprint_uses_environment_free_wire_defaults() {
        let environment_derived = Config {
            epoch: "2042-03-04T05:06:07.890123456Z".parse().unwrap(),
            seed: 41,
            sched_seed: Some(42),
            cpu_profile: crate::cpu_profile::CpuProfile::X86_64V3,
            ..Config::default()
        };

        let canonical = config_wire_default();

        assert_ne!(
            serde_json::to_string(&environment_derived).unwrap(),
            serde_json::to_string(&canonical).unwrap()
        );
        assert_eq!(
            canonical.epoch,
            DEFAULT_EPOCH_STR.parse::<DateTime<Utc>>().unwrap()
        );
        assert_eq!(canonical.seed, 0);
        assert_eq!(canonical.sched_seed, None);
        assert_eq!(
            canonical.cpu_profile,
            crate::cpu_profile::CpuProfile::X86_64V2
        );
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    #[test]
    #[should_panic(expected = "max_timeslice must be at least one RCB")]
    fn validate_scales_one_rcb_minimum_with_chaos_slowdown() {
        let mut config = Config {
            chaos: true,
            chaos_per_thread_slowdown: true,
            chaos_slowdown_max_factor: 4.0,
            max_timeslice: NonZeroU64::new(39),
            ..Default::default()
        };
        config.validate();
    }

    /// The one-RCB minimum multiplies like the PMU epsilon of Detcore's
    /// post-hook, `NANOS_PER_RCB * (clock_multiplier * factor)` in
    /// `detcore/src/lib.rs`, bit for bit. Every pair below rounds differently
    /// when multiplied left to right; in the first three that moves the
    /// minimum: at `--clock-multiplier=2.5` and
    /// `--chaos-slowdown-max-factor=4.4` the left-to-right product refused
    /// `--max-timeslice=110` as below "at least 111".
    #[test]
    fn one_rcb_minimum_multiplies_like_detcores_pmu_epsilon() {
        let slowed = |clock_multiplier: f64, factor: f64, max_timeslice: u64| Config {
            chaos: true,
            chaos_per_thread_slowdown: true,
            chaos_slowdown_max_factor: factor,
            clock_multiplier: Some(clock_multiplier),
            max_timeslice: NonZeroU64::new(max_timeslice),
            ..Default::default()
        };

        let mut config = slowed(2.5, 4.4, 110);
        assert_eq!(config.minimum_max_timeslice_nanos(), 110);
        config.validate();

        for (clock_multiplier, factor, minimum) in [
            (2.5, 4.4, 110),
            (2.5, 2.2, 55),
            (7.0, 1.1, 78),
            (0.7, 3.0, 21),
            (1.1, 5.5, 61),
        ] {
            let detcore = NANOS_PER_RCB * (clock_multiplier * factor);
            assert_ne!(
                (NANOS_PER_RCB * clock_multiplier * factor).to_bits(),
                detcore.to_bits(),
                "{clock_multiplier} x {factor} does not tell the orders apart"
            );
            let config = slowed(clock_multiplier, factor, 1_000_000);
            assert_eq!(
                config.largest_nanos_per_rcb().to_bits(),
                detcore.to_bits(),
                "{clock_multiplier} x {factor}"
            );
            assert_eq!(
                config.minimum_max_timeslice_nanos(),
                minimum,
                "{clock_multiplier} x {factor}"
            );
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    #[test]
    #[should_panic(expected = "chaos_slowdown_max_factor must be finite and in")]
    fn validate_rejects_unrepresentable_chaos_slowdown_factor() {
        let mut config = Config {
            chaos_slowdown_max_factor: RcbTimeMultiplier::MAX * 2.0,
            ..Default::default()
        };
        config.validate();
    }
}

#[cfg(test)]
mod strict_boundary_mode_tests {
    use super::Config;

    #[test]
    fn strict_boundary_policy_is_explicit_and_round_trips_the_actual_wire() {
        let ordinary = Config::default();
        assert!(!ordinary.strict_schedule_boundaries);
        let mut old_shape = serde_json::to_value(&ordinary).unwrap();
        assert_eq!(
            old_shape
                .as_object_mut()
                .unwrap()
                .remove("strict_schedule_boundaries"),
            Some(false.into())
        );
        assert!(
            !serde_json::from_value::<Config>(old_shape)
                .unwrap()
                .strict_schedule_boundaries
        );
        let explicit = Config {
            strict_schedule_boundaries: true,
            ..ordinary
        };
        let encoded = serde_json::to_value(&explicit).unwrap();
        assert_eq!(encoded["strict_schedule_boundaries"], true);
        assert!(
            serde_json::from_value::<Config>(encoded)
                .unwrap()
                .strict_schedule_boundaries
        );
        assert!(
            explicit
                .to_string()
                .contains("--strict-schedule-boundaries")
        );
        for config in [&Config::default(), &explicit] {
            let bytes = bincode::serde::encode_to_vec(config, bincode::config::legacy()).unwrap();
            let (decoded, consumed): (Config, usize) =
                bincode::serde::decode_from_slice(&bytes, bincode::config::legacy()).unwrap();
            assert_eq!(consumed, bytes.len());
            assert_eq!(
                decoded.strict_schedule_boundaries,
                config.strict_schedule_boundaries
            );
            assert_eq!(
                bincode::serde::encode_to_vec(&decoded, bincode::config::legacy()).unwrap(),
                bytes
            );
        }
        let legacy = super::to_legacy_backend_json(&Config::default()).unwrap();
        assert!(!legacy.contains("strict_schedule_boundaries"));
        assert!(
            !super::from_legacy_backend_json(&legacy)
                .unwrap()
                .strict_schedule_boundaries
        );
        assert!(super::to_legacy_backend_json(&explicit).is_err());
    }
}
