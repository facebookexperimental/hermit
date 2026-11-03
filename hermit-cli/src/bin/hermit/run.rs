/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::hash::Hash;
use std::hash::Hasher;
use std::io::Read;
use std::io::Seek;
use std::io::Write;
use std::num::NonZeroU64;
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::LazyLock;
use std::time::Duration;
use std::time::SystemTime;

use clap::Parser;

#[cfg(test)]
mod preemption_tests;
use colored::Colorize;
use detcore_model::backend_engagement::BackendEngagement;
use detcore_model::backend_engagement::BackendEngagementReport;
use detcore_model::happens_before::HappensBeforeProgram;
use detcore_model::happens_before::Strength;
use detcore_model::network_trace::NetworkTraceConfig;
use detcore_model::network_trace::NetworkTraceMode;
use detcore_model::network_trace::NetworkTraceV2;
use detcore_model::summary::RunSummary;
use hermit::Backend;
use hermit::Context;
use hermit::DetConfig;
use hermit::Error;
use hermit::Shebang;
use hermit::SkidOvershootError;
use hermit::canonical_verdict::INEXACT_BRANCH_COUNTER_WARNING;
use hermit::canonical_verdict::Verdict;
use hermit::happens_before::DebugInfoResolver;
use hermit::happens_before::HbLaunch;
use hermit::happens_before::describe_anchor;
use hermit::happens_before::load_program;
use hermit::happens_before::refuse_unenforceable_anchors;
use hermit::happens_before::resolve_program;
use hermit::host_input_change::HostInputs;
use hermit::host_input_change::PatternDifference;
use hermit::host_input_change::explains_divergence;
use hermit::host_input_change::find_pattern_difference;
use hermit::host_input_change::open_position;
use hermit::host_input_change::read_host_inputs;
use reverie::process::Bind;
use reverie::process::Command;
use reverie::process::Container;
use reverie::process::ExitStatus;
use reverie::process::Mount;
use reverie::process::MountFlags;
use reverie::process::Output;

use super::container::GUEST_DEV_SHM;
use super::container::GUEST_NAMESPACES;
use super::container::HostLayout;
use super::container::IdentityGuard;
use super::container::LogCapExceeded;
use super::container::PlannedMount;
use super::container::PolicyRefusal;
use super::container::SchedulerOutcome;
use super::container::apply_affinity;
use super::container::container_proc_mount;
use super::container::identity_hardening_mounts_on;
use super::container::image_container;
use super::container::is_below_dev_shm;
use super::container::is_dev_shm;
use super::container::is_in_dev_shm;
use super::container::namespace_container;
use super::container::private_dev_shm;
use super::container::private_mqueue;
use super::container::refuse_bind_from_host_dev_shm;
use super::container::refuse_covered_bind_source;
use super::container::refuse_mount_below_private_mqueue;
use super::global_opts::GlobalOpts;
use super::host_capabilities::cpu_identity;
use super::host_capabilities::exact_branch_counter_verdict;
use super::host_capabilities::host_inexact_branch_counter;
use super::record_envelope::RecordEnvelope;
use super::tracing::BoundedWriter;
use super::tracing::CappedWriter;
use super::tracing::init_sync_file_tracing;
use super::tracing::log_max_bytes;
use super::verify::ComparedRun;
use super::verify::ComparisonOptions;
use super::verify::LogCompareStrictness;
use super::verify::NoResultReason;
use super::verify::SecondRun;
use super::verify::SignalTerminationReports;
use super::verify::VerificationOutcome;
use super::verify::VerificationReport;
use super::verify::VerificationRun;
use super::verify::VerificationRuntime;
use super::verify::announce_verification_outcome;
use super::verify::compare_two_runs;
use super::verify::default_failed_verify_log_retention;
use super::verify::emit_compared_guest_output;
use super::verify::publish_run_failure;
use super::verify::retain_logs_after_verification_error;
use super::verify::retain_verification_error;
use super::verify::retain_verification_logs;
use super::verify::run_verification_execution;
use super::verify::store_exclusive_retries;
use super::verify::store_exclusive_retries_line;
use super::verify::temp_log_files_in;
use super::verify::validate_log_level;
use super::verify::verification_log_level;
use super::verify::verification_runtime_from_summaries;
use super::verify::write_host_input_change_verification_json;
use super::verify::write_pending_verification_json;
use super::verify::write_report_json;
use super::verify::write_skid_overshoot_verification_json;
use super::verify::write_skid_overshoot_without_comparison_json;
use super::verify::write_verification_json;
use super::verify_run;

const TMP_DIR: &str = "/tmp";

/// A timeslice of at most this many skid margins gets
/// [`RunOpts::small_max_timeslice_notice`]: Reverie steps the guest through up
/// to one margin of every slice to place its precise timer event, so at least
/// a tenth of such a slice is stepped, and a ptrace step costs thousands of
/// times a native branch.
const SMALL_TIMESLICE_SKID_MARGINS: u64 = 10;

/// The skid margin this process's precise timers use: the processor's, or
/// `--skid-margin` once [`RunOpts::install_pmu_config`] has recorded it. A
/// processor outside Reverie's table has no precise timer; the largest margin
/// in the table stands in for it.
pub(crate) fn installed_skid_margin() -> u64 {
    reverie::pmu_config::try_get_pmu_config().map_or(
        reverie::pmu_config::LARGEST_TABLE_SKID_MARGIN,
        reverie::pmu_config::PmuConfig::skid_margin,
    )
}

/// The first `--mount` target that `exists` reports missing, of those Hermit
/// mounts onto as they are. A target inside /tmp goes to Hermit's private
/// /tmp, which creates it. A target under an earlier `--mount` of the same
/// run can be provided by that mount (a tmpfs, or a bind of a directory that
/// has it), which only the container's mount table shows, so it is left to
/// container setup. Checked before setup, a missing target is named; at
/// setup it is only ENOENT from a mount, without the path.
fn missing_mount_target(mounts: &[MountArg], exists: impl Fn(&Path) -> bool) -> Option<&Path> {
    mounts.iter().enumerate().find_map(|(index, mount)| {
        let target = mount.get_target();
        let under_an_earlier_mount = mounts[..index]
            .iter()
            .any(|earlier| target.starts_with(earlier.get_target()));
        (!target.starts_with(TMP_DIR) && !under_an_earlier_mount && !exists(target))
            .then_some(target)
    })
}

/// Refuse a `--mount` target before setup, by the host's paths (`exists`,
/// `is_dir`): first one the guest's own `/dev/mqueue` would cover
/// ([`refuse_mount_below_private_mqueue`]), which would otherwise be named as
/// missing, since a host's `/dev/mqueue` holds only its queues; then the
/// first [`missing_mount_target`].
fn validate_mount_targets(
    mounts: &[MountArg],
    exists: impl Fn(&Path) -> bool,
    is_dir: impl Fn(&Path) -> bool,
) -> Result<(), Error> {
    refuse_mount_below_private_mqueue(mounts.iter().map(|mount| mount.get_target()), is_dir)?;
    if let Some(target) = missing_mount_target(mounts, exists) {
        anyhow::bail!(
            "--mount target {} does not exist on this host. Hermit mounts onto an existing \
             path outside {TMP_DIR}: create it first, or choose a target under {TMP_DIR}, \
             which Hermit creates in the guest's private {TMP_DIR}.",
            target.display()
        );
    }
    Ok(())
}

/// Refuse a run on a host without `/tmp` (Android) when `TMPDIR` is unset:
/// Hermit keeps its own scratch files (the guest's private `/tmp` staging,
/// the frozen group database) in `TMPDIR`, which falls back to `/tmp`, and
/// creating them there would fail with an ENOENT that names no path.
fn validate_host_scratch(layout: HostLayout, tmpdir_set: bool) -> Result<(), Error> {
    anyhow::ensure!(
        layout.tmp || tmpdir_set,
        "this host has no {TMP_DIR} and TMPDIR is unset: Hermit keeps its scratch files in \
         TMPDIR, so set it to a writable directory (on Android, /data/local/tmp)"
    );
    Ok(())
}

#[cfg(feature = "native-exec")]
type NativeContainerRun = super::native_exec_cli::PhysicalContainer;
#[cfg(not(feature = "native-exec"))]
type NativeContainerRun = ();

#[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
struct MappedVerificationRun {
    image: std::sync::Arc<super::mapped_first_admission::RetainedImage>,
    policy: super::mapped_first_admission::CapturePolicy,
}

/// Descriptor-free forwarding selection retained by the common container owner.
/// Open the one physical stream only inside that container's child.
pub(super) enum ForwardedDetlogPlan {
    None,
    Socket { opened: bool },
}

impl ForwardedDetlogPlan {
    fn requires_socket(&self) -> bool {
        matches!(self, Self::Socket { .. })
    }

    #[cfg(test)]
    pub(super) fn is_none(&self) -> bool {
        !self.requires_socket()
    }

    #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
    pub(super) fn open_mapped_in_container(&mut self) -> Result<(OwnedFd, OwnedFd), Error> {
        self.open_in_container()?
            .ok_or_else(|| Error::msg("mapped verification requires its actual DETLOG plan"))
    }

    fn open_in_container(&mut self) -> Result<Option<(OwnedFd, OwnedFd)>, Error> {
        match self {
            Self::None => Ok(None),
            Self::Socket { opened } => {
                if std::mem::replace(opened, true) {
                    return Err(Error::msg("forwarded DETLOG plan already consumed"));
                }
                RunOpts::open_forwarded_detlog_channel()
            }
        }
    }
}

/// Why this host cannot arm a PMU preemption timer (reverie-ptrace's or
/// reverie-kvm's: both count retired conditional branches with perf events).
#[derive(Debug, Clone, PartialEq, Eq)]
enum TimerUnavailable {
    /// `perf_event_open` cannot open a hardware counter here.
    NoPerfEvents,
    /// The counters work, but Reverie has no profile (event and skid margin)
    /// for this CPU, so it gives no task a timer.
    NoPmuProfile(String),
}

/// Whether [`reverie_timer_support`] finds this host's timer, for tests
/// elsewhere in the binary whose expectation depends on it.
#[cfg(test)]
pub(crate) fn reverie_timer_support_for_tests() -> bool {
    reverie_timer_support().is_ok()
}

/// Whether this host can arm a PMU preemption timer.
fn reverie_timer_support() -> Result<(), TimerUnavailable> {
    if !reverie_ptrace::is_perf_supported() {
        return Err(TimerUnavailable::NoPerfEvents);
    }
    reverie_ptrace::host_pmu_profile()
        .map_err(|error| TimerUnavailable::NoPmuProfile(error.to_string()))
}

/// A `--bind`'s host source and guest target.
fn bind_paths(bind: &Bind) -> (&Path, &Path) {
    (
        Path::new(OsStr::from_bytes(bind.source.to_bytes())),
        Path::new(OsStr::from_bytes(bind.target.to_bytes())),
    )
}

/// Whether the container applies `bind`: one at or below guest `/tmp`, one
/// below the private `/dev/shm` (where the guest gets one,
/// `private_dev_shm`), or one at `/dev/shm` from another host directory,
/// which replaces it. `--bind /dev/shm` mounts nothing: it keeps the host's
/// `/dev/shm`.
fn bind_is_applied(bind: &Bind, private_dev_shm: bool) -> bool {
    let (source, target) = bind_paths(bind);
    target.starts_with(TMP_DIR)
        || (is_dev_shm(target) && !is_dev_shm(source))
        || (is_below_dev_shm(target) && private_dev_shm)
}

/// Whether `bind` is `--bind /dev/shm`, which keeps the host's `/dev/shm`
/// for the guest and mounts nothing.
fn keeps_host_dev_shm(bind: &Bind) -> bool {
    let (source, target) = bind_paths(bind);
    is_dev_shm(source) && is_dev_shm(target)
}

fn warn_unapplied_bind(bind: &Bind) {
    let target = bind.target.to_string_lossy();
    if is_below_dev_shm(bind_paths(bind).1) {
        crate::app::tracing::diagnostic_eprintln!(
            "WARNING: --bind target {target} is below a guest /dev/shm that is not Hermit's \
             private one (a --mount or --bind at /dev/shm replaced it, or --bind /dev/shm kept \
             the host's), so this option has no effect; use --mount to mount into it"
        );
    } else {
        crate::app::tracing::diagnostic_eprintln!(
            "WARNING: --bind target {target} is outside guest /tmp and the guest's private \
             /dev/shm, so this option has no effect; files outside them are already visible \
             unless another mount hides them"
        );
    }
}
const FAIL_CLOSED_ENV: &str = "HERMIT_FAIL_CLOSED";
const NORMALIZED_FORWARDED_DETLOG_TIMESTAMP: &str = "1970-01-01T00:00:00.000000Z";

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn read_verify_summary(path: &Path) -> Option<RunSummary> {
    match fs::read(path)
        .with_context(|| format!("reading verification run summary {}", path.display()))
        .and_then(|bytes| {
            serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing verification run summary {}", path.display()))
        }) {
        Ok(summary) => Some(summary),
        Err(error) => {
            // Also runs between the two --verify runs, before run 2 can
            // cross --max-log-bytes.
            crate::app::tracing::diagnostic_eprintln!(
                "WARNING: verification runtime statistics unavailable: {error:#}"
            );
            None
        }
    }
}

pub(super) fn first_run_rejected_report(
    output: &Output,
    runtime: Option<VerificationRuntime>,
) -> VerificationReport {
    let mut report = VerificationReport::no_result();
    report.no_result_reason = Some(NoResultReason::FirstRunRejected {
        exit_code: output.status.code(),
        signal: output.status.signal(),
        stdout_bytes: u64::try_from(output.stdout.len()).expect("guest stdout length fits u64"),
        stderr_bytes: u64::try_from(output.stderr.len()).expect("guest stderr length fits u64"),
    });
    report.guest_exit_code = output.status.code();
    report.guest_signal = output.status.signal();
    report.runtime = runtime;
    report
}

/// Where the private run summaries below are written.
///
/// Hermit's isolated /tmp is not the host /tmp, so these cannot go to the
/// default temporary directory: they must live on a path BOTH run containers
/// can see, which is why they were written into the checkout in the first
/// place. That part of the original reasoning is correct and is preserved.
///
/// ⚠️ WHAT WAS WRONG WITH THE CHECKOUT ROOT, measured on hermit main
/// ba3bfc97671666ae946841804624e7a2c355a0e4 in validate run 1819. Prepared
/// source identity enumerates untracked, non-ignored files:
///
/// ```text
/// git ls-files --others --exclude-standard -z
/// ```
///
/// (`ci/manifest-plan/src/nextest_binaries.rs` source_identity, and the
/// byte-identical enumeration in `ci/prepare-rust-scripts.sh` write_state).
/// A summary here IS such a file, so creating one and dropping it on the same
/// run changes the identity, and dropping it between enumeration and hashing
/// makes it vanish mid-read. Both were observed: 13 nodes refused with
/// "prepared executables are stale: source identity changed", 6 with "cannot
/// inspect prepared input <path>: No such file or directory", and one with
/// "sha256sum: .hermit-verify-summary-...: No such file or directory". That is
/// 20 of the run's 26 failed nodes, none of which had run a single test.
///
/// ⚠️ THE FIX MOVES THE FILE; IT DOES NOT EXCUSE THE NAME. Adding
/// `.hermit-verify-summary-*` to .gitignore would also satisfy the enumeration,
/// and it is the wrong repair: `scripts/validate.rs` checkout_attribution_bracket
/// exists because run 1573 leaked these same files, and its stated property is
/// "No pathname is excused". Excusing the name would hide a genuine leak
/// anywhere in the tree -- and that bracket builds its own repository under a
/// temporary directory, so it would never have read this repo's .gitignore and
/// would not have caught the weakening.
///
/// `ignored/` is the existing directory for exactly this. Its .gitignore entry
/// reads "Everything under ignored/ is disposable and must not taint source
/// provenance or the version string." Using it adds no new ignore rule, and a
/// stray summary left anywhere else is still caught -- verified both ways.
fn private_summary_dir() -> Result<PathBuf, Error> {
    summary_dir_under(&std::env::current_dir()?)
}

/// Whether `<root>/ignored` is actually excluded from source accounting there.
///
/// ⚠️ THE WALK ABOVE CANNOT JUST TRUST A `.git`, and this is measured rather
/// than hypothetical: this host has a `/tmp/.git`. Without this check a run
/// whose working directory is any temporary directory resolves its "work tree"
/// to `/tmp` and writes the summary to `/tmp/ignored` -- which is not the
/// project, and worse, is not visible to both run containers, since Hermit's
/// isolated /tmp is not the host /tmp. That is the very property the original
/// comment was protecting.
///
/// So the candidate is accepted only where the disposable directory is really
/// disposable. That also removes an assumption the tests could not see: the
/// repair depends on `ignored/` being ignored in whichever repository the file
/// lands in, and nothing previously checked it.
fn disposable_dir_is_ignored(root: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(root.join(".gitignore")) else {
        return false;
    };
    text.lines()
        .map(|line| line.split('#').next().unwrap_or_default().trim())
        .any(|rule| matches!(rule, "ignored/" | "/ignored/" | "ignored" | "/ignored"))
}

/// Resolve the summary directory for a given starting directory.
///
/// Split out from [`private_summary_dir`] for one reason: it is the only part
/// that can be tested. The reviewer of PR 3026 found that the tests shipped
/// with the first version of this change lived in `hermit-manifest-plan` and
/// never named any of these functions -- a private `src/bin` item is not
/// importable from another crate, so that whole test surface could not reach
/// the code it was written for, even in principle. Taking the directory as an
/// argument lets the unit tests below drive every branch.
fn summary_dir_under(start: &Path) -> Result<PathBuf, Error> {
    // The enumeration is rooted at the work tree, not at the process cwd, so a
    // summary in ANY tracked subdirectory still trips it. Resolve the work-tree
    // root rather than assuming this runs from it.
    let mut root = start;
    loop {
        if root.join(".git").exists() && disposable_dir_is_ignored(root) {
            let disposable = root.join("ignored");
            // ⚠️ INSIDE A WORK TREE THIS REFUSES RATHER THAN FALLING BACK.
            //
            // The first version returned `cwd` when the directory could not be
            // created, justifying it as failing soft. It is the opposite: cwd
            // inside a work tree is exactly where prepared source identity
            // enumerates, so the fallback silently restored the defect. The
            // reviewer's case is concrete -- a regular FILE named `ignored`
            // makes create_dir_all fail -- and a measurement that only checked
            // writability and directory-ness of checkouts existing at the time
            // could not have ruled it out for checkouts made later.
            //
            // A loud failure here costs one run. The silent fallback costs a
            // whole validation, which is what it already cost once.
            std::fs::create_dir_all(&disposable).with_context(|| {
                format!(
                    "creating the disposable run-summary directory {}; refusing to fall back to \
                     the work tree, where this file would be counted as prepared source",
                    disposable.display()
                )
            })?;
            return Ok(disposable);
        }
        match root.parent() {
            Some(parent) => root = parent,
            None => break,
        }
    }
    // Outside a work tree only. Nothing enumerates source here, so the previous
    // location is still correct. It is NOT necessarily visible inside the run
    // container -- a working directory under the host /tmp is hidden behind
    // the guest's private /tmp -- which is why the container reaches these
    // files only through `private_summary_descriptor_path`, never by this name.
    Ok(start.to_path_buf())
}

/// The pathname through which the run container reaches a private summary.
///
/// ⚠️ NEVER HAND THE CONTAINER `file.path()`. That name is a HOST path, and
/// the container mounts a private directory over `/tmp`, so a summary created
/// under a host working directory below `/tmp` does not exist there. Detcore's
/// end-of-run summary write then failed with NotFound and panicked as PID 1 of
/// the container, so `hermit run --verify -- /bin/true` exited 125 before run 2
/// for every caller that starts tests from a temporary directory (Buck/TPX,
/// lit): https://github.com/rrnewton/hermit/issues/3260, 221 of the fbsource
/// import's Buck/TPX failures. A git work tree under `/tmp` fails the same way,
/// through `<work tree>/ignored/`.
///
/// The controller keeps `file` open across the container fork, and a fork
/// keeps descriptor numbers, so this one spelling names the same inode in the
/// controller and in the container whatever either mount namespace hides.
/// Captured runs already hand their staged summary to the container this way
/// (`staged_summary::writer_path`). The named file and its location are
/// unchanged, so what the guest can observe is unchanged too.
fn private_summary_descriptor_path(file: &tempfile::NamedTempFile) -> PathBuf {
    descriptor_path(file.as_file())
}

/// The `/proc/self/fd` spelling of `file`, which the run container reaches
/// whatever its mount namespace hides; see [`private_summary_descriptor_path`].
fn descriptor_path(file: &fs::File) -> PathBuf {
    use std::os::fd::AsRawFd;
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

/// The first host-input pattern difference between the runs and where its
/// open sits in each run's log (see `hermit::host_input_change`), or `None`
/// when the runs' records show none or cannot be read.
fn place_host_input_change(
    run1: std::io::Result<HostInputs>,
    run2: std::io::Result<HostInputs>,
    logs: [&Path; 2],
) -> Option<(PatternDifference, [Option<usize>; 2])> {
    let read = |result: std::io::Result<HostInputs>| {
        result
            .inspect_err(|error| {
                eprintln!(
                    "WARNING: the runs' host-input records cannot be read, so a host file \
                     change cannot be named: {error}"
                )
            })
            .ok()
    };
    let run1 = read(run1)?;
    let run2 = read(run2)?;
    let difference = find_pattern_difference(&run1, &run2)?;
    let place = |log: &Path, label: &str| {
        fs::read(log)
            .inspect_err(|error| {
                eprintln!(
                    "WARNING: the {label} log cannot be read, so a host file change cannot be \
                     placed: {error}"
                )
            })
            .ok()
            .and_then(|bytes| {
                let text = String::from_utf8_lossy(&bytes);
                // An ambiguous (dtid, syscall) refuses: no core backend
                // restarts per-thread syscall numbering across images.
                open_position(&text, difference.dtid, difference.syscall)
            })
    };
    let positions = [place(logs[0], "run 1"), place(logs[1], "run 2")];
    Some((difference, positions))
}

/// The `--strict` refusal on a `backend` whose virtual clock is the host's
/// retired-conditional-branch counter (see
/// `RunOpts::refuse_strict_with_inexact_branch_counter`): `Ok` when
/// `inexact_branch_counter` finds the counter exact, else a `PolicyRefusal`
/// that names the backend, Reverie's reason and the CPU. `hermit run`,
/// `hermit analyze`, `hermit bisect` and `hermit record start` share it, so
/// all four refuse with the same message.
pub(crate) fn refuse_an_inexact_branch_counter(
    backend: Backend,
    inexact_branch_counter: fn() -> Option<String>,
) -> Result<(), Error> {
    let Some(reason) = inexact_branch_counter() else {
        return Ok(());
    };
    Err(Error::new(PolicyRefusal).context(format!(
        "--strict refuses the `{}` backend on this host: its retired-conditional-branch \
         counter failed Reverie's performance-counter validation ({reason}; {}). That counter \
         is the virtual clock on this backend, so two runs of one program could diverge. Run \
         on a host whose counters pass the validation, or drop --strict to run without the \
         determinism guarantee (see https://github.com/rrnewton/hermit/issues/3794).",
        backend.as_str(),
        cpu_identity()
    )))
}

/// What reads the retired-conditional-branch counter on a command that
/// [`refuse_an_inexact_branch_counter`] refuses with `--strict`, and so what
/// [`warning_for_an_inexact_branch_counter`] says is not guaranteed without it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CounterReader {
    /// A `hermit run`.
    Run,
    /// The trials of one `hermit analyze` or `hermit bisect`.
    Trials,
    /// A `hermit record start` recording.
    Recording,
}

/// The one-line stderr warning for a command that goes on without `--strict`
/// where [`refuse_an_inexact_branch_counter`] refuses it with `--strict`:
/// Hermit times preemptions, and by default virtual time, with the
/// retired-conditional-branch counter, so on a counter that failed Reverie's
/// validation what `reader` would repeat is not guaranteed to repeat, and the
/// command says so (https://github.com/rrnewton/hermit/issues/4515) and how
/// to get a reproducible result. `None` when `inexact_branch_counter` finds the
/// counter exact.
///
/// The fixed text comes first and Reverie's reason last: that reason carries
/// measured counts that differ from one process to the next, so one host's
/// warnings for one command share everything up to it. Reverie's message can
/// span lines (`HardwareCountersNotWorking` embeds two newlines); the warning
/// stays one line.
pub(crate) fn warning_for_an_inexact_branch_counter(
    reader: CounterReader,
    inexact_branch_counter: fn() -> Option<String>,
) -> Option<String> {
    let reason = inexact_branch_counter()?;
    let reason = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    let not_guaranteed = match reader {
        CounterReader::Run => {
            "the same arguments and --seed, or a preemption record, are not guaranteed to \
             reproduce this run"
        }
        CounterReader::Trials => {
            "analyze and bisect trials can disagree for a hardware reason, and a seed, \
             preemption record or schedule they report is not guaranteed to reproduce its outcome"
        }
        CounterReader::Recording => "a replay of this recording is not guaranteed to follow it",
    };
    Some(format!(
        "{INEXACT_BRANCH_COUNTER_WARNING} Hermit times preemptions, and by default virtual time, \
         with that counter, so {not_guaranteed}. For reproducible results use a host whose \
         counters pass the validation (`hermit host-capabilities` reports exact-branch-counter: \
         PRESENT, validation passed); --strict refuses this host. Reverie reports: {reason}\n"
    ))
}

/// `hermit host-capabilities`'s own `exact_branch_counter` verdict, for a
/// verification report (`VerificationReport::exact_branch_counter`).
pub(crate) fn branch_counter_verdict() -> hermit::canonical_verdict::BranchCounterVerdict {
    let verdict = exact_branch_counter_verdict();
    hermit::canonical_verdict::BranchCounterVerdict {
        present: verdict.present,
        evidence: verdict.evidence,
    }
}

/// Why a run on `backend`, with `--passthru-opt` when `passthru_opt` is set,
/// can run guest code or syscalls that Hermit does not observe, or `None` when
/// Hermit sees every syscall of every guest process that can change a file,
/// from its first instruction after exec. `--untrap-passthrough-syscalls`
/// leaves untrapped only calls that change no file. In such a run a host file change found before the
/// first divergence is not evidence that the guest did not make the change
/// itself (see `hermit::host_input_change`).
fn unobserved_guest_execution(backend: Backend, passthru_opt: bool) -> Option<&'static str> {
    if passthru_opt {
        // Detcore subscribes only to its passthrough allow-list, so the
        // guest's own rename, link and unlink calls are never reported.
        return Some(
            "--passthru-opt lets every syscall outside Detcore's allow-list, rename and link \
             among them, run unobserved",
        );
    }
    match backend {
        // These observe every syscall Detcore subscribes to, from the first
        // instruction after exec. ptrace leaves one window open by design:
        // Reverie's seccomp filter lets a syscall through untraced when it is
        // issued from the untraced stub of Reverie's private page
        // (`cp::TRAMPOLINE_BASE`, reverie-ptrace `seccomp_filter`), which
        // Reverie needs to inject syscalls. Such a syscall is invisible to
        // Detcore. The instruction after that stub is `ud2`, so a guest that
        // uses it takes a SIGILL, and Detcore then records a change it could
        // not establish, so that no host input change is named in that run
        // (detcore `handle_signal_event`). The SIGILL does not stop a
        // deliberate guest: a handler can keep the result, or a forked child
        // can die of it and leave its effect behind, so determinism is lost
        // with no race needed (https://github.com/rrnewton/reverie/issues/1064).
        // A guest that rewrites or replaces the page to avoid the fault is
        // recorded the same way when it does so (`untraced_code_range`). See
        // docs/DIVERGENCE_CLASSES.md.
        Backend::Ptrace | Backend::Kvm => None,
        Backend::Liteinst | Backend::InGuestTrap => {
            Some("in-guest LiteInst's preload starts after an exec'd program's .preinit_array runs")
        }
    }
}

/// Reverie ptrace's private page, `[start, end)`, which Reverie maps in every
/// guest process (reverie-ptrace `cp::PRIVATE_PAGE_OFFSET` and
/// `cp::PRIVATE_PAGE_SIZE`, which that crate keeps private). It holds the
/// untraced stub described in [`unobserved_guest_execution`]. The cli test
/// `reverie_private_page_is_the_range_hermit_guards` pins it against a
/// guest's own memory map.
const REVERIE_PTRACE_PRIVATE_PAGE: (u64, u64) = (0x7100_0000, 0x7100_1000);

/// The guest code that makes syscalls `backend` lets through untraced
/// (`DetConfig::untraced_code_range`), which Detcore guards in a run that
/// records host inputs: Reverie's private page on the ptrace runtime, and
/// none elsewhere. KVM traps every syscall, and the other backends never
/// name a host input change ([`unobserved_guest_execution`]).
fn untraced_code_range(backend: Backend) -> Option<(u64, u64)> {
    match backend {
        Backend::Ptrace => Some(REVERIE_PTRACE_PRIVATE_PAGE),
        Backend::Kvm | Backend::Liteinst | Backend::InGuestTrap => None,
    }
}

fn private_verify_summary() -> Result<tempfile::NamedTempFile, Error> {
    tempfile::Builder::new()
        .prefix(".hermit-verify-summary-")
        // See private_summary_dir: visible to both run containers, and outside
        // prepared source identity. Removed on drop.
        .tempfile_in(private_summary_dir()?)
        .context("creating private verification run summary")
}

fn private_backend_engagement_summary() -> Result<tempfile::NamedTempFile, Error> {
    tempfile::Builder::new()
        .prefix(".hermit-backend-engagement-summary-")
        // See private_summary_dir: visible to the run container, and outside
        // prepared source identity. Removed on drop.
        .tempfile_in(private_summary_dir()?)
        .context("creating private backend-engagement run summary")
}

fn clear_machine_record(path: &Path, description: &str) -> Result<(), Error> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("clearing stale {description} record {}", path.display())),
    }
}

fn write_backend_engagement(path: &Path, engagement: BackendEngagement) -> Result<(), Error> {
    let report = BackendEngagementReport::new(engagement);
    report.validate().map_err(Error::msg)?;
    let mut bytes = serde_json::to_vec(&report)?;
    bytes.push(b'\n');
    fs::write(path, bytes)
        .with_context(|| format!("writing backend engagement record {}", path.display()))
}

fn take_verify_summary_before_next_run(path: &Path) -> Result<Option<RunSummary>, Error> {
    let summary = read_verify_summary(path);
    fs::write(path, b"").with_context(|| {
        format!(
            "resetting private verification run summary {} before run 2",
            path.display()
        )
    })?;
    Ok(summary)
}

/// Moves the DETLOG records an in-guest Tool forwarded into the run's log at
/// `path`, under the log's size bound `limit` (`0`: unbounded), and returns how
/// many are syscall records. `stream` is what carried them: the guest's standard
/// error for SaBRe's plugin (`records_only` false), or, for in-guest LiteInst,
/// a socket that carries nothing but records (`records_only` true). On return
/// `stream` holds every line that was not a record.
///
/// A record is a line whose text, after leading whitespace, starts with
/// `INFO detcore` and contains ` DETLOG ` (`detcore::detlog::forwarded_line`).
/// From the guest's standard error every other line stays exactly as the guest
/// wrote it. A records-only stream must hold nothing else: a
/// `detcore::detlog::FORWARDING_LOSS_NOTICE` (a record the Tool could not send)
/// or any other line is an error, so verification reports no result instead of
/// comparing incomplete records.
///
/// The records are appended through the same bound as the log itself, counting
/// the bytes already in it, and never after an existing truncation marker: the
/// comparators recognize a truncated log only by that marker on the log's last
/// line, and a log whose observations were cut must compare as no result.
fn extract_forwarded_detlogs(
    path: &Path,
    stream: &mut Vec<u8>,
    limit: u64,
    records_only: bool,
) -> Result<usize, Error> {
    let mut kept = Vec::with_capacity(stream.len());
    let mut records = Vec::new();
    let mut syscall_records = 0;
    for line in stream.split_inclusive(|byte| *byte == b'\n') {
        let start = line
            .iter()
            .position(|byte| !byte.is_ascii_whitespace())
            .unwrap_or(line.len());
        let payload = line[start..].strip_suffix(b"\n").unwrap_or(&line[start..]);
        let payload = payload.strip_suffix(b"\r").unwrap_or(payload);
        if payload.starts_with(b"INFO detcore") && find_bytes(payload, b" DETLOG ").is_some() {
            records.extend_from_slice(NORMALIZED_FORWARDED_DETLOG_TIMESTAMP.as_bytes());
            records.push(b' ');
            records.extend_from_slice(payload);
            records.push(b'\n');
            syscall_records += usize::from(find_bytes(payload, b"DETLOG [syscall]").is_some());
        } else {
            kept.extend_from_slice(line);
        }
    }
    if records_only && !kept.is_empty() {
        let lost = kept
            .split(|byte| *byte == b'\n')
            .filter(|line| line.starts_with(detcore::detlog::FORWARDING_LOSS_NOTICE.as_bytes()))
            .count();
        if lost > 0 {
            anyhow::bail!(
                "the in-guest Tool could not forward {lost} DETLOG record(s) (it sent {} instead), \
                 so the compared records would be incomplete",
                detcore::detlog::FORWARDING_LOSS_NOTICE
            );
        }
        anyhow::bail!(
            "the in-guest DETLOG forwarding socket carried {} bytes outside any record",
            kept.len()
        );
    }
    *stream = kept;
    append_within_log_bound(path, &records, limit)?;
    Ok(syscall_records)
}

/// Appends `bytes` to the log at `path` within its size bound; see
/// [`extract_forwarded_detlogs`].
fn append_within_log_bound(path: &Path, bytes: &[u8], limit: u64) -> Result<(), Error> {
    let log = OpenOptions::new().read(true).append(true).open(path)?;
    if log_ends_truncated(&log)? {
        return Ok(());
    }
    let written = log.metadata()?.len();
    let mut log = BoundedWriter::resume(log, limit, written);
    log.write_all(bytes)?;
    log.flush()?;
    Ok(())
}

/// Whether `log` already ends with the bound's truncation marker.
fn log_ends_truncated(mut log: &fs::File) -> Result<bool, Error> {
    let written = log.metadata()?.len();
    // The truncation marker is a few hundred bytes; its whole last line fits.
    let tail_len = written.min(4096);
    log.seek(std::io::SeekFrom::Start(written - tail_len))?;
    let mut tail = Vec::new();
    log.read_to_end(&mut tail)?;
    Ok(detcore::logdiff::log_was_truncated(
        &String::from_utf8_lossy(&tail),
    ))
}

/// Starts a line the coordinator writes into a run's log for a message on the
/// forwarding socket that is not a record (a loss notice, or anything else), so
/// verification refuses the run instead of comparing incomplete records.
const FORWARDING_REFUSED_MARKER: &str = "HERMIT_DETLOG_FORWARDING_REFUSED";

/// A verification run's log writer, shared between the tracing subscriber and
/// the coordinator's forwarded records (`detcore::detlog::set_forwarded_source`).
/// Each write takes the one lock for the whole buffer, so a record and a
/// tracing event never interleave within a line, and both are counted against
/// `--max-log-bytes` and go through the log's byte bound.
///
/// Neither writer can act on a failed write: the forwarded-record sink has no
/// caller to return it to, and the tracing subscriber drops it. So the log
/// keeps the first write or flush error ([`SharedLog::failure`]), and
/// verification refuses the run ([`RunLatches`]) even when later writes
/// recover and the log reads complete.
#[derive(Clone)]
struct SharedLog {
    writer: std::sync::Arc<std::sync::Mutex<Box<dyn Write + Send>>>,
    failure: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

impl SharedLog {
    fn new(writer: impl Write + Send + 'static) -> Self {
        Self {
            writer: std::sync::Arc::new(std::sync::Mutex::new(Box::new(writer))),
            failure: Default::default(),
        }
    }

    /// Runs `op` on the writer under its lock, keeping its error if it is the
    /// first.
    fn with<R>(&self, op: impl FnOnce(&mut dyn Write) -> std::io::Result<R>) -> std::io::Result<R> {
        let result = op(&mut **self.writer.lock().unwrap());
        if let Err(error) = &result {
            self.failure
                .lock()
                .unwrap()
                .get_or_insert_with(|| error.to_string());
        }
        result
    }

    /// The log's first write or flush error, if any.
    fn failure(&self) -> Option<String> {
        self.failure.lock().unwrap().clone()
    }

    /// Writes each forwarded record as the line `extract_forwarded_detlogs`
    /// writes for one (the normalized timestamp, then the record), and any other
    /// message behind [`FORWARDING_REFUSED_MARKER`].
    fn record_sink(&self) -> detcore::detlog::ForwardedRecordSink {
        let log = self.clone();
        Box::new(move |message: &[u8]| {
            // Binary typed payload is checked before legacy newline handling.
            // No arbitrary INFO text gets the typed class's acceptance.
            if message.starts_with(b"HINFO") {
                match detcore::detlog::decode_plain_info(message) {
                    Ok((target, text)) => {
                        let line = format!(
                            "{NORMALIZED_FORWARDED_DETLOG_TIMESTAMP} INFO {target}: {text}\n"
                        );
                        let _ = log.with(|writer| writer.write_all(line.as_bytes()));
                    }
                    Err(reason) => {
                        // No source-lock reentry: the collector may hold it.
                        detcore::detlog::record_determinism_loss(reason);
                        let line = format!("{FORWARDING_REFUSED_MARKER} {reason}\n");
                        let _ = log.with(|writer| writer.write_all(line.as_bytes()));
                    }
                }
                return;
            }
            let payload = message.strip_suffix(b"\n").unwrap_or(message);
            let mut line = Vec::with_capacity(payload.len() + 40);
            if payload.starts_with(b"INFO detcore") && find_bytes(payload, b" DETLOG ").is_some() {
                line.extend_from_slice(NORMALIZED_FORWARDED_DETLOG_TIMESTAMP.as_bytes());
                line.push(b' ');
                line.extend_from_slice(payload);
            } else {
                line.extend_from_slice(FORWARDING_REFUSED_MARKER.as_bytes());
                line.push(b' ');
                line.extend_from_slice(&payload[..payload.len().min(200)]);
            }
            line.push(b'\n');
            // The sink cannot return an error; a failed write is kept in the
            // log's failure latch, which verification checks.
            let _ = log.with(|writer| writer.write_all(&line));
        })
    }
}

/// The writer of a verification run's log: [`BoundedWriter`] bounds the file
/// at `limit` bytes and [`CappedWriter`] charges every byte to `budget`, the
/// `--max-log-bytes` counter (`None` without the flag). The tracing subscriber
/// and the forwarded-record sink both write through the returned log.
fn verification_log(
    log_file: fs::File,
    limit: u64,
    budget: Option<super::tracing::LogBudget>,
) -> SharedLog {
    SharedLog::new(CappedWriter::new(
        BoundedWriter::new(log_file, limit),
        budget,
    ))
}

fn verification_log_with_tracing(
    file: File,
    level: tracing::metadata::LevelFilter,
    budget: Option<super::tracing::LogBudget>,
    backend: Backend,
) -> Result<(SharedLog, super::tracing::TracingGuard), Error> {
    let limit = log_max_bytes().map_err(Error::msg)?;
    let log = verification_log(file, limit, budget);
    let guard = init_sync_file_tracing(Some(level), log.clone(), backend);
    Ok((log, guard))
}

/// Affine core ownership of one external tracing capture and its real stream.
/// No writer or forwarding-latch policy is implemented by the backend closure.
pub(super) struct VerificationLogSession {
    log: SharedLog,
    _tracing: super::tracing::TracingGuard,
    forwarding: bool,
    closed: bool,
}

impl VerificationLogSession {
    pub(super) fn open(
        file: File,
        forwarded: &mut ForwardedDetlogPlan,
        level: tracing::metadata::LevelFilter,
        budget: Option<super::tracing::LogBudget>,
    ) -> Result<Self, Error> {
        let channel = forwarded.open_in_container()?;
        hermit::proc_mount::warn_if_readonly_proc();
        let (log, tracing) = verification_log_with_tracing(file, level, budget, Backend::Ptrace)?;
        let forwarding = channel.is_some();
        if let Some((sender, receiver)) = channel {
            hermit::forward_in_guest_detlogs_to(sender);
            detcore::detlog::set_forwarded_source(receiver, log.record_sink());
        }
        Ok(Self {
            log,
            _tracing: tracing,
            forwarding,
            closed: false,
        })
    }

    pub(super) fn finish(&mut self) -> RunLatches {
        if !self.closed {
            if self.forwarding {
                // Drop the coordinator's sender before the original ordered drain.
                hermit::clear_in_guest_detlog_forwarding();
                detcore::detlog::clear_forwarded_source();
            }
            let _ = self.log.clone().flush();
            self.closed = true;
        }
        RunLatches {
            determinism_loss: detcore::detlog::determinism_loss(),
            log_sink_failure: self.log.failure(),
        }
    }
}

impl Drop for VerificationLogSession {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

/// The `--print-verify-logs` echo of the first verification run's log.
///
/// With --verify the first run's `--log` output was diverted to a temporary
/// file for later comparison rather than shown to the user. Echo that log to
/// stderr so the user still sees `--log` output, matching a normal
/// (non-verify) run. The log file is fully flushed here because run_verify
/// runs each execution in a child process that has already exited.
///
/// Run 2 has not started yet, so under --max-log-bytes this echo must not
/// wait on stderr: the log goes through `DroppingStderr`, which waits for a
/// full pipe only within `RetryingStderr`'s bounded deadline and drops what it
/// then cannot deliver. Without the cap it is written exactly as before, and a
/// failed write is still an error.
fn echo_first_run_log(log1_path: &Path) -> Result<(), Error> {
    match fs::read(log1_path) {
        Ok(bytes) if hermit::nonwaiting_write::diagnostics_must_not_wait() => {
            let _ = detcore::util::DroppingStderr.write_all(&bytes);
        }
        Ok(bytes) => std::io::stderr().write_all(&bytes)?,
        Err(err) => crate::app::tracing::diagnostic_eprintln!(
            "WARNING: --print-verify-logs could not read first-run log {}: {}",
            log1_path.display(),
            err
        ),
    }
    Ok(())
}

impl Write for SharedLog {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.with(|writer| writer.write(bytes))
    }

    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.with(|writer| writer.write_all(bytes))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.with(|writer| writer.flush())
    }
}

/// What one verification run recorded that makes its records untrustworthy,
/// carried out of the run's container process. Verification checks it before
/// any comparison, independently of what reached the log: a lost or failed
/// write can leave a log that reads complete.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct RunLatches {
    /// Detcore's determinism-loss latch (`detcore::detlog::determinism_loss`):
    /// a record was lost, or an event happened at a moment the host chose.
    /// Under DBT it comes from the run's evidence instead
    /// (`backends::dbt_run_latches`), since the latch is in the guest.
    pub determinism_loss: Option<String>,
    /// The first write or flush error of the run's log (`SharedLog::failure`).
    pub log_sink_failure: Option<String>,
}

/// Immutable evidence declarations for one common verification pair.
pub(super) struct VerificationEvidence {
    requirement: super::backend_hook::EvidenceRequirement,
    records_run_metadata: bool,
    transport: Option<super::backend_hook::VerificationLogTransport>,
    native_identity_diagnostic: Option<&'static str>,
}

/// One completed physical verification run. DBT's typed statistics remain
/// deferred until the common owner has checked the run's latches and status.
pub(super) struct VerificationRunOutput {
    pub(super) output: Output,
    pub(super) skid_overshoots: u64,
    pub(super) latches: RunLatches,
    /// The run ended with a scheduler terminal outcome and the backend unwound
    /// the guest: a deadlock under Hermit's model (not a guest result, and a
    /// native hang only if Hermit models every wake the program relies on) or
    /// a virtual timeout (a bound Hermit imposed). `output` is what the guest
    /// wrote before it, and its status is the outcome's, not one the guest
    /// exited with.
    pub(super) terminal_outcome: Option<detcore::TerminalOutcome>,
}

impl VerificationRunOutput {
    #[cfg(test)]
    pub(super) fn into_physical(
        self,
    ) -> super::backend_hook::PhysicalRun<super::core_adapter::CorePending> {
        let pending = super::core_adapter::CorePending::Common;
        super::backend_hook::PhysicalRun {
            output: self.output,
            skid_overshoots: self.skid_overshoots,
            latches: self.latches,
            terminal_outcome: self.terminal_outcome,
            pending,
        }
    }
}

/// A verification run recorded a determinism loss ([`RunLatches`]), so it is
/// not compared. A type rather than a message so `run --verify` records the
/// run as failed by a determinism loss instead of leaving the `not_run` stamp.
#[derive(Debug)]
pub(super) struct DeterminismLossRecorded {
    run: String,
    reason: String,
}

impl DeterminismLossRecorded {
    #[cfg(test)]
    pub(super) fn new(run: &str, reason: &str) -> Self {
        Self {
            run: run.to_owned(),
            reason: reason.to_owned(),
        }
    }
}

impl std::fmt::Display for DeterminismLossRecorded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: determinism loss recorded: {}",
            self.run, self.reason
        )
    }
}

impl std::error::Error for DeterminismLossRecorded {}

impl RunLatches {
    /// Refuses to compare `run` when either latch is set.
    pub(super) fn refuse_comparison(&self, run: &str) -> Result<(), Error> {
        if let Some(reason) = &self.determinism_loss {
            return Err(Error::new(DeterminismLossRecorded {
                run: run.to_owned(),
                reason: reason.clone(),
            }));
        }
        if let Some(error) = &self.log_sink_failure {
            anyhow::bail!("{run}: log sink failure: {error}");
        }
        Ok(())
    }
}

/// How many forwarded syscall records the coordinator wrote into the log at
/// `path` during the run. A [`FORWARDING_REFUSED_MARKER`] line (the socket
/// carried a loss notice or anything else) is an error, so verification
/// reports no result instead of comparing incomplete records.
fn forwarded_records_in_log(path: &Path) -> Result<usize, Error> {
    let text = fs::read(path)?;
    let mut syscall_records = 0;
    for line in text.split(|byte| *byte == b'\n') {
        if let Some(refused) = line.strip_prefix(FORWARDING_REFUSED_MARKER.as_bytes()) {
            let refused = String::from_utf8_lossy(refused);
            if refused
                .trim_start()
                .starts_with(detcore::detlog::FORWARDING_LOSS_NOTICE)
            {
                anyhow::bail!(
                    "the in-guest Tool could not forward every DETLOG record (it sent{refused} \
                     instead), so the compared records would be incomplete"
                );
            }
            anyhow::bail!(
                "the in-guest DETLOG forwarding socket carried a message that is not a record:{refused}"
            );
        }
        if line.starts_with(NORMALIZED_FORWARDED_DETLOG_TIMESTAMP.as_bytes())
            && find_bytes(line, b"DETLOG [syscall]").is_some()
        {
            syscall_records += 1;
        }
    }
    Ok(syscall_records)
}

struct PreparedMounts {
    mounts: Vec<PlannedMount>,
    identity_sources: IdentityGuard,
}

/// A rewritten copy of one guest file, bind-mounted over the path the guest
/// knows it by, so preprocessing never changes what the program sees of
/// itself. Used by the e9patch preprocessor and by the aarch64 exclusive-loop
/// rewrite.
#[derive(Debug, Clone)]
struct ExecutableOverlayMount {
    source: PathBuf,
    target: PathBuf,
    /// For an exclusive-loop overlay, the sequences its copy rewrote. They
    /// reach the guest only once the container has mounted the copy, so they
    /// are reported as applied from inside it, and as planned before.
    rewritten: Option<usize>,
}

/// The line `--rewrite-exclusives` prints for one file it rewrote into the
/// overlay cache: what the copy holds is planned, not applied, until the
/// container mounts it over the original
/// ([`exclusive_rewrite_applied_lines`]).
fn exclusive_rewrite_planned_line(
    guest_path: &Path,
    report: &hermit::aarch64_lse::elf::FileReport,
) -> String {
    let mount = if report.patched_total() == 0 {
        "nothing to rewrite, so no copy is mounted"
    } else {
        "applied when the container mounts the copy"
    };
    format!(
        ":: Exclusive-loop rewrite: {}: exclusive_instructions={}; planned={} {:?}; \
         left_alone={} {:?}; skipped_segments={:?}; {mount}",
        guest_path.display(),
        report.exclusive_instructions,
        report.patched_total(),
        report.patched,
        report.refused_total(),
        report.refused,
        report.skipped_segments,
    )
}

/// The lines that state the exclusive-loop overlays as applied, printed from
/// inside the container, after its mounts succeeded: one per mounted copy.
fn exclusive_rewrite_applied_lines(overlays: &[ExecutableOverlayMount]) -> Vec<String> {
    overlays
        .iter()
        .filter_map(|overlay| {
            overlay.rewritten.map(|rewritten| {
                format!(
                    ":: Exclusive-loop rewrite applied: {}: rewritten={rewritten}, mounted over \
                     the original",
                    overlay.target.display()
                )
            })
        })
        .collect()
}

/// The bytes digest and mode of the artifact at `source`, or `None` when it
/// cannot be read. Recorded when a trial preparation makes the artifact.
fn overlay_fingerprint(source: &Path) -> Option<(String, u32)> {
    let bytes = fs::read(source).ok()?;
    let mode = fs::metadata(source).ok()?.permissions().mode() & 0o7777;
    Some((detcore::Digest::new(&bytes).to_string(), mode))
}

/// Whether every remembered overlay artifact still holds the bytes and mode
/// its preparation recorded. A missing, replaced or re-moded artifact is
/// stale and must be prepared again rather than mounted.
fn prepared_overlays_current(
    overlays: &[ExecutableOverlayMount],
    fingerprints: &[(PathBuf, String, u32)],
) -> bool {
    overlays.len() == fingerprints.len()
        && overlays.iter().all(|overlay| {
            fingerprints.iter().any(|(path, digest, mode)| {
                *path == overlay.source
                    && overlay_fingerprint(&overlay.source)
                        .is_some_and(|(now, now_mode)| now == *digest && now_mode == *mode)
            })
        })
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

/// How long the tracer may spend writing one fatal core before it gives up
/// on it. The guest's clock does not move meanwhile; only the host waits.
const FATAL_CORE_TIME_LIMIT: Duration = Duration::from_secs(10);

/// A token that names one invocation's fatal cores and no other's: the
/// controller's process ID, for the reader, and 64 random bits, because a
/// later run on this host, or a run in another PID namespace sharing the
/// directory, can have the same process ID.
fn fatal_core_owner_token() -> String {
    let mut nonce = [0u8; 8];
    // SAFETY: getrandom writes at most `nonce.len()` bytes into `nonce`.
    let filled = unsafe { libc::getrandom(nonce.as_mut_ptr().cast(), nonce.len(), 0) };
    let nonce = if filled == nonce.len() as isize {
        u64::from_le_bytes(nonce)
    } else {
        // Host time only names a file; it never reaches the guest.
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos() as u64)
    };
    format!("{}-{nonce:016x}", std::process::id())
}

/// Removes the files in `dir` that the invocation named by `owner` wrote:
/// its cores and any temporary file a capture left behind. Returns how many.
fn remove_fatal_core_files(dir: &Path, owner: &str) -> usize {
    let prefix = format!("hermit-{owner}-");
    let temp_prefix = format!(".{prefix}");
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if (name.starts_with(&prefix) || name.starts_with(&temp_prefix))
            && fs::remove_file(entry.path()).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

/// Set, to `PID:TOKEN` (see [`dap_supervisor_socket_name`]), in the run that
/// `hermit run --dap-listen` starts and supervises, which is the same command
/// run as a plain `--gdbserver` run.
const DAP_LISTEN_INNER_ENV: &str = "HERMIT_DAP_LISTEN_INNER";

/// The abstract Unix socket a supervising `hermit run --dap-listen` holds
/// for as long as it supervises its run: `hermit-dap-listen/PID/TOKEN`, with
/// TOKEN random. The run is told PID and TOKEN through
/// [`DAP_LISTEN_INNER_ENV`]; the socket itself, unlike a passed descriptor,
/// survives the run's start (which closes every inherited descriptor above
/// 2), and nothing but its holder can make the kernel list it.
fn dap_supervisor_socket_name(pid: u32, token: &str) -> String {
    format!("hermit-dap-listen/{pid}/{token}")
}

/// Binds this process's supervisor socket (see
/// [`dap_supervisor_socket_name`]), returning it, to be held while the run is
/// supervised, and the value of [`DAP_LISTEN_INNER_ENV`] for the run,
/// `PID:TOKEN`.
fn bind_dap_supervisor_socket() -> Result<(std::os::unix::net::UnixListener, String), Error> {
    use std::os::linux::net::SocketAddrExt;
    let mut bytes = [0u8; 16];
    fs::File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .context("Failed to read a random token for the DAP supervisor")?;
    let token: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let pid = std::process::id();
    let address = std::os::unix::net::SocketAddr::from_abstract_name(
        dap_supervisor_socket_name(pid, &token).as_bytes(),
    )
    .context("Failed to name the DAP supervisor's socket")?;
    let socket = std::os::unix::net::UnixListener::bind_addr(&address)
        .context("Failed to bind the DAP supervisor's socket")?;
    Ok((socket, format!("{pid}:{token}")))
}

/// Whether this process's parent is the `hermit run --dap-listen` that
/// `owner`, the value of [`DAP_LISTEN_INNER_ENV`], names as `PID:TOKEN`: its
/// process ID, running this same `hermit` executable (the same file), and
/// holding the supervisor socket named by PID and TOKEN (see
/// [`dap_supervisor_socket_name`]). This does not depend on how the parent
/// was given `--dap-listen` (on its command line or in a `--config` file).
/// Any other parent, a shell that set the variable to its own ID for one, is
/// not a supervisor.
/// Where `RunOpts::main` sends an invocation once its prelude is settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunDispatch {
    /// `--dap-listen` without a supervisor marker: supervise a child run.
    DapSupervisor,
    /// The run itself (also the child a DAP supervisor started).
    Run,
}

fn started_by_a_dap_supervisor(owner: &OsStr) -> bool {
    let parent = std::os::unix::process::parent_id();
    let Some((pid, token)) = owner.to_str().and_then(|owner| owner.split_once(':')) else {
        return false;
    };
    if pid.parse() != Ok(parent)
        || token.len() != 32
        || !token.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return false;
    }
    let identity = |path: String| {
        fs::metadata(path)
            .map(|metadata| {
                (
                    std::os::unix::fs::MetadataExt::dev(&metadata),
                    std::os::unix::fs::MetadataExt::ino(&metadata),
                )
            })
            .ok()
    };
    let same_executable = identity(format!("/proc/{parent}/exe"))
        .is_some_and(|parent| Some(parent) == identity("/proc/self/exe".to_owned()));
    // The socket's inode, from /proc/net/unix ("Num RefCount Protocol Flags
    // Type St Inode Path", an abstract path shown with a leading '@').
    let name = format!("@{}", dap_supervisor_socket_name(parent, token));
    let inodes: Vec<String> = fs::read_to_string("/proc/net/unix")
        .unwrap_or_default()
        .lines()
        .skip(1)
        .filter_map(|row| {
            let fields: Vec<&str> = row.split_whitespace().collect();
            (fields.len() >= 8 && fields[7] == name).then(|| format!("socket:[{}]", fields[6]))
        })
        .collect();
    let holds_the_socket = !inodes.is_empty()
        && fs::read_dir(format!("/proc/{parent}/fd")).is_ok_and(|entries| {
            entries.filter_map(Result::ok).any(|entry| {
                fs::read_link(entry.path()).is_ok_and(|target| {
                    inodes
                        .iter()
                        .any(|inode| target.as_os_str() == inode.as_str())
                })
            })
        });
    same_executable && holds_the_socket
}

/// The role `hermit verify-inputs --input ROLE:PATH` seals the host's
/// dynamic-linker cache under for a `--verify-run` pair, and its path.
pub(crate) const LD_SO_CACHE_ROLE: &str = "ld-so-cache";
pub(crate) const LD_SO_CACHE_PATH: &str = "/etc/ld.so.cache";

/// Command-line options for the "run" subcommand.
#[derive(Debug, Parser, Clone)]
#[clap(group(clap::ArgGroup::new("verification").args(["verify", "verify_run"])))]
pub struct RunOpts {
    /// Run the guest's first image through the packaged singleton exec-stop launcher,
    /// which starts the in-guest Detcore runtime before the guest's first instruction.
    /// Admitted only on in-guest-trap with the `exec-stop-launcher` and `liteinst`
    /// features, an installed private runtime package, and supported direct-run or
    /// singleton verification options. Namespace-only, strace-only, debugger and
    /// network recording/replay/perturbation requests refuse before launch with
    /// exec-stop-runtime-not-ready. Threads and a later exec remain named refusals.
    #[clap(long)]
    in_guest_exec_stop: bool,

    #[cfg_attr(
        feature = "native-exec",
        doc = "Freeze explicitly declared dynamic files for the native trap exec domain."
    )]
    #[cfg_attr(
        feature = "native-exec",
        doc = "Static files, scripts, threads, fork and timers remain outside this domain."
    )]
    #[cfg_attr(
        all(feature = "native-exec-input-admission", not(feature = "native-exec")),
        doc = "Recognize native exec enrollment requests and refuse execution in this admission-only build."
    )]
    #[cfg_attr(
        all(feature = "native-exec-input-admission", not(feature = "native-exec")),
        doc = "Rebuild hermit-run with --features native-exec, or drop --native-exec-inputs."
    )]
    #[cfg(any(feature = "native-exec", feature = "native-exec-input-admission"))]
    #[clap(long, value_name = "DECLARATION", requires = "verification")]
    native_exec_inputs: Option<PathBuf>,

    #[cfg(feature = "native-exec")]
    #[clap(skip)]
    native_exec_capture: Option<std::sync::Arc<super::native_exec_cli::Capture>>,

    /// The process instrumentation backend. It is selected only by the global
    /// option (`hermit --backend <BACKEND> run ...`); `run` does not accept
    /// `--backend` itself. [`RunOpts::main`] copies the global value here.
    #[clap(skip)]
    backend: Option<Backend>,

    /// A statically selected executable's backend, independent of the built-in
    /// enum. Ordinary `hermit` never sets this field.
    #[clap(skip)]
    application_backend: Option<&'static str>,

    /// The physical adapter's immutable capabilities. Only a
    /// admitted executable and its ContainerScope carry it. It is bound from
    /// the selected adapter type before common launch admission.
    #[clap(skip)]
    adapter_capabilities: Option<reverie::BackendCapabilities>,

    /// Program to run. Bare names are resolved using the guest PATH. Paths under host `/tmp` are
    /// hidden by Hermit's isolated `/tmp` unless `--tmp=/tmp` or an explicit mount exposes them.
    /// With `--image`, may be omitted: the image's Entrypoint/Cmd then supplies it.
    #[clap(value_name = "PROGRAM")]
    program: Option<PathBuf>,

    /// Arguments for the program.
    #[clap(value_name = "ARGS")]
    args: Vec<String>,

    /// Serve this run to one Debug Adapter Protocol client, such as an
    /// editor, on the TCP address [HOST:]PORT (HOST defaults to 127.0.0.1).
    /// Implies --gdbserver: it starts `hermit-dap --listen` from beside this
    /// executable, and the client's attach request names the program and the
    /// GDB server, as printed. Not with --verify, which runs the program
    /// twice, --namespace-only, which has no GDB server, a backend without
    /// one (only ptrace serves gdb), or --image, whose program lives in the
    /// image. See DEBUGGING.md.
    #[clap(
        long,
        value_name = "[HOST:]PORT",
        conflicts_with_all = ["verify", "namespace_only", "image"]
    )]
    dap_listen: Option<String>,

    /// The GDB, with its DAP interpreter, that `--dap-listen`'s adapter runs
    /// (default: /usr/bin/gdb, else gdb on PATH).
    #[clap(long, value_name = "PATH", requires = "dap_listen")]
    dap_gdb: Option<PathBuf>,

    /// Path to a happens-before specification (JSON; see RFC #1146) that places
    /// deterministic ordering edges between anchored events. Anchor code
    /// locations (`func`, `file:line`) are resolved against the guest program's
    /// debug info. The scheduler enforces anchors that name "the thread's Nth
    /// syscall" and anchors that name a syscall, optionally on one file
    /// descriptor ({"syscall": "writev", "fd": 9, "nth": 3}); a syscall anchor
    /// that never fires ends the run with HERMIT_HB_ANCHOR_NEVER_FIRED. A
    /// syscall anchor fires at the syscall's entry, or, with "phase":
    /// "posthook", when its result is final, so an edge from it to an entry
    /// anchor orders its completed effect; a posthook anchor on an interrupted
    /// call ends the run with HERMIT_HB_POSTHOOK_INTERRUPTED, and posthook
    /// anchors are refused with `--replay-schedule-from`.
    /// A version-2 spec ("version": 2) adds `futex_op` (one futex operation,
    /// {"syscall": "futex", "futex_op": 393}), `from` (count a syscall anchor
    /// strictly after another event on the same thread, so no calibration run
    /// is needed) and `max_hold_ns` (how long, in virtual time, a thread may be
    /// held, default 10 s; past it the run ends with
    /// HERMIT_HB_HOLD_BUDGET_EXCEEDED). A version-2 spec needs the preemption
    /// timer and is refused without it. Both
    /// kinds are enforced only when the guest runs under the ptrace runtime
    /// (the default, or `--backend=e9patch`): with `--namespace-only` or
    /// another backend, the run is refused before the guest starts (exit 122)
    /// rather than run with the ordering ignored. A spec the user must fix
    /// (missing, unreadable, not valid JSON, not a valid program) is refused the
    /// same way; any other failure to read it, such as EIO, is a Hermit failure
    /// (exit 125). Combine with `--hb-list-events` to preview how the spec
    /// resolves against the binary.
    #[clap(long, value_name = "filepath")]
    happens_before: Option<PathBuf>,

    /// Load the `--happens-before` specification, resolve its anchors against the
    /// guest program's debug info, print the resolved anchor/edge program, and
    /// exit without running the guest.
    #[clap(long, requires = "happens_before")]
    hb_list_events: bool,

    /// Runtime-only cache of the resolved happens-before program. Populated in
    /// `main()` after the guest binary's debug info is available (so anchor code
    /// locations resolve to addresses), then copied into `DetConfig` for the
    /// scheduler. It is never a CLI argument and never serialized; `#[clap(skip)]`
    /// defaults it to `None` when `RunOpts` is parsed or round-tripped.
    #[clap(skip)]
    resolved_happens_before: Option<HappensBeforeProgram>,

    /// Runtime-only: why argument validation turned `--max-timeslice` off,
    /// when the host cannot arm the preemption timer. Set by
    /// `validate_args_with_timer_support`; read by
    /// `refuse_happens_before_without_preemption`. Never a CLI argument.
    #[clap(skip)]
    timer_downgrade: Option<TimerUnavailable>,

    /// Runtime-only: where `--verify` has this run record the host files the
    /// guest opens (`DetConfig::host_input_log`). Never a CLI argument and
    /// never serialized, like `resolved_happens_before`.
    #[clap(skip)]
    host_input_log: Option<PathBuf>,

    /// Runtime-only: record host inputs even where `host_input_log` cannot
    /// name the destination (DBT: the in-guest runtime learns the
    /// guest-visible path from its private environment instead).
    #[clap(skip)]
    record_host_inputs: bool,

    /// Opt in to keeping a core file for each guest process that a
    /// core-dumping signal (SIGSEGV, SIGABRT, SIGBUS, ...) kills. The core is
    /// written zstd-compressed into DIR as
    /// `hermit-<HERMIT PID>-<RANDOM>-core.<PID>.<TID>.<SIGNAL>.zst`, where PID
    /// and TID are the guest's (TID is the thread that took the signal) and
    /// RANDOM is unique to this invocation. Like the kernel's, the core holds
    /// every thread of the process, the one that took the signal first. Off by
    /// default: without this option no core is written. Only ptrace-backed
    /// backends write cores. A run that exits 0 (for `--verify`, whose two
    /// runs also matched) deletes the cores it wrote again. Writing a core
    /// stops the whole guest for up to 10 seconds of host time, which counts
    /// against `--timeout`.
    #[clap(long, value_name = "DIR")]
    fatal_core_dir: Option<PathBuf>,

    /// With `--fatal-core-dir`, the most bytes one compressed core may take. A
    /// core that does not fit is retried with only the live part of each
    /// thread's stack, then with only its registers and memory layout.
    #[clap(long, value_name = "BYTES", default_value_t = 256 << 20, requires = "fatal_core_dir")]
    fatal_core_max_bytes: u64,

    /// With `--fatal-core-dir`, the most bytes the regular files directly in
    /// DIR may take together, counting files that were there before the run.
    #[clap(long, value_name = "BYTES", default_value_t = 1 << 30, requires = "fatal_core_dir")]
    fatal_core_total_max_bytes: u64,

    /// Runtime-only: `--fatal-core-dir`, held open by the controller so the
    /// run container reaches it through `/proc/self/fd` whatever its mount
    /// namespace hides (see [`descriptor_path`]). Opened by `main` just before
    /// the guest starts.
    #[clap(skip)]
    fatal_core_dir_handle: Option<std::sync::Arc<File>>,

    /// Runtime-only: which `--verify` run this is, so the two runs' cores do
    /// not share names.
    #[clap(skip)]
    fatal_core_run_label: Option<&'static str>,

    /// Runtime-only: Entrypoint/Cmd resolution already applied (idempotence).
    #[clap(skip)]
    image_program_resolved: bool,

    /// Runtime-only: the token naming this invocation's cores, made when
    /// `--fatal-core-dir` is opened (see [`fatal_core_owner_token`]). The
    /// tracer computes names inside the run's PID namespace, where its own
    /// process ID is not the controller's.
    #[clap(skip)]
    fatal_core_owner: String,

    /// Whether this invocation's epoch was captured from the host clock because
    /// neither `--epoch` nor `HERMIT_EPOCH` supplied an explicit input.
    ///
    /// This is CLI provenance only: the resolved timestamp itself is copied into
    /// `DetConfig` before any backend starts and is the sole value serialized or
    /// observed by the guest.
    #[clap(skip)]
    epoch_captured_from_host: bool,

    /// Whether this `--no-namespace` run's guest ID base was drawn from the
    /// host (`resolve_guest_ids`): `free-window` for a base taken from the
    /// windows free on this host. A base given by `--pid-base` or required by
    /// `--strict` is not; nor are the host's own IDs, which no rerun can
    /// reproduce (the summary and the preemption record say `Host`).
    #[clap(skip)]
    guest_ids_drawn_from_host: Option<&'static str>,

    /// Whether neither `--epoch` nor `HERMIT_EPOCH` supplied the epoch, whether
    /// or not virtual time then sampled the host clock for it.
    #[clap(skip)]
    epoch_omitted: bool,

    /// An omitted virtual epoch was left unresolved because this opt-in request
    /// was refused. Package appearance later cannot admit this invocation.
    #[clap(skip)]
    exec_stop_epoch_refused: bool,

    /// Whether the epoch was taken from the preemption record being replayed,
    /// replacing an omitted one. See `adopt_replayed_schedule_epoch`.
    #[clap(skip)]
    epoch_from_recording: bool,

    /// Runtime-only: the options this invocation was given, captured from the
    /// parsed command line for `--save-config` (see [`crate::app::run_config::capture`]).
    #[clap(skip)]
    config_capture: Option<crate::app::run_config::RunConfig>,

    /// Clap provenance, captured before values are lowered to DetConfig.
    /// Programmatic callers have an effective configuration and must not have
    /// a positive maximum silently replaced merely because it equals a default.
    #[clap(skip = true)]
    max_timeslice_explicit: bool,

    /// Whether `resolve_chaos_timeslice_range` took the run's timeslice from a
    /// replayed record, which a `--chaos-timeslice-draw=run` draw must not
    /// replace.
    #[clap(skip)]
    chaos_timeslice_adopted: bool,

    /// Runtime-only: the seed `--seed-from` chose, which `--save-config`
    /// writes in its place.
    #[clap(skip)]
    seed_from_resolved: Option<u64>,

    #[clap(flatten)]
    pub(crate) det_opts: DetOptions,

    /// Require Hermit's deterministic defaults and reject incompatible opt-outs. Unsupported
    /// syscalls already fail closed in ordinary runs. `--no-virtualize-time` is still accepted:
    /// clock syscalls then return host time, so the run is not reproducible; record it with
    /// `hermit record` to reproduce it. `--verify --verify-strict` reports the difference; plain
    /// `--verify` reports it only when host time reaches the guest's output. On the ptrace and
    /// e9patch backends, `--strict` also refuses to run when the host's performance counters fail
    /// Reverie's validation, because the virtual clock counts retired branches and cannot be exact;
    /// with `--max-timeslice=disabled` Detcore uses no branch count, so that run is not refused
    /// (Reverie still sets up, calibrates and incidentally reads the counters, but none of
    /// those reads reaches the virtual clock, a preemption point or the guest). Every input must be
    /// on the command line: a strict run without `--epoch` (or `HERMIT_EPOCH`, or a replayed
    /// recording that supplies it), or with `--seed-from=SystemRandom`, is refused before the
    /// guest starts rather than drawing that input from the host.
    #[clap(
        long,
        conflicts_with_all = ["no_sequentialize_threads", "no_deterministic_io", "strace_only"]
    )]
    strict: bool,

    /// Permit unsupported syscalls to reach the host kernel. This weakens determinism and exists
    /// only as an explicit compatibility escape hatch; ordinary runs fail closed by default.
    #[clap(
        long,
        conflicts_with_all = ["strict", "panic_on_unsupported_syscalls"]
    )]
    allow_unsupported_syscalls: bool,

    /// Kill the run if the guest does not finish within this many seconds.
    ///
    /// Hermit created the container, so hermit unwinds it: on expiry the guest
    /// future is dropped, reverie reaps its tracees, detcore's state is torn
    /// down, and the container init returns normally. Exits 124, the same code
    /// GNU `timeout` and `hermit record --record-timeout` already use for a
    /// deadline. Wall-clock, so it never affects guest-visible determinism.
    #[clap(long, value_name = "SECONDS")]
    timeout: Option<NonZeroU64>,

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

    /// Override the processor-specific PMU skid margin, in retired conditional branches.
    ///
    /// A precise timer event is programmed to interrupt the guest this many branches before its
    /// target, and the guest is then single-stepped onto the target. A larger margin makes it
    /// less likely that a late interrupt lands past the target (an overshoot, which refuses the
    /// run), and costs more single steps per delivered event. Any override also turns off the
    /// two-stage arming Reverie otherwise uses for long events, which changes the cost but not
    /// where an event is delivered.
    ///
    /// Up to 10000, the largest margin in Reverie's processor table, the margin changes only how
    /// a target is reached: never the target, and never whether a stop keeps or cancels an event,
    /// which is decided by a keep margin of 10005 on every processor, with or without the
    /// override. Such a run's schedule and output are those of a run without it. A larger margin
    /// moves the keep margin to the margin plus 5: the run is still deterministic and the same
    /// on every processor, but its schedule can differ from a run without the override, so
    /// compare runs that pass the same value. (`REVERIE_SKID_MARGIN_OVERRIDE`, a fault-injection
    /// variable, sets the margin too; this flag wins when both are set.)
    // TODO-HUMAN-REVIEW(PR-991): Review the ptrace PMU skid-margin CLI override.
    #[clap(long, value_name = "RCBS")]
    skid_margin: Option<u64>,

    /// Mount a file or directory. This uses the same syntax as Docker's `--mount` option. The
    /// source must exist on the host. For simple bind mounts into guest `/tmp`, use `--bind`.
    #[clap(long, value_name = "path")]
    mount: Vec<MountArg>,

    /// Bind-mount a host file or directory into guest `/tmp` or the guest's private `/dev/shm`.
    /// Use `SOURCE` to preserve its path or `SOURCE:TARGET` to choose a target under `/tmp` or
    /// `/dev/shm`; the source must already exist, and one bound below `/dev/shm` must not be in
    /// the host's `/dev/shm`. `--bind /dev/shm` keeps the host's `/dev/shm` in place of the
    /// guest's private one; `--bind DIR:/dev/shm` uses host directory `DIR` instead.
    #[clap(long, value_name = "path")]
    pub(crate) bind: Vec<Bind>,

    /// Rewrite canonical aarch64 load/store-exclusive retry loops to single
    /// LSE atomic instructions before the guest runs them.
    ///
    /// An `LDXR`/`STXR` pair retries whenever an exception clears the local
    /// exclusive monitor between the two, and each retry retires the loop's
    /// branches again, so the same guest work does not retire the same number
    /// of branches. A single atomic instruction cannot retry. This is
    /// preprocessing for the ptrace backend, not a backend, and anything that
    /// is not a fully decoded canonical loop is left alone and reported. Only
    /// what ARM mapping symbols mark as code is rewritten, either way: a file
    /// without them, such as every stripped file, is left unchanged and named
    /// in the run's diagnostics and its summary.
    ///
    /// Where the guest has a mount namespace, a rewritten copy of the guest
    /// executable (and of each `--rewrite-exclusives-also` file) is
    /// bind-mounted over the original. With `--no-namespace` there is nothing
    /// to carry such an overlay, so the loops of every executable file the
    /// guest maps are rewritten in its memory instead, as each is mapped; see
    /// `--rewrite-exclusives-mode`. An `--image` guest (and `hermit oci
    /// run`'s) loads the files of the image's rootfs, which an overlay does
    /// not reach, so there only the in-memory rewrite applies.
    #[clap(long)]
    pub(crate) rewrite_exclusives: bool,

    /// Also rewrite this file (a shared library the guest loads), with
    /// `--rewrite-exclusives` in overlay mode. Repeatable.
    #[clap(long, value_name = "FILE", requires = "rewrite_exclusives")]
    pub(crate) rewrite_exclusives_also: Vec<PathBuf>,

    /// Where `--rewrite-exclusives` applies its rewrite. `overlay` rewrites
    /// copies of the guest executable and the `--rewrite-exclusives-also`
    /// files and bind-mounts them over the originals, which needs a mount
    /// namespace. `memory` rewrites the loops of every executable file the
    /// guest maps, in the guest's own pages, before any of their instructions
    /// run: the files are untouched, and a guest that reads its own code sees
    /// the rewritten instructions. `auto`, the default, is `memory` under
    /// `--no-namespace` and `overlay` otherwise.
    ///
    /// `memory` writes into a mapping only when it can prove the mapped
    /// object is the file it planned from: through `/proc/<pid>/map_files`,
    /// which needs `CAP_CHECKPOINT_RESTORE` or `CAP_SYS_ADMIN`, or by the
    /// file's inode and device where those identify it. On btrfs or
    /// overlayfs, whose inode numbers are not identities across subvolumes
    /// or layers, an unprivileged run therefore writes nothing: the summary
    /// reports each file as not written, and why, and the guest runs its
    /// original loops.
    #[clap(
        long,
        value_enum,
        value_name = "MODE",
        default_value_t,
        requires = "rewrite_exclusives"
    )]
    pub(crate) rewrite_exclusives_mode: RewriteExclusivesMode,

    /// Select guest networking. `local` creates an isolated loopback interface; `host` exposes the
    /// host network and compromises isolation and deterministic reproducibility.
    ///
    /// `local`, the default, gives each run its own network namespace, so a run's ports and
    /// abstract unix socket names (`@name`) are its own. Under `host` the guest shares the host's:
    /// every guest's first process is pid 3 and Hermit picks autobind names the same way in every
    /// run, so two concurrent `host` runs whose guests bind the same abstract name, for instance
    /// one built from the pid, collide and the second bind fails with EADDRINUSE. On the ptrace
    /// backend Hermit prints a one-time NOTICE on stderr when a guest binds an abstract name under
    /// `--network=host`, or fails to with EADDRINUSE. Use `host` only
    /// for a guest that must reach the host's network, and run such guests one at a time when
    /// they bind abstract names.
    #[clap(
        long,
        alias = "net",
        value_name = "local|host",
        default_value = "local"
    )]
    network: NetworkingMode,

    /// Record outbound TCP traffic to a new trace at this path. The guest reaches the host
    /// network; every byte it receives is stamped with the deterministic time at which the
    /// guest observed it, so `--replay-networking` reproduces the run without the network.
    #[clap(
        long,
        value_name = "NEW_TRACE",
        conflicts_with_all = ["replay_networking", "network", "verify", "namespace_only"]
    )]
    record_networking: Option<PathBuf>,

    /// Replay outbound TCP traffic from a trace written by `--record-networking`, without
    /// contacting the network. Any difference in what the guest sends ends the run.
    #[clap(
        long,
        value_name = "TRACE",
        conflicts_with_all = ["record_networking", "network", "namespace_only"]
    )]
    replay_networking: Option<PathBuf>,

    /// Run with namespaces but without ptrace or determinization. This is a useful smoke test
    /// when diagnosing ptrace/seccomp policy failures; PID and `/tmp` isolation still apply. The
    /// guest carries one seccomp filter, which stops only `kill`: a `kill(0, sig)` from Hermit's
    /// own process group is refused with EPERM, unlike Linux, so it cannot reach Hermit and the
    /// processes that started it. The guest can see the filter (`Seccomp: 2` in
    /// /proc/self/status), its own seccomp listener fails with EBUSY, and a hermit run inside it
    /// refuses the inherited filter unless given --unsafe-ignore-host-seccomp.
    #[clap(
        long,
        alias = "lite",
        conflicts_with = "chaos",
        conflicts_with = "verify"
    )]
    namespace_only: bool,

    /// Run syscall interception directly on the host without creating Linux namespaces or
    /// mounting an isolated `/tmp`. This is not a sandbox and must only be used with trusted
    /// guests. Host process, filesystem, and network state are shared, reducing determinism.
    /// A recorded schedule or preemption record replays only where Hermit chose the guest's
    /// IDs, both when recording and when replaying: the replay takes the record's base (see
    /// `--pid-base`) and is refused if it cannot have those IDs.
    ///
    /// Where Hermit may choose process IDs (as root, with CAP_CHECKPOINT_RESTORE or
    /// CAP_SYS_ADMIN over the host's PID namespace), the guest's IDs do not come from the host:
    /// see `--pid-base`. Otherwise the guest gets the host's next free IDs, which differ from run
    /// to run, and Hermit warns.
    ///
    /// The guest's mount table (/proc/self/mountinfo, /proc/mounts) is not the host's: it
    /// lists the mounts holding the guest's working directory and the host /tmp, their
    /// ancestors, and the base mountpoints (/, /proc, /dev, /sys, /run and a few below them).
    /// Other host mounts are hidden; a file on one reports its nearest listed ancestor.
    ///
    /// Incompatible with `--image`, which chroots the guest into a materialized
    /// OCI rootfs and therefore requires namespaces; that conflict is reported
    /// at runtime with an explanatory message rather than as a generic clap
    /// argument conflict.
    #[clap(
        long,
        visible_alias = "core-only",
        conflicts_with_all = [
            "mount",
            "bind",
            "network",
            "tmp",
            "namespace_only",
            "analyze_networking"
        ]
    )]
    no_namespace: bool,

    /// With `--no-namespace`, the process ID of the guest's first process. Every later process
    /// or thread Hermit numbers gets the next ID not already used by the guest (including its
    /// own clone3 set_tid choices), in the guest's own order of creation, and no
    /// ID is given out twice, so the guest's process and thread IDs depend only on this base and
    /// the guest itself, as a PID namespace's would. The kernel hands out exactly these IDs
    /// (`clone3` with `set_tid`), so they are what the guest's own calls, `/proc` and futex owner
    /// words show. Hermit must be allowed to choose IDs: root, or CAP_CHECKPOINT_RESTORE or
    /// CAP_SYS_ADMIN over the host's PID namespace; so must the guest's tasks, so a guest that
    /// gives those capabilities up gets host IDs from then on.
    ///
    /// The ID space is cut into aligned windows of 1024 IDs (256 where `pid_max` is above 65536),
    /// and a run reserves the window or windows its IDs start in for as long as it runs, so two
    /// Hermit runs on one host do not start in the same IDs (IDs past those windows are not
    /// reserved; see below). The reservation is an abstract Unix socket
    /// named after the PID namespace and the window, so it covers runs that share a network
    /// namespace; a run in another one can still take the same window, and the later run then
    /// ends with status 122, naming the ID, when its first process cannot have it.
    ///
    /// Given explicitly (and under `--strict`, whose base is half of `pid_max` unless given), the
    /// base is a requirement: before the run, Hermit checks that no host task holds one of the
    /// window's IDs from it, that no other run holds its windows, and that it may choose IDs, and
    /// refuses the run otherwise. During the run, a task that cannot have its ID (taken by a host
    /// task meanwhile; a call Hermit cannot issue with a chosen ID, such as one under a seccomp
    /// filter of the guest's own or in a PID namespace of the guest's) ends the run. Both refusals
    /// exit with status 122 and name the cause.
    /// Chosen IDs also require a host probe to show that legacy clone validates its original
    /// exit signal before Hermit rewrites the call. If that result is absent or inconclusive,
    /// required IDs are refused; automatic selection warns and uses host IDs. Use a host that
    /// passes the probe, or omit `--no-namespace` and `--pid-base` for a new run in a PID namespace.
    ///
    /// Without this option, the run takes the first window no host task holds and no other run
    /// has reserved, starting just behind the kernel's PID allocation cursor (so that host
    /// processes reach it last) and going down, then from the top, and reports the base it took.
    /// Such a run tolerates host IDs: a task that cannot have its chosen ID gets a host ID with a
    /// warning, and the run's summary and preemption record say its IDs are not deterministic.
    /// With no window left, the guest gets host IDs and Hermit warns. A guest that creates more
    /// tasks than a window holds goes on into the next window. Needs the ptrace backend with
    /// sequentialized threads.
    ///
    /// Only the window or windows the IDs start in are checked and reserved. IDs are never given
    /// out twice in a run, so every task the guest ever creates (each process and thread,
    /// including those that exited) reserves its ID, and a guest that creates more tasks than
    /// those windows hold goes on into IDs that nobody checked or reserved, where a host task or
    /// another run may already be. Its outcome then depends on the host, though never silently:
    /// a task that cannot have its ID ends a run with an explicit base, or gets a host ID with a
    /// warning and marks the run's record.
    ///
    /// A replay of a record whose IDs were chosen (`--replay-preemptions-from`,
    /// `--replay-schedule-from`) takes its base from the record, as if given here.
    ///
    /// For tooling: the same IDs come back in every run from the same base, and a window is free
    /// for the next run as soon as one ends, so a process ID remembered from one run may name a
    /// later run's guest. Check a remembered process's identity (a pidfd, its start time or its
    /// command line) before signalling it.
    #[clap(long, value_name = "pid", requires = "no_namespace")]
    pid_base: Option<i32>,

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

    /// PROTOTYPE: run the guest against the root filesystem of an OCI image
    /// instead of the host filesystem. The image is materialized into a
    /// read-only cached root and the guest is `chroot`ed into it with an
    /// isolated writable `/tmp`, so its FILE INPUTS come deterministically
    /// from the image. Three forms are accepted. (1) A reference into the
    /// local podman store, e.g. `--image docker.io/library/busybox@sha256:...`
    /// (pin by digest for reproducibility); the image must already be in the
    /// store — fetch it first with `hermit oci pull` or `podman pull`, as
    /// `hermit run` never pulls. (2) `--image=oci-dir:/absolute/path` (or
    /// `./relative/path`): an OCI layout directory (`oci-layout` +
    /// `index.json` + `blobs/sha256/`), for example from
    /// `podman save --format oci-dir`. (3)
    /// `--image=docker-archive:/absolute/file.tar` (or `./file.tar`): a
    /// `podman save`/`docker save` archive holding exactly one image. The
    /// local forms are read directly by Hermit — podman is not invoked.
    /// Verification is per format: an oci-dir's manifest blob digest,
    /// config digest, layer blob digests, and layer diff_ids are all
    /// verified, while a docker-archive (which has no manifest blob
    /// digest) is verified by its config hash and layer diff_ids. Only
    /// linux/amd64 images with uncompressed or gzip layers are supported:
    /// where the image declares an os/architecture it must be
    /// linux/amd64, and an unspecified os/architecture is accepted.
    /// Layer uid/gid are flattened to the invoking user
    /// (materialization is unprivileged; the flattened-entry count is
    /// logged), while modes and mtimes are preserved (subsecond mtimes
    /// included where the layer records them; directories created only
    /// as implicit parents get mode 0755 and mtime 0). The guest program
    /// path must resolve inside the image: give an absolute path (e.g.
    /// `/bin/sh`) or a relative path containing a `/` (e.g. `./bin/sh`),
    /// which is resolved against the image working directory; a bare
    /// command name is searched on the image's PATH. Requires namespaces
    /// (incompatible with `--no-namespace`). The prototype supports the
    /// ptrace and kvm backends and does not yet compose with custom
    /// `--mount`/`--bind` options.
    #[clap(long, value_name = "OCI-REFERENCE")]
    image: Option<String>,

    /// Exactly like "seed" but we generate a seed for you. This is useful if multiple
    /// hermit runs execute in parallel and rand based collisions exist.  "Args" generates
    /// the seed from the other arguments passed to hermit, "SystemRandom" uses system
    /// randomness to generate a seed, and creates a log message recording it.
    #[clap(long, value_name = "'Args'|'SystemRandom'")]
    seed_from: Option<SeedFrom>,

    /// With `--chaos`, draw `--max-timeslice` from the scheduler seed instead of using one value
    /// for every seed. Chaos ends each timeslice after a random number of retired conditional
    /// branches, drawn from an exponential distribution whose mean parameter is the maximum
    /// timeslice and capped at the maximum, so slices average about 0.63 of it before rounding to
    /// whole branches. A race whose window is a few branches long needs a maximum of about a
    /// hundred branches, and a race whose other thread must first run far needs one that lets it
    /// get there. Two in five seeds draw their maximum from 1000 to 2000 virtual nanoseconds (100
    /// to 200 branches), two in five from 1000000 to 200000000 (the `--max-timeslice` default), and
    /// one in five from the decades between, each log-uniformly within its range, so a sweep over
    /// seeds tries every scale and most often the two that races have needed. Where
    /// `--clock-multiplier` makes one RCB longer than the value drawn, the maximum is one RCB. A
    /// seed (`--sched-seed`, else `--seed`) always draws the same value, and the run is the run
    /// with that value given as `--max-timeslice`, except that the guest can read Hermit's own
    /// command line as PID 1's. Hermit prints the drawn value on stderr, and `--summary` reports
    /// it. The draw is part of what a seed selects: a Hermit release that changed it would change
    /// every seed's schedule, so pass the printed value as `--max-timeslice` to pin a run. Seeds
    /// that draw small values run much slower: every timeslice ends with a timer event, and the
    /// ptrace backend steps the guest through the last skid margin of each slice (all of a slice
    /// shorter than the margin, which is 100 to 10000 branches depending on the processor). Needs a
    /// backend with a PMU timer (ptrace, with or without e9patch preprocessing); elsewhere use
    /// `--target-timeslice`. Cannot be combined with a positive `--max-timeslice`, with
    /// `--target-timeslice` or with `--chaos-timeslice-range`. There is nothing to draw with
    /// `--max-timeslice=disabled` or on a host without usable performance counters. A run that
    /// replays preemptions or a schedule, or whose chaos `hermit analyze` turns off for a
    /// baseline, draws nothing: it keeps the default maximum, or one RCB where that is longer (for
    /// a replay under the same clock multiplier and slowdown settings, what its recording drew),
    /// and a replay takes its preemption points from the recording.
    #[clap(long, requires = "chaos")]
    chaos_seeded_timeslice: bool,

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
    /// of substantially larger logs and stricter comparison. Implies the strict,
    /// bitwise comparison of --verify-strict, and additionally raises the diff
    /// verbosity (larger logs, more syscall history).
    #[clap(long, requires = "verification")]
    verify_verbose: bool,

    /// Compare the internal logs under the CANONICAL parity policy: strip only
    /// the real wall-clock timestamp prefix (genuinely irreproducible),
    /// canonicalize host memory addresses to first-appearance ordinals (so an
    /// ASLR shift is tolerated but allocation-order and aliasing changes still
    /// diverge), and compare every INFO message's remaining bytes — virtual-time
    /// timestamps, raw syscall argument/result values, counts, sizes, flags —
    /// exactly. An explicit `--log=debug` or `--log=trace` remains captured for
    /// `--print-verify-logs` diagnostics but does not change this INFO verdict; use
    /// `--verify-verbose` to request an all-level diagnostic comparison. Without
    /// this (and without --verify-verbose) the default `--verify` normalizes away
    /// numbers, addresses, tmp paths, and timestamps before comparing, so a
    /// "verified" result asserts only stripped parity, not bitwise identity.
    /// Unlike --verify-verbose this stays quiet: it changes only the comparison,
    /// not the diff output volume, so a determinism ratchet can require parity
    /// without drowning in trace logs.
    #[clap(long, requires = "verification")]
    verify_strict: bool,

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
    #[clap(long = "print-verify-logs", alias = "verify-logs", requires = "verify")]
    print_verify_logs: bool,

    /// Retain the captured verification logs after the comparison. After a
    /// match, only the first run's log is kept, as the golden log; the second
    /// run's log, which compared equal to it under the selected comparison
    /// policy, is deleted. After a divergence, a no-result, or an error, both
    /// logs are kept, or only the first run's when verification stopped before
    /// a second run.
    /// Logs are written under --verify-log-dir when provided; otherwise under
    /// $XDG_STATE_HOME/hermit/verify-logs (normally
    /// ~/.local/state/hermit/verify-logs). The final paths are printed.
    /// Without --keep-logs the logs are temporary files in $HERMIT_VERIFY_TMPDIR
    /// when it is set and not empty (also for `record start --verify`), otherwise
    /// in the system temporary directory; a verify that is killed leaves them
    /// there, and a HERMIT_VERIFY_TMPDIR that cannot hold them is an error that
    /// names it. Hermit does not pass HERMIT_VERIFY_TMPDIR on to the guest unless
    /// it is given with --env.
    #[clap(long, requires = "verify")]
    keep_logs: bool,

    /// Directory for logs retained by --keep-logs.
    #[clap(long, value_name = "DIRECTORY", requires = "keep_logs")]
    verify_log_dir: Option<PathBuf>,

    /// With --verify, write the verification verdict as a single JSON line to
    /// this path: `{"verified":bool,"bitwise_parity":bool,
    /// "verdict":"matched"|"diverged","comparison":{"strictness":
    /// "stripped"|"canonical","display_name":str,
    /// "compare_logs":bool,"compare_io_buffers":bool,
    /// "log_scope":
    /// "deterministic"|"info"|"full_trace","record_envelope":
    /// "all_records_v1","strip_lines":bool,
    /// "canonicalize_addresses":bool,"full_trace":bool,"exact_remainder":bool,
    /// "stripped_prefixes":[str],"canonicalizations":[str],"ignore_lines":bool,
    /// "skip_commit":bool,"skip_detlog":bool},"dbt_counted_branches":
    /// {"left":int,"right":int},"guest_exit_code":int|null,
    /// "guest_signal":int|null,"first_divergent_scheduler_turn":int|null,
    /// "first_divergent_virtual_nanoseconds":int|null,
    /// "first_divergent_record":int|null,"first_divergent_syscall":int|null}`.
    /// `dbt_counted_branches` is present only when DBT completed a typed
    /// whole-process comparison; it is omitted for other backends and no-result.
    /// If a sandbox exits before returning its guest Output, the report instead
    /// records `verdict: "no_result"` and `no_result_reason.kind:
    /// "container_failed"`, with `run: "run1"|"run2"` and the container's
    /// `exited` (code) or `signaled` (signal, core_dumped) disposition. Guest
    /// status, comparison, counts and divergence coordinates remain absent.
    /// An unchanged `not_run` reason means only that the initial stamp was not
    /// replaced; it does not prove that no execution was attempted.
    /// ALL FOUR divergence coordinates are emitted, and they are four
    /// DIFFERENT KEYSPACES that must never be compared across axes: one real
    /// divergence was record 7495, syscall 1074, scheduler turn 196 -- three
    /// numbers for one event. A consumer that reads a subset silently drops
    /// located evidence. This is the
    /// exit-code-independent verdict
    /// channel: `verified` reflects whether the two runs matched, regardless of
    /// what the guest exited with, so a caller need not (and must not) infer the
    /// verdict from the process exit code. A determinism / record-replay parity
    /// ratchet must key on `bitwise_parity`, NOT `verified`: `bitwise_parity` is
    /// true only under the `canonical` (`BitwiseInfoV1`) policy — a full-INFO
    /// comparison inside a named canonical record envelope that strips only the
    /// real wall-clock prefix, canonicalizes host addresses to first-appearance
    /// ordinals, includes syscall output-buffer hashes, and compares everything
    /// else exactly (see --verify-strict) — so it cannot be silently weakened to
    /// a stripped, content-blind, or opaque filtered compare.
    #[clap(long, requires = "verify", value_name = "PATH")]
    verify_json: Option<PathBuf>,

    /// Run the guest once, as one of the two executions of `--verify`, and
    /// write that execution's evidence to NEW_DIR, which must not exist: its
    /// verification log, the guest's standard output and error, its run
    /// summary, the host files its guest opened, and a verify-run.json record
    /// of how it ended. `hermit log-diff --verify-json REPORT DIR1 DIR2`
    /// compares two such directories and writes the report that `--verify
    /// --verify-json` writes, so a caller can start the two runs itself, in
    /// parallel if it likes, and keep both logs. `--verify-strict`,
    /// `--verify-verbose` and `--no-detlog-io-buffers` choose the comparison
    /// as they do for `--verify`, and both runs must choose the same one. Give
    /// both runs one `--epoch` (or HERMIT_EPOCH): the comparison refuses runs
    /// whose virtual clocks started at different epochs. On DBT the directory
    /// also holds DynamoRIO's typed statistics stream. The exit status is the
    /// guest's when the run completed.
    #[clap(
        long,
        value_name = "NEW_DIR",
        conflicts_with_all = ["verify", "namespace_only"]
    )]
    verify_run: Option<PathBuf>,

    /// INTERNAL, the native-exec pairing seam: adopt the sealed input set that
    /// `hermit verify-inputs` left at this descriptor, the same one in both
    /// runs of the pair (see `verify_inputs`). Hidden until a native-exec
    /// consumer lands.
    #[clap(long, value_name = "FD", requires = "verify_run", hide = true)]
    native_exec_shared_inputs: Option<std::os::fd::RawFd>,

    /// The inputs adopted from `--native-exec-shared-inputs`, held until the
    /// run ends.
    #[clap(skip)]
    shared_inputs: Option<std::sync::Arc<super::verify_inputs::AdoptedInputs>>,

    /// The guest's `/etc/ld.so.cache`, when one snapshot serves both runs of a
    /// verification: taken once by `run --verify`, or the object the holder
    /// sealed as [`LD_SO_CACHE_ROLE`] for `--verify-run`. `None` reads the
    /// host's cache at container setup
    /// (<https://github.com/rrnewton/hermit/issues/4693>).
    #[clap(skip)]
    ld_so_cache_snapshot: Option<super::container::LdSoCacheSnapshot>,
    /// The mounts of the namespace this process was launched in, read just
    /// before it creates a run's container (`capture_launch_namespace_mounts`)
    /// for `detcore::Config::launch_namespace_mounts`.
    #[clap(skip)]
    launch_namespace_mounts: detcore_model::host_input::LaunchNamespaceMounts,
    /// Exact original package for both verification runs: inherited from the
    /// shared-input holder, or opened once by inline verification.
    #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
    #[clap(skip)]
    mapped_shared_image: Option<std::sync::Arc<super::mapped_first_admission::RetainedImage>>,

    /// The run's frozen copy of the Android system property area
    /// (`super::property_snapshot`), taken once before the guest's first exec
    /// and shared by both runs of a `--verify`; removed when the last holder
    /// drops it.
    #[clap(skip)]
    property_snapshot: Option<std::sync::Arc<super::property_snapshot::PropertySnapshotDir>>,

    /// Print a summary of the process tree's execution to stderr before exiting.
    #[clap(long, short = 'u')]
    pub(crate) summary: bool,

    /// Print a machine readable version of --summary to a file.
    ///
    /// The summary is written from inside the run container, so an absolute
    /// path the container cannot see is refused before the guest starts: one
    /// under host /tmp (unless --tmp=/tmp or a --bind exposes it) or under a
    /// --mount without a source.
    #[clap(long)]
    pub(crate) summary_json: Option<PathBuf>,

    /// Write the selected backend's own engagement evidence as JSON.
    ///
    /// This is a single-run record. Verification has separate per-attempt
    /// records and therefore cannot use one path without losing an attempt.
    #[clap(
        long,
        conflicts_with_all = ["verify", "namespace_only"],
        value_name = "PATH"
    )]
    backend_engagement_json: Option<PathBuf>,

    /// Diagnose non-zero network binds. Implies an isolated network namespace and conflicts with
    /// `--network=host`.
    #[clap(long)]
    analyze_networking: bool,

    /// The base environment that is presented to the guest. "Empty" is completely empty, and "Host"
    /// allows through all the environment variables in hermit's own environment.
    /// "Minimal" provides a minimal deterministic environment that sets PATH, HOSTNAME, HOME, USER
    /// and LOGNAME.
    ///
    /// "Minimal" sets exactly HOSTNAME=hermetic-container.local,
    /// PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin, HOME=/root, USER=root
    /// and LOGNAME=root, the same on every host, and like every instrumented guest the guest also
    /// gets ASAN_OPTIONS=detect_leaks=0 and LSAN_OPTIONS=detect_leaks=0. HOME, USER and LOGNAME
    /// name the guest's own identity: the guest's getuid() is 0, which its user database names
    /// root with home directory /root. That directory is usually the host's, which the guest
    /// cannot write; give a writable one with `--env HOME=/tmp`, and replace any of these values
    /// with `--env NAME=VALUE`.
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

    /// Load options from a run config file: YAML (or JSON) naming `hermit run` options by their
    /// long names, as `--save-config` writes it. The file's options count as given on the command
    /// line and are checked the same way. An option given on the command line replaces the
    /// file's value for that option (a repeatable option's whole list), a guest program given on
    /// the command line replaces the file's `program` and `args`, and the file's values replace
    /// the options' environment variables. For example, `{schema: hermit-run-config/v1, global:
    /// {log: info}, run: {seed: 7, env: [A=1, B=2], verify: true}, program: /bin/date}`; the
    /// format is described under "Run Configuration Files" in docs/USER_GUIDE.md. Unknown keys and any
    /// `schema` other than `hermit-run-config/v1` are refused.
    #[clap(long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Write this run's options to FILE as a run config that `--config` loads to reproduce the
    /// run: the options given on the command line, in a `--config` file or through their
    /// environment variables, plus the implicit inputs hermit resolved (an epoch taken from the
    /// host clock or from a replayed recording, and the seed `--seed-from` chose). Defaults are
    /// not written, and neither is the host environment `--base-env=host` passes through. The
    /// file is written on the host before the guest starts, so a run that fails still leaves it.
    #[clap(long, value_name = "FILE")]
    pub save_config: Option<PathBuf>,

    /// Rerun the run a manifest records: load its options and guest command as `--config` loads a
    /// run config. RECORDING is `last` (the newest `hermit record` or `--record-networking`
    /// recording), `last-run` (the newest of any kind, a plain run's manifest included), a recording ID or a unique prefix of at least 6 hex digits, or the
    /// path of a `manifest.json` or of its directory; `hermit list` lists them. Options and a guest
    /// command given on this command line replace the manifest's, so `hermit run --from last-run
    /// --sched-seed=7` reruns the newest run under another schedule. A run that draws an
    /// input from the host (an epoch from the host clock, a seed from `--seed-from=SystemRandom`)
    /// writes such a manifest under the data directory (`HERMIT_DATA_DIR`, else
    /// `~/.cache/hermit`) and names it in a diagnostic
    /// (<https://github.com/rrnewton/hermit/issues/4271>). Like a run config, a manifest does not
    /// hold the host environment `--base-env=host` passes through, the files the guest reads, or
    /// the working directory (relative paths resolve against this one), and it leaves out output
    /// destinations such as `--log-file`.
    #[clap(long, value_name = "RECORDING", conflicts_with = "config")]
    from: Option<PathBuf>,

    /// When to write a run manifest, the small recording that reruns this execution with
    /// `--from`: `nonreproducible-only` (the default) writes one only when the run draws an input
    /// from the host; `always` also writes a `history` manifest for a reproducible run; `never`
    /// writes none. Without this option, the `manifest` key of `~/.config/hermit/config.yaml`
    /// decides, then the `HERMIT_MANIFEST` environment variable.
    #[clap(long, value_name = "POLICY", value_enum)]
    manifest: Option<crate::app::manifest::ManifestPolicy>,

    /// Runtime-only: where this run's manifest was written, once it is.
    #[clap(skip)]
    manifest_path: Option<PathBuf>,

    /// Runtime-only: a network recording's manifest, written only once the
    /// run has written its trace (`finish_manifest`).
    #[clap(skip)]
    pending_network_manifest: Option<crate::app::manifest::Manifest>,

    /// Read-only overlay that exposes the rewritten ELF at its original guest path.
    #[clap(skip)]
    executable_overlay: Option<ExecutableOverlayMount>,

    /// Runtime-only: the rewritten files `--rewrite-exclusives` produced,
    /// each bind-mounted over the path the guest loads it from.
    #[clap(skip)]
    exclusive_rewrite_overlays: Vec<ExecutableOverlayMount>,

    /// Runtime-only: the guest path of every file the overlay rewrite was
    /// given, rewritten or not (the executable and each
    /// `--rewrite-exclusives-also` file), for `Config::rewrite_exclusives_overlay`.
    #[clap(skip)]
    exclusive_rewrite_covered: Vec<PathBuf>,

    /// Runtime-only: the guest path of every file `--rewrite-exclusives` left
    /// unchanged, wholly or in part, because no mapping symbol it accepts
    /// marks its code (none, or one that is neither `$x` nor `$d`), for
    /// `Config::rewrite_exclusives_unmarked`.
    #[clap(skip)]
    exclusive_rewrite_unmarked: Vec<PathBuf>,

    /// Runtime-only: how many of the last `--mount`s and `--bind`s a backend
    /// adapter added to the caller's (`with_dbt_mounts`). They are installed
    /// as a backend's own mounts (`PlannedMount::backend`), which the guest's
    /// container mount table leaves out by that identity, so the table is
    /// the same on every backend.
    #[clap(skip)]
    backend_mounts: (usize, usize),

    /// Resolved guest executable path used after e9patch preprocessing.
    #[clap(skip)]
    prepared_program: Option<PathBuf>,

    /// What `--cpu-profile auto` resolved to and why, for the diagnostic
    /// line `--log=debug` and finer show.
    #[clap(skip)]
    auto_resolution: Option<String>,

    /// Why this host's retired-branch counter is inexact, if it is. Only the
    /// `--strict` refusal and the warning without `--strict` consult it; tests
    /// replace it to reach both on a host whose counters pass.
    #[clap(skip = host_inexact_branch_counter as fn() -> Option<String>)]
    inexact_branch_counter: fn() -> Option<String>,
}

pub(super) fn parse_assignment(src: &str) -> Result<(String, Option<String>), Error> {
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

/// The whole environment `--base-env=minimal` gives the guest, and the default
/// an `--image` guest gets for each of these its image does not declare. Every
/// value is a constant, so the guest sees the same environment on every host.
///
/// HOME, USER and LOGNAME name the guest's own identity, as login(1) sets them
/// for a user: Detcore presents every guest the fixed identity uid 0
/// (`getuid()` returns 0 with or without the container's user namespace, which
/// maps the invoking user to uid 0), and the guest's `getpwuid(0)` names that
/// user `root` with home directory `/root`.
const MINIMAL_BASE_ENV: [(&str, &str); 5] = [
    ("HOSTNAME", "hermetic-container.local"),
    (
        "PATH",
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    ),
    ("HOME", "/root"),
    ("USER", "root"),
    ("LOGNAME", "root"),
];

/// Replace the guest's environment with [`MINIMAL_BASE_ENV`].
fn apply_minimal_base_environment(command: &mut Command) {
    command.env_clear();
    command.envs(MINIMAL_BASE_ENV);
}

/// Replace the guest's environment with an `--image` guest's: the image's
/// declared `Env` over [`MINIMAL_BASE_ENV`], so each value the image declares
/// replaces the default it names.
fn apply_image_base_environment(command: &mut Command, image_env: &[(String, String)]) {
    apply_minimal_base_environment(command);
    command.envs(image_env.iter().map(|(key, value)| (key, value)));
}

fn apply_base_and_explicit_environment(
    command: &mut Command,
    base_env: &BaseEnv,
    env: &[(String, Option<String>)],
) -> Result<(), Error> {
    match base_env {
        BaseEnv::Empty => {
            command.env_clear();
        }
        BaseEnv::Minimal => {
            apply_minimal_base_environment(command);
        }
        BaseEnv::Host => {
            // Hermit's own settings, not the guest's: where its verify logs go,
            // its race probes, which must not change what the guest sees, and
            // the mark on the run that `--dap-listen` supervises, so the
            // guest's environment is that of the same run without it.
            command.env_remove(super::verify::VERIFY_TMPDIR_ENV);
            command.env_remove(detcore::race_probe::RACE_PROBE_ENV);
            command.env_remove(detcore::race_probe::RACE_PROBE_SEED_ENV);
            command.env_remove(detcore::race_probe::RACE_PROBE_HOLD_DIR_ENV);
            command.env_remove(DAP_LISTEN_INNER_ENV);
        }
    }
    for (name, value) in env {
        if let Some(value) = value {
            command.env(name, value);
        } else if let Ok(value) = std::env::var(name) {
            command.env(name, value);
        } else {
            anyhow::bail!(
                "Attempt to pass through env var {}, but it is not set in the host environment",
                name
            );
        }
    }

    Ok(())
}

fn disable_sanitizer_leak_detection(command: &mut Command) {
    command.env("ASAN_OPTIONS", "detect_leaks=0");
    command.env("LSAN_OPTIONS", "detect_leaks=0");
}

pub(super) fn apply_base_environment(
    command: &mut Command,
    base_env: &BaseEnv,
    env: &[(String, Option<String>)],
) -> Result<(), Error> {
    apply_base_and_explicit_environment(command, base_env, env)?;
    disable_sanitizer_leak_detection(command);
    Ok(())
}

/// One prepared analysis's key, its overlays, the guest paths of every
/// file its overlay rewrite was given, the files it left unchanged for
/// want of mapping symbols, and a fingerprint (path, bytes digest, mode)
/// of every overlay artifact it made.
type PreparedTrialPreprocessing = (
    String,
    Vec<ExecutableOverlayMount>,
    Vec<PathBuf>,
    Vec<PathBuf>,
    Vec<(PathBuf, String, u32)>,
);
/// The overlays prepared for analyze trials in this process, by
/// [`RunOpts::trial_preprocessing_key`].
static PREPARED_TRIAL_PREPROCESSING: std::sync::Mutex<Vec<PreparedTrialPreprocessing>> =
    std::sync::Mutex::new(Vec::new());

/// Where `--rewrite-exclusives` applies its rewrite.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum RewriteExclusivesMode {
    /// `memory` under `--no-namespace`, `overlay` otherwise.
    #[default]
    Auto,
    /// Bind-mount rewritten copies of the named files over the originals.
    Overlay,
    /// Rewrite every executable file mapping in the guest's memory.
    Memory,
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

impl NetworkingMode {
    /// Apply this networking mode to a container. `run` and `record start`
    /// both build their containers through this one function, so a recording
    /// sees the same network as the run it records. `analyze_networking`
    /// forces an isolated namespace even under `host`.
    pub(crate) fn configure(self, container: &mut Container, analyze_networking: bool) {
        if self.uses_local_namespace(analyze_networking) {
            container.local_networking_only();
        }
    }

    /// The dedicated DBT launcher must use the same resolved network policy.
    pub(super) fn uses_local_namespace(self, analyze_networking: bool) -> bool {
        self == Self::Local || analyze_networking
    }
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
    pub(crate) fn satisfies(&self, status: ExitStatus) -> bool {
        match self {
            VerifyAllow::Success => status == ExitStatus::SUCCESS,
            VerifyAllow::Failure => status != ExitStatus::SUCCESS,
            VerifyAllow::Both => true,
        }
    }
}

fn describe_exit_status(status: ExitStatus) -> String {
    match status {
        ExitStatus::Exited(code) => format!("exited with code {code}"),
        ExitStatus::Signaled(signal, core_dumped) => {
            let core = if core_dumped { " (core dumped)" } else { "" };
            format!("terminated by signal {} ({signal:?}){core}", signal as i32)
        }
        ExitStatus::SignaledRaw(signal, core_dumped) => {
            let core = if core_dumped { " (core dumped)" } else { "" };
            format!("terminated by signal {signal} (SIG{signal}){core}")
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(super) enum BaseEnv {
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

/// A `--mount`: the mount, and the spec it was parsed from, so that a
/// rendered command line (`hermit analyze`'s reproducers) passes the same
/// mount again. Reverie's `Mount` does not give its fields back, and some
/// (its flags, data, pinned source) have no spec syntax.
#[derive(Clone, Debug)]
pub struct MountArg {
    mount: Mount,
    spec: String,
}

impl FromStr for MountArg {
    type Err = <Mount as FromStr>::Err;

    fn from_str(spec: &str) -> Result<Self, Self::Err> {
        Ok(Self {
            mount: spec.parse()?,
            spec: spec.to_owned(),
        })
    }
}

impl std::ops::Deref for MountArg {
    type Target = Mount;

    fn deref(&self) -> &Mount {
        &self.mount
    }
}

/// Displays the `run` subcommand's own options, so that
/// `hermit{global_backend_arg()} run{self}` is a runnable command. The backend is a
/// global option and is therefore rendered by [`RunOpts::global_backend_arg`],
/// not here.
impl fmt::Display for RunOpts {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.in_guest_exec_stop {
            write!(f, " --in-guest-exec-stop")?;
        }
        // The rewrite changes which instructions the guest executes, so a
        // reproducer without it reproduces a different program.
        if self.rewrite_exclusives {
            write!(f, " --rewrite-exclusives")?;
            match self.rewrite_exclusives_mode {
                RewriteExclusivesMode::Auto => {}
                RewriteExclusivesMode::Overlay => write!(f, " --rewrite-exclusives-mode=overlay")?,
                RewriteExclusivesMode::Memory => write!(f, " --rewrite-exclusives-mode=memory")?,
            }
            for also in &self.rewrite_exclusives_also {
                write!(
                    f,
                    " --rewrite-exclusives-also={}",
                    shell_words::quote(&also.to_string_lossy())
                )?;
            }
        }
        let mut dop = self.det_opts.det_config.clone();
        // Fail-closed is the `run` default, so serialize only the compatibility
        // opt-out. `DetConfig` is also used outside `run` and retains a neutral
        // false default, so its Display would otherwise add the explicit opt-in.
        dop.panic_on_unsupported_syscalls = false;

        if let Some(skid_margin) = self.skid_margin {
            write!(f, " --skid-margin={skid_margin}")?;
        }
        if let Some(timeout) = self.timeout {
            write!(f, " --timeout={timeout}")?;
        }
        // `--strict` refuses runs a plain one accepts and makes a signal-delivery
        // contradiction fatal (`Config::signal_delivery_check_fatal`), so a
        // reproducer without it would not end the same way.
        if self.strict {
            write!(f, " --strict")?;
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
        if self.allow_unsupported_syscalls {
            write!(f, " --allow-unsupported-syscalls")?;
        }
        // Recording switches to host networking; render the flag that asked for it.
        if let Some(path) = &self.record_networking {
            write!(
                f,
                " --record-networking={}",
                shell_words::quote(&path.to_string_lossy())
            )?;
        } else if self.network != Default::default() {
            write!(f, " --network={}", self.network)?;
        }
        if let Some(path) = &self.replay_networking {
            write!(
                f,
                " --replay-networking={}",
                shell_words::quote(&path.to_string_lossy())
            )?;
        }
        if self.namespace_only {
            write!(f, " --namespace-only")?;
        }
        if self.no_namespace {
            write!(f, " --no-namespace")?;
        }
        if let Some(base) = self.pid_base {
            write!(f, " --pid-base={base}")?;
        }
        if self.summary {
            write!(f, " --summary")?;
        }
        if let Some(p) = &self.summary_json {
            let s = p.to_str().expect("valid unicode path");
            write!(f, " --summary-json={}", shell_words::quote(s))?;
        }
        if let Some(p) = &self.backend_engagement_json {
            let s = p.to_str().expect("valid unicode path");
            write!(f, " --backend-engagement-json={}", shell_words::quote(s))?;
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
        if self.verify_strict {
            write!(f, " --verify-strict")?;
        }
        if self.print_verify_logs {
            write!(f, " --print-verify-logs")?;
        }
        if self.keep_logs {
            write!(f, " --keep-logs")?;
        }
        if let Some(p) = &self.verify_log_dir {
            let s = p.to_str().expect("valid unicode path");
            write!(f, " --verify-log-dir={}", shell_words::quote(s))?;
        }
        if let Some(p) = &self.verify_json {
            let s = p.to_str().expect("valid unicode path");
            write!(f, " --verify-json={}", shell_words::quote(s))?;
        }
        if let Some(p) = &self.verify_run {
            let s = p.to_str().expect("valid unicode path");
            write!(f, " --verify-run={}", shell_words::quote(s))?;
        }
        if let Some(fd) = self.native_exec_shared_inputs {
            write!(f, " --native-exec-shared-inputs={fd}")?;
        }
        if let Some(p) = &self.tmp {
            let s = p.to_str().expect("valid unicode path");
            write!(f, " --tmp={}", shell_words::quote(s))?;
        }
        if let Some(image) = &self.image {
            write!(f, " --image={}", shell_words::quote(image))?;
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
        if let Some(p) = &self.happens_before {
            let s = p
                .to_str()
                .expect("valid string provided to --happens-before");
            write!(f, " --happens-before={}", shell_words::quote(s))?;
        }
        if self.hb_list_events {
            write!(f, " --hb-list-events")?;
        }

        for mount in &self.mount {
            write!(f, " --mount={}", shell_words::quote(&mount.spec))?;
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

        // A seeded maximum is rendered as the flag the run was given, which
        // draws the same value from the same seed; the value beside it would
        // be refused as a conflict. Without chaos the flag draws nothing (and
        // would need `--chaos`), and a disabled maximum is rendered as such,
        // so neither renders the flag.
        if self.chaos_seeded_timeslice && dop.chaos && dop.max_timeslice.is_some() {
            write!(f, " --chaos-seeded-timeslice")?;
            dop.max_timeslice = Some(hermit::chaos_timeslice::DEFAULT_MAX_TIMESLICE);
        }

        // A reproducer must retain the actual epoch, including the stable
        // library default: an omitted CLI epoch now captures a new host time.
        // Config already emits nondefault epochs, so add only its omitted case.
        write!(f, "{}", dop)?;
        if dop.has_default_epoch() {
            write!(f, " --epoch={}", dop.epoch.to_rfc3339())?;
        }

        write!(
            f,
            " -- {}",
            shell_words::quote(
                self.program
                    .as_deref()
                    .and_then(|p| p.to_str())
                    .unwrap_or("<image-default>")
            )
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

/// Whether the emulator's own command line already asks for QEMU's
/// instruction-derived clock.
///
/// `-icount` is the remedy this advisory recommends, so a run that already
/// passes it has nothing left to act on and the advisory only adds noise.
/// Accepts both spellings QEMU takes: `-icount shift=0,sleep=off` as two
/// arguments, and `-icount=shift=0,sleep=off` as one.
fn emulator_uses_instruction_count_clock(args: &[String]) -> bool {
    args.iter()
        .any(|arg| arg == "-icount" || arg.starts_with("-icount="))
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
///
/// WHAT THIS USED TO SAY, AND WHY IT NO LONGER SAYS IT. Until 2026-08-21 the
/// message offered `--no-virtualize-time --no-virtualize-metadata` as one of two
/// remedies. Measured on 2026-08-21 with hermit `f05bf04e4f`, a busybox guest at
/// one vCPU and no `-icount`, that option changes nothing about this failure:
/// the run with it and the run without it both panic in guest timer setup and
/// both produce a 9,233-byte console. At the time, combined with `--strict` it
/// was worse than useless — strict mode rejected the unvirtualized
/// `gettimeofday` and Hermit exited 1 before the guest started (clock syscalls
/// now pass through to the host clock instead; see
/// https://github.com/rrnewton/hermit/issues/1176). Recommending it sent readers down a dead end
/// at the moment they were most likely to follow the advice, so the message now
/// names only remedies that were measured to reach a booted guest.
fn vmm_time_virtualization_warning(
    program: &Path,
    args: &[String],
    virtualize_time: bool,
) -> Option<String> {
    if virtualize_time && is_vmm_program(program) && !emulator_uses_instruction_count_clock(args) {
        Some(format!(
            "WARNING: {} looks like a hardware emulator (VMM). Hermit's host-time \
             virtualization exposes mutually inconsistent clock sources (a synthetic RDTSC \
             versus a virtualized clock_gettime) to the emulated guest, which can corrupt its \
             clock calibration (for example \"Unable to calibrate against PIT\", TSC marked \
             unstable, or \"No current clocksource\") and stall boot. Two routes were measured \
             to reach a booted guest: give the emulator a single instruction-derived clock \
             (for QEMU: -icount shift=0,sleep=off), or pass no_timer_check on the nested \
             guest's kernel command line. Note that -icount forecloses multi-threaded TCG, \
             which QEMU refuses to combine with it. See docs/QEMU_BOOT.md.",
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
        let warning = vmm_time_virtualization_warning(Path::new(program), &[], true);
        let message = warning
            .unwrap_or_else(|| panic!("expected a warning for {program} under virtual time"));
        assert!(message.contains("-icount"));
        assert!(message.contains("no_timer_check"));
    }
}

/// The advisory must not send a reader to a route that does not work.
///
/// This assertion replaces one that required the message to CONTAIN
/// `--no-virtualize-time`. That earlier assertion was not wrong when it was
/// written — it pinned the wording deliberately — but it outlived the
/// measurement, so a passing test stood between the reader and a correction.
/// The pin is kept and inverted rather than deleted: the wording is still
/// asserted, now against what the measurement supports.
#[test]
fn vmm_time_warning_does_not_recommend_disabling_virtual_time() {
    let message = vmm_time_virtualization_warning(Path::new("qemu-system-x86_64"), &[], true)
        .expect("expected a warning under virtual time");
    assert!(
        !message.contains("--no-virtualize-time"),
        "the advisory must not recommend --no-virtualize-time: measured 2026-08-21, it leaves \
         the guest panicking in timer setup with a byte-identical console, and combined with \
         --strict it aborts the run before the guest starts. Message was: {message}"
    );
    assert!(
        !message.contains("--no-virtualize-metadata"),
        "same: --no-virtualize-metadata is half of a route that does not work. Message was: \
         {message}"
    );
}

/// The second half of the defect: the advisory fired whenever virtual time was
/// on, including for runs that had already applied its own recommendation.
/// Measured before the fix: `hermit run --no-sequentialize-threads -- \
/// qemu-system-x86_64 -icount shift=0,sleep=off ...` printed the warning.
#[test]
fn vmm_time_warning_silent_when_the_emulator_already_uses_icount() {
    for args in [
        vec!["-icount".to_string(), "shift=0,sleep=off".to_string()],
        vec!["-icount=shift=0,sleep=off".to_string()],
        vec![
            "-nographic".to_string(),
            "-icount".to_string(),
            "shift=auto".to_string(),
        ],
    ] {
        assert!(
            vmm_time_virtualization_warning(Path::new("qemu-system-x86_64"), &args, true).is_none(),
            "the emulator already uses the instruction-derived clock this advisory \
             recommends, so there is nothing left to act on: {args:?}"
        );
    }
}

/// The guard on the guard: a command line that merely mentions icount in some
/// other position must still warn, or the silence above would swallow real
/// cases.
#[test]
fn vmm_time_warning_still_fires_for_arguments_that_only_resemble_icount() {
    for args in [
        vec!["-no-icount".to_string()],
        vec!["--icount".to_string()],
        vec!["-append".to_string(), "icount=1".to_string()],
        vec!["-drive".to_string(), "file=icount.qcow2".to_string()],
    ] {
        assert!(
            vmm_time_virtualization_warning(Path::new("qemu-system-x86_64"), &args, true).is_some(),
            "these do not enable QEMU's instruction-derived clock, so the advisory still \
             applies: {args:?}"
        );
    }
}

#[test]
fn vmm_time_warning_silent_without_virtual_time() {
    // Hermit's virtual clock is off, so there is no clock-domain mismatch to
    // warn about. (This is not an endorsement of --no-virtualize-time as a
    // remedy for a stalled boot; see the note on the warning function.)
    assert!(
        vmm_time_virtualization_warning(Path::new("qemu-system-x86_64"), &[], false).is_none(),
        "no warning is expected once virtual time is disabled"
    );
}

#[test]
fn vmm_time_warning_silent_for_non_vmm_programs() {
    for program in ["ls", "/bin/echo", "qemu-img", "my-qemu-wrapper"] {
        assert!(
            vmm_time_virtualization_warning(Path::new(program), &[], true).is_none(),
            "unexpected VMM warning for {program}"
        );
    }
}

/// The guest IDs the trials of this process's analysis or bisection take
/// under `--no-namespace`, once resolved (`RunOpts::prepare_trial_guest_ids`).
/// The memo has no key: it serves one analysis per process, as `hermit
/// analyze` and `hermit bisect` are, whose trials share every option that
/// numbers a guest.
static TRIAL_GUEST_IDS: std::sync::Mutex<Option<detcore_model::pid::GuestIds>> =
    std::sync::Mutex::new(None);

/// The guest IDs every trial of this process's analysis or bisection takes,
/// once resolved: `None` before that, and without `--no-namespace`. Analyze
/// stamps every record it writes for a trial with them
/// (`AnalyzeOpts::stamp_trial_record`).
pub(crate) fn resolved_trial_guest_ids() -> Option<detcore_model::pid::GuestIds> {
    #[cfg(test)]
    if let Some(ids) = TRIAL_GUEST_IDS_IN_TEST.get() {
        return Some(ids);
    }
    *TRIAL_GUEST_IDS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
thread_local! {
    /// The trial guest IDs a unit test set on its own thread
    /// ([`resolve_trial_guest_ids_in_test`]), so that tests running beside it
    /// see the process's memo as before.
    static TRIAL_GUEST_IDS_IN_TEST: std::cell::Cell<Option<detcore_model::pid::GuestIds>> =
        const { std::cell::Cell::new(None) };
}

/// Clears the trial guest IDs [`resolve_trial_guest_ids_in_test`] set when it
/// drops.
#[cfg(test)]
pub(crate) struct TrialGuestIdsInTest;

#[cfg(test)]
impl Drop for TrialGuestIdsInTest {
    fn drop(&mut self) {
        TRIAL_GUEST_IDS_IN_TEST.set(None);
    }
}

/// Makes `ids` this thread's resolved trial guest IDs, as
/// `RunOpts::prepare_trial_guest_ids` leaves them for every trial, until the
/// returned guard drops. Resolving them for real probes the host
/// (`guest_ids::may_choose_ids`), which a unit test cannot rely on.
#[cfg(test)]
#[must_use]
pub(crate) fn resolve_trial_guest_ids_in_test(
    ids: detcore_model::pid::GuestIds,
) -> TrialGuestIdsInTest {
    TRIAL_GUEST_IDS_IN_TEST.set(Some(ids));
    TrialGuestIdsInTest
}

/// The guest IDs every trial of an analysis or bisection under
/// `--no-namespace` uses, from those its first trial resolved: chosen IDs,
/// with any task that cannot have its own refused rather than given a host ID
/// that no replay could follow. Host IDs are refused.
fn trial_guest_ids(
    resolved: detcore_model::pid::GuestIds,
) -> Result<detcore_model::pid::GuestIds, String> {
    use detcore_model::pid::GuestIds;
    match resolved {
        GuestIds::Chosen { base, .. } => Ok(GuestIds::Chosen {
            base,
            refuse_host_ids: true,
        }),
        other => Err(format!(
            "`hermit analyze` and `hermit bisect` under --no-namespace need guest process IDs \
             that Hermit chooses, because their trials replay one another's records, and this \
             trial's guest would be numbered {other}; the warning above names why Hermit could \
             not choose them (no permission to, or no free window of IDs). Run as a user that \
             may choose process IDs (CAP_CHECKPOINT_RESTORE or CAP_SYS_ADMIN) where a window of \
             IDs is free, or with a PID namespace"
        )),
    }
}

#[test]
fn trials_under_no_namespace_take_only_chosen_ids_and_refuse_host_ones() {
    use detcore_model::pid::GuestIds;
    for refuse_host_ids in [false, true] {
        assert_eq!(
            trial_guest_ids(GuestIds::Chosen {
                base: 31744,
                refuse_host_ids
            }),
            Ok(GuestIds::Chosen {
                base: 31744,
                refuse_host_ids: true
            })
        );
    }
    for other in [GuestIds::Host, GuestIds::Namespace] {
        let refusal = trial_guest_ids(other).expect_err("only chosen IDs replay");
        assert!(refusal.contains("CAP_CHECKPOINT_RESTORE"), "{refusal}");
    }
}

/// Why a `--no-namespace` replay of a record whose guest numbering is
/// `recorded` (named by `source`, an option and a path) is refused, when this
/// run's numbering is `replay` (`None` before it is resolved), if it is. A
/// replay follows a record only where Hermit chooses the guest's IDs and
/// numbers them alike: never a record whose IDs came from the host, nor on a
/// run whose own IDs Hermit does not choose (KVM and DBT number the guest
/// themselves; without sequentialized threads the host does).
fn replay_guest_ids_refusal(
    source: &str,
    recorded: detcore_model::pid::GuestIds,
    replay: Option<detcore_model::pid::GuestIds>,
) -> Option<String> {
    use detcore_model::pid::GuestIds;
    match (recorded, replay) {
        (GuestIds::Host, _) => Some(format!(
            "{source} was recorded with guest process IDs {recorded}. The record names every \
             thread by its ID, so no replay can follow it. Record the run again under \
             --no-namespace with --pid-base or --strict, which refuse a run whose IDs come from \
             the host"
        )),
        (_, None) => None,
        (_, Some(replay @ (GuestIds::Namespace | GuestIds::Host))) => Some(format!(
            "{source} cannot be replayed under --no-namespace here: a --no-namespace replay \
             follows a record only where Hermit chooses the guest's process IDs (the ptrace \
             backend with sequentialized threads), and this run's would be {replay}"
        )),
        (recorded, Some(replay)) => {
            detcore::preemptions::guest_ids_mismatch(source, recorded, replay)
        }
    }
}

#[test]
fn no_namespace_replays_follow_only_records_whose_ids_were_chosen_alike() {
    use detcore_model::pid::GuestIds;
    let chosen = |base| GuestIds::Chosen {
        base,
        refuse_host_ids: true,
    };
    let refused = |recorded, replay| replay_guest_ids_refusal("rec", recorded, replay);
    // A host-numbered record is refused before anything is resolved, and
    // whatever the replay would be.
    for replay in [None, Some(GuestIds::Host), Some(chosen(5000))] {
        let refusal = refused(GuestIds::Host, replay).expect("a host-numbered record");
        assert!(refusal.contains("no replay can follow it"), "{refusal}");
    }
    assert_eq!(refused(chosen(5000), None), None);
    assert_eq!(refused(chosen(5000), Some(chosen(5000))), None);
    assert!(refused(chosen(5000), Some(chosen(6000))).is_some());
    // A namespace (or older) record, under a run whose IDs Hermit chooses.
    assert!(refused(GuestIds::Namespace, Some(chosen(5000))).is_some());
    // Runs whose IDs Hermit does not choose follow nothing.
    for replay in [GuestIds::Namespace, GuestIds::Host] {
        for recorded in [GuestIds::Namespace, chosen(5000)] {
            let refusal = refused(recorded, Some(replay)).expect("not chosen here");
            assert!(refusal.contains("only where Hermit chooses"), "{refusal}");
        }
    }
}

/// Parse `hermit [GLOBAL...] run [RUN...]` through the real top-level parser and
/// apply the global `--backend` the way [`RunOpts::main`] does. `run` itself does
/// not accept `--backend`; see `cli.rs::run_rejects_subcommand_level_backend`.
#[cfg(test)]
fn run_opts_for(argv: &[&str]) -> RunOpts {
    use clap::CommandFactory;
    use clap::FromArgMatches;
    let schema = crate::app::Args::command();
    let raw = argv
        .iter()
        .map(std::ffi::OsString::from)
        .collect::<Vec<_>>();
    // Use the shipped executable-selector/config parser. This is not a
    // local argument-removal shim; guest operands retain their original bytes.
    let application = crate::app::run_config::external_selection(&schema, &raw)
        .unwrap()
        .map(|selection| match selection.backend.as_str() {
            "dbt" => ("dbt", reverie::BackendCapabilities::DBT),
            "sabre" => ("sabre", reverie::BackendCapabilities::SABRE),
            "e9patch" => ("e9patch", reverie::BackendCapabilities::PTRACE),
            name => panic!("no declared test adapter capabilities for {name}"),
        });
    let parsed = application.map(|(name, _)| {
        crate::app::run_config::application_arguments(&schema, &raw, name)
            .unwrap()
            .0
    });
    let matches = schema
        .try_get_matches_from(parsed.as_deref().unwrap_or(&raw))
        .unwrap_or_else(|error| panic!("{argv:?} should parse: {error}"));
    let args = crate::app::Args::from_arg_matches(&matches).unwrap();
    let crate::app::Subcommand::Run(mut run) = args.command else {
        panic!("{argv:?} is not a `run` command");
    };
    run.capture_max_timeslice_source(matches.subcommand_matches("run").unwrap());
    run.backend = args.global.backend;
    if let Some((name, capabilities)) = application {
        run.select_application_backend(name).unwrap();
        run.adapter_capabilities = Some(capabilities);
    }
    *run
}

#[test]
fn application_admission_refuses_missing_immutable_capabilities_before_launch() {
    let directory = tempfile::tempdir().unwrap();
    let recording = directory.path().join("checkpoint-settings.json");
    let guest = directory.path().join("nonexistent-admission-guest");
    // Reuse the complete checkpoint envelope from the CLI admission fixture.
    let history = serde_json::json!({
        "global": [],
        "per_thread": {
            "3": {
                "final_prio": 1,
                "prio_changes": [[1_767_225_600_000_001_000u64, 2]],
                "preemption_rcbs": [100],
                "chaos_epochs": []
            }
        },
        "epoch": "2026-01-01T00:00:00Z",
        "signal_state_copy": true,
        "callback_checkpoints": {
            "3": [{
                "history_index": 0,
                "origin": {
                    "callback": "PmuTimer",
                    "site": "Prehook",
                    "progress": 0,
                    "last_op": null,
                    "time": 1_767_225_600_000_001_000u64,
                    "rcbs": 100
                }
            }]
        }
    });
    fs::write(&recording, serde_json::to_vec(&history).unwrap()).unwrap();
    let admitted = run_opts_for(&[
        "hermit",
        "--backend=e9patch",
        "run",
        "--chaos",
        "--sequentialize-threads",
        "--max-timeslice=1000000",
        "--epoch=2026-01-01T00:00:00Z",
        "--replay-preemptions-from",
        recording.to_str().unwrap(),
        "--",
        guest.to_str().unwrap(),
    ]);
    assert_eq!(
        admitted.adapter_capabilities,
        Some(reverie::BackendCapabilities::PTRACE)
    );
    let before = format!("{:#?}", admitted.det_opts.det_config);
    let mut missing = admitted.clone();
    missing.adapter_capabilities = None;
    let validation_error = missing.validate_args_with_perf_support(true).unwrap_err();
    let annotated_error = missing.annotated_priority_replay_admission().unwrap_err();
    for (phase, error) in [
        ("argument validation", validation_error),
        ("annotated replay admission", annotated_error),
    ] {
        assert!(
            error.downcast_ref::<PolicyRefusal>().is_some(),
            "{phase}: {error:#}"
        );
        assert!(
            error
                .to_string()
                .contains("missing immutable physical capabilities"),
            "{phase}: {error:#}"
        );
    }
    assert_eq!(format!("{:#?}", missing.det_opts.det_config), before);
    admitted.resolve_preemption_settings().unwrap();
    assert!(admitted.annotated_priority_replay_admission().unwrap());
    assert_eq!(format!("{:#?}", admitted.det_opts.det_config), before);
    assert_eq!(admitted.program_path(), guest);
    assert!(
        !guest.exists(),
        "admission must not start the nonexistent guest"
    );
}

#[cfg(test)]
fn assert_exec_stop_not_ready(error: &Error) {
    assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
    assert!(
        error.to_string().contains("exec-stop-runtime-not-ready"),
        "{error:#}"
    );
}

#[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
#[test]
fn exec_stop_admission_reads_the_global_backend_before_launch() {
    use clap::Parser;
    for (backend, verify, accepted) in [
        ("in-guest-trap", false, true),
        ("ptrace", false, false),
        ("in-guest-trap", true, false),
    ] {
        let backend_arg = format!("--backend={backend}");
        let mut argv = vec!["hermit", &backend_arg, "run", "--in-guest-exec-stop"];
        if verify {
            argv.push("--verify");
        }
        argv.extend(["--", "/bin/true"]);
        let args = crate::app::Args::try_parse_from(argv).unwrap();
        let crate::app::Subcommand::Run(mut run) = args.command else {
            unreachable!()
        };
        assert!(run.backend.is_none(), "backend is a global CLI input");
        run.set_backend(args.global.backend);
        // Availability is explicit DATA here: no installed package, mutable
        // environment, executable image or Ready state is needed by this test.
        for package_available in [false, true] {
            assert_eq!(
                run.exec_stop_direct_domain(package_available),
                accepted && package_available,
                "backend={backend} verify={verify} package_available={package_available}"
            );
        }
    }
}

#[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
#[test]
fn exec_stop_verification_admits_both_shared_comparators_without_clearing_flags() {
    for strict in [false, true] {
        let mut argv = vec![
            "hermit",
            "--backend=in-guest-trap",
            "run",
            "--in-guest-exec-stop",
            "--verify",
        ];
        if strict {
            argv.push("--verify-strict");
        }
        argv.extend(["--", "/bin/true"]);
        let options = run_opts_for(&argv);
        assert!(options.verify);
        assert_eq!(options.verify_strict, strict);
        assert!(!options.exec_stop_direct_domain(true));
        assert!(options.exec_stop_verification_domain(true));
        options
            .refuse_unready_exec_stop_with_availability(true)
            .unwrap();
        assert_exec_stop_not_ready(
            &options
                .refuse_unready_exec_stop_with_availability(false)
                .unwrap_err(),
        );
        assert_eq!(
            options.verification_strictness(),
            if strict {
                LogCompareStrictness::Canonical
            } else {
                LogCompareStrictness::Stripped
            }
        );
    }
}

#[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
#[test]
fn exec_stop_verification_preserves_unsupported_domain_refusals() {
    // Clap rejects some of these combinations too. Check the runtime guard
    // independently for constructed callers without relaxing those conflicts.
    for forbidden in ["no-namespace", "namespace-only", "strace-only", "debugger"] {
        let mut options = run_opts_for(&[
            "hermit",
            "--backend=in-guest-trap",
            "run",
            "--in-guest-exec-stop",
            "--verify",
            "--",
            "/bin/true",
        ]);
        match forbidden {
            "no-namespace" => options.no_namespace = true,
            "namespace-only" => options.namespace_only = true,
            "strace-only" => options.strace_only = true,
            "debugger" => options.dap_listen = Some("127.0.0.1:0".to_owned()),
            _ => unreachable!(),
        }
        assert_exec_stop_not_ready(
            &options
                .refuse_unready_exec_stop_with_availability(true)
                .unwrap_err(),
        );
    }
    let mut split = run_opts_for(&[
        "hermit",
        "--backend=in-guest-trap",
        "run",
        "--in-guest-exec-stop",
        "--verify-run=/tmp/mapped-split-refusal",
        "--",
        "/bin/true",
    ]);
    assert_exec_stop_not_ready(
        &split
            .refuse_unready_exec_stop_with_availability(true)
            .unwrap_err(),
    );
    split.native_exec_shared_inputs = Some(197);
    assert!(split.exec_stop_verification_domain(true));
    assert!(!split.exec_stop_verification_domain(false));
    // Option admission alone is not adoption or image validation. The real
    // availability check still refuses this constructed run with no image.
    assert_exec_stop_not_ready(&split.refuse_unready_exec_stop().unwrap_err());
    split.epoch_omitted = true;
    let epoch_error = split.refuse_unready_exec_stop().unwrap_err();
    assert!(epoch_error.is::<PolicyRefusal>());
    assert!(
        format!("{epoch_error:#}")
            .contains("mapped-shared-verification-requires-one-explicit-pair-epoch")
    );
    split.epoch_omitted = false;
    split.verify = true; // An in-memory caller cannot combine the two domains.
    assert_exec_stop_not_ready(
        &split
            .refuse_unready_exec_stop_with_availability(true)
            .unwrap_err(),
    );
    let mut debugger = run_opts_for(&[
        "hermit",
        "--backend=in-guest-trap",
        "run",
        "--in-guest-exec-stop",
        "--verify",
        "--",
        "/bin/true",
    ]);
    debugger.det_opts.det_config.gdbserver = true;
    assert_exec_stop_not_ready(
        &debugger
            .refuse_unready_exec_stop_with_availability(true)
            .unwrap_err(),
    );
}

#[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
#[test]
fn admitted_exec_stop_captures_one_effective_epoch() {
    let mut run = run_opts_for(&[
        "hermit",
        "--backend=in-guest-trap",
        "run",
        "--in-guest-exec-stop",
        "--",
        "/bin/true",
    ]);
    let admitted = run.refuse_unready_exec_stop_with_availability(true).is_ok();
    assert!(admitted);
    let calls = std::cell::Cell::new(0);
    run.capture_default_epoch_with_admission(
        || {
            calls.set(calls.get() + 1);
            SystemTime::UNIX_EPOCH + Duration::from_secs(1)
        },
        admitted,
    );
    assert_eq!(calls.get(), 1);
    assert_eq!(
        run.epoch_capture_for_test(),
        ("1970-01-01T00:00:01+00:00".to_owned(), true)
    );
    assert!(run.epoch_omitted);
    assert!(!run.exec_stop_epoch_refused);
    run.refuse_unready_exec_stop_with_availability(true)
        .unwrap();
    assert_eq!(
        run.clone().epoch_capture_for_test(),
        run.epoch_capture_for_test()
    );
    assert_eq!(calls.get(), 1);
}

#[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
#[test]
fn pending_exec_stop_epoch_cannot_be_admitted_by_later_package_appearance() {
    let mut run = run_opts_for(&[
        "hermit",
        "--backend=in-guest-trap",
        "run",
        "--in-guest-exec-stop",
        "--",
        "/bin/true",
    ]);
    let admitted = run
        .refuse_unready_exec_stop_with_availability(false)
        .is_ok();
    assert!(!admitted);
    run.capture_default_epoch_with_admission(
        || panic!("a pending opt-in request must not sample the host clock"),
        admitted,
    );
    assert!(run.epoch_omitted);
    assert!(!run.epoch_captured_from_host);
    assert!(run.exec_stop_epoch_refused);
    for mut options in [run.clone(), run] {
        // The ordinary domain would now accept the available package, but this
        // invocation never resolved its omitted epoch and remains refused.
        assert!(options.exec_stop_direct_domain(true));
        assert_exec_stop_not_ready(
            &options
                .refuse_unready_exec_stop_with_availability(true)
                .unwrap_err(),
        );
        let admitted = options
            .refuse_unready_exec_stop_with_availability(true)
            .is_ok();
        options.capture_default_epoch_with_admission(
            || panic!("retrying a pending request must not sample a later epoch"),
            admitted,
        );
        assert!(options.exec_stop_epoch_refused);
        assert!(!options.epoch_captured_from_host);
    }
}

/// A reproducer that drops the rewrite flags reproduces a different program,
/// because the guest's instructions are not the same ones.
/// An exclusive-loop overlay is reported as rewritten only from inside the
/// container, once mounted: the line printed when the copy is prepared says
/// planned, and a plan whose mount never happens has no other line.
#[test]
fn an_exclusive_rewrite_overlay_reports_rewritten_only_once_mounted() {
    let mut report = hermit::aarch64_lse::elf::FileReport {
        exclusive_instructions: 28,
        ..Default::default()
    };
    report.patched.insert("rmw", 5);
    report.patched.insert("swap", 4);
    let planned = exclusive_rewrite_planned_line(Path::new("/bin/guest"), &report);
    assert!(planned.contains("planned=9"), "{planned}");
    assert!(
        planned.contains("applied when the container mounts the copy"),
        "{planned}"
    );
    assert!(!planned.contains("rewritten"), "{planned}");
    // A file with nothing to rewrite gets no copy, and no promise of one.
    let nothing = hermit::aarch64_lse::elf::FileReport::default();
    let planned = exclusive_rewrite_planned_line(Path::new("/bin/plain"), &nothing);
    assert!(planned.contains("planned=0"), "{planned}");
    assert!(planned.contains("no copy is mounted"), "{planned}");
    let overlays = [
        ExecutableOverlayMount {
            source: PathBuf::from("/cache/guest"),
            target: PathBuf::from("/bin/guest"),
            rewritten: Some(9),
        },
        // e9patch's overlay rewrites no exclusive loop.
        ExecutableOverlayMount {
            source: PathBuf::from("/cache/patched-echo"),
            target: PathBuf::from("/bin/echo"),
            rewritten: None,
        },
    ];
    assert_eq!(
        exclusive_rewrite_applied_lines(&overlays),
        [":: Exclusive-loop rewrite applied: /bin/guest: rewritten=9, mounted over the original"]
    );
    assert!(exclusive_rewrite_applied_lines(&[]).is_empty());
}

/// An analyze trial is refused where `--rewrite-exclusives` cannot apply even
/// when its key is already in the prepared-overlay cache: the cache holds
/// artifacts, not permission.
#[test]
fn analyze_trials_are_refused_before_the_rewrite_cache_is_consulted() {
    for (args, refusal) in [
        (
            &[
                "--no-namespace",
                "--no-sequentialize-threads",
                "--rewrite-exclusives",
            ][..],
            "under --no-sequentialize-threads",
        ),
        (
            &[
                "--namespace-only",
                "--rewrite-exclusives",
                "--rewrite-exclusives-mode=memory",
            ][..],
            "which --namespace-only does not run",
        ),
        (
            &[
                "--no-namespace",
                "--rewrite-exclusives",
                "--rewrite-exclusives-mode=overlay",
            ][..],
            "has no mount namespace to carry them",
        ),
        (
            &["--image=example.invalid/guest", "--rewrite-exclusives"][..],
            "which those mounts do not reach",
        ),
    ] {
        let mut argv = vec!["hermit", "run"];
        argv.extend_from_slice(args);
        argv.extend_from_slice(&["--", "/nonexistent/guest"]);
        let mut trial = run_opts_for(&argv);
        PREPARED_TRIAL_PREPROCESSING
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((
                trial.trial_preprocessing_key(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ));
        let error = trial
            .prepare_trial_preprocessing()
            .expect_err("a cached key must not admit a refused trial");
        assert!(
            format!("{error:#}").contains(refusal),
            "{args:?}: {error:#}"
        );
    }
}

#[test]
fn the_exclusive_rewrite_is_retained_in_reproducers() {
    let mut plain = run_opts_for(&["hermit", "run", "--", "/nonexistent/guest"]);
    plain.validate_args_with_perf_support(true).unwrap();
    assert!(!plain.to_string().contains("--rewrite-exclusives"));

    let mut rewritten = run_opts_for(&[
        "hermit",
        "run",
        "--rewrite-exclusives",
        "--rewrite-exclusives-also=/lib/a space.so",
        "--",
        "/nonexistent/guest",
    ]);
    rewritten.validate_args_with_perf_support(true).unwrap();
    let rendered = rewritten.to_string();
    assert!(rendered.contains(" --rewrite-exclusives"), "{rendered}");
    assert!(
        rendered.contains(" --rewrite-exclusives-also='/lib/a space.so'"),
        "a path with a space must come back shell-quoted: {rendered}"
    );
    // A namespaced run overlays files; Detcore does not rewrite memory.
    assert!(
        !rewritten
            .effective_det_config()
            .rewrite_exclusives_in_memory
    );
    assert!(
        !rendered.contains("--rewrite-exclusives-mode"),
        "{rendered}"
    );

    // Under --no-namespace the default is the in-memory rewrite, which the
    // reproducer carries through --no-namespace itself; an explicit mode is
    // carried as given.
    for (args, in_memory, mode) in [
        (&["--no-namespace", "--rewrite-exclusives"][..], true, None),
        (
            &["--rewrite-exclusives", "--rewrite-exclusives-mode=memory"][..],
            true,
            Some(" --rewrite-exclusives-mode=memory"),
        ),
        (
            &["--rewrite-exclusives", "--rewrite-exclusives-mode=overlay"][..],
            false,
            Some(" --rewrite-exclusives-mode=overlay"),
        ),
    ] {
        let mut argv = vec!["hermit", "run"];
        argv.extend_from_slice(args);
        argv.extend(["--", "/nonexistent/guest"]);
        let mut opts = run_opts_for(&argv);
        opts.validate_args_with_perf_support(true).unwrap();
        assert_eq!(
            opts.effective_det_config().rewrite_exclusives_in_memory,
            in_memory,
            "{args:?}"
        );
        let rendered = opts.to_string();
        assert!(rendered.contains(" --rewrite-exclusives"), "{rendered}");
        if let Some(mode) = mode {
            assert!(rendered.contains(mode), "{rendered}");
        }
    }
}

/// Every path that would prepare the rewrite refuses an image run: the
/// preparation itself, which each of them reaches
/// (`prepare_exclusive_rewrite`, also through an analyze trial's
/// `prepare_trial_preprocessing`), and `RunOpts::run`, the entry a library
/// caller can use without validating anything.
#[test]
fn the_exclusive_rewrite_is_refused_at_every_entry_that_would_prepare_it() {
    let refusal = "a guest run from an OCI image (--image, hermit oci run)";
    let image = run_opts_for(&[
        "hermit",
        "run",
        "--image=example.invalid/guest",
        "--rewrite-exclusives",
        "--",
        "/nonexistent/guest",
    ]);

    let error = image
        .clone()
        .prepare_exclusive_rewrite(Backend::Ptrace)
        .unwrap_err();
    assert!(format!("{error:#}").contains(refusal), "{error:#}");

    // And so does an analyze trial's preparation, which reaches it.
    let error = image
        .clone()
        .prepare_trial_preprocessing()
        .expect_err("a trial's preparation must be refused too");
    assert!(format!("{error:#}").contains(refusal), "{error:#}");

    let global = GlobalOpts::parse_from(["hermit"]);
    let error = image
        .run(&global, false)
        .expect_err("a library caller's run must be refused too");
    assert!(format!("{error:#}").contains(refusal), "{error:#}");

    // Without the rewrite, none of them is refused for it.
    let plain = run_opts_for(&[
        "hermit",
        "run",
        "--image=example.invalid/guest",
        "--",
        "/nonexistent/guest",
    ]);
    // (The preparation still refuses this x86_64 host for want of FEAT_LSE,
    // which is a different refusal.)
    if let Err(error) = plain.clone().prepare_exclusive_rewrite(Backend::Ptrace) {
        assert!(!format!("{error:#}").contains(refusal), "{error:#}");
    }
    if let Err(error) = plain.run(&global, false) {
        assert!(!format!("{error:#}").contains(refusal), "{error:#}");
    }
}

/// An OCI image run does not mount the rewritten copies, so the rewrite is
/// refused in argument validation, by name, before the image is resolved;
/// without the rewrite the same image run is not refused for it.
#[test]
fn the_exclusive_rewrite_is_refused_for_an_image_run() {
    let refusal = "a guest run from an OCI image (--image, hermit oci run)";
    let mut image = run_opts_for(&[
        "hermit",
        "run",
        "--image=example.invalid/guest",
        "--rewrite-exclusives",
        "--",
        "/nonexistent/guest",
    ]);
    let error = image.validate_args_with_perf_support(true).unwrap_err();
    assert!(format!("{error:#}").contains(refusal), "{error:#}");

    let mut plain = run_opts_for(&[
        "hermit",
        "run",
        "--image=example.invalid/guest",
        "--",
        "/nonexistent/guest",
    ]);
    if let Err(error) = plain.validate_args_with_perf_support(true) {
        assert!(!format!("{error:#}").contains(refusal), "{error:#}");
    }
}

/// Trials reuse prepared overlays only for the same inputs: another
/// `--tmp`, `--workdir`, `--image` or guest environment, or a file whose
/// bytes changed, is another preparation.
#[test]
fn trial_preparations_are_keyed_by_every_input_they_read() {
    let directory = tempfile::tempdir().unwrap();
    let library = directory.path().join("liblse.so");
    std::fs::write(&library, b"first").unwrap();
    let also = format!("--rewrite-exclusives-also={}", library.display());
    let key = |extra: &[&str]| {
        let mut argv = vec!["hermit", "run", "--rewrite-exclusives", &also];
        argv.extend_from_slice(extra);
        argv.extend_from_slice(&["--", "/nonexistent/guest"]);
        run_opts_for(&argv).trial_preprocessing_key()
    };
    let plain = key(&[]);
    assert_eq!(key(&[]), plain, "the same inputs share one preparation");
    for other in [
        "--tmp=/var/tmp/elsewhere",
        "--workdir=/elsewhere",
        "--image=example.invalid/guest",
        "--env=PATH=/elsewhere",
        "--base-env=empty",
    ] {
        assert_ne!(key(&[other]), plain, "{other}");
    }
    std::fs::write(&library, b"second").unwrap();
    let changed = key(&[]);
    assert_ne!(changed, plain, "a file whose bytes changed");

    // The copy carries the source's permissions, so a changed mode is a
    // different preparation even with the same bytes.
    let mut mode = std::fs::metadata(&library).unwrap().permissions();
    mode.set_mode(mode.mode() ^ 0o111);
    std::fs::set_permissions(&library, mode).unwrap();
    assert_ne!(key(&[]), changed, "a file whose mode changed");

    // And so is another cache: the copies the key stands for live there.
    // SAFETY: single-threaded test; the variable is restored below.
    let home = std::env::var_os("XDG_CACHE_HOME");
    unsafe { std::env::set_var("XDG_CACHE_HOME", directory.path()) };
    let elsewhere = key(&[]);
    match home {
        Some(home) => unsafe { std::env::set_var("XDG_CACHE_HOME", home) },
        None => unsafe { std::env::remove_var("XDG_CACHE_HOME") },
    }
    assert_ne!(elsewhere, key(&[]), "another cache directory");
}

/// A memoized trial artifact is reused only while it still holds the bytes
/// and mode its preparation recorded: deletion or replacement between
/// preparations (with the original inputs unchanged) makes it stale, so
/// the next preparation prepares again instead of mounting the path.
#[test]
fn a_stale_prepared_overlay_is_not_reused() {
    let directory = tempfile::tempdir().unwrap();
    let artifact = directory.path().join("guest");
    std::fs::write(&artifact, b"the rewritten guest").unwrap();
    let overlays = vec![ExecutableOverlayMount {
        source: artifact.clone(),
        target: PathBuf::from("/guest"),
        rewritten: None,
    }];
    let fingerprints = vec![{
        let (digest, mode) = overlay_fingerprint(&artifact).unwrap();
        (artifact.clone(), digest, mode)
    }];
    assert!(prepared_overlays_current(&overlays, &fingerprints));

    // Replaced bytes at the same path are not the preparation's artifact.
    std::fs::write(&artifact, b"someone else's bytes").unwrap();
    assert!(!prepared_overlays_current(&overlays, &fingerprints));

    // Nor is a missing artifact.
    std::fs::remove_file(&artifact).unwrap();
    assert!(!prepared_overlays_current(&overlays, &fingerprints));

    // The same bytes under another mode are not it either.
    std::fs::write(&artifact, b"the rewritten guest").unwrap();
    let mut mode = std::fs::metadata(&artifact).unwrap().permissions();
    mode.set_mode(mode.mode() ^ 0o111);
    std::fs::set_permissions(&artifact, mode).unwrap();
    assert!(!prepared_overlays_current(&overlays, &fingerprints));
}

#[test]
fn exec_stop_opt_in_is_uniform_and_retained_in_reproducers() {
    let mut ordinary = run_opts_for(&["hermit", "run", "--", "/nonexistent/guest"]);
    assert!(!ordinary.in_guest_exec_stop);
    ordinary.refuse_unready_exec_stop().unwrap();
    ordinary.validate_args_with_perf_support(true).unwrap();
    assert!(!ordinary.to_string().contains("--in-guest-exec-stop"));

    for backend in <Backend as clap::ValueEnum>::value_variants() {
        let backend_arg = format!("--backend={}", backend.as_str());
        let options = run_opts_for(&[
            "hermit",
            &backend_arg,
            "run",
            "--in-guest-exec-stop",
            "--",
            "/nonexistent/guest",
        ]);
        assert!(options.in_guest_exec_stop);
        assert_exec_stop_not_ready(&options.refuse_unready_exec_stop().unwrap_err());
        // Display consumes a normalized configuration. A pending launch is
        // deliberately refused before normalization; test its serialization
        // separately, without bypassing the admission exercised above.
        let mut rendered =
            run_opts_for(&["hermit", &backend_arg, "run", "--", "/nonexistent/guest"]);
        rendered.validate_args_with_perf_support(true).unwrap();
        rendered.in_guest_exec_stop = true;
        assert!(rendered.to_string().contains("--in-guest-exec-stop"));
    }
}

#[test]
fn exec_stop_direct_entries_refuse_before_launch_work() {
    let args = crate::app::Args::try_parse_from([
        "hermit",
        "run",
        "--in-guest-exec-stop",
        "--dap-listen=127.0.0.1:0",
        "--",
        "/nonexistent/guest",
    ])
    .unwrap();
    let crate::app::Subcommand::Run(mut options) = args.command else {
        unreachable!()
    };
    // main must refuse before it can spawn the DAP supervisor; validation
    // must refuse before it probes timers, and run before it creates a container.
    assert_exec_stop_not_ready(&options.main(&args.global).unwrap_err());
    assert_exec_stop_not_ready(&options.validate_args().unwrap_err());
    for timer_available in [false, true] {
        assert_exec_stop_not_ready(
            &options
                .validate_args_with_perf_support(timer_available)
                .unwrap_err(),
        );
    }
    assert_exec_stop_not_ready(&options.run(&args.global, false).unwrap_err());
    let container_result = options.launch_in_container(
        (),
        "exec-stop refusal test",
        None,
        |_, _, _| -> Result<(), Error> { panic!("flagged request reached container work") },
    );
    assert_exec_stop_not_ready(&container_result.unwrap_err());
    let verification_result = options.verify_with_execution(|_, _, _, _, _| {
        panic!("flagged request reached verification execution")
    });
    assert_exec_stop_not_ready(&verification_result.unwrap_err());
}

/// A version-2 happens-before spec needs an effective preemption timer.
/// Validation can turn a requested timer off when the host cannot arm it; the
/// check right after validation must then refuse the run, naming the spec and
/// the reason, while a supported timer and a version-1 spec are not refused.
#[test]
fn a_version_2_spec_is_refused_when_validation_turns_the_timer_off() {
    let dir = tempfile::tempdir().unwrap();
    let v2 = dir.path().join("v2.json");
    let v1 = dir.path().join("v1.json");
    std::fs::write(
        &v2,
        r#"{"version": 2, "events": {"w": {"thread": "3", "syscall": "write", "fd": 1}}}"#,
    )
    .unwrap();
    std::fs::write(
        &v1,
        r#"{"version": 1, "events": {"w": {"thread": "3", "syscall": "write", "fd": 1}}}"#,
    )
    .unwrap();
    let after_validation = |spec: &Path, timer: Result<(), TimerUnavailable>| {
        let mut ro = run_opts_for(&[
            "hermit",
            "run",
            "--happens-before",
            spec.to_str().unwrap(),
            "fakeprog",
        ]);
        assert!(
            ro.det_opts.det_config.max_timeslice.is_some(),
            "the default requests a timer"
        );
        ro.validate_args_with_timer_support(timer).unwrap();
        let refusal = ro.refuse_happens_before_without_preemption();
        (ro.det_opts.det_config.max_timeslice.is_some(), refusal)
    };

    let (timer, refusal) = after_validation(&v2, Err(TimerUnavailable::NoPerfEvents));
    assert!(!timer);
    let error = refusal.expect_err("no perf counters");
    assert!(
        error
            .downcast_ref::<super::container::PolicyRefusal>()
            .is_some(),
        "{error:#}"
    );
    let message = format!("{error:#}");
    assert!(
        message.contains("HERMIT_HB_HOLD_WITHOUT_PREEMPTION: happens-before spec ")
            && message.contains(v2.to_str().unwrap())
            && message.contains("is version 2")
            && message.contains("this host cannot arm it: perf_event_open is unavailable"),
        "{message}"
    );

    let profile = "Unsupported CPU family 0x6, model 0x1: Reverie has no performance-counter \
                   profile for it";
    let (timer, refusal) =
        after_validation(&v2, Err(TimerUnavailable::NoPmuProfile(profile.to_owned())));
    assert!(!timer);
    let message = format!("{:#}", refusal.expect_err("no PMU profile"));
    assert!(
        message.contains(&format!("this host cannot arm it: {profile}")),
        "{message}"
    );

    let (timer, refusal) = after_validation(&v2, Ok(()));
    assert!(timer);
    refusal.expect("a supported timer is not refused");

    let (timer, refusal) = after_validation(&v1, Err(TimerUnavailable::NoPerfEvents));
    assert!(!timer);
    refusal.expect("a version-1 spec has no budget and runs without the timer");
}

/// The launch-time happens-before refusal keys on the runtime backend:
/// ptrace enforces count and syscall-occurrence anchors, so neither is refused
/// there. KVM and the in-guest backends do not, so both are refused.
#[test]
fn happens_before_refusal_accepts_ptrace_and_refuses_in_guest_backends() {
    let dir = tempfile::tempdir().unwrap();
    let spec = dir.path().join("spec.json");
    let spec_arg = spec.to_str().unwrap();
    for (name, json) in [
        (
            "count",
            r#"{"version": 1, "events": {"c": {"thread": "3", "syscalls": 5}}}"#,
        ),
        (
            "occurrence",
            r#"{"version": 1, "events": {"w": {"thread": "3", "syscall": "write", "fd": 1}}}"#,
        ),
    ] {
        std::fs::write(&spec, json).unwrap();
        let check = |backend: Option<&str>| {
            let mut argv = vec!["hermit"];
            if let Some(backend) = backend {
                argv.extend(["--backend", backend]);
            }
            argv.extend(["run", "--happens-before", spec_arg, "fakeprog"]);
            run_opts_for(&argv).refuse_unenforceable_happens_before()
        };
        for backend in [None, Some("ptrace")] {
            if let Err(error) = check(backend) {
                panic!("{name} anchor refused under {backend:?}: {error:#}");
            }
        }
        for backend in ["kvm", "liteinst", "in-guest-trap"] {
            let error = check(Some(backend)).expect_err(backend);
            assert!(
                error
                    .downcast_ref::<super::container::PolicyRefusal>()
                    .is_some()
                    && format!("{error:#}").contains("enforced only on the ptrace backend"),
                "{name} anchor under {backend}: {error:#}"
            );
        }
    }
}

/// A spec the user must fix is a policy refusal; a failure to read it that is
/// not the user's (here `EIO`, from reading `/proc/self/mem` at address 0) is
/// not.
#[test]
fn happens_before_spec_load_errors_are_refusals_except_host_read_failures() {
    let dir = tempfile::tempdir().unwrap();
    let nth_zero = dir.path().join("nth0.json");
    std::fs::write(
        &nth_zero,
        r#"{"version": 1, "events": {"z": {"thread": "3", "syscall": "write", "nth": 0}}}"#,
    )
    .unwrap();
    let not_json = dir.path().join("bad.json");
    std::fs::write(&not_json, "{").unwrap();
    let not_utf8 = dir.path().join("latin1.json");
    std::fs::write(&not_utf8, b"\xff").unwrap();
    for path in [
        nth_zero,
        not_json,
        not_utf8,
        dir.path().join("missing.json"),
        dir.path().to_path_buf(),
        // ENOTDIR: a path through a regular file.
        dir.path().join("bad.json").join("spec.json"),
    ] {
        let error = RunOpts::load_happens_before_spec(&path).expect_err("must not load");
        assert!(
            error
                .downcast_ref::<super::container::PolicyRefusal>()
                .is_some(),
            "{}: {error:#}",
            path.display()
        );
    }
    let error = RunOpts::load_happens_before_spec(Path::new("/proc/self/mem"))
        .expect_err("reading /proc/self/mem at address 0 fails");
    assert_eq!(
        error
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::raw_os_error),
        Some(libc::EIO),
        "{error:#}"
    );
    assert!(
        error
            .downcast_ref::<super::container::PolicyRefusal>()
            .is_none(),
        "{error:#}"
    );
}

#[cfg(test)]
impl RunOpts {
    /// These options as a run config must reproduce them: every field
    /// except the runtime-only provenance that a reload legitimately changes.
    /// An epoch the first parse took from the clock is explicit after a
    /// reload, and `--save-config` is never written into the file.
    pub(crate) fn config_equivalence_key(&self) -> String {
        let mut options = self.clone();
        options.epoch_omitted = false;
        options.epoch_captured_from_host = false;
        options.epoch_from_recording = false;
        options.config_capture = None;
        options.save_config = None;
        options.manifest = None;
        options.manifest_path = None;
        options.pending_network_manifest = None;
        format!("{options:#?}")
    }
}

/// Whether a `--no-namespace` run's guest IDs `ids` have a base drawn from the
/// host: `free-window` for a base taken from the windows free on this host,
/// unless `--pid-base` or `--strict` `required` it. `None` for host IDs, which
/// no rerun can reproduce, and in a PID namespace.
fn guest_ids_drawn_from_host(
    no_namespace: bool,
    ids: detcore_model::pid::GuestIds,
    required: bool,
) -> Option<&'static str> {
    use detcore_model::pid::GuestIds;
    match ids {
        GuestIds::Chosen { .. } if no_namespace && !required => Some("free-window"),
        _ => None,
    }
}

#[test]
fn a_base_the_host_decided_is_host_drawn() {
    use detcore_model::pid::GuestIds;
    let chosen = GuestIds::Chosen {
        base: 7168,
        refuse_host_ids: false,
    };
    assert_eq!(
        guest_ids_drawn_from_host(true, chosen, false),
        Some("free-window")
    );
    assert_eq!(guest_ids_drawn_from_host(true, chosen, true), None);
    assert_eq!(guest_ids_drawn_from_host(true, GuestIds::Host, false), None);
    assert_eq!(
        guest_ids_drawn_from_host(false, GuestIds::Namespace, false),
        None
    );
}

/// A base Hermit chose for a `--no-namespace` run is saved as if given by
/// `--pid-base`, so the saved run takes the same IDs.
#[test]
fn saved_config_records_the_guest_id_base_hermit_chose() {
    let argv = [
        "hermit",
        "run",
        "--no-namespace",
        "--save-config=unused.yaml",
        "/bin/true",
    ];
    let matches = <crate::app::Args as clap::CommandFactory>::command()
        .try_get_matches_from(argv)
        .unwrap();
    let mut args = crate::app::args_from_matches_with_clock(&matches, SystemTime::now).unwrap();
    let crate::app::Subcommand::Run(ro) = &mut args.command else {
        panic!("{argv:?} is not a `run` command");
    };
    assert_eq!(ro.saved_run_config().unwrap().run.get("pid-base"), None);
    // What `resolve_guest_ids` leaves behind for a default base.
    ro.det_opts.det_config.guest_ids = detcore_model::pid::GuestIds::Chosen {
        base: 7168,
        refuse_host_ids: false,
    };
    let saved = ro.saved_run_config().unwrap();
    assert_eq!(saved.run["pid-base"], 7168, "{saved:?}");
}

/// `--save-config` writes the seed `--seed-from` chose in its place, so the
/// saved run does not choose again.
#[test]
fn saved_config_records_the_seed_seed_from_chose() {
    let argv = [
        "hermit",
        "run",
        "--seed-from=SystemRandom",
        "--seed=3",
        "--save-config=unused.yaml",
        "/bin/true",
    ];
    let matches = <crate::app::Args as clap::CommandFactory>::command()
        .try_get_matches_from(argv)
        .unwrap();
    let args = crate::app::args_from_matches_with_clock(&matches, SystemTime::now).unwrap();
    let crate::app::Subcommand::Run(mut ro) = args.command else {
        panic!("{argv:?} is not a `run` command");
    };
    let before = ro.saved_run_config().unwrap();
    assert_eq!(before.run["seed-from"], "SystemRandom");
    assert_eq!(before.run["seed"], 3);
    ro.validate_args_with_perf_support(true).unwrap();
    let chosen = ro.det_opts.det_config.seed;
    let saved = ro.saved_run_config().unwrap();
    assert_eq!(saved.run.get("seed-from"), None, "{saved:?}");
    assert_eq!(saved.run["seed"], chosen, "{saved:?}");
}

/// An epoch adopted from a replayed recording is an implicit input too.
#[test]
fn saved_config_records_an_epoch_adopted_from_a_recording() {
    let argv = ["hermit", "run", "--save-config=unused.yaml", "/bin/true"];
    let matches = <crate::app::Args as clap::CommandFactory>::command()
        .try_get_matches_from(argv)
        .unwrap();
    let mut args = crate::app::args_from_matches_with_clock(&matches, SystemTime::now).unwrap();
    let crate::app::Subcommand::Run(ro) = &mut args.command else {
        panic!("{argv:?} is not a `run` command");
    };
    // What `adopt_replayed_schedule_epoch` leaves behind.
    ro.det_opts.det_config.epoch = "2001-02-03T04:05:06.5Z".parse().unwrap();
    ro.epoch_captured_from_host = false;
    ro.epoch_from_recording = true;
    let saved = ro.saved_run_config().unwrap();
    assert_eq!(saved.run["epoch"], "2001-02-03T04:05:06.500+00:00");
}

/// Without `--save-config` there is nothing to capture, and asking for the
/// config says why instead of writing an empty one.
#[test]
fn saved_config_requires_the_parsers_capture() {
    let ro = RunOpts::parse_from(["fakehermit", "--save-config=x.yaml", "fakeprog"]);
    let error = ro.saved_run_config().unwrap_err().to_string();
    assert!(
        error.contains("only by a `hermit run` invocation"),
        "{error}"
    );
}

#[test]
fn display_runopts1() {
    let vec: Vec<&str> = vec!["fakehermit", "fakeprog", "arg1", "arg2"];
    let mut ro = RunOpts::parse_from(vec.iter());
    ro.validate_args_with_perf_support(true).unwrap();
    assert_eq!(
        format!("{}", ro),
        " --epoch=2026-01-01T00:00:00+00:00 -- fakeprog arg1 arg2"
    );
}

#[test]
fn backend_defaults_to_ptrace() {
    let mut ro = RunOpts::parse_from(["fakehermit", "fakeprog"]);
    ro.validate_args_with_perf_support(true).unwrap();
    assert_eq!(ro.backend, None);
    assert_eq!(ro.selected_backend(), Backend::Ptrace);
    assert_eq!(
        format!("{}", ro),
        " --epoch=2026-01-01T00:00:00+00:00 -- fakeprog"
    );
}

#[test]
fn backend_values_parse_and_round_trip() {
    for (value, expected) in [
        ("ptrace", Some(Backend::Ptrace)),
        ("liteinst", Some(Backend::Liteinst)),
        ("in-guest-trap", Some(Backend::InGuestTrap)),
        ("kvm", Some(Backend::Kvm)),
    ] {
        let mut ro = run_opts_for(&["hermit", "--backend", value, "run", "fakeprog"]);
        ro.validate_args_with_perf_support(true).unwrap();
        assert_eq!(ro.backend, expected);
        assert_eq!(ro.execution_backend_name(), value);
        if let Some(expected) = expected {
            assert_eq!(ro.selected_backend(), expected);
        }
        // The backend is global, so it renders before the subcommand and is not
        // part of the `run` options' own rendering.
        assert_eq!(ro.global_backend_arg(), format!(" --backend={value}"));
        assert_eq!(
            format!("{}", ro),
            " --epoch=2026-01-01T00:00:00+00:00 -- fakeprog"
        );
    }
}

/// A PLAIN `--verify` CANNOT REACH THE CANONICAL COMPARATOR, AND `--verify-strict`
/// ALWAYS CAN — ON EVERY BACKEND, INCLUDING KVM.
///
/// Ported from the superseded hermit#2217, which fixed
/// `let kvm_output_only = self.selected_backend() == Backend::Kvm` by making the
/// bypass conditional. Current main fixed it more strongly, by DELETING the
/// backend term outright, so that pull request's own test could not be ported as
/// written: it asserted on a `compare_verification_logs(backend, ..)` helper that
/// main has no equivalent of, and re-creating one to test would have pinned the
/// new helper rather than the shipped call sites.
///
/// ⚠️ THE HAZARD THIS ANSWERS: a removed branch leaves nothing behind to notice
/// its return. `bitwise_parity_contract_accepts_only_named_canonical_envelopes`
/// already pins that only a canonical envelope may claim parity, but nothing
/// pinned WHICH REQUESTS REACH canonical — so re-introducing a backend term in
/// `verification_strictness` would have restored the original defect silently.
/// Iterating every `Backend` variant is what makes that impossible: a KVM-shaped
/// special case fails here by name.
#[test]
fn comparator_choice_does_not_depend_on_the_backend() {
    for value in ["ptrace", "liteinst", "in-guest-trap", "kvm"] {
        let plain = run_opts_for(&["hermit", "--backend", value, "run", "--verify", "fakeprog"]);
        assert_eq!(plain.execution_backend_name(), value);
        assert_eq!(
            plain.verification_strictness(),
            LogCompareStrictness::Stripped,
            "plain --verify on {value} must stay on the lossy comparator, so it \
             cannot claim canonical bitwise parity"
        );

        for flag in ["--verify-strict", "--verify-verbose"] {
            let canonical = run_opts_for(&[
                "hermit",
                "--backend",
                value,
                "run",
                "--verify",
                flag,
                "fakeprog",
            ]);
            assert_eq!(
                canonical.verification_strictness(),
                LogCompareStrictness::Canonical,
                "{flag} on {value} must reach the canonical comparator; KVM \
                 bypassing it unconditionally was the hermit#2217 defect"
            );
        }
    }
}

#[test]
fn verification_always_compares_retained_logs() {
    for backend in ["ptrace", "liteinst", "in-guest-trap", "kvm"] {
        let run = run_opts_for(&[
            "hermit",
            "--backend",
            backend,
            "run",
            "--verify",
            "fakeprog",
        ]);
        assert!(
            run.verification_comparison_options().compare_logs,
            "--verify on {backend} must compare both retained logs"
        );
    }
}

#[test]
fn every_backend_keeps_io_buffer_checking_on_by_default() {
    for backend in ["ptrace", "liteinst", "in-guest-trap", "kvm"] {
        let run = run_opts_for(&[
            "hermit",
            "--backend",
            backend,
            "run",
            "--verify",
            "--verify-strict",
            "fakeprog",
        ]);
        let prepared =
            hermit::prepare_backend_config(run.effective_det_config(), run.runtime_backend());
        assert!(
            prepared.detlog_io_buffers,
            "{backend} disabled syscall output-buffer checking while the ordinary strict-verify \
             request left it enabled"
        );
        {
            assert_eq!(
                run.verification_comparison_options().compare_io_buffers,
                prepared.detlog_io_buffers,
                "{backend} reported an io-buffer comparison state different from the \
                 configuration that backend actually receives"
            );
        }

        let opted_out = run_opts_for(&[
            "hermit",
            "--backend",
            backend,
            "run",
            "--verify",
            "--verify-strict",
            "--no-detlog-io-buffers",
            "fakeprog",
        ]);
        let prepared_opt_out = hermit::prepare_backend_config(
            opted_out.effective_det_config(),
            opted_out.runtime_backend(),
        );
        assert!(
            !prepared_opt_out.detlog_io_buffers,
            "{backend} ignored the explicit --no-detlog-io-buffers relaxation"
        );
        {
            assert_eq!(
                opted_out
                    .verification_comparison_options()
                    .compare_io_buffers,
                prepared_opt_out.detlog_io_buffers,
                "{backend} reported an io-buffer comparison state different from its explicit \
                 opt-out"
            );
        }
    }
}

// Both runs of an in-process `--verify` bind the snapshot their owner took,
// and a run refuses to set up its container without it
// (https://github.com/rrnewton/hermit/issues/4693).
#[test]
fn verification_runs_bind_one_shared_ld_so_cache_snapshot() {
    let tmpfs = tempfile::tempdir().unwrap();
    let mut ro = run_opts_for(&["hermit", "run", "/bin/true"]);
    assert!(
        ro.mounts(tmpfs.path()).is_ok(),
        "a plain run reads the host's cache"
    );
    ro.fatal_core_run_label = Some("run2");
    let refused = ro
        .mounts(tmpfs.path())
        .err()
        .expect("run 2 without the snapshot");
    assert!(
        refused
            .to_string()
            .contains("verification run run2 has no shared /etc/ld.so.cache snapshot"),
        "{refused:#}"
    );
    let shared = ro.with_verification_ld_so_cache().unwrap();
    assert_eq!(
        shared.ld_so_cache_snapshot,
        Some(super::container::LdSoCacheSnapshot::of_host().unwrap())
    );
    assert!(shared.mounts(tmpfs.path()).is_ok());
    // An existing snapshot (one the holder sealed) is kept, not re-read.
    let sealed = super::container::LdSoCacheSnapshot::from_bytes(b"sealed".to_vec());
    ro.ld_so_cache_snapshot = Some(sealed.clone());
    assert_eq!(
        ro.with_verification_ld_so_cache()
            .unwrap()
            .ld_so_cache_snapshot,
        Some(sealed)
    );
}

// `--verify-run` under the holder takes the sealed /etc/ld.so.cache as its
// snapshot, and only that role at that path.
#[test]
fn an_adopted_ld_so_cache_role_becomes_the_runs_snapshot() {
    let mut ro = run_opts_for(&["hermit", "run", "/bin/true"]);
    let dir = tempfile::tempdir().unwrap();
    let other = dir.path().join("other");
    fs::write(&other, b"not the cache").unwrap();
    let unrelated =
        super::verify_inputs::AdoptedInputs::sealed_for_test(&[("data".into(), other.clone())])
            .unwrap();
    ro.adopt_shared_ld_so_cache(&unrelated).unwrap();
    assert_eq!(ro.ld_so_cache_snapshot, None);
    let misplaced =
        super::verify_inputs::AdoptedInputs::sealed_for_test(&[(LD_SO_CACHE_ROLE.into(), other)])
            .unwrap();
    assert!(ro.adopt_shared_ld_so_cache(&misplaced).is_err());
    let Ok(host) = fs::read(LD_SO_CACHE_PATH) else {
        return;
    };
    let sealed = super::verify_inputs::AdoptedInputs::sealed_for_test(&[(
        LD_SO_CACHE_ROLE.into(),
        PathBuf::from(LD_SO_CACHE_PATH),
    )])
    .unwrap();
    // A run that holds the sealed role but did not adopt it is refused.
    let tmpfs = tempfile::tempdir().unwrap();
    ro.shared_inputs = Some(std::sync::Arc::new(sealed));
    let refused = ro
        .mounts(tmpfs.path())
        .err()
        .expect("an unadopted sealed cache");
    assert!(
        refused
            .to_string()
            .contains("the shared inputs seal /etc/ld.so.cache, but this run did not adopt it"),
        "{refused:#}"
    );
    let adopted = std::sync::Arc::clone(ro.shared_inputs.as_ref().unwrap());
    let (sealed_bytes, sealed_identity) = adopted
        .read_role(LD_SO_CACHE_ROLE, Path::new(LD_SO_CACHE_PATH))
        .unwrap()
        .unwrap();
    assert_eq!(sealed_bytes, host);
    ro.adopt_shared_ld_so_cache(&adopted).unwrap();
    assert_eq!(
        ro.ld_so_cache_snapshot,
        Some(super::container::LdSoCacheSnapshot::from_captured(
            host,
            sealed_identity
        ))
    );
    assert!(ro.mounts(tmpfs.path()).is_ok());
}

#[test]
fn e9patch_preserves_executable_identity_and_uses_ptrace_runtime() {
    // The executable overlay is core launch code that an executable-selected
    // preprocessing adapter (e9patch, now external) fills in; it is exercised
    // here on the ptrace runtime such an adapter runs on.
    let mut ro = run_opts_for(&["hermit", "--backend", "ptrace", "run", "/bin/echo", "hello"]);
    ro.executable_overlay = Some(ExecutableOverlayMount {
        source: PathBuf::from("/cache/patched-echo"),
        target: PathBuf::from("/bin/echo"),
        rewritten: None,
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
        .find(|planned| planned.mount.get_source() == Some(Path::new("/cache/patched-echo")))
        .unwrap();
    assert_eq!(overlay.mount.get_target(), Path::new("/bin/echo"));
    // The overlay is e9patch's own mount, which the guest's mount table
    // leaves out by this identity.
    assert!(overlay.backend);
}

/// A host without `/tmp` or `/etc/group` (Android) gets neither mount: the
/// container's setup mounted onto both and failed with ENOENT before the
/// guest started. A host with both keeps them.
#[test]
fn a_host_without_tmp_or_a_group_file_mounts_neither() {
    let ro = run_opts_for(&["hermit", "run", "/bin/true"]);
    let tmpfs = tempfile::tempdir().unwrap();
    let targets = |layout| -> Vec<PathBuf> {
        ro.mounts_on(tmpfs.path(), layout)
            .unwrap()
            .mounts
            .iter()
            .map(|planned| planned.mount.get_target().to_path_buf())
            .collect()
    };
    let linux = targets(HostLayout {
        tmp: true,
        group_file: true,
    });
    for target in ["/tmp", "/etc/group"] {
        assert!(linux.iter().any(|t| t == Path::new(target)), "{linux:?}");
    }
    let android = targets(HostLayout {
        tmp: false,
        group_file: false,
    });
    for target in ["/tmp", "/etc/group"] {
        assert!(
            !android.iter().any(|t| t == Path::new(target)),
            "{android:?}"
        );
    }
}

/// There, `--tmp` and a mount or bind aimed into `/tmp` are refused with the
/// reason, rather than left to fail at container setup.
#[test]
fn a_host_without_tmp_refuses_what_would_go_into_it() {
    let tmpfs = tempfile::tempdir().unwrap();
    let android = HostLayout {
        tmp: false,
        group_file: true,
    };
    for (args, named) in [
        (
            &["hermit", "run", "--tmp=/data/scratch", "/bin/true"][..],
            "none for --tmp /data/scratch to go over",
        ),
        (
            &[
                "hermit",
                "run",
                "--mount=type=tmpfs,target=/tmp/x",
                "/bin/true",
            ][..],
            "cannot place /tmp/x there",
        ),
        (
            &["hermit", "run", "--bind=/etc:/tmp/etc", "/bin/true"][..],
            "cannot place /tmp/etc there",
        ),
    ] {
        let refused = run_opts_for(args)
            .mounts_on(tmpfs.path(), android)
            .err()
            .unwrap_or_else(|| panic!("{args:?} was not refused"));
        assert!(refused.to_string().contains(named), "{refused:#}");
    }
    let outside = run_opts_for(&[
        "hermit",
        "run",
        "--mount=type=tmpfs,target=/opt",
        "/bin/true",
    ]);
    assert!(outside.mounts_on(tmpfs.path(), android).is_ok());
    // `--strace-only` sets `--tmp` itself, and is named instead.
    let mut strace = run_opts_for(&["hermit", "run", "--strace-only", "/bin/true"]);
    strace.lower_strace_only();
    let refused = strace.mounts_on(tmpfs.path(), android).err().unwrap();
    assert!(
        refused
            .to_string()
            .contains("--strace-only shares the host's /tmp"),
        "{refused:#}"
    );
    // A backend's own mount into /tmp names the backend, not a choice the
    // caller could change.
    let mut backend = run_opts_for(&[
        "hermit",
        "run",
        "--mount=type=tmpfs,target=/tmp/x",
        "/bin/true",
    ]);
    backend.backend_mounts = (1, 0);
    let refused = backend.mounts_on(tmpfs.path(), android).err().unwrap();
    assert!(
        refused
            .to_string()
            .contains("where the ptrace backend places /tmp/x"),
        "{refused:#}"
    );
    // And Hermit's own scratch files need TMPDIR there.
    let refused = validate_host_scratch(android, false).unwrap_err();
    assert!(
        refused.to_string().contains("TMPDIR is unset"),
        "{refused:#}"
    );
    assert!(validate_host_scratch(android, true).is_ok());
    let linux = HostLayout {
        tmp: true,
        group_file: true,
    };
    assert!(validate_host_scratch(linux, false).is_ok());
}

/// A `--mount` target that only an earlier `--mount` of the same run
/// provides is left to container setup, which mounts in order: the host
/// check would refuse it, though the container can mount it. Alone, the same
/// missing target is named, and one inside /tmp is never checked.
#[test]
fn a_mount_target_an_earlier_mount_provides_is_not_refused() {
    let host = |path: &Path| path == Path::new("/srv/data");
    let mounts = |args: &[&str]| {
        let mut argv = vec!["hermit", "run"];
        argv.extend(args);
        argv.push("/bin/true");
        run_opts_for(&argv).mount
    };
    let bind = "--mount=type=bind,source=/srv/source,target=/srv/data";
    let nested = "--mount=type=tmpfs,target=/srv/data/sub";
    assert_eq!(missing_mount_target(&mounts(&[bind, nested]), host), None);
    assert_eq!(
        missing_mount_target(&mounts(&[nested]), host),
        Some(Path::new("/srv/data/sub"))
    );
    assert_eq!(
        missing_mount_target(&mounts(&["--mount=type=tmpfs,target=/tmp/x"]), host),
        None
    );
}

/// A `--mount` below `/dev/mqueue`, which the guest's own mqueue filesystem
/// would cover, is refused as that before its target is looked for on the
/// host, whose `/dev/mqueue` holds only the host's queues. On a host without
/// `/dev/mqueue` no private one is mounted, and the missing target is named.
#[test]
fn a_mount_the_private_dev_mqueue_would_cover_is_refused_before_its_target_is_looked_for() {
    let mounts = run_opts_for(&[
        "hermit",
        "run",
        "--mount=type=tmpfs,target=/dev/mqueue/below",
        "/bin/true",
    ])
    .mount;
    let dev_mqueue = |path: &Path| path == Path::new("/dev/mqueue");
    let refused = validate_mount_targets(&mounts, dev_mqueue, dev_mqueue)
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains(
            "cannot mount at /dev/mqueue/below: the guest's /dev/mqueue is its own mqueue filesystem"
        ),
        "{refused}"
    );
    let no_dev_mqueue = |_: &Path| false;
    let refused = validate_mount_targets(&mounts, no_dev_mqueue, no_dev_mqueue)
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("--mount target /dev/mqueue/below does not exist on this host"),
        "{refused}"
    );
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
    let mut ro = run_opts_for(&[
        "hermit",
        "--backend",
        "ptrace",
        "run",
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
    ro.prepared_program = Some(guest);
    let command = ro.guest_command().unwrap();
    assert_eq!(command.get_program(), "/tmp/tool");
    assert_eq!(command.get_arg0(), "tool");
}

#[test]
fn non_e9patch_validation_preserves_parent_component_paths() {
    let ro = run_opts_for(&["hermit", "--backend", "ptrace", "run", "/bin/../bin/echo"]);
    ro.validate_program().unwrap();
}

/// The guest environment must be identical across backends. Sanitizer leak
/// detection is disabled in `guest_command()` (the single backend-independent
/// place) rather than only in the ptrace tracer, so the out-of-process KVM
/// backend presents the guest the same environment as ptrace and the
/// in-guest backends.
/// Regression test for the KVM-vs-ptrace env divergence, where the guest saw
/// two fewer variables under KVM.
#[test]
fn guest_env_disables_sanitizer_leak_detection_on_every_backend() {
    let asan = OsStr::new("ASAN_OPTIONS");
    let lsan = OsStr::new("LSAN_OPTIONS");
    let expected = OsStr::new("detect_leaks=0").to_os_string();

    // The two variables are present with identical values regardless of the
    // selected backend, so no backend-specific spawn hook is required for parity.
    for backend in ["ptrace", "kvm", "liteinst", "in-guest-trap"] {
        let ro = run_opts_for(&["hermit", "--backend", backend, "run", "/bin/echo", "hi"]);
        let envs = ro.guest_command().unwrap().get_captured_envs();
        assert_eq!(
            envs.get(asan),
            Some(&expected),
            "ASAN_OPTIONS not set for backend {backend}"
        );
        assert_eq!(
            envs.get(lsan),
            Some(&expected),
            "LSAN_OPTIONS not set for backend {backend}"
        );
    }
}

#[test]
fn namespace_only_guest_command_applies_process_options_once() {
    let ro = RunOpts::parse_from([
        "fakehermit",
        "--namespace-only",
        "--base-env=minimal",
        "--env=NAMESPACE_ONLY_EXPLICIT=present",
        "--workdir=/test",
        "/bin/echo",
        "first",
        "second",
    ]);
    let command = ro.namespace_only_guest_command().unwrap();

    assert_eq!(command.get_program(), OsStr::new("/bin/echo"));
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        &[OsStr::new("first"), OsStr::new("second")]
    );
    assert_eq!(command.get_current_dir(), Some(Path::new("/test")));

    let envs = command.get_captured_envs();
    assert_eq!(
        envs.len(),
        6,
        "unexpected namespace-only environment: {envs:?}"
    );
    assert_eq!(
        envs.get(OsStr::new("HOME")).map(|value| value.as_os_str()),
        Some(OsStr::new("/root"))
    );
    assert_eq!(
        envs.get(OsStr::new("USER")).map(|value| value.as_os_str()),
        Some(OsStr::new("root"))
    );
    assert_eq!(
        envs.get(OsStr::new("LOGNAME"))
            .map(|value| value.as_os_str()),
        Some(OsStr::new("root"))
    );
    assert_eq!(
        envs.get(OsStr::new("HOSTNAME"))
            .map(|value| value.as_os_str()),
        Some(OsStr::new("hermetic-container.local"))
    );
    assert_eq!(
        envs.get(OsStr::new("PATH")).map(|value| value.as_os_str()),
        Some(OsStr::new(
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
        ))
    );
    assert_eq!(
        envs.get(OsStr::new("NAMESPACE_ONLY_EXPLICIT"))
            .map(|value| value.as_os_str()),
        Some(OsStr::new("present"))
    );
}

#[test]
fn namespace_only_guest_command_preserves_sanitizer_environment() {
    let ro = RunOpts::parse_from(["fakehermit", "--namespace-only", "/bin/true"]);
    let command = ro.namespace_only_guest_command().unwrap();
    let sanitizer_overrides: Vec<_> = command
        .get_envs()
        .filter(|(name, _)| {
            *name == OsStr::new("ASAN_OPTIONS") || *name == OsStr::new("LSAN_OPTIONS")
        })
        .map(|(name, value)| (name.to_owned(), value.map(OsStr::to_owned)))
        .collect();

    assert!(
        sanitizer_overrides.is_empty(),
        "namespace-only added explicit sanitizer overrides: {sanitizer_overrides:?}"
    );

    let missing = format!("HERMIT_NAMESPACE_ONLY_MISSING_ENV_{}", std::process::id());
    assert!(std::env::var_os(&missing).is_none());
    let option = format!("--env={missing}");
    let ro = RunOpts::parse_from(["fakehermit", "--namespace-only", &option, "/bin/true"]);
    let error = match ro.namespace_only_guest_command() {
        Ok(_) => panic!("namespace-only accepted a missing pass-through environment variable"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains(&missing), "unexpected error: {error}");
    assert!(error.contains("not set in the host environment"), "{error}");
}

#[test]
fn guest_path_normalization_rejects_parent_components() {
    let error = normalize_guest_path(Path::new("/mnt/../tool")).unwrap_err();
    assert!(error.to_string().contains("parent components"));
}

#[test]
fn e9patch_mount_target_rejects_parent_components() {
    let ro = run_opts_for(&[
        "hermit",
        "--backend",
        "ptrace",
        "run",
        "--mount=type=tmpfs,target=/tmp/../bin",
        "/bin/echo",
    ]);
    let error = ro
        .validate_executable_overlay_mount_targets()
        .unwrap_err()
        .to_string();
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
    let ro = run_opts_for(&["hermit", "--backend", "ptrace", "run", &mount, "/bin/echo"]);
    let error = ro.validate_executable_overlay_mount_targets().unwrap_err();
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

/// The rendered options (what `hermit analyze` prints as a reproducer and
/// writes to its `*.repro` files) are a shell command line that parses back
/// to the same options: each `--mount` is its own word and keeps its whole
/// spec, type and `readonly` included. Before, the first `--mount` was glued
/// to the option before it, and every mount was shown as its source and
/// target only, so a pasted reproducer lost the bind.
#[test]
fn rendered_options_parse_back_to_the_same_mounts() {
    let bind = "type=bind,source=/etc/hostname,target=/etc/ld.so.cache,readonly";
    let argv = [
        "fakehermit",
        "--save-config=/tmp/run one.config",
        &format!("--mount={bind}"),
        "--mount=type=tmpfs,target=/test",
        "--bind=/tmp",
        "--",
        "/bin/echo",
        "a b",
    ];
    // Options as `hermit run` and `hermit analyze` hold them, validated.
    let validated = |argv: Vec<String>| {
        let mut options = RunOpts::parse_from(argv);
        options.validate_args_with_timer_support(Ok(())).unwrap();
        options
    };
    let options = validated(argv.iter().map(|arg| arg.to_string()).collect());
    let rendered = options.to_string();
    assert!(
        rendered.contains(&format!(
            " --mount={} --mount={} ",
            shell_words::quote(bind),
            shell_words::quote("type=tmpfs,target=/test")
        )),
        "{rendered}"
    );
    let words = shell_words::split(&rendered).unwrap();
    let reparsed = validated(
        std::iter::once("fakehermit".to_owned())
            .chain(words)
            .collect(),
    );
    let mounts = |options: &RunOpts| -> Vec<Mount> {
        options.mount.iter().map(|arg| arg.mount.clone()).collect()
    };
    assert_eq!(mounts(&reparsed), mounts(&options));
    // The bind is still read-only: the source and target alone, the old
    // rendering, parse to another mount.
    let source_and_target: Mount = "source=/etc/hostname,target=/etc/ld.so.cache"
        .parse()
        .unwrap();
    assert_ne!(mounts(&reparsed)[0], source_and_target);
    assert_eq!(reparsed.save_config, options.save_config);
    assert_eq!(reparsed.args, options.args);
    assert_eq!(reparsed.to_string(), rendered);
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
    let ro = run_opts_for(&[
        "hermit",
        "--backend",
        "e9patch",
        "run",
        &unrelated_mount,
        &tmp,
        "--",
        "/tmp/script",
    ]);
    let plan = ro.adapter_executable_overlay_plan().unwrap();
    let ro = ro.adapter_with_executable_overlay(&plan, None).unwrap();
    assert!(ro.executable_overlay.is_none());
}

#[test]
fn executable_overlay_uses_canonical_target_without_custom_mounts() {
    let ro = run_opts_for(&["hermit", "--backend", "ptrace", "run", "/bin/echo"]);
    assert_eq!(
        ro.resolve_executable_overlay_target(Path::new("/bin/echo"), Path::new("/bin/echo"))
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
    let ro = run_opts_for(&[
        "hermit",
        "--backend",
        "ptrace",
        "run",
        &mount,
        "/e9patch-test/link",
    ]);
    let error = ro
        .resolve_executable_overlay_target(Path::new("/e9patch-test/link"), &link)
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
    let ro = run_opts_for(&["hermit", "--backend", "ptrace", "run", &mount, "/bin/echo"]);
    let error = ro
        .resolve_executable_overlay_target(Path::new("/bin/echo"), Path::new("/bin/echo"))
        .unwrap_err();
    assert!(error.to_string().contains("symlinked executable"));
}

#[test]
fn detects_symlink_resolution_through_implicit_mounts() {
    use std::os::fd::AsRawFd;

    // ⚠️ THE PREFIX MUST BE THE TEMP DIRECTORY IN USE, NOT THE LITERAL `/tmp`.
    // `NamedTempFile` honours `TMPDIR`, and validate runs this suite with
    // `TMPDIR` pointed inside its own runtime directory. Hardcoding `/tmp`
    // therefore asserted a property of the developer's default environment
    // rather than of `path_resolution_visits_prefix`, and the test passed
    // locally while failing in every validate run.
    let temp_root = std::env::temp_dir();
    let file = tempfile::NamedTempFile::new().unwrap();
    let proc_fd = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
    assert!(path_resolution_visits_prefix(&proc_fd, &temp_root).unwrap());
    assert!(path_resolution_visits_prefix(&proc_fd, Path::new("/proc")).unwrap());
    assert!(!path_resolution_visits_prefix(Path::new("/bin/echo"), &temp_root).unwrap());
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
    assert_eq!(
        format!("{}", ro),
        " --epoch=2026-01-01T00:00:00+00:00 -- fakeprog arg1 arg2"
    );
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
    assert_eq!(
        format!("{}", ro),
        " --epoch=2026-01-01T00:00:00+00:00 -- fakeprog arg1"
    );
}

#[test]
fn unsupported_syscalls_fail_closed_by_default_with_explicit_opt_out() {
    let mut normal = RunOpts::parse_from(["fakehermit", "fakeprog"]);
    normal.validate_args_with_perf_support(true).unwrap();
    assert!(normal.det_opts.det_config.panic_on_unsupported_syscalls);
    assert!(!normal.det_opts.det_config.passthru_opt);
    assert_eq!(
        format!("{}", normal),
        " --epoch=2026-01-01T00:00:00+00:00 -- fakeprog"
    );

    let mut strict = RunOpts::parse_from(["fakehermit", "--strict", "fakeprog"]);
    strict.validate_args_with_perf_support(true).unwrap();

    assert!(strict.det_opts.det_config.sequentialize_threads);
    assert!(strict.det_opts.det_config.deterministic_io);
    assert!(!strict.det_opts.det_config.passthru_opt);
    assert!(strict.det_opts.det_config.panic_on_unsupported_syscalls);
    assert_eq!(
        format!("{}", strict),
        " --strict --epoch=2026-01-01T00:00:00+00:00 -- fakeprog"
    );

    let mut compatibility =
        RunOpts::parse_from(["fakehermit", "--allow-unsupported-syscalls", "fakeprog"]);
    compatibility.validate_args_with_perf_support(true).unwrap();
    assert!(
        !compatibility
            .det_opts
            .det_config
            .panic_on_unsupported_syscalls
    );
    assert_eq!(
        format!("{}", compatibility),
        " --allow-unsupported-syscalls --epoch=2026-01-01T00:00:00+00:00 -- fakeprog"
    );
}

#[test]
fn panic_on_rbc_overshoot_flag_wires_to_detcore_config() {
    let default = RunOpts::parse_from(["fakehermit", "fakeprog"]);
    assert!(!default.det_opts.det_config.panic_on_rcb_overshoot);

    let mut opts = RunOpts::parse_from(["fakehermit", "--panic-on-rbc-overshoot", "fakeprog"]);
    opts.validate_args_with_perf_support(true).unwrap();
    assert!(opts.det_opts.det_config.panic_on_rcb_overshoot);
    assert_eq!(
        format!("{}", opts),
        " --panic-on-rbc-overshoot --epoch=2026-01-01T00:00:00+00:00 -- fakeprog"
    );
}

#[test]
fn passthru_opt_leaves_guest_syscalls_unobserved_on_every_backend() {
    for backend in [
        Backend::Ptrace,
        Backend::Kvm,
        Backend::Liteinst,
        Backend::InGuestTrap,
    ] {
        let reason = unobserved_guest_execution(backend, true)
            .unwrap_or_else(|| panic!("{backend:?} under --passthru-opt observes everything"));
        assert!(reason.contains("--passthru-opt"), "{backend:?}: {reason}");
    }
    for backend in [Backend::Ptrace, Backend::Kvm] {
        assert_eq!(
            unobserved_guest_execution(backend, false),
            None,
            "{backend:?}"
        );
    }
    for backend in [Backend::Liteinst, Backend::InGuestTrap] {
        assert!(
            unobserved_guest_execution(backend, false).is_some(),
            "{backend:?}"
        );
    }
}

/// Detcore guards Reverie's private page on the ptrace runtime, the one
/// backend that names host input changes and lets the guest's syscalls past
/// its tracer from code it maps; nowhere else is any range guarded.
#[test]
fn the_ptrace_runtime_guards_reverie_private_page() {
    assert_eq!(
        untraced_code_range(Backend::Ptrace),
        Some(REVERIE_PTRACE_PRIVATE_PAGE)
    );
    for backend in [Backend::Kvm, Backend::Liteinst, Backend::InGuestTrap] {
        assert_eq!(untraced_code_range(backend), None, "{backend:?}");
    }
    let mut ro = RunOpts::parse_from(["fakehermit", "fakeprog"]);
    assert_eq!(
        ro.effective_det_config().untraced_code_range,
        Some((0x7100_0000, 0x7100_1000))
    );
    ro.set_backend(Some(Backend::Kvm));
    assert_eq!(ro.effective_det_config().untraced_code_range, None);
}

#[test]
fn untrap_passthrough_syscalls_is_an_opt_in_refused_where_its_calls_are_counted() {
    let mut opts = RunOpts::parse_from(["fakehermit", "--untrap-passthrough-syscalls", "fakeprog"]);
    opts.validate_args_with_perf_support(true).unwrap();
    assert!(opts.det_opts.det_config.untrap_passthrough_syscalls);
    assert_eq!(
        format!("{}", opts),
        " --untrap-passthrough-syscalls --epoch=2026-01-01T00:00:00+00:00 -- fakeprog"
    );
    let mut default = RunOpts::parse_from(["fakehermit", "fakeprog"]);
    default.validate_args_with_perf_support(true).unwrap();
    assert!(!default.det_opts.det_config.untrap_passthrough_syscalls);

    // Validation reads a replayed record before this refusal, so the replay
    // case names a readable one; this one states nothing.
    let directory = tempfile::tempdir().unwrap();
    let record = directory.path().join("preemptions.json");
    detcore::preemptions::PreemptionRecord::from_sched_events(Vec::new())
        .write_to_disk(&record)
        .unwrap();
    let replay = format!("--replay-schedule-from={}", record.display());
    for (argument, backend, refused_with) in [
        (Some("--strict"), None, "--strict"),
        // KVM refuses the default preemption timer before this check.
        (
            Some("--max-timeslice=disabled"),
            Some(Backend::Kvm),
            "backend other than ptrace",
        ),
        (
            Some("--max-timeslice=disabled"),
            Some(Backend::Liteinst),
            "backend other than ptrace",
        ),
        (
            Some("--record-preemptions-to=/tmp/p"),
            None,
            "preemption record",
        ),
        (Some(replay.as_str()), None, "preemption record"),
        (Some("--happens-before=/tmp/hb"), None, "--happens-before"),
        (Some("--verify-run=/tmp/v"), None, "--verify-run"),
        (Some("--namespace-only"), None, "backend other than ptrace"),
        (Some("--max-timeslice=disabled"), None, "RCB time"),
        (Some("--no-rcb-time"), None, "RCB time"),
    ] {
        let mut opts = RunOpts::parse_from(
            ["fakehermit", "--untrap-passthrough-syscalls"]
                .into_iter()
                .chain(argument)
                .chain(["fakeprog"]),
        );
        opts.set_backend(backend);
        let error = opts
            .validate_args_with_perf_support(true)
            .expect_err("refused");
        let message = format!("{error:#}");
        assert!(
            message.contains("--untrap-passthrough-syscalls") && message.contains(refused_with),
            "{argument:?} {backend:?}: {message}"
        );
    }
    // Nor on a host without usable performance counters, where validation
    // turns the timer off.
    let mut opts = RunOpts::parse_from(["fakehermit", "--untrap-passthrough-syscalls", "fakeprog"]);
    let message = format!(
        "{:#}",
        opts.validate_args_with_timer_support(Err(TimerUnavailable::NoPerfEvents))
            .expect_err("refused without a timer")
    );
    assert!(message.contains("RCB time"), "{message}");

    // Nor on an executable-selected backend: its built-in enum still reads
    // as the ptrace default, so the backend comparison above cannot see it.
    // The inherited positive maximum is retained (no --max-timeslice), so
    // preemption admission alone would accept the run; the refusal is the
    // executable-selected qualification check (revcodex2 on PR 4764).
    let application = run_opts_for(&[
        "hermit",
        "--backend=dbt",
        "run",
        "--untrap-passthrough-syscalls",
        "--",
        "fakeprog",
    ]);
    assert_eq!(application.application_backend, Some("dbt"));
    assert_eq!(
        application.adapter_capabilities,
        Some(reverie::BackendCapabilities::DBT)
    );
    assert_eq!(application.selected_backend(), Backend::Ptrace);
    assert!(application.det_opts.det_config.max_timeslice.is_some());
    let error = application
        .refuse_unqualified_application_options(
            <super::core_adapter::CoreApplicationAdapter as super::backend_hook::BackendAdapter>::NONDEFAULT_CPU_PROFILE_REFUSAL,
        )
        .expect_err("refused on an executable-selected backend");
    assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
    assert_eq!(
        error.to_string(),
        "--untrap-passthrough-syscalls is not qualified for executable-selected backend `dbt`"
    );
}

/// A preemption record holds a callback for each trapped call the recorded run
/// made, `getppid` among them, with its charge and its preemption point. With
/// `--untrap-passthrough-syscalls` those calls never reach Detcore, so a replay
/// of the record would place its checkpoints and priority changes at other
/// guest progress. Validation refuses such a replay, and recording one, before
/// the guest runs, whether the options come from the command line, a
/// `--config` file or a `--from` manifest. Without the flag each route accepts
/// the same record, so the refusal is the flag's.
#[test]
fn untrap_passthrough_syscalls_refuses_preemption_records_by_every_route() {
    use clap::CommandFactory as _;
    use detcore::types::DetTid;
    use detcore::types::SchedEvent;
    use detcore::types::SyscallPhase;
    use reverie::syscalls::Sysno;

    let directory = tempfile::tempdir().unwrap();
    let record = directory.path().join("preemptions.json");
    let thread = DetTid::from_raw(3);
    detcore::preemptions::PreemptionRecord::from_sched_events(vec![
        SchedEvent::syscall(thread, Sysno::getppid, SyscallPhase::Prehook),
        SchedEvent::syscall(thread, Sysno::getppid, SyscallPhase::Posthook),
    ])
    .write_to_disk(&record)
    .unwrap();
    let record = record.to_str().expect("record path is UTF-8").to_owned();

    // Validates `hermit run ARGS -- /bin/true`, expanded as `main` expands a
    // `--config` or `--from` before parsing.
    let validate = |run: &[&str]| -> Result<(), Error> {
        let argv: Vec<std::ffi::OsString> = ["hermit", "run"]
            .iter()
            .chain(run)
            .chain(&["--", "/bin/true"])
            .map(std::ffi::OsString::from)
            .collect();
        let argv = crate::app::run_config::expand_argv(&crate::app::Args::command(), &argv)?
            .unwrap_or(argv);
        let matches = crate::app::Args::command().try_get_matches_from(&argv)?;
        let mut args = crate::app::args_from_matches_with_clock(&matches, || {
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_767_225_600)
        })?;
        let crate::app::Subcommand::Run(run) = &mut args.command else {
            panic!("{argv:?} is not a `run` command");
        };
        run.validate_args_with_perf_support(true)
    };
    let refused = |run: &[&str], route: &str| {
        let message = format!("{:#}", validate(run).expect_err(route));
        assert!(
            message.contains("--untrap-passthrough-syscalls")
                && message.contains("preemption record"),
            "{route}: {message}"
        );
    };

    let replay = format!("--replay-preemptions-from={record}");
    validate(&[replay.as_str()]).expect("the record replays without the flag");
    refused(
        &["--untrap-passthrough-syscalls", replay.as_str()],
        "command line, --replay-preemptions-from",
    );
    validate(&["--record-preemptions"]).expect("a run records preemptions without the flag");
    refused(
        &["--untrap-passthrough-syscalls", "--record-preemptions"],
        "command line, --record-preemptions",
    );

    let config = |untrap: bool| {
        let path = directory.path().join(format!("run-{untrap}.yaml"));
        std::fs::write(
            &path,
            format!(
                "schema: {}\nrun:\n  replay-preemptions-from: {record}\n  \
                 untrap-passthrough-syscalls: {untrap}\n",
                crate::app::run_config::SCHEMA
            ),
        )
        .unwrap();
        path.to_str().expect("config path is UTF-8").to_owned()
    };
    let (with, without) = (config(true), config(false));
    validate(&["--config", without.as_str()]).expect("the config replays without the flag");
    refused(&["--config", with.as_str()], "--config");

    let manifest = |config: &str, name: &str| {
        let loaded = crate::app::run_config::read(Path::new(config)).unwrap();
        let (global, run) = crate::app::run_config::to_manifest_parts(&loaded).unwrap();
        let manifest = crate::app::manifest::Manifest::new(
            hermit::Id::unique(),
            crate::app::manifest::Kind::InputsOnly,
            crate::app::manifest::Command {
                program: Some("/bin/true".to_owned()),
                args: Vec::new(),
                cwd: None,
            },
            crate::app::manifest::Inputs {
                global,
                run,
                host_drawn: Default::default(),
            },
        );
        let at = directory
            .path()
            .join(name)
            .join(crate::app::manifest::MANIFEST_NAME);
        crate::app::manifest::write(&at, &manifest).unwrap();
        at.parent().unwrap().to_str().unwrap().to_owned()
    };
    let (with, without) = (manifest(&with, "untrapped"), manifest(&without, "trapped"));
    validate(&["--from", without.as_str()]).expect("the manifest replays without the flag");
    refused(&["--from", with.as_str()], "--from");
}

#[test]
fn passthru_optimization_requires_explicit_compatibility_opt_out() {
    let mut ro = RunOpts::parse_from([
        "fakehermit",
        "--allow-unsupported-syscalls",
        "--passthru-opt",
        "fakeprog",
    ]);
    ro.validate_args_with_perf_support(true).unwrap();

    assert!(ro.det_opts.det_config.passthru_opt);
    assert_eq!(
        format!("{}", ro),
        " --allow-unsupported-syscalls --passthru-opt --epoch=2026-01-01T00:00:00+00:00 -- fakeprog"
    );
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-644): Review rejecting optimization that bypasses fail-closed policy.
#[test]
fn passthru_optimization_rejects_fail_closed_modes() {
    for arguments in [
        vec!["fakehermit", "--passthru-opt", "fakeprog"],
        vec!["fakehermit", "--passthru-opt", "--strict", "fakeprog"],
        vec![
            "fakehermit",
            "--passthru-opt",
            "--panic-on-unsupported-syscalls",
            "fakeprog",
        ],
    ] {
        let mut opts = RunOpts::parse_from(arguments);
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
        assert!(
            message.contains("--allow-unsupported-syscalls"),
            "unexpected error: {message}"
        );
    }
}

#[test]
fn network_trace_is_off_without_a_networking_flag() {
    let mut ro = RunOpts::parse_from(["fakehermit", "fakeprog"]);
    ro.validate_args_with_perf_support(true).unwrap();
    assert_eq!(
        ro.det_opts.det_config.network_trace,
        NetworkTraceConfig::default()
    );
    assert_eq!(ro.network, NetworkingMode::Local);
}

#[test]
fn record_networking_selects_record_mode_and_host_networking() {
    let dir = tempfile::tempdir().unwrap();
    let trace = dir.path().join("net.trace");
    let flag = format!("--record-networking={}", trace.display());
    let mut ro = RunOpts::parse_from(["fakehermit", "--strict", &flag, "fakeprog"]);
    ro.validate_args_with_perf_support(true).unwrap();
    let config = &ro.det_opts.det_config.network_trace;
    assert_eq!(config.mode, NetworkTraceMode::Record);
    assert_eq!(config.path.as_deref(), Some(trace.as_path()));
    assert_eq!(ro.network, NetworkingMode::Host);

    let rendered = format!("{}", ro);
    assert!(rendered.contains(&flag), "{rendered}");
    assert!(!rendered.contains("--network="), "{rendered}");
    let mut reparsed_args = vec!["fakehermit".to_owned()];
    reparsed_args.extend(shell_words::split(&rendered).unwrap());
    let mut reparsed = RunOpts::parse_from(reparsed_args);
    reparsed.validate_args_with_perf_support(true).unwrap();
    assert_eq!(
        reparsed.det_opts.det_config.network_trace,
        ro.det_opts.det_config.network_trace
    );
}

#[test]
fn record_networking_refuses_a_missing_directory_and_conflicting_flags() {
    let mut ro = RunOpts::parse_from([
        "fakehermit",
        "--record-networking=/nonexistent-hermit-dir/net.trace",
        "fakeprog",
    ]);
    let error = ro.validate_args_with_perf_support(true).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("create it or choose a trace path"),
        "{error:#}"
    );
    for conflicting in ["--verify", "--network=host", "--replay-networking=t"] {
        assert!(
            RunOpts::try_parse_from([
                "fakehermit",
                "--record-networking=t",
                conflicting,
                "fakeprog"
            ])
            .is_err(),
            "{conflicting} must conflict with --record-networking"
        );
    }
}

#[test]
fn replay_networking_reads_the_trace_before_the_guest_starts() {
    let dir = tempfile::tempdir().unwrap();
    let trace = dir.path().join("net.trace");
    let epoch = "2026-01-01T00:00:00Z".parse().unwrap();
    detcore_model::network_engine::NetworkEngine::new_record(epoch)
        .finish()
        .unwrap()
        .write_framed(File::create(&trace).unwrap())
        .unwrap();
    let flag = format!("--replay-networking={}", trace.display());
    let mut ro = RunOpts::parse_from(["fakehermit", &flag, "fakeprog"]);
    ro.validate_args_with_perf_support(true).unwrap();
    let config = &ro.det_opts.det_config.network_trace;
    assert_eq!(config.mode, NetworkTraceMode::Replay);
    assert_eq!(config.path.as_deref(), Some(trace.as_path()));
    assert_eq!(ro.network, NetworkingMode::Local);
    assert!(format!("{}", ro).contains(&flag));

    let missing = dir.path().join("missing.trace");
    let flag = format!("--replay-networking={}", missing.display());
    let mut ro = RunOpts::parse_from(["fakehermit", &flag, "fakeprog"]);
    let error = ro.validate_args_with_perf_support(true).unwrap_err();
    assert!(
        format!("{error:#}").contains("pass a trace written by --record-networking"),
        "{error:#}"
    );

    fs::write(&missing, b"not a trace").unwrap();
    let mut ro = RunOpts::parse_from(["fakehermit", &flag, "fakeprog"]);
    let error = ro.validate_args_with_perf_support(true).unwrap_err();
    assert!(
        format!("{error:#}").contains("is not a valid network trace"),
        "{error:#}"
    );
}

/// A preemption record whose change points for one thread are not strictly
/// increasing in time is refused before the container starts, naming the
/// record, the thread and the change point, instead of panicking the
/// container child where Detcore reads it (rrnewton/hermit#4859). A valid
/// record, and a run with no replayed record, pass.
#[test]
fn a_replayed_record_that_does_not_increase_is_refused_before_the_container() {
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, times: [u64; 3]| {
        let path = dir.path().join(name);
        let rows: Vec<_> = times
            .iter()
            .map(|ns| serde_json::json!([ns, detcore::DEFAULT_PRIORITY]))
            .collect();
        let record = serde_json::json!({
            "global": [],
            "per_thread": {
                "3": {
                    "prio_changes": rows,
                    "preemption_rcbs": [100, 100, 200],
                    "final_prio": detcore::DEFAULT_PRIORITY,
                }
            }
        });
        std::fs::write(&path, record.to_string()).unwrap();
        path
    };
    let refused = |path: &std::path::Path| {
        let flag = format!("--replay-preemptions-from={}", path.display());
        let ro = RunOpts::parse_from(["fakehermit", &flag, "fakeprog"]);
        ro.refuse_an_invalid_replayed_record()
    };

    let bad = write("bad.json", [1_000, 2_500, 2_000]);
    let error = refused(&bad).unwrap_err();
    assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
    let text = format!("{error:#}");
    assert!(
        text.contains(&bad.display().to_string())
            && text.contains("thread 3's change point 3 at")
            && text.contains("is not after change point 2"),
        "{text}"
    );

    assert!(refused(&write("good.json", [1_000, 2_000, 2_500])).is_ok());

    // A truncated record is refused the same way, as is a missing one.
    let truncated = dir.path().join("truncated.json");
    let whole = std::fs::read(dir.path().join("good.json")).unwrap();
    std::fs::write(&truncated, &whole[..whole.len() / 2]).unwrap();
    let error = refused(&truncated).unwrap_err();
    assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
    let error = refused(&dir.path().join("missing.json")).unwrap_err();
    assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
    let ro = RunOpts::parse_from(["fakehermit", "fakeprog"]);
    assert!(ro.refuse_an_invalid_replayed_record().is_ok());
}

#[test]
fn replay_networking_adopts_the_trace_epoch_and_refuses_another() {
    let dir = tempfile::tempdir().unwrap();
    let trace = dir.path().join("net.trace");
    let recorded = "2026-01-01T00:00:00Z".parse().unwrap();
    detcore_model::network_engine::NetworkEngine::new_record(recorded)
        .finish()
        .unwrap()
        .write_framed(File::create(&trace).unwrap())
        .unwrap();
    let flag = format!("--replay-networking={}", trace.display());
    let parse = |arguments: &[&str]| {
        let mut ro = RunOpts::parse_from(arguments);
        if !arguments
            .iter()
            .any(|argument| argument.starts_with("--epoch"))
        {
            ro.capture_default_epoch(SystemTime::now);
        }
        ro.validate_args_with_perf_support(true).unwrap();
        ro
    };

    let mut ro = parse(&["fakehermit", &flag, "fakeprog"]);
    ro.adopt_replayed_schedule_epoch().unwrap();
    assert_eq!(ro.det_opts.det_config.epoch, recorded);
    assert!(ro.epoch_from_recording && !ro.epoch_captured_from_host);

    let mut ro = parse(&[
        "fakehermit",
        "--epoch=2026-01-01T00:00:00Z",
        &flag,
        "fakeprog",
    ]);
    ro.adopt_replayed_schedule_epoch().unwrap();
    assert_eq!(ro.det_opts.det_config.epoch, recorded);

    let mut ro = parse(&[
        "fakehermit",
        "--epoch=2027-01-01T00:00:00Z",
        &flag,
        "fakeprog",
    ]);
    let error = format!("{:#}", ro.adopt_replayed_schedule_epoch().unwrap_err());
    assert!(
        error.contains("the explicit virtual-time epoch 2027-01-01T00:00:00+00:00")
            && error.contains("pass --epoch=2026-01-01T00:00:00+00:00"),
        "{error}"
    );

    // As if a preemption record from another run had fixed the epoch first:
    // the trace must then agree with it.
    let mut ro = parse(&["fakehermit", &flag, "fakeprog"]);
    ro.epoch_from_recording = true;
    ro.det_opts.det_config.epoch = "2027-01-01T00:00:00Z".parse().unwrap();
    let error = format!("{:#}", ro.adopt_replayed_schedule_epoch().unwrap_err());
    assert!(
        error.contains("adopted from another recording replayed in this run")
            && error.contains("replay them separately"),
        "{error}"
    );
}

#[test]
fn marked_replay_records_are_refused_before_the_container() {
    use detcore::preemptions::PreemptionRecord;
    use detcore::preemptions::PreemptionWriter;
    use detcore::preemptions::boundary_recording::BoundaryRecording;
    use detcore::preemptions::boundary_recording::Recording;
    use detcore_model::schedule::boundary::Schedule;

    let dir = tempfile::tempdir().unwrap();
    for marked in [false, true] {
        for (flag, boundaries) in [
            ("--replay-preemptions-from", false),
            ("--replay-schedule-from", false),
            ("--replay-schedule-from", true),
        ] {
            let mut writer = PreemptionWriter::new(None);
            if marked {
                writer.mark_replay_departed(7);
            }
            let record: PreemptionRecord = serde_json::from_str(&writer.into_string()).unwrap();
            record.validate().unwrap();
            let recording = if boundaries {
                Recording::Boundaries(
                    BoundaryRecording::new(record, Schedule::new(vec![]).unwrap()).unwrap(),
                )
            } else {
                Recording::Legacy(record)
            };
            let path = dir
                .path()
                .join(format!("{flag}-{boundaries}-{marked}.json"));
            recording.write(&path).unwrap();
            let argument = format!("{flag}={}", path.display());
            let mut args = vec!["fakehermit", argument.as_str()];
            if boundaries {
                args.push("--strict-schedule-boundaries");
            }
            args.push("fakeprog");
            let options = RunOpts::parse_from(args);
            let result = options.refuse_an_invalid_replayed_record();
            if marked {
                let error = result.expect_err("an incomplete record cannot be a faithful replay");
                assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
                let text = format!("{error:#}");
                assert!(text.contains("HERMIT_INCOMPLETE_REPLAY_RECORD"), "{text}");
                assert!(text.contains("7"), "{text}");
                assert!(text.contains(&path.display().to_string()), "{text}");
            } else {
                result.unwrap();
            }
        }
    }
}

#[test]
fn edited_history_cannot_certify_faithful_verification() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("edited.json");
    let input = format!("--replay-preemptions-from={}", path.display());
    let capture = format!("--verify-run={}", dir.path().join("verification").display());
    for marked in [false, true] {
        let mut writer = detcore::preemptions::PreemptionWriter::new(None);
        if marked {
            writer.mark_replay_departed(7);
        }
        std::fs::write(&path, writer.into_string()).unwrap();
        let options = RunOpts::parse_from([
            "fakehermit",
            input.as_str(),
            "--replay-edited-preemptions",
            "fakeprog",
        ]);
        options.refuse_an_invalid_replayed_record().unwrap();
        for verification in [
            vec!["--verify"],
            vec!["--verify", "--verify-strict"],
            vec![capture.as_str()],
        ] {
            let mut args = vec!["fakehermit", input.as_str(), "--replay-edited-preemptions"];
            args.extend(verification.iter().copied());
            args.push("fakeprog");
            let options = RunOpts::parse_from(args);
            let error = options
                .refuse_an_invalid_replayed_record()
                .expect_err("edited exploration cannot certify faithful replay");
            assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
            let text = format!("{error:#}");
            assert!(text.contains("--replay-edited-preemptions"), "{text}");
            assert!(text.contains("verification"), "{text}");
            if !marked {
                let mut args = vec!["fakehermit", input.as_str()];
                args.extend(verification.iter().copied());
                args.push("fakeprog");
                RunOpts::parse_from(args)
                    .refuse_an_invalid_replayed_record()
                    .unwrap();
            }
        }
    }
}

#[test]
fn replay_takes_the_recorded_rng_seed_and_refuses_another() {
    let directory = tempfile::tempdir().unwrap();
    let record = directory.path().join("schedule.preempts");
    fs::write(&record, r#"{"per_thread":{},"global":[],"rng_seed":6}"#).unwrap();
    let replay_arg = format!("--replay-schedule-from={}", record.display());

    let mut omitted =
        RunOpts::parse_from(["fakehermit", "--seed=1", replay_arg.as_str(), "fakeprog"]);
    omitted.adopt_replayed_rng_seed().unwrap();
    let config = &omitted.det_opts.det_config;
    assert_eq!(
        config.rng_seed(),
        6,
        "the recorded seed replaces the one --seed implies"
    );
    assert_eq!(
        config.sched_seed(),
        1,
        "the schedule seed stays the caller's"
    );

    let mut explicit = RunOpts::parse_from([
        "fakehermit",
        "--rng-seed=1",
        replay_arg.as_str(),
        "fakeprog",
    ]);
    let refusal = explicit
        .adopt_replayed_rng_seed()
        .expect_err("the guest would read other random values than the recording");
    assert!(
        format!("{refusal:#}").contains("--rng-seed=1 differs from the seed 6"),
        "{refusal:#}"
    );

    let mut matching = RunOpts::parse_from([
        "fakehermit",
        "--rng-seed=6",
        replay_arg.as_str(),
        "fakeprog",
    ]);
    matching.adopt_replayed_rng_seed().unwrap();
    assert_eq!(matching.det_opts.det_config.rng_seed(), 6);
}

/// `--chaos-timeslice-range` sets the run's `--max-timeslice` to the range's
/// MAX, through `validate_args` and again on a second validation (analyze
/// revalidates its trials), and refuses an explicit `--max-timeslice` in any
/// spelling, or `--chaos-seeded-timeslice`, beside it.
#[test]
fn a_chaos_timeslice_range_caps_the_run_at_its_max() {
    let mut validated = run_opts_for(&[
        "hermit",
        "run",
        "--chaos",
        "--chaos-timeslice-range=1000..5000000",
        "prog",
    ]);
    for _ in 0..2 {
        validated.validate_args_with_perf_support(true).unwrap();
        assert_eq!(
            validated.det_opts.det_config.max_timeslice,
            std::num::NonZeroU64::new(5_000_000)
        );
    }
    for explicit in [
        "--max-timeslice=disabled",
        "--preemption-timeout=1000",
        "--chaos-seeded-timeslice",
    ] {
        let mut refused = run_opts_for(&[
            "hermit",
            "run",
            "--chaos",
            explicit,
            "--chaos-timeslice-range=1000..5000000",
            "prog",
        ]);
        let error = format!(
            "{:#}",
            refused.validate_args_with_perf_support(true).unwrap_err()
        );
        assert!(
            error.contains("give one of them, not both"),
            "{explicit}: {error}"
        );
        if explicit == "--chaos-seeded-timeslice" {
            assert!(
                error.contains("--chaos-seeded-timeslice draws --max-timeslice from the scheduler"),
                "{error}"
            );
        }
    }
    let resolve = |argv: &[&str]| {
        let mut ro = run_opts_for(argv);
        ro.resolve_chaos_timeslice_range().map(|()| ro)
    };
    let range = "--chaos-timeslice-range=1000..5000000";
    let ro = resolve(&["hermit", "run", "--chaos", range, "prog"]).unwrap();
    assert_eq!(
        ro.det_opts.det_config.max_timeslice,
        std::num::NonZeroU64::new(5_000_000)
    );
    let error = format!(
        "{:#}",
        resolve(&[
            "hermit",
            "run",
            "--chaos",
            "--max-timeslice=5000000",
            range,
            "prog"
        ])
        .unwrap_err()
    );
    assert!(error.contains("give one of them, not both"), "{error}");
    let plain = resolve(&["hermit", "run", "--chaos", "prog"]).unwrap();
    assert_eq!(
        plain.det_opts.det_config.max_timeslice,
        std::num::NonZeroU64::new(200_000_000)
    );
}

/// Analysis selects a fixed non-chaos continuation after the target run's
/// options were validated. Every later launch and saved command must keep
/// that maximum, including options that previously drew or resolved it.
#[test]
fn analyze_continuations_survive_range_and_seed_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let record = dir.path().join("prefix.json");
    let chosen = std::num::NonZeroU64::new(500_000_000);
    detcore::preemptions::PreemptionRecord::from_sched_events(Vec::new())
        .with_chaos_timeslice(chosen.map(std::num::NonZeroU64::get))
        .write_to_disk(&record)
        .unwrap();
    for selection in [
        &["--chaos-timeslice-range=1000..5000000"][..],
        &[
            "--chaos-timeslice-range=1000..5000000",
            "--chaos-timeslice-draw=run",
        ][..],
        &["--chaos-seeded-timeslice"][..],
    ] {
        let mut args = vec!["hermit", "run", "--chaos", "--seed=7"];
        args.extend_from_slice(selection);
        args.push("/bin/true");
        let mut options = run_opts_for(&args);
        options.validate_args_with_perf_support(true).unwrap();
        assert_ne!(options.det_opts.det_config.max_timeslice, chosen);
        options.set_analysis_continuation_timeslice(chosen);
        options.det_opts.det_config.replay_schedule_from = Some(record.clone());
        for _ in 0..2 {
            options.validate_args_with_perf_support(true).unwrap();
            assert_eq!(
                options.det_opts.det_config.max_timeslice, chosen,
                "{selection:?}"
            );
            assert!(!options.det_opts.det_config.chaos);
        }
        let command = shell_words::split(&format!("hermit run {options}")).unwrap();
        let words: Vec<&str> = command.iter().map(String::as_str).collect();
        let mut repro = run_opts_for(&words);
        repro.validate_args_with_perf_support(true).unwrap();
        assert_eq!(repro.det_opts.det_config.max_timeslice, chosen);
        assert!(!repro.det_opts.det_config.chaos);
    }
}

/// `--chaos-timeslice-draw=run` draws the run's `--max-timeslice` from the
/// scheduler seed and keeps it for the run's record. A replay of a record that
/// states a drawn timeslice runs under it, from a range or in place of the
/// default maximum, and refuses another explicit `--max-timeslice`. A range
/// with an explicit `--max-timeslice` is refused, and so is
/// `--chaos-seeded-timeslice` beside a run draw or on a replay of a drawn
/// timeslice.
#[test]
fn a_run_draw_picks_the_maximum_and_a_replay_takes_the_recorded_one() {
    let resolve = |argv: &[&str]| {
        let mut ro = run_opts_for(argv);
        ro.resolve_chaos_timeslice_range().map(|()| ro)
    };
    // The draw follows the effective seed, so it happens in validation.
    let validate = |argv: &[&str]| {
        let mut ro = run_opts_for(argv);
        ro.validate_args_with_perf_support(true).unwrap();
        ro
    };
    let range = "--chaos-timeslice-range=1000..200000000";
    let run = "--chaos-timeslice-draw=run";
    let parsed_range = "1000..200000000".parse().unwrap();
    let mut ro = validate(&["hermit", "run", "--chaos", "--seed=7", range, run, "prog"]);
    let drawn = detcore::preemptions::draw_chaos_timeslice(parsed_range, 7);
    assert_eq!(ro.det_opts.det_config.max_timeslice, Some(drawn));
    assert_eq!(ro.det_opts.det_config.chaos_timeslice, Some(drawn));
    let mut bare = validate(&["hermit", "run", "--chaos", "--seed=7", run, "prog"]);
    for _ in 0..2 {
        assert_eq!(
            bare.det_opts.det_config.chaos_timeslice_range,
            Some(parsed_range)
        );
        assert_eq!(bare.det_opts.det_config.max_timeslice, Some(drawn));
        assert_eq!(bare.det_opts.det_config.chaos_timeslice, Some(drawn));
        bare.validate_args_with_perf_support(true).unwrap();
    }
    // Saving the resolved shorthand freezes its range as an explicit input.
    let matches = <crate::app::Args as clap::CommandFactory>::command()
        .try_get_matches_from([
            "hermit",
            "run",
            "--chaos",
            "--seed=7",
            run,
            "--save-config=unused.yaml",
            "prog",
        ])
        .unwrap();
    let mut args = crate::app::args_from_matches_with_clock(&matches, SystemTime::now).unwrap();
    let crate::app::Subcommand::Run(saved) = &mut args.command else {
        panic!("not a run command");
    };
    saved.validate_args_with_perf_support(true).unwrap();
    let config = saved.saved_run_config().unwrap();
    assert_eq!(
        config.run["chaos-timeslice-range"],
        parsed_range.to_string()
    );
    assert_eq!(config.run["chaos-timeslice-draw"], "run");
    for explicit in ["--max-timeslice=disabled", "--chaos-seeded-timeslice"] {
        let mut refused = run_opts_for(&["hermit", "run", "--chaos", explicit, run, "prog"]);
        let error = format!(
            "{:#}",
            refused.validate_args_with_perf_support(true).unwrap_err()
        );
        assert!(
            error.contains("give one of them, not both"),
            "{explicit}: {error}"
        );
    }
    let mut unavailable = run_opts_for(&["hermit", "run", "--chaos", run, "prog"]);
    let error = format!(
        "{:#}",
        unavailable
            .validate_args_with_perf_support(false)
            .unwrap_err()
    );
    assert!(
        error.contains("--chaos-timeslice-draw=run") && error.contains("usable PMU"),
        "{error}"
    );
    // A backend that delivers no preemption timer events (in-guest LiteInst,
    // with or without site patching) refuses the draw; KVM delivers them, so it
    // draws the same maximum as ptrace.
    for backend in ["--backend=liteinst", "--backend=in-guest-trap"] {
        let mut unavailable = run_opts_for(&["hermit", backend, "run", "--chaos", run, "prog"]);
        let error = format!(
            "{:#}",
            unavailable
                .validate_args_with_perf_support(true)
                .unwrap_err()
        );
        assert!(
            error.contains("unsupported_preemption"),
            "{backend}: {error}"
        );
        assert!(
            error.contains("--chaos-timeslice-draw=run"),
            "{backend}: {error}"
        );
    }
    let kvm = validate(&[
        "hermit",
        "--backend=kvm",
        "run",
        "--chaos",
        "--seed=7",
        range,
        run,
        "prog",
    ]);
    assert_eq!(kvm.det_opts.det_config.max_timeslice, Some(drawn));
    assert_eq!(kvm.det_opts.det_config.chaos_timeslice, Some(drawn));
    ro.validate_args_with_perf_support(true).unwrap();
    assert_eq!(
        ro.det_opts.det_config.max_timeslice,
        Some(drawn),
        "revalidated"
    );
    let sched = validate(&[
        "hermit",
        "run",
        "--chaos",
        "--sched-seed=7",
        range,
        run,
        "prog",
    ]);
    assert_eq!(sched.det_opts.det_config.max_timeslice, Some(drawn));
    // `--seed-from=Args` settles the seed during validation; the draw uses it.
    let derived = validate(&[
        "hermit",
        "run",
        "--chaos",
        "--seed-from=Args",
        range,
        run,
        "prog",
    ]);
    let seed = derived.det_opts.det_config.sched_seed();
    assert_eq!(
        derived.det_opts.det_config.max_timeslice,
        Some(detcore::preemptions::draw_chaos_timeslice(
            parsed_range,
            seed
        ))
    );
    let error = format!(
        "{:#}",
        resolve(&[
            "hermit",
            "run",
            "--chaos",
            "--max-timeslice=5000",
            range,
            "prog"
        ])
        .unwrap_err()
    );
    assert!(error.contains("give one of them, not both"), "{error}");
    let mut seeded = run_opts_for(&[
        "hermit",
        "run",
        "--chaos",
        "--chaos-seeded-timeslice",
        "--seed=7",
        range,
        run,
        "prog",
    ]);
    let error = format!(
        "{:#}",
        seeded.validate_args_with_perf_support(true).unwrap_err()
    );
    assert!(
        error.contains("--chaos-seeded-timeslice draws --max-timeslice")
            && error.contains("give one of them, not both"),
        "{error}"
    );
    assert_eq!(seeded.det_opts.det_config.chaos_timeslice, None);

    let dir = tempfile::tempdir().unwrap();
    let marked = dir.path().join("marked.json");
    detcore::preemptions::PreemptionRecord::from_sched_events(Vec::new())
        .with_chaos_timeslice(Some(4321))
        .write_to_disk(&marked)
        .unwrap();
    let unmarked = dir.path().join("unmarked.json");
    detcore::preemptions::PreemptionRecord::from_sched_events(Vec::new())
        .write_to_disk(&unmarked)
        .unwrap();
    let replay = |path: &Path| format!("--replay-schedule-from={}", path.display());
    let marked_arg = replay(&marked);
    let recorded = std::num::NonZeroU64::new(4321);
    for argv in [
        &["hermit", "run", &marked_arg, "prog"][..],
        &[
            "hermit",
            "run",
            "--chaos",
            "--seed=8",
            range,
            &marked_arg,
            "prog",
        ],
        &["hermit", "run", "--max-timeslice=4321", &marked_arg, "prog"],
        &["hermit", "run", "--chaos", run, &marked_arg, "prog"],
    ] {
        let ro = resolve(argv).unwrap();
        assert_eq!(ro.det_opts.det_config.max_timeslice, recorded, "{argv:?}");
        assert_eq!(ro.det_opts.det_config.chaos_timeslice, recorded, "{argv:?}");
    }
    let error = format!(
        "{:#}",
        resolve(&["hermit", "run", "--max-timeslice=5000", &marked_arg, "prog"]).unwrap_err()
    );
    assert!(error.contains("pass --max-timeslice=4321"), "{error}");
    let seeded_replay = ["hermit", "run", "--chaos", "--chaos-seeded-timeslice"];
    let error = format!(
        "{:#}",
        resolve(&[&seeded_replay[..], &[&marked_arg, "prog"]].concat()).unwrap_err()
    );
    assert!(
        error.contains("recorded under a --max-timeslice of 4321 ns")
            && error.contains("Omit --chaos-seeded-timeslice"),
        "{error}"
    );
    // A record without a drawn timeslice changes nothing.
    let unmarked_arg = replay(&unmarked);
    // That includes a seeded replay, which keeps the seeded flag's own
    // replay fallback.
    let seeded = resolve(&[&seeded_replay[..], &[&unmarked_arg, "prog"]].concat()).unwrap();
    assert_eq!(seeded.det_opts.det_config.chaos_timeslice, None);
    let plain = resolve(&["hermit", "run", &unmarked_arg, "prog"]).unwrap();
    assert_eq!(
        plain.det_opts.det_config.max_timeslice,
        std::num::NonZeroU64::new(200_000_000)
    );
    assert_eq!(plain.det_opts.det_config.chaos_timeslice, None);
    // An unmarked legacy record keeps the existing requested draw policy.
    // The shorthand must behave exactly like the explicit recommended range.
    let explicit = validate(&[
        "hermit",
        "run",
        "--chaos",
        "--seed=7",
        range,
        run,
        &unmarked_arg,
        "prog",
    ]);
    let bare = validate(&[
        "hermit",
        "run",
        "--chaos",
        "--seed=7",
        run,
        &unmarked_arg,
        "prog",
    ]);
    assert_eq!(explicit.det_opts.det_config.chaos_timeslice, Some(drawn));
    assert_eq!(bare.det_opts.det_config.chaos_timeslice, Some(drawn));
    assert_eq!(
        bare.det_opts.det_config.max_timeslice,
        explicit.det_opts.det_config.max_timeslice
    );
}

/// The environment variable that turned the signal-state copy on while it
/// was opt-in (before A5 step 2d). It is no longer read.
const SIGNAL_STATE_CHECK_ENV: &str = "HERMIT_SIGNAL_STATE_CHECK";

/// Refuses a run whose environment sets [`SIGNAL_STATE_CHECK_ENV`], with any
/// value. The copy is a run option now (`--no-signal-state-copy`), which a
/// saved run configuration, a rendered command line and a recorded schedule
/// all carry; a variable read beside it would be a hidden input, and one
/// silently ignored would leave a run that asked for the copy off with it
/// on.
fn refuse_signal_state_check_env(value: Option<&std::ffi::OsStr>) -> Result<(), String> {
    match value {
        None => Ok(()),
        Some(value) => Err(format!(
            "{SIGNAL_STATE_CHECK_ENV}={value:?} is no longer read: the signal-state copy is on \
             by default, and --no-signal-state-copy turns it off. Unset the variable."
        )),
    }
}

/// `HERMIT_SIGNAL_STATE_CHECK` is refused whatever its value, so that no run
/// depends on it.
#[test]
fn the_retired_signal_state_check_variable_is_refused() {
    use std::ffi::OsStr;
    assert_eq!(refuse_signal_state_check_env(None), Ok(()));
    for value in ["0", "1", "", "false"] {
        let error = refuse_signal_state_check_env(Some(OsStr::new(value))).unwrap_err();
        assert!(error.contains("--no-signal-state-copy"), "{error}");
    }
}

/// A replay under an explicit `--expired-wait-charge` other than the rule its
/// record states is refused, under both replay flags; a record without the
/// field charged each wait; a replay that leaves the option unset is never
/// refused (it takes the record's rule).
#[test]
fn replay_refuses_another_expired_wait_charge() {
    let dir = tempfile::tempdir().unwrap();
    let record = |name: &str, once: bool| {
        let path = dir.path().join(name);
        let record = detcore::preemptions::PreemptionRecord::from_sched_events(Vec::new())
            .with_expired_wait_runs_charged_once(once);
        std::fs::write(&path, serde_json::to_string(&record).unwrap()).unwrap();
        path
    };
    let each = record("each.json", false);
    let once = record("once.json", true);
    for flag in ["--replay-schedule-from", "--replay-preemptions-from"] {
        for (path, charge, refused) in [
            (&each, None, false),
            (&each, Some("each"), false),
            (&each, Some("once"), true),
            (&once, None, false),
            (&once, Some("once"), false),
            (&once, Some("each"), true),
        ] {
            let replay = format!("{flag}={}", path.display());
            let mut argv = vec!["fakehermit".to_owned(), replay];
            if let Some(charge) = charge {
                argv.push(format!("--expired-wait-charge={charge}"));
            }
            argv.push("fakeprog".to_owned());
            let ro = RunOpts::parse_from(&argv);
            let result = ro.refuse_replay_under_another_expired_wait_charge();
            assert_eq!(result.is_err(), refused, "{argv:?}: {result:?}");
            if let Err(error) = result {
                let error = format!("{error:#}");
                assert!(
                    error.contains(flag) && error.contains("cannot reproduce the run"),
                    "{error}"
                );
            }
        }
    }
}

/// A replay under an explicit `--sleep-registration` other than the rule its
/// record states is refused, under both replay flags; a record without the
/// field registered sleeps when chosen; a replay that leaves the option unset
/// is never refused (it takes the record's rule).
#[test]
fn replay_refuses_another_sleep_registration() {
    let dir = tempfile::tempdir().unwrap();
    let record = |name: &str, first_pass: bool| {
        let path = dir.path().join(name);
        let record = detcore::preemptions::PreemptionRecord::from_sched_events(Vec::new())
            .with_queued_sleeps_registered_at_first_pass(first_pass);
        std::fs::write(&path, serde_json::to_string(&record).unwrap()).unwrap();
        path
    };
    let chosen = record("when-chosen.json", false);
    let first = record("first-pass.json", true);
    for flag in ["--replay-schedule-from", "--replay-preemptions-from"] {
        for (path, registration, refused) in [
            (&chosen, None, false),
            (&chosen, Some("when-chosen"), false),
            (&chosen, Some("first-pass"), true),
            (&first, None, false),
            (&first, Some("first-pass"), false),
            (&first, Some("when-chosen"), true),
        ] {
            let replay = format!("{flag}={}", path.display());
            let mut argv = vec!["fakehermit".to_owned(), replay];
            if let Some(registration) = registration {
                argv.push(format!("--sleep-registration={registration}"));
            }
            argv.push("fakeprog".to_owned());
            let ro = RunOpts::parse_from(&argv);
            let result = ro.refuse_replay_under_another_sleep_registration();
            assert_eq!(result.is_err(), refused, "{argv:?}: {result:?}");
            if let Err(error) = result {
                let error = format!("{error:#}");
                assert!(
                    error.contains(flag) && error.contains("cannot reproduce the run"),
                    "{error}"
                );
            }
        }
    }
}

/// A replay of a record whose timeslices `--chaos-instruction-offset` ended
/// past their branch targets is refused where it cannot single-step to them:
/// off the ptrace backend, with imprecise or disabled timers, and from the
/// record's schedule trace. A record without offsets replays as before.
#[test]
fn replay_of_instruction_offsets_is_refused_where_unsupported() {
    use detcore::preemptions::InstructionOffset;
    use detcore::preemptions::PreemptionWriter;
    let dir = tempfile::tempdir().unwrap();
    let record = |name: &str, offset: Option<InstructionOffset>| {
        let path = dir.path().join(name);
        let tid = detcore::types::DetTid::from_raw(3);
        let mut writer = PreemptionWriter::new(None);
        writer.register_thread(tid, detcore::DEFAULT_PRIORITY);
        writer.insert_reprioritization(
            tid,
            detcore::types::LogicalTime::from_nanos(10),
            12,
            detcore::DEFAULT_PRIORITY,
            5,
            offset,
        );
        std::fs::write(&path, writer.into_string()).unwrap();
        path
    };
    let with = record(
        "with.json",
        Some(InstructionOffset {
            rcb_target: 10,
            instructions: 4,
        }),
    );
    let without = record("without.json", None);
    for (path, flag, extra, refused) in [
        (&with, "--replay-preemptions-from", None, None),
        (
            &with,
            "--replay-preemptions-from",
            Some("--imprecise-timers"),
            Some("--imprecise-timers takes no single steps"),
        ),
        (
            &with,
            "--replay-preemptions-from",
            Some("--max-timeslice=disabled"),
            Some("the PMU timer that steps to them is disabled"),
        ),
        (
            &with,
            "--replay-schedule-from",
            None,
            Some("its schedule trace does not record the instruction offsets"),
        ),
        (&without, "--replay-schedule-from", None, None),
        (
            &without,
            "--replay-preemptions-from",
            Some("--imprecise-timers"),
            None,
        ),
    ] {
        let mut argv = vec!["hermit".to_owned(), "run".to_owned()];
        argv.push(format!("{flag}={}", path.display()));
        argv.extend(extra.map(str::to_owned));
        argv.push("fakeprog".to_owned());
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        let result = run_opts_for(&argv).refuse_instruction_offset_replay_where_unsupported();
        match refused {
            None => assert!(result.is_ok(), "{argv:?}: {result:?}"),
            Some(expected) => {
                let error = format!("{:#}", result.expect_err(&format!("{argv:?}")));
                assert!(
                    error.contains(expected) && error.contains("--chaos-instruction-offset"),
                    "{argv:?}: {error}"
                );
            }
        }
    }
    let kvm = format!("--replay-preemptions-from={}", with.display());
    let error = run_opts_for(&["hermit", "--backend=kvm", "run", &kvm, "fakeprog"])
        .refuse_instruction_offset_replay_where_unsupported()
        .expect_err("kvm cannot single-step to the offsets");
    assert!(
        format!("{error:#}").contains("backend `kvm` cannot single-step to them"),
        "{error:#}"
    );
    // On aarch64 the replay is a policy refusal, as recording is.
    let ptrace = format!("--replay-preemptions-from={}", with.display());
    let opts = run_opts_for(&["hermit", "run", &ptrace, "fakeprog"]);
    let error = opts
        .refuse_instruction_offset_replay_on(true)
        .expect_err("aarch64 single steps are not validated");
    assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
    assert!(
        format!("{error:#}").contains("validated only on x86_64"),
        "{error:#}"
    );
    opts.refuse_instruction_offset_replay_on(false).unwrap();
}

/// A replay of a record made under another `--pipe-wake-latency` is refused,
/// in both directions and for a different latency, under both replay flags;
/// `0` is not off; a record without the field is off
/// (https://github.com/rrnewton/hermit/issues/4380).
#[test]
fn replay_refuses_another_pipe_wake_latency() {
    let dir = tempfile::tempdir().unwrap();
    let record = |name: &str, latency: Option<u64>| {
        let path = dir.path().join(name);
        let record = detcore::preemptions::PreemptionRecord::from_sched_events(Vec::new())
            .with_pipe_wake_latency(latency);
        std::fs::write(&path, serde_json::to_string(&record).unwrap()).unwrap();
        path
    };
    let off = record("off.json", None);
    let zero = record("zero.json", Some(0));
    let three = record("three.json", Some(3_000_000));
    assert!(
        !std::fs::read_to_string(&off)
            .unwrap()
            .contains("pipe_wake_latency")
    );
    for flag in ["--replay-schedule-from", "--replay-preemptions-from"] {
        for (path, latency, refused) in [
            (&off, None, false),
            (&off, Some("0"), true),
            (&zero, None, true),
            (&zero, Some("0"), false),
            (&zero, Some("3000000"), true),
            (&three, Some("0"), true),
            (&three, Some("3000000"), false),
        ] {
            let replay = format!("{flag}={}", path.display());
            let mut argv = vec!["fakehermit".to_owned(), replay];
            if let Some(latency) = latency {
                argv.push(format!("--pipe-wake-latency={latency}"));
            }
            argv.push("fakeprog".to_owned());
            let ro = RunOpts::parse_from(&argv);
            let result = ro.refuse_replay_under_another_pipe_wake_latency();
            assert_eq!(result.is_err(), refused, "{argv:?}: {result:?}");
            if let Err(error) = result {
                let error = format!("{error:#}");
                assert!(
                    error.contains(flag) && error.contains("cannot reproduce the run"),
                    "{error}"
                );
            }
        }
    }
}

/// A record made under the removed stripped-file heuristic rewrite is
/// refused under every current setting: no supported rewrite reproduces
/// the instructions that heuristic wrote, under both replay flags. The
/// refusal is the typed `PolicyRefusal`, as at ff9382cb before the check
/// was lost in the recomposition onto main's rewriter.
#[test]
fn a_replay_under_the_other_stripped_rewrite_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let heuristic = dir.path().join("heuristic.json");
    std::fs::write(
        &heuristic,
        r#"{"per_thread":{},"global":[],"rewrite_exclusives_stripped_heuristic":true}"#,
    )
    .unwrap();
    for flag in ["--replay-schedule-from", "--replay-preemptions-from"] {
        for rewrite in [
            Vec::new(),
            vec!["--rewrite-exclusives"],
            vec!["--rewrite-exclusives", "--rewrite-exclusives-mode=memory"],
            vec!["--rewrite-exclusives", "--rewrite-exclusives-mode=overlay"],
        ] {
            let replay = format!("{flag}={}", heuristic.display());
            let mut argv = vec!["fakehermit".to_owned(), replay];
            argv.extend(rewrite.iter().map(|flag| flag.to_string()));
            argv.push("fakeprog".to_owned());
            let ro = RunOpts::parse_from(&argv);
            let error = ro
                .refuse_replay_under_another_exclusive_rewrite()
                .expect_err("a heuristic record must be refused");
            assert!(
                error.downcast_ref::<PolicyRefusal>().is_some(),
                "{argv:?}: {error:#}"
            );
            assert!(format!("{error:#}").contains(flag), "{error:#}");
        }
    }
}

/// A replay is refused when its exclusive rewrite (mode and rewrite
/// format version) is not the one its record states: the default overlay
/// and the in-memory rewrite are different provenances, another format
/// version under the same mode retires other instructions, and a record
/// made before records carried the provenance cannot be shown compatible
/// with a replay that rewrites at all.
#[test]
fn replay_refuses_another_exclusive_rewrite_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, json: &str| {
        let path = dir.path().join(name);
        std::fs::write(&path, json).unwrap();
        path
    };
    let plain = write("plain.json", r#"{"per_thread":{},"global":[]}"#);
    let legacy_memory = write(
        "legacy-memory.json",
        r#"{"per_thread":{},"global":[],"rewrite_exclusives_in_memory":true}"#,
    );
    let memory6 = write(
        "memory6.json",
        r#"{"per_thread":{},"global":[],"rewrite_exclusives_in_memory":true,"rewrite_exclusives":{"mode":"memory","format_version":6}}"#,
    );
    let memory5 = write(
        "memory5.json",
        r#"{"per_thread":{},"global":[],"rewrite_exclusives_in_memory":true,"rewrite_exclusives":{"mode":"memory","format_version":5}}"#,
    );
    let overlay6 = write(
        "overlay6.json",
        r#"{"per_thread":{},"global":[],"rewrite_exclusives":{"mode":"overlay","format_version":6}}"#,
    );
    // Records of this Hermit's rewriter; format 6 is the one before reads
    // through an ADRP's page or an ADR's address protected bytes (7).
    let current = hermit::aarch64_lse::REWRITE_FORMAT_VERSION;
    let memory_now = write(
        "memory-now.json",
        &format!(
            r#"{{"per_thread":{{}},"global":[],"rewrite_exclusives_in_memory":true,"rewrite_exclusives":{{"mode":"memory","format_version":{current}}}}}"#
        ),
    );
    let overlay_now = write(
        "overlay-now.json",
        &format!(
            r#"{{"per_thread":{{}},"global":[],"rewrite_exclusives":{{"mode":"overlay","format_version":{current}}}}}"#
        ),
    );
    let none: &[&str] = &[];
    let overlay: &[&str] = &["--rewrite-exclusives", "--rewrite-exclusives-mode=overlay"];
    let memory: &[&str] = &["--rewrite-exclusives", "--rewrite-exclusives-mode=memory"];
    for flag in ["--replay-schedule-from", "--replay-preemptions-from"] {
        for (path, rewrite, refused) in [
            (&plain, none, false),
            (&plain, overlay, true),
            (&plain, memory, true),
            (&legacy_memory, none, true),
            // Same in-memory boolean, but the record states no format
            // version, so its rewritten instructions are unknown.
            (&legacy_memory, memory, true),
            (&memory6, none, true),
            (&memory6, overlay, true),
            (&memory_now, memory, false),
            (&memory6, memory, true),
            (&memory5, memory, true),
            (&overlay6, none, true),
            (&overlay6, memory, true),
            (&overlay_now, overlay, false),
            (&overlay6, overlay, true),
        ] {
            let replay = format!("{flag}={}", path.display());
            let mut argv = vec!["fakehermit".to_owned(), replay];
            argv.extend(rewrite.iter().map(|flag| flag.to_string()));
            argv.push("fakeprog".to_owned());
            let ro = RunOpts::parse_from(&argv);
            let result = ro.refuse_replay_under_another_exclusive_rewrite();
            assert_eq!(result.is_err(), refused, "{argv:?}: {result:?}");
            if let Err(error) = result {
                assert!(
                    error.downcast_ref::<PolicyRefusal>().is_some(),
                    "{argv:?}: {error:#}"
                );
                let error = format!("{error:#}");
                assert!(
                    error.contains(flag) && error.contains("cannot reproduce the run"),
                    "{error}"
                );
            }
        }
    }
}

/// A replay whose flags state the same rewrite its record does is
/// admitted by the CLI's own early validation, which must run against
/// the effective config -- the one the rewrite flags populate -- not
/// the raw parsed one, whose rewrite fields are still off. Covers all
/// three selections: memory, explicit overlay, and the default
/// (namespaced) overlay, under both replay flags, through
/// `resolve_guest_ids`, the first of the CLI's shared-preflight sites
/// and the one analyze trials take.
#[test]
fn matching_exclusive_rewrite_replays_are_admitted_by_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, json: &str| {
        let path = dir.path().join(name);
        std::fs::write(&path, json).unwrap();
        path
    };
    // Records of this Hermit's rewriter, whatever its format version.
    let current = hermit::aarch64_lse::REWRITE_FORMAT_VERSION;
    let memory_now = write(
        "memory-now.json",
        &format!(
            r#"{{"per_thread":{{}},"global":[],"rewrite_exclusives_in_memory":true,"rewrite_exclusives":{{"mode":"memory","format_version":{current}}}}}"#
        ),
    );
    let overlay_now = write(
        "overlay-now.json",
        &format!(
            r#"{{"per_thread":{{}},"global":[],"rewrite_exclusives":{{"mode":"overlay","format_version":{current}}}}}"#
        ),
    );
    for flag in ["--replay-schedule-from", "--replay-preemptions-from"] {
        for (path, rewrite) in [
            (
                &memory_now,
                &["--rewrite-exclusives", "--rewrite-exclusives-mode=memory"][..],
            ),
            (
                &overlay_now,
                &["--rewrite-exclusives", "--rewrite-exclusives-mode=overlay"][..],
            ),
            (&overlay_now, &["--rewrite-exclusives"][..]),
        ] {
            let replay = format!("{flag}={}", path.display());
            let mut argv = vec!["fakehermit".to_owned(), replay];
            argv.extend(rewrite.iter().map(|flag| flag.to_string()));
            argv.push("fakeprog".to_owned());
            let mut ro = RunOpts::parse_from(&argv);
            let result = ro.resolve_guest_ids();
            assert!(result.is_ok(), "{argv:?}: {result:?}");
        }
    }
}

#[test]
fn network_trace_is_reached_through_a_descriptor_opened_before_the_container() {
    let dir = tempfile::tempdir().unwrap();
    let trace = dir.path().join("net.trace");
    fs::write(&trace, b"stale trace").unwrap();
    let flag = format!("--record-networking={}", trace.display());
    let mut ro = RunOpts::parse_from(["fakehermit", &flag, "fakeprog"]);
    ro.validate_args_with_perf_support(true).unwrap();
    let file = ro.open_network_trace().unwrap().unwrap();
    let inner = ro.det_opts.det_config.network_trace.path.clone().unwrap();
    assert!(inner.starts_with("/proc/self/fd/"), "{}", inner.display());
    assert_eq!(
        fs::read(&trace).unwrap(),
        b"",
        "a record truncates up front"
    );
    let epoch = "2026-01-01T00:00:00Z".parse().unwrap();
    detcore_model::network_engine::NetworkEngine::new_record(epoch)
        .finish()
        .unwrap()
        .write_framed(File::create(&inner).unwrap())
        .unwrap();
    drop(file);
    NetworkTraceV2::read_framed(File::open(&trace).unwrap()).unwrap();

    let flag = format!("--replay-networking={}", trace.display());
    let mut ro = RunOpts::parse_from(["fakehermit", &flag, "fakeprog"]);
    ro.validate_args_with_perf_support(true).unwrap();
    let _file = ro.open_network_trace().unwrap().unwrap();
    let inner = ro.det_opts.det_config.network_trace.path.clone().unwrap();
    assert!(inner.starts_with("/proc/self/fd/"), "{}", inner.display());
    NetworkTraceV2::read_framed(File::open(&inner).unwrap()).unwrap();

    let mut ro = RunOpts::parse_from(["fakehermit", "fakeprog"]);
    ro.validate_args_with_perf_support(true).unwrap();
    assert!(ro.open_network_trace().unwrap().is_none());
    assert_eq!(ro.det_opts.det_config.network_trace.path, None);

    for networking in [
        "--record-networking=t".to_owned(),
        format!("--replay-networking={}", trace.display()),
    ] {
        assert!(
            RunOpts::try_parse_from(["fakehermit", &networking, "--namespace-only", "fakeprog"])
                .is_err(),
            "{networking} must conflict with --namespace-only, which bypasses detcore"
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
        " --max-timeslice=100000 --target-timeslice=20000 --epoch=2026-01-01T00:00:00+00:00 -- fakeprog"
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
    assert!(!reparsed.det_opts.det_config.target_timeslice_syscalls_only);

    // --target-timeslice-syscalls-only parses, is rendered into the reproduce
    // command, round-trips, and is refused without --target-timeslice.
    let mut syscalls_only = RunOpts::parse_from([
        "fakehermit",
        "--target-timeslice=20000",
        "--target-timeslice-syscalls-only",
        "fakeprog",
    ]);
    syscalls_only.validate_args_with_perf_support(true).unwrap();
    assert!(
        syscalls_only
            .det_opts
            .det_config
            .target_timeslice_syscalls_only
    );
    let rendered = format!("{}", syscalls_only);
    assert!(
        rendered.contains(" --target-timeslice=20000 --target-timeslice-syscalls-only"),
        "{rendered}"
    );
    let mut reparsed_args = vec!["fakehermit".to_owned()];
    reparsed_args.extend(shell_words::split(&rendered).unwrap());
    let reparsed = RunOpts::parse_from(reparsed_args);
    assert!(reparsed.det_opts.det_config.target_timeslice_syscalls_only);
    assert!(
        RunOpts::try_parse_from(["fakehermit", "--target-timeslice-syscalls-only", "fakeprog"])
            .is_err(),
        "--target-timeslice-syscalls-only must require --target-timeslice"
    );
}

#[test]
fn skid_margin_override_parses_and_round_trips() {
    let mut opts = RunOpts::parse_from(["fakehermit", "--skid-margin=500", "fakeprog"]);
    opts.validate_args_with_perf_support(true).unwrap();

    assert_eq!(opts.skid_margin, Some(500));
    assert_eq!(
        format!("{opts}"),
        " --skid-margin=500 --epoch=2026-01-01T00:00:00+00:00 -- fakeprog"
    );
}

/// `--skid-margin` accepts any margin, including the 20000 and more that
/// the calibrated `hermit analyze` super-validation node passes through
/// `--run-arg`. A margin above the table's largest moves the keep margin
/// (see `skid_margin_override_keeps_the_keep_margin_up_to_the_table_largest`),
/// which is deterministic, so it is documented rather than refused.
#[test]
fn skid_margin_override_accepts_margins_above_the_table_largest() {
    let largest = reverie::pmu_config::LARGEST_TABLE_SKID_MARGIN;
    assert_eq!(largest, 10_000);
    for accepted in [0, 1, 2_000, largest, largest + 1, 20_000, 100_000] {
        let margin = format!("--skid-margin={accepted}");
        let mut opts = RunOpts::try_parse_from(["fakehermit", margin.as_str(), "fakeprog"])
            .unwrap_or_else(|error| panic!("{margin} refused: {error}"));
        opts.validate_args_with_perf_support(true).unwrap();
        assert_eq!(opts.skid_margin, Some(accepted));
    }
}

/// On every processor in the table, a margin up to the table's largest leaves
/// the keep margin, which decides whether a stop keeps or cancels a precise
/// event, exactly where the processor's own margin leaves it, so the override
/// changes only how a target is reached. A larger margin moves it to the
/// margin plus 5, the same on every processor.
#[test]
fn skid_margin_override_keeps_the_keep_margin_up_to_the_table_largest() {
    use reverie::pmu_config::PmuConfig;
    let largest = reverie::pmu_config::LARGEST_TABLE_SKID_MARGIN;
    // Intel Sapphire Rapids, AMD family 19h model A0h, EPYC 9D85, another Zen.
    for (family, model) in [(0x06, 0x8F), (0x19, 0xA0), (0x1A, 0x11), (0x19, 0x11)] {
        let config = PmuConfig::for_family_model(family, model).unwrap();
        let keep = config.keep_margin();
        for margin in [0, 1, config.skid_margin(), 5_000, largest] {
            let overridden = config.clone().with_skid_margin_override(margin);
            assert_eq!(overridden.skid_margin(), margin);
            assert_eq!(
                overridden.keep_margin(),
                keep,
                "family {family:#x} model {model:#x} margin {margin}"
            );
        }
        for margin in [largest + 1, 20_000] {
            assert_eq!(
                config
                    .clone()
                    .with_skid_margin_override(margin)
                    .keep_margin(),
                margin + reverie::pmu_config::SINGLESTEP_TIMEOUT_RCBS,
                "family {family:#x} model {model:#x} margin {margin}"
            );
        }
    }
}

/// `hermit run` records `--skid-margin` as the skid margin of Reverie's
/// per-process PMU configuration, so a later margin is refused; where this
/// host has a PMU profile, the configuration built from it carries the margin
/// and the processor's own keep margin. That configuration is per-process,
/// so the test runs in a process of its own.
#[test]
fn run_installs_its_skid_margin() {
    if crate::app::test_isolation::rerun_alone() {
        return;
    }
    let mut opts = RunOpts::parse_from(["fakehermit", "--skid-margin=7777", "fakeprog"]);
    opts.validate_args_with_perf_support(true).unwrap();
    opts.install_pmu_config().unwrap();
    assert!(
        reverie_ptrace::set_skid_margin_override(1).is_err(),
        "run did not install --skid-margin"
    );
    if let Some(config) = reverie::pmu_config::try_get_pmu_config() {
        assert_eq!(config.skid_margin(), 7777);
        assert_eq!(
            config.keep_margin(),
            reverie::pmu_config::LARGEST_TABLE_SKID_MARGIN
                + reverie::pmu_config::SINGLESTEP_TIMEOUT_RCBS
        );
    }
}

/// `--skid-margin` configures the PMU preemption timer: a backend that
/// delivers no timer events refuses it by name, and KVM, whose timer reads the
/// same Reverie PMU configuration as reverie-ptrace's, accepts it.
#[test]
fn skid_margin_override_needs_a_timer_delivering_backend() {
    let mut kvm = run_opts_for(&[
        "hermit",
        "--backend=kvm",
        "run",
        "--skid-margin=500",
        "fakeprog",
    ]);
    kvm.validate_args_with_perf_support(true)
        .unwrap_or_else(|error| panic!("kvm: {error:#}"));
    assert_eq!(kvm.skid_margin, Some(500));
    for backend in ["liteinst", "in-guest-trap"] {
        let mut opts = run_opts_for(&[
            "hermit",
            &format!("--backend={backend}"),
            "run",
            "--skid-margin=500",
            "fakeprog",
        ]);
        let error = opts.validate_args_with_perf_support(true).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains(&format!(
                "unsupported_preemption: --backend={backend} refuses --skid-margin"
            )),
            "unexpected {backend} error: {message}"
        );
        assert!(
            message.contains("cannot deliver Detcore preemption timer events"),
            "{message}"
        );
    }
}

/// `--chaos-instruction-offset` parses, is rendered into the reproduce
/// command and round-trips. It is refused where a run would ignore it or a
/// replay could not stop where it stopped: without `--chaos`, past the
/// step bound, off the ptrace backend, with imprecise or disabled timers,
/// with a replay or the strict slice-boundary protocol.
#[test]
fn chaos_instruction_offset_round_trips_and_is_refused_where_inert() {
    let mut opts = RunOpts::parse_from([
        "fakehermit",
        "--chaos",
        "--chaos-instruction-offset=16",
        "fakeprog",
    ]);
    opts.validate_args_with_perf_support(true).unwrap();
    assert_eq!(opts.det_opts.det_config.chaos_instruction_offset, 16);
    let rendered = format!("{opts}");
    assert!(
        rendered.contains(" --chaos-instruction-offset=16"),
        "{rendered}"
    );
    let mut reparsed_args = vec!["fakehermit".to_owned()];
    reparsed_args.extend(shell_words::split(&rendered).unwrap());
    let mut reparsed = RunOpts::parse_from(reparsed_args);
    reparsed.validate_args_with_perf_support(true).unwrap();
    assert_eq!(reparsed.det_opts.det_config.chaos_instruction_offset, 16);

    let mut default = RunOpts::parse_from(["fakehermit", "--chaos", "fakeprog"]);
    default.validate_args_with_perf_support(true).unwrap();
    assert_eq!(default.det_opts.det_config.chaos_instruction_offset, 0);
    assert!(!format!("{default}").contains("instruction-offset"));

    let mut bound = RunOpts::parse_from([
        "fakehermit",
        "--chaos",
        "--chaos-instruction-offset=256",
        "fakeprog",
    ]);
    bound.validate_args_with_perf_support(true).unwrap();

    // aarch64's single steps are not validated yet: a policy refusal there.
    let error =
        refuse_chaos_instruction_offset(&bound.det_opts.det_config, Backend::Ptrace.as_str(), true)
            .unwrap_err();
    assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
    assert!(
        format!("{error:#}").contains("validated only on x86_64"),
        "{error:#}"
    );
    refuse_chaos_instruction_offset(&bound.det_opts.det_config, Backend::Ptrace.as_str(), false)
        .unwrap();

    // A readable record, so that validation reaches the offset refusal
    // rather than stopping where it first reads the record (the drawn
    // timeslice it may state, `resolve_chaos_timeslice_range`).
    let dir = tempfile::tempdir().unwrap();
    let record = dir.path().join("record.json");
    detcore::preemptions::PreemptionRecord::from_sched_events(Vec::new())
        .write_to_disk(&record)
        .unwrap();
    let replay_arg = format!("--replay-preemptions-from={}", record.display());
    for (argv, expected) in [
        (
            vec!["hermit", "run", "--chaos-instruction-offset=4", "fakeprog"],
            "it needs --chaos",
        ),
        (
            vec![
                "hermit",
                "run",
                "--chaos",
                "--chaos-instruction-offset=257",
                "fakeprog",
            ],
            "is above 256",
        ),
        (
            vec![
                "hermit",
                "--backend=kvm",
                "run",
                "--chaos",
                "--chaos-instruction-offset=4",
                "--",
                "fakeprog",
            ],
            "backend `kvm` is refused",
        ),
        (
            vec![
                "hermit",
                "--backend=e9patch",
                "run",
                "--chaos",
                "--chaos-instruction-offset=4",
                "--",
                "fakeprog",
            ],
            "backend `e9patch` is refused",
        ),
        (
            vec![
                "hermit",
                "run",
                "--chaos",
                "--chaos-instruction-offset=4",
                "--imprecise-timers",
                "fakeprog",
            ],
            "--imprecise-timers takes no single steps",
        ),
        (
            vec![
                "hermit",
                "run",
                "--chaos",
                "--chaos-instruction-offset=4",
                "--max-timeslice=disabled",
                "fakeprog",
            ],
            "--max-timeslice=disabled turns off",
        ),
        (
            vec![
                "hermit",
                "run",
                "--chaos",
                "--chaos-instruction-offset=4",
                replay_arg.as_str(),
                "fakeprog",
            ],
            "a replay takes its offsets from the record",
        ),
        (
            vec![
                "hermit",
                "run",
                "--chaos",
                "--chaos-instruction-offset=4",
                "--strict-schedule-boundaries",
                "fakeprog",
            ],
            "--strict-schedule-boundaries",
        ),
    ] {
        let error = run_opts_for(&argv)
            .validate_args_with_perf_support(true)
            .expect_err(&format!("{argv:?} should be refused"));
        assert!(
            format!("{error:#}").contains(expected),
            "{argv:?}: {error:#}"
        );
    }
}

/// `--futex-wake-yields` parses, is rendered into the reproduce
/// command and round-trips. It is refused where it cannot apply: without
/// sequentialized threads, outside the precise futex mode, and on every
/// backend but ptrace, where its handoff has not been tested.
#[test]
fn futex_wake_yields_parses_round_trips_and_is_refused_where_inert() {
    let mut opts = RunOpts::parse_from(["fakehermit", "--futex-wake-yields", "fakeprog"]);
    opts.validate_args_with_perf_support(true).unwrap();
    assert!(opts.det_opts.det_config.futex_wake_yields);
    let rendered = format!("{opts}");
    assert!(rendered.contains(" --futex-wake-yields"), "{rendered}");
    let mut reparsed_args = vec!["fakehermit".to_owned()];
    reparsed_args.extend(shell_words::split(&rendered).unwrap());
    let mut reparsed = RunOpts::parse_from(reparsed_args);
    reparsed.validate_args_with_perf_support(true).unwrap();
    assert!(reparsed.det_opts.det_config.futex_wake_yields);

    let mut default = RunOpts::parse_from(["fakehermit", "fakeprog"]);
    default.validate_args_with_perf_support(true).unwrap();
    assert!(!default.det_opts.det_config.futex_wake_yields);
    assert!(!format!("{default}").contains("futex-wake"));

    for (argv, expected) in [
        (
            vec![
                "hermit",
                "run",
                "--no-sequentialize-threads",
                "--futex-wake-yields",
                "fakeprog",
            ],
            "--no-sequentialize-threads",
        ),
        (
            vec![
                "hermit",
                "run",
                "--debug-futex-mode=polling",
                "--futex-wake-yields",
                "fakeprog",
            ],
            "--debug-futex-mode=precise",
        ),
        (
            vec![
                "hermit",
                "--backend=liteinst",
                "run",
                "--futex-wake-yields",
                "fakeprog",
            ],
            "--futex-wake-yields has been tested only on the ptrace backend",
        ),
        (
            vec![
                "hermit",
                "--backend=kvm",
                "run",
                "--futex-wake-yields",
                "fakeprog",
            ],
            "--futex-wake-yields has been tested only on the ptrace backend",
        ),
        (
            vec![
                "hermit",
                "run",
                "--namespace-only",
                "--futex-wake-yields",
                "fakeprog",
            ],
            "--namespace-only (--lite) runs the guest natively",
        ),
        (
            vec!["hermit", "run", "--lite", "--futex-wake-yields", "fakeprog"],
            "--namespace-only (--lite) runs the guest natively",
        ),
        (
            vec![
                "hermit",
                "run",
                "--strace-only",
                "--futex-wake-yields",
                "fakeprog",
            ],
            "--strace-only turns sequentialization off",
        ),
    ] {
        let error = run_opts_for(&argv)
            .validate_args_with_perf_support(true)
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{argv:?}: {error}");
    }
}

/// `--work-span-report` is refused wherever the report would be empty or
/// lost: without sequentialized threads, with `--strace-only` or
/// `--namespace-only`, with `--image`
/// (whose root the report would be written into), and on backends other than
/// ptrace.
#[test]
fn work_span_report_is_refused_where_there_are_no_turns() {
    let mut accepted = run_opts_for(&["hermit", "run", "--work-span-report=/var/ws.json", "prog"]);
    accepted.validate_args_with_perf_support(true).unwrap();
    for (argv, expected) in [
        (
            vec![
                "hermit",
                "run",
                "--no-sequentialize-threads",
                "--work-span-report=/tmp/ws.json",
                "prog",
            ],
            "--no-sequentialize-threads",
        ),
        (
            vec![
                "hermit",
                "run",
                "--strace-only",
                "--work-span-report=/tmp/ws.json",
                "prog",
            ],
            "--strace-only",
        ),
        (
            vec![
                "hermit",
                "--backend=kvm",
                "run",
                "--work-span-report=/var/ws.json",
                "prog",
            ],
            "backend `kvm` is refused",
        ),
        (
            vec![
                "hermit",
                "run",
                "--namespace-only",
                "--work-span-report=/var/ws.json",
                "prog",
            ],
            "--namespace-only",
        ),
        (
            vec![
                "hermit",
                "run",
                "--image=busybox",
                "--work-span-report=/var/ws.json",
                "prog",
            ],
            "--image",
        ),
        (
            vec![
                "hermit",
                "--backend=liteinst",
                "run",
                "--work-span-report=/var/ws.json",
                "prog",
            ],
            "backend `liteinst` is refused",
        ),
    ] {
        let error = run_opts_for(&argv)
            .validate_args_with_perf_support(true)
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{argv:?}: {error}");
    }
}

/// Why `--watch` (or `--watch-from`) cannot run as given, if it cannot: its
/// hits go to the access trace, the backend must set hardware watchpoints
/// (`slots`, the backend's count, `None` when it sets none), and a thread
/// holds at most that many.
fn watch_refusal(config: &DetConfig, slots: u8, backend: &str) -> Option<String> {
    let Some(first) = config.watch.first() else {
        return (config.watch_from != 0).then(|| {
            format!(
                "--watch-from={} only says when --watch arms; add --watch=ADDR, or drop it",
                config.watch_from
            )
        });
    };
    if config.access_trace.is_none() {
        return Some(format!(
            "--watch={first} records its hits in the access trace; add \
             --record-access-trace-to=PATH, or drop --watch"
        ));
    }
    match slots {
        0 => Some(format!(
            "--watch needs hardware watchpoints, and backend `{backend}` sets none on this \
             architecture; use --backend=ptrace on x86-64, or drop --watch"
        )),
        slots if config.watch.len() > usize::from(slots) => Some(format!(
            "{} --watch flags, and a thread holds {slots} hardware watchpoints; drop {}",
            config.watch.len(),
            config.watch.len() - usize::from(slots)
        )),
        _ => None,
    }
}

/// `--watch` is refused where it would watch nothing or could not arm: without
/// the access trace its hits go to, beyond the backend's watchpoint count, on
/// a backend without watchpoints, and `--watch-from` without a watch.
#[test]
fn watch_is_refused_where_it_could_not_arm_or_report() {
    // Four-slot admission and capacity go through the `watch_refusal`
    // seam with the slot count stated, so this test does not assume the
    // host's architecture (off x86-64 the backend advertises 0 slots):
    // one watch is admitted, five are refused naming the four slots.
    let config = |count: u64| DetConfig {
        access_trace: Some("/var/a".into()),
        watch: (0..count)
            .map(|i| format!("{:#x}", 0x404010 + 8 * i).parse().unwrap())
            .collect(),
        ..DetConfig::default()
    };
    assert_eq!(watch_refusal(&config(1), 4, "ptrace"), None);
    let error = watch_refusal(&config(5), 4, "ptrace").expect("five watches");
    assert!(
        error.contains("5 --watch flags, and a thread holds 4 hardware watchpoints; drop 1"),
        "{error}"
    );
    // A watch the hardware cannot hold does not parse.
    let misaligned = <crate::app::Args as clap::CommandFactory>::command()
        .try_get_matches_from(["hermit", "run", "--watch=0x404014:8", "prog"])
        .unwrap_err()
        .to_string();
    assert!(
        misaligned.contains("use 0x404010:8 or a shorter length"),
        "{misaligned}"
    );

    let no_trace = run_opts_for(&["hermit", "run", "--watch=0x404010", "prog"])
        .validate_args_with_perf_support(true)
        .unwrap_err()
        .to_string();
    assert!(
        no_trace.contains("add --record-access-trace-to=PATH, or drop --watch"),
        "{no_trace}"
    );
    let from_alone = run_opts_for(&["hermit", "run", "--watch-from=5", "prog"])
        .validate_args_with_perf_support(true)
        .unwrap_err()
        .to_string();
    assert!(
        from_alone.contains("--watch-from=5 only says when --watch arms"),
        "{from_alone}"
    );

    // A backend without watchpoints (every one but ptrace, and ptrace off
    // x86-64) refuses, naming itself.
    let config = DetConfig {
        access_trace: Some("/var/a".into()),
        watch: vec!["0x404010".parse().unwrap()],
        ..DetConfig::default()
    };
    let error = watch_refusal(&config, 0, "kvm").expect("no watchpoints");
    assert!(
        error.contains("backend `kvm` sets none") && error.contains("--backend=ptrace"),
        "{error}"
    );
    assert_eq!(watch_refusal(&config, 4, "ptrace"), None);
}

/// `--record-access-trace-to` is refused wherever its records would name no
/// recorded schedule or no turns: without a recorded schedule, without
/// sequentialized threads, with `--namespace-only`, and on backends other
/// than ptrace. `--access-trace-from`, `--access-trace-max-records` and
/// `--access-trace-max-bytes` without the trace are refused too.
/// Each refusal names the flag to add or drop.
#[test]
fn access_trace_is_refused_where_its_records_would_name_nothing() {
    let mut accepted = run_opts_for(&[
        "hermit",
        "run",
        "--record-preemptions-to=/var/run.events",
        "--record-access-trace-to=/var/run.access",
        "--access-trace-from=10",
        "prog",
    ]);
    accepted.validate_args_with_perf_support(true).unwrap();
    for (argv, expected) in [
        (
            vec!["hermit", "run", "--record-access-trace-to=/var/a", "prog"],
            "add --record-preemptions-to=PATH",
        ),
        (
            vec![
                "hermit",
                "run",
                "--no-sequentialize-threads",
                "--record-preemptions-to=/var/e",
                "--record-access-trace-to=/var/a",
                "prog",
            ],
            "drop --no-sequentialize-threads, or drop --record-access-trace-to",
        ),
        (
            vec![
                "hermit",
                "run",
                "--strace-only",
                "--record-preemptions-to=/var/e",
                "--record-access-trace-to=/var/a",
                "prog",
            ],
            "drop --strace-only, or drop --record-access-trace-to",
        ),
        (
            vec![
                "hermit",
                "run",
                "--image=busybox",
                "--record-preemptions-to=/var/e",
                "--record-access-trace-to=/var/a",
                "prog",
            ],
            "drop --image, or drop --record-access-trace-to",
        ),
        (
            vec![
                "hermit",
                "run",
                "--namespace-only",
                "--record-preemptions-to=/var/e",
                "--record-access-trace-to=/var/a",
                "prog",
            ],
            "drop --namespace-only, or drop --record-access-trace-to",
        ),
        (
            vec![
                "hermit",
                "--backend=kvm",
                "run",
                "--record-preemptions-to=/var/e",
                "--record-access-trace-to=/var/a",
                "prog",
            ],
            "backend `kvm` is refused: use --backend=ptrace",
        ),
        (
            vec!["hermit", "run", "--access-trace-from=5", "prog"],
            "--access-trace-from=5 only shapes an access trace",
        ),
        (
            vec!["hermit", "run", "--access-trace-max-records=5", "prog"],
            "--access-trace-max-records=5 only shapes an access trace; add \
             --record-access-trace-to=PATH, or drop it",
        ),
        (
            vec!["hermit", "run", "--access-trace-max-bytes=4096", "prog"],
            "--access-trace-max-bytes=4096 only shapes an access trace; add \
             --record-access-trace-to=PATH, or drop it",
        ),
    ] {
        let error = run_opts_for(&argv)
            .validate_args_with_perf_support(true)
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{argv:?}: {error}");
    }
    let hidden = run_opts_for(&[
        "hermit",
        "run",
        "--record-preemptions-to=/var/e",
        "--record-access-trace-to=/var/../tmp/a",
        "prog",
    ]);
    let error = hidden
        .validate_access_trace_visibility()
        .unwrap_err()
        .to_string();
    assert!(error.contains("`..` component"), "{error}");
}

/// A run manifest leaves the access trace out, as it does every output
/// destination, and `--access-trace-from`, `--access-trace-max-records`,
/// `--access-trace-max-bytes`, `--watch` and `--watch-from` with it, which a
/// rerun without the trace would refuse.
#[test]
fn a_manifest_leaves_out_the_access_trace() {
    let argv = [
        "hermit",
        "run",
        "--record-preemptions-to=/var/e",
        "--record-access-trace-to=/var/a",
        "--access-trace-from=4",
        "--access-trace-max-records=9",
        "--access-trace-max-bytes=4096",
        "--watch=0x404010",
        "--watch-from=7",
        "/bin/true",
    ];
    let matches = <crate::app::Args as clap::CommandFactory>::command()
        .try_get_matches_from(argv)
        .unwrap();
    let args = crate::app::args_from_matches_with_clock(&matches, SystemTime::now).unwrap();
    let crate::app::Subcommand::Run(ro) = args.command else {
        panic!("{argv:?} is not a `run` command");
    };
    let config = ro.saved_run_config().unwrap();
    let (mut global, mut run) = crate::app::run_config::to_manifest_parts(&config).unwrap();
    assert!(run.contains_key("record-access-trace-to"), "{run:?}");
    assert!(run.contains_key("access-trace-from"), "{run:?}");
    assert!(run.contains_key("access-trace-max-records"), "{run:?}");
    assert!(run.contains_key("access-trace-max-bytes"), "{run:?}");
    assert!(
        run.contains_key("watch") && run.contains_key("watch-from"),
        "{run:?}"
    );
    crate::app::manifest::drop_outputs(&mut global, &mut run);
    assert!(!run.contains_key("record-access-trace-to"), "{run:?}");
    assert!(!run.contains_key("access-trace-from"), "{run:?}");
    assert!(!run.contains_key("access-trace-max-records"), "{run:?}");
    assert!(!run.contains_key("access-trace-max-bytes"), "{run:?}");
    assert!(
        !run.contains_key("watch") && !run.contains_key("watch-from"),
        "{run:?}"
    );
}

/// A run manifest leaves `--work-span-report` out, as it does every output
/// destination, so a rerun with `--from` does not overwrite the original
/// report; the saved run configuration keeps it.
#[test]
fn a_manifest_leaves_out_the_work_span_report() {
    let argv = [
        "hermit",
        "run",
        "--work-span-report=/var/ws.json",
        "/bin/true",
    ];
    let matches = <crate::app::Args as clap::CommandFactory>::command()
        .try_get_matches_from(argv)
        .unwrap();
    let args = crate::app::args_from_matches_with_clock(&matches, SystemTime::now).unwrap();
    let crate::app::Subcommand::Run(ro) = args.command else {
        panic!("{argv:?} is not a `run` command");
    };
    let config = ro.saved_run_config().unwrap();
    let (mut global, mut run) = crate::app::run_config::to_manifest_parts(&config).unwrap();
    assert!(run.contains_key("work-span-report"), "{run:?}");
    crate::app::manifest::drop_outputs(&mut global, &mut run);
    assert!(!run.contains_key("work-span-report"), "{run:?}");
}

/// A report path with a `..` component, which the container can resolve
/// into its private `/tmp`, is refused before the guest runs.
#[test]
fn a_work_span_report_path_with_a_parent_component_is_refused() {
    let ro = run_opts_for(&[
        "hermit",
        "run",
        "--work-span-report=/var/../tmp/ws.json",
        "prog",
    ]);
    let error = ro
        .validate_work_span_report_visibility()
        .unwrap_err()
        .to_string();
    assert!(error.contains("`..` component"), "{error}");
    let plain = run_opts_for(&["hermit", "run", "--work-span-report=/var/ws.json", "prog"]);
    plain.validate_work_span_report_visibility().unwrap();
}

/// `--signal-at` is accepted on the ptrace backend, and refused before launch
/// wherever it would not reach the guest: every other backend (on KVM a host
/// kill reaches the sandbox monitor), `--namespace-only` and `--lite`,
/// `--strace-only` and `--no-sequentialize-threads`.
#[test]
fn signal_at_is_refused_where_it_would_not_reach_the_guest() {
    let argv = [
        "hermit",
        "--backend=ptrace",
        "run",
        "--signal-at=turn=5:SIGUSR1",
        "fakeprog",
    ];
    if let Err(error) = run_opts_for(&argv).validate_args_with_perf_support(true) {
        assert!(
            !error.to_string().contains("signal-at"),
            "{argv:?}: {error}"
        );
    }
    let mut refused: Vec<(Vec<String>, String)> = ["liteinst", "in-guest-trap", "kvm"]
        .iter()
        .map(|backend| {
            (
                vec![format!("--backend={backend}"), "run".to_owned()],
                format!("backend `{backend}` is refused"),
            )
        })
        .collect();
    for (flag, expected) in [
        (
            "--namespace-only",
            "--namespace-only (--lite) runs the guest natively",
        ),
        (
            "--lite",
            "--namespace-only (--lite) runs the guest natively",
        ),
        ("--strace-only", "--strace-only turns sequentialization off"),
        (
            "--no-sequentialize-threads",
            "cannot be combined with --no-sequentialize-threads",
        ),
    ] {
        refused.push((vec!["run".to_owned(), flag.to_owned()], expected.to_owned()));
    }
    for (prefix, expected) in refused {
        let mut argv = vec!["hermit".to_owned()];
        argv.extend(prefix);
        argv.extend([
            "--signal-at=turn=5:SIGUSR1".to_owned(),
            "fakeprog".to_owned(),
        ]);
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        let error = run_opts_for(&argv)
            .validate_args_with_perf_support(true)
            .unwrap_err();
        assert!(error.to_string().contains(&expected), "{argv:?}: {error}");
    }
}

/// `--pipe-wake-latency` parses, is rendered into the reproduce command and
/// round-trips, keeping `0` distinct from unset. It is refused wherever it
/// would be ignored: without sequentialized threads, with `--namespace-only`
/// (`--lite`) or `--strace-only`, under every `--sched-heuristic` that does
/// not select by priority (the same predicate the scheduler uses), and on
/// every backend but ptrace (https://github.com/rrnewton/hermit/issues/4380).
#[test]
fn pipe_wake_latency_parses_round_trips_and_is_refused_where_inert() {
    for value in ["0", "3000000"] {
        let flag = format!("--pipe-wake-latency={value}");
        let mut opts = RunOpts::parse_from(["fakehermit", flag.as_str(), "fakeprog"]);
        opts.validate_args_with_perf_support(true).unwrap();
        let expected = Some(value.parse::<u64>().unwrap());
        assert_eq!(opts.det_opts.det_config.pipe_wake_latency, expected);
        let rendered = format!("{opts}");
        assert!(rendered.contains(&format!(" {flag}")), "{rendered}");
        let mut reparsed_args = vec!["fakehermit".to_owned()];
        reparsed_args.extend(shell_words::split(&rendered).unwrap());
        let mut reparsed = RunOpts::parse_from(reparsed_args);
        reparsed.validate_args_with_perf_support(true).unwrap();
        assert_eq!(reparsed.det_opts.det_config.pipe_wake_latency, expected);
    }
    let mut default = RunOpts::parse_from(["fakehermit", "fakeprog"]);
    default.validate_args_with_perf_support(true).unwrap();
    assert_eq!(default.det_opts.det_config.pipe_wake_latency, None);
    assert!(!format!("{default}").contains("pipe-wake"));

    for heuristic in ["none", "connectbind", "random", "stickyrandom"] {
        let parsed: detcore::SchedHeuristic = heuristic.parse().unwrap();
        let flag = format!("--sched-heuristic={heuristic}");
        let argv = [
            "hermit",
            "run",
            flag.as_str(),
            "--pipe-wake-latency=0",
            "fakeprog",
        ];
        let result = run_opts_for(&argv).validate_args_with_perf_support(true);
        assert_eq!(
            result.is_ok(),
            parsed.is_priority_ordered(),
            "{argv:?}: {result:?}"
        );
    }

    for (argv, expected) in [
        (
            vec![
                "hermit",
                "run",
                "--no-sequentialize-threads",
                "--pipe-wake-latency=0",
                "fakeprog",
            ],
            "--no-sequentialize-threads",
        ),
        (
            vec![
                "hermit",
                "--backend=liteinst",
                "run",
                "--pipe-wake-latency=0",
                "fakeprog",
            ],
            "--pipe-wake-latency has been tested only on the ptrace backend",
        ),
        (
            vec![
                "hermit",
                "run",
                "--debug-externalize-sockets",
                "--pipe-wake-latency=0",
                "fakeprog",
            ],
            "--debug-externalize-sockets makes them block in the kernel",
        ),
        (
            vec![
                "hermit",
                "run",
                "--namespace-only",
                "--pipe-wake-latency=0",
                "fakeprog",
            ],
            "--namespace-only (--lite) runs the guest natively",
        ),
        (
            vec![
                "hermit",
                "run",
                "--lite",
                "--pipe-wake-latency=0",
                "fakeprog",
            ],
            "--namespace-only (--lite) runs the guest natively",
        ),
        (
            vec![
                "hermit",
                "run",
                "--strace-only",
                "--pipe-wake-latency=0",
                "fakeprog",
            ],
            "--strace-only turns sequentialization off",
        ),
        (
            vec![
                "hermit",
                "run",
                "--sched-heuristic=random",
                "--pipe-wake-latency=0",
                "fakeprog",
            ],
            "does not select by priority",
        ),
    ] {
        let error = run_opts_for(&argv)
            .validate_args_with_perf_support(true)
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{argv:?}: {error}");
    }
}

/// `--cap-preemption-charge` parses, is rendered into the reproduce command
/// and round-trips. It is refused where no turn ends an expired timeslice:
/// without a timeslice (given or downgraded for a host without a PMU timer),
/// without Detcore's sequentializing scheduler. Executable-selected DBT is
/// refused by application admission while this option remains unqualified.
/// A logical `--target-timeslice` without the PMU maximum still ends slices.
#[test]
fn cap_preemption_charge_parses_round_trips_and_is_refused_where_inert() {
    let mut opts = RunOpts::parse_from(["fakehermit", "--cap-preemption-charge", "fakeprog"]);
    opts.validate_args_with_perf_support(true).unwrap();
    assert!(opts.det_opts.det_config.cap_preemption_charge);
    let rendered = format!("{opts}");
    assert!(rendered.contains(" --cap-preemption-charge"), "{rendered}");
    let mut reparsed_args = vec!["fakehermit".to_owned()];
    reparsed_args.extend(shell_words::split(&rendered).unwrap());
    let mut reparsed = RunOpts::parse_from(reparsed_args);
    reparsed.validate_args_with_perf_support(true).unwrap();
    assert!(reparsed.det_opts.det_config.cap_preemption_charge);

    let mut default = RunOpts::parse_from(["fakehermit", "fakeprog"]);
    default.validate_args_with_perf_support(true).unwrap();
    assert!(!default.det_opts.det_config.cap_preemption_charge);
    assert!(!format!("{default}").contains("preemption-charge"));

    let mut logical = RunOpts::parse_from([
        "fakehermit",
        "--cap-preemption-charge",
        "--max-timeslice=disabled",
        "--target-timeslice=1000000",
        "fakeprog",
    ]);
    logical.validate_args_with_perf_support(true).unwrap();

    let error = RunOpts::parse_from(["fakehermit", "--cap-preemption-charge", "fakeprog"])
        .validate_args_with_perf_support(false)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("no timeslice ends by expiry because this host's PMU timer is unavailable")
            && error.contains("Give a --max-timeslice"),
        "{error}"
    );

    let mut application =
        RunOpts::parse_from(["fakehermit", "--cap-preemption-charge", "fakeprog"]);
    application.select_application_backend("dynamorio").unwrap();
    let error = application
        .refuse_unqualified_application_options(
            <super::core_adapter::CoreApplicationAdapter as
                super::backend_hook::BackendAdapter>::NONDEFAULT_CPU_PROFILE_REFUSAL,
        )
        .unwrap_err();
    assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
    assert!(
        error.to_string().contains("--cap-preemption-charge"),
        "{error:#}"
    );
    assert!(error.to_string().contains("`dynamorio`"), "{error:#}");

    for (argv, expected) in [
        (
            vec![
                "hermit",
                "run",
                "--max-timeslice=disabled",
                "--cap-preemption-charge",
                "fakeprog",
            ],
            "no timeslice ends by expiry because --max-timeslice=disabled",
        ),
        (
            vec![
                "hermit",
                "run",
                "--no-sequentialize-threads",
                "--cap-preemption-charge",
                "fakeprog",
            ],
            "Drop --no-sequentialize-threads or --cap-preemption-charge",
        ),
        (
            vec![
                "hermit",
                "run",
                "--strace-only",
                "--cap-preemption-charge",
                "fakeprog",
            ],
            "Drop --strace-only or --cap-preemption-charge",
        ),
        (
            vec![
                "hermit",
                "run",
                "--namespace-only",
                "--cap-preemption-charge",
                "fakeprog",
            ],
            "Drop --namespace-only or --cap-preemption-charge",
        ),
        (
            vec![
                "hermit",
                "--backend=dbt",
                "run",
                "--cap-preemption-charge",
                "--",
                "fakeprog",
            ],
            "--cap-preemption-charge is not qualified for executable-selected backend `dbt`",
        ),
    ] {
        let mut opts = run_opts_for(&argv);
        let error = if argv.contains(&"--backend=dbt") {
            assert_eq!(opts.application_backend, Some("dbt"));
            assert_eq!(
                opts.adapter_capabilities,
                Some(reverie::BackendCapabilities::DBT)
            );
            let error = opts
                .refuse_unqualified_application_options(
                    <super::core_adapter::CoreApplicationAdapter as
                        super::backend_hook::BackendAdapter>::NONDEFAULT_CPU_PROFILE_REFUSAL,
                )
                .err()
                .unwrap_or_else(|| panic!("{argv:?} was accepted"));
            assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
            assert!(
                error.to_string().contains("--cap-preemption-charge"),
                "{error:#}"
            );
            assert!(error.to_string().contains("`dbt`"), "{error:#}");
            assert_eq!(error.to_string(), expected, "{argv:?}: {error}");
            error
        } else {
            opts.validate_args_with_perf_support(true)
                .err()
                .unwrap_or_else(|| panic!("{argv:?} was accepted"))
        };
        assert!(error.to_string().contains(expected), "{argv:?}: {error}");
    }
}

/// A replay of a record made under the other `--cap-preemption-charge`
/// setting is refused before launch with the setting to replay with; the
/// recorded setting is accepted.
#[test]
fn a_replay_under_another_cap_preemption_charge_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("schedule.json");
    let record = detcore::preemptions::PreemptionWriter::new(None)
        .with_cap_preemption_charge(true)
        .into_string();
    std::fs::write(&path, record).unwrap();
    let replay = |extra: &[&str]| {
        let schedule = format!("--replay-schedule-from={}", path.display());
        let mut argv = vec!["fakehermit", schedule.as_str()];
        argv.extend_from_slice(extra);
        argv.push("fakeprog");
        RunOpts::parse_from(argv).refuse_replay_under_another_cap_preemption_charge()
    };
    replay(&["--cap-preemption-charge"]).unwrap();
    let error = replay(&[]).unwrap_err();
    assert!(
        format!("{error:#}").contains("Replay with --cap-preemption-charge."),
        "{error:#}"
    );
}

/// `--scheduler-turn-cost` parses, is rendered into the reproduce command and
/// round-trips; zero, and a cost the clock multiplier scales below one
/// nanosecond or past the bound, are refused.
#[test]
fn scheduler_turn_cost_parses_round_trips_and_is_refused_where_out_of_range() {
    let mut opts = RunOpts::parse_from(["fakehermit", "--scheduler-turn-cost=10000", "fakeprog"]);
    opts.validate_args_with_perf_support(true).unwrap();
    assert_eq!(
        opts.det_opts.det_config.scheduler_turn_cost,
        std::num::NonZeroU64::new(10_000)
    );
    let rendered = format!("{opts}");
    assert!(
        rendered.contains(" --scheduler-turn-cost=10000"),
        "{rendered}"
    );
    let mut reparsed_args = vec!["fakehermit".to_owned()];
    reparsed_args.extend(shell_words::split(&rendered).unwrap());
    let reparsed = RunOpts::parse_from(reparsed_args);
    assert_eq!(
        reparsed.det_opts.det_config.scheduler_turn_cost,
        std::num::NonZeroU64::new(10_000)
    );
    let mut default = RunOpts::parse_from(["fakehermit", "fakeprog"]);
    default.validate_args_with_perf_support(true).unwrap();
    assert!(!format!("{default}").contains("turn-cost"));
    assert!(
        RunOpts::try_parse_from(["fakehermit", "--scheduler-turn-cost=0", "fakeprog"]).is_err(),
        "a zero charge would freeze time across turns"
    );
    // A cost that the multiplier scales below one nanosecond would charge
    // nothing per turn; a cost above the bound is refused too.
    for args in [
        vec!["--scheduler-turn-cost=9", "--clock-multiplier=0.1"],
        vec!["--scheduler-turn-cost=1000000001", "--clock-multiplier=1"],
        // A permitted cost that the multiplier scales past the bound.
        vec![
            "--scheduler-turn-cost=1000000000",
            "--clock-multiplier=9000000000",
            "--max-timeslice=disabled",
        ],
    ] {
        let mut argv = vec!["fakehermit"];
        argv.extend(args.iter().copied());
        argv.push("fakeprog");
        let error = RunOpts::parse_from(argv)
            .validate_args_with_perf_support(true)
            .unwrap_err();
        assert!(
            error.to_string().contains("--scheduler-turn-cost"),
            "{args:?}: {error}"
        );
    }
    let mut smallest = RunOpts::parse_from([
        "fakehermit",
        "--scheduler-turn-cost=10",
        "--clock-multiplier=0.1",
        "fakeprog",
    ]);
    smallest.validate_args_with_perf_support(true).unwrap();
}

#[test]
fn virtual_timeout_with_debugger_is_refused_before_dispatch_and_validation() {
    for debugger in ["--dap-listen=127.0.0.1:0", "--gdbserver"] {
        for bounded in [false, true] {
            let mut argv = vec!["fakehermit", debugger];
            if bounded {
                argv.push("--virtual-timeout=5000000000");
            }
            argv.push("/nonexistent/guest");
            let options = RunOpts::parse_from(argv);
            let mut dispatch = options.clone();
            let result = dispatch.dispatch_before_validation(None, None);
            if bounded {
                let error = result.unwrap_err();
                assert!(
                    error.is::<detcore_model::virtual_timeout::VirtualTimeoutWithDebugger>(),
                    "{error}"
                );
                for timer_available in [false, true] {
                    let error = options
                        .clone()
                        .validate_args_with_perf_support(timer_available)
                        .unwrap_err();
                    assert!(
                        error.is::<detcore_model::virtual_timeout::VirtualTimeoutWithDebugger>(),
                        "{error}"
                    );
                }
                let error = options.clone().validate_args().unwrap_err();
                assert!(
                    error.is::<detcore_model::virtual_timeout::VirtualTimeoutWithDebugger>(),
                    "{error}"
                );
            } else {
                assert_eq!(
                    result.unwrap(),
                    if options.dap_listen.is_some() {
                        RunDispatch::DapSupervisor
                    } else {
                        RunDispatch::Run
                    }
                );
                options
                    .clone()
                    .validate_args_with_perf_support(true)
                    .unwrap();
            }
        }
    }
    let mut bounded = RunOpts::parse_from([
        "fakehermit",
        "--virtual-timeout=5000000000",
        "/nonexistent/guest",
    ]);
    assert_eq!(
        bounded.dispatch_before_validation(None, None).unwrap(),
        RunDispatch::Run
    );
    bounded.validate_args_with_perf_support(true).unwrap();
}

#[test]
fn virtual_timeout_with_debugger_is_refused_before_raw_container_launch() {
    // Occupied port makes an accidentally reached bind fail promptly. The
    // required policy refusal must precede that bind, not merely guest work.
    let held = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = format!("--gdbserver-port={}", held.local_addr().unwrap().port());
    let options = RunOpts::parse_from([
        "fakehermit",
        "--gdbserver",
        port.as_str(),
        "--virtual-timeout=5000000000",
        "/nonexistent/guest",
    ]);
    let result = options.launch_in_container(
        (),
        "debugger admission test",
        None,
        |_, _, _| -> Result<(), Error> { panic!("refused combination reached container work") },
    );
    let error = result.unwrap_err();
    assert!(
        error.is::<detcore_model::virtual_timeout::VirtualTimeoutWithDebugger>(),
        "{error}"
    );
}

/// `--virtual-timeout` takes virtual nanoseconds (not zero, not a unit), is
/// rendered into the reproduce command and round-trips; its help says what it
/// bounds and how it differs from `--timeout`. A launch without Detcore's
/// sequentializing scheduler, which is where the bound is checked, is refused
/// with a message that names the cause and the way out.
#[test]
fn virtual_timeout_parses_round_trips_and_is_refused_without_the_scheduler() {
    use clap::CommandFactory;

    let mut opts = RunOpts::parse_from(["fakehermit", "--virtual-timeout=3500000000", "fakeprog"]);
    opts.validate_args_with_perf_support(true).unwrap();
    assert_eq!(
        opts.det_opts.det_config.virtual_timeout,
        std::num::NonZeroU64::new(3_500_000_000)
    );
    let rendered = format!("{opts}");
    assert!(
        rendered.contains(" --virtual-timeout=3500000000"),
        "{rendered}"
    );
    let mut reparsed_args = vec!["fakehermit".to_owned()];
    reparsed_args.extend(shell_words::split(&rendered).unwrap());
    assert_eq!(
        RunOpts::parse_from(reparsed_args)
            .det_opts
            .det_config
            .virtual_timeout,
        std::num::NonZeroU64::new(3_500_000_000)
    );
    for value in ["0", "2s", "-1"] {
        let flag = format!("--virtual-timeout={value}");
        assert!(
            RunOpts::try_parse_from(["fakehermit", flag.as_str(), "fakeprog"]).is_err(),
            "{flag}"
        );
    }

    let help = RunOpts::command().render_long_help().to_string();
    let help = help.split_whitespace().collect::<Vec<_>>().join(" ");
    for expected in [
        "--virtual-timeout <virtual-nanoseconds>",
        "HERMIT_RUN_TIMEOUT class=virtual-timeout bound_ns=N",
        "the same virtual time `--summary` reports as elapsed (`virttime_elapsed`",
        "Unlike `--timeout`, a wall-clock bound",
        "(`class=run-timeout`)",
        "fires at the same point of the same schedule on every host and in every run",
        "Also refused before guest or debugger startup with `--gdbserver` or `--dap-listen`",
    ] {
        assert!(
            help.contains(expected),
            "{expected:?} missing from:\n{help}"
        );
    }

    for (argv, cause) in [
        (
            vec!["hermit", "run", "--no-sequentialize-threads"],
            "--no-sequentialize-threads",
        ),
        (vec!["hermit", "run", "--strace-only"], "--strace-only"),
        (
            vec!["hermit", "run", "--namespace-only"],
            "--namespace-only",
        ),
        (
            vec![
                "hermit",
                "run",
                "--no-virtualize-time",
                "--no-virtualize-metadata",
            ],
            "with --no-virtualize-time the run's summary reports no elapsed virtual time",
        ),
    ] {
        let mut argv = argv;
        argv.extend(["--virtual-timeout=1000", "fakeprog"]);
        let error = run_opts_for(&argv)
            .validate_args_with_perf_support(true)
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("--virtual-timeout=1000 bounds the virtual time")
                && error.contains(cause)
                && error.contains("bound wall-clock time with --timeout instead"),
            "{argv:?}: {error}"
        );
    }
    // Fail-closed: only the backends a run of the bound was measured on.
    for (backend, cause) in [
        ("liteinst", "measured only on the ptrace and kvm backends"),
        (
            "in-guest-trap",
            "measured only on the ptrace and kvm backends",
        ),
    ] {
        let flag = format!("--backend={backend}");
        let argv = [
            "hermit",
            flag.as_str(),
            "run",
            "--max-timeslice=disabled",
            "--virtual-timeout=1000",
            "fakeprog",
        ];
        let error = run_opts_for(&argv)
            .validate_args_with_perf_support(true)
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with(&format!(
                "--virtual-timeout=1000 is not supported on the `{backend}` backend: "
            )) && error.contains(cause)
                && error.contains("Run it with --backend=ptrace or --backend=kvm"),
            "{argv:?}: {error}"
        );
    }
    // A comparison needs the ptrace runtime, which finishes a stopped run;
    // KVM's scheduler exits the container child instead. The refusal names
    // the flag that was given.
    for verify in [
        &["--verify"][..],
        &["--verify-run", "/tmp/never-created"][..],
    ] {
        let mut argv = vec!["hermit", "--backend=kvm", "run"];
        argv.extend_from_slice(verify);
        argv.extend(["--virtual-timeout=1000", "fakeprog"]);
        let error = run_opts_for(&argv)
            .validate_args_with_perf_support(true)
            .unwrap_err()
            .to_string();
        let flag = verify[0];
        assert!(
            error.starts_with(&format!(
                "--virtual-timeout=1000 with {flag} is supported only on the ptrace backend"
            )) && error.ends_with(&format!("Run it with --backend=ptrace, or drop {flag}")),
            "{argv:?}: {error}"
        );
    }
    for argv in [
        &[
            "hermit",
            "run",
            "--verify",
            "--virtual-timeout=1000",
            "fakeprog",
        ][..],
        &[
            "hermit",
            "--backend=kvm",
            "run",
            "--virtual-timeout=1000",
            "fakeprog",
        ][..],
    ] {
        run_opts_for(argv)
            .validate_args_with_perf_support(true)
            .unwrap_or_else(|error| panic!("{argv:?}: {error}"));
    }
}

#[cfg(test)]
fn liteinst_in_guest_refusal(options: &[&str]) -> Result<(), anyhow::Error> {
    in_guest_refusal("--backend=liteinst", options)
}

#[cfg(test)]
fn in_guest_refusal(backend_flag: &str, options: &[&str]) -> Result<(), anyhow::Error> {
    let mut argv = vec!["hermit", backend_flag, "run"];
    argv.extend_from_slice(options);
    argv.push("fakeprog");
    let run = run_opts_for(&argv);
    run.resolve_preemption_settings()?;
    run.refuse_unqualified_liteinst_in_guest_options()
}

/// in-guest-trap runs the in-guest LiteInst runtime, so it refuses exactly
/// the options liteinst refuses, and each refusal names the backend the
/// caller selected.
#[test]
fn in_guest_trap_refuses_the_options_liteinst_refuses() {
    #[cfg(feature = "liteinst")]
    {
        let error = in_guest_refusal("--backend=in-guest-trap", &[]).unwrap_err();
        assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
        assert!(
            error.to_string().contains("pass --max-timeslice=disabled"),
            "{error:#}"
        );
    }
    for option in ["--timeout=3", "--skid-margin=500", "--gdbserver"] {
        let options = ["--max-timeslice=disabled", option];
        let trap = in_guest_refusal("--backend=in-guest-trap", &options).unwrap_err();
        let liteinst = liteinst_in_guest_refusal(&options).unwrap_err();
        assert!(
            trap.downcast_ref::<PolicyRefusal>().is_some(),
            "{option}: {trap:#}"
        );
        assert_eq!(
            trap.to_string()
                .replace("--backend=in-guest-trap", "--backend=liteinst"),
            liteinst.to_string(),
            "{option}"
        );
        assert!(
            trap.to_string()
                .starts_with(if option.starts_with("--skid-margin") {
                    "unsupported_preemption: --backend=in-guest-trap refuses --skid-margin:"
                } else {
                    "--backend=in-guest-trap "
                }),
            "{option}: {trap:#}"
        );
    }
    in_guest_refusal(
        "--backend=in-guest-trap",
        &["--max-timeslice=disabled", "--verify"],
    )
    .unwrap();
    let run = run_opts_for(&[
        "hermit",
        "--backend=in-guest-trap",
        "run",
        "--max-timeslice=disabled",
        "fakeprog",
    ]);
    assert!(run.uses_in_guest_liteinst());
    assert!(run.forwards_in_guest_detlogs());
    assert!(!run.arms_pmu_preemption_timer());
}

#[test]
fn liteinst_in_guest_refuses_options_it_cannot_honour() {
    // Preserve the existing inherited-maximum refusal for the in-guest
    // runtime. The explicit request has the shared named refusal as well.
    #[cfg(feature = "liteinst")]
    {
        let error = liteinst_in_guest_refusal(&[]).unwrap_err();
        assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
        assert!(
            error.to_string().contains("pass --max-timeslice=disabled"),
            "{error:#}"
        );
    }
    // The other refusals hold in every build.
    for (option, reason) in [
        ("--timeout=3", "--timeout has not been qualified"),
        ("--skid-margin=500", "refuses --skid-margin"),
        ("--gdbserver", "--gdbserver needs the ptrace tracer"),
    ] {
        let error = liteinst_in_guest_refusal(&["--max-timeslice=disabled", option]).unwrap_err();
        assert!(
            error.downcast_ref::<PolicyRefusal>().is_some(),
            "{option}: {error:#}"
        );
        assert!(error.to_string().contains(reason), "{option}: {error:#}");
    }
    liteinst_in_guest_refusal(&["--max-timeslice=disabled"]).unwrap();
    // The in-guest Tool forwards its DETLOG records, so `--verify` compares them.
    liteinst_in_guest_refusal(&["--max-timeslice=disabled", "--verify"]).unwrap();
    // A build with the feature never refuses for the feature.
    #[cfg(feature = "liteinst")]
    run_opts_for(&["hermit", "--backend=liteinst", "run", "fakeprog"])
        .refuse_liteinst_without_the_feature()
        .unwrap();
}

/// Only the ptrace backend serves gdb. `--gdbserver` and `--dap-listen` on a
/// backend without a gdbserver (KVM), `--strict` included, and with
/// `--namespace-only`, are refused naming the remedy; the ptrace backend keeps
/// them, and the other backends run without them as before.
#[test]
fn backends_without_a_gdbserver_refuse_gdbserver_and_dap_listen() {
    let refusal = |argv: &[&str]| {
        let mut argv = argv.to_vec();
        argv.push("fakeprog");
        run_opts_for(&argv).refuse_gdbserver_without_a_gdbserver()
    };
    for backend in ["kvm"] {
        let flag = format!("--backend={backend}");
        for (options, option) in [
            (&["--gdbserver"][..], "--gdbserver"),
            (&["--strict", "--gdbserver"], "--gdbserver"),
            (&["--dap-listen=4711"], "--dap-listen"),
        ] {
            let mut argv = vec!["hermit", flag.as_str(), "run"];
            argv.extend_from_slice(options);
            let error = refusal(&argv).unwrap_err();
            let message = error.to_string();
            assert!(
                error.downcast_ref::<PolicyRefusal>().is_some(),
                "{argv:?}: {error:#}"
            );
            assert!(
                message.starts_with(&format!("{option} needs a gdbserver")),
                "{argv:?}: {message}"
            );
            assert!(
                message.contains(&format!("the {backend} backend has none")),
                "{argv:?}: {message}"
            );
            assert!(
                message.contains(&format!("drop {flag} (ptrace is the default)")),
                "{argv:?}: {message}"
            );
        }
        refusal(&["hermit", flag.as_str(), "run"]).unwrap();
    }
    let error = refusal(&["hermit", "run", "--namespace-only", "--gdbserver"]).unwrap_err();
    assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
    assert!(
        error.to_string().contains("drop --namespace-only"),
        "{error:#}"
    );
    refusal(&["hermit", "--backend=ptrace", "run", "--gdbserver"]).unwrap();
    refusal(&[
        "hermit",
        "--backend=ptrace",
        "run",
        "--strict",
        "--gdbserver",
    ])
    .unwrap();
    refusal(&["hermit", "run", "--dap-listen=4711"]).unwrap();
    // Argument validation refuses it too, before any other check of the run.
    let mut strict_kvm = run_opts_for(&[
        "hermit",
        "--backend=kvm",
        "run",
        "--strict",
        "--gdbserver",
        "fakeprog",
    ]);
    let error = strict_kvm.validate_args().unwrap_err();
    assert!(
        error.to_string().contains("the kvm backend has none"),
        "{error:#}"
    );
}

/// Without the `liteinst` feature there is no in-guest runtime to advise
/// about: a `--backend=liteinst` run that no other in-guest refusal stops,
/// including one with the default maximum timeslice, is refused for the
/// missing build feature instead.
#[test]
#[cfg(not(feature = "liteinst"))]
fn liteinst_without_the_feature_is_refused_for_the_feature() {
    for options in [
        &[][..],
        &["--max-timeslice=disabled"][..],
        &["--verify"][..],
        &["--max-timeslice=disabled", "--verify"][..],
    ] {
        liteinst_in_guest_refusal(options).unwrap_or_else(|error| panic!("{options:?}: {error:#}"));
        let mut argv = vec!["hermit", "--backend=liteinst", "run"];
        argv.extend_from_slice(options);
        argv.push("fakeprog");
        let error = run_opts_for(&argv)
            .refuse_liteinst_without_the_feature()
            .unwrap_err();
        let unavailable = error
            .downcast_ref::<hermit::BackendUnavailable>()
            .unwrap_or_else(|| panic!("{options:?}: {error:#}"));
        assert_eq!(unavailable.backend(), Backend::Liteinst, "{options:?}");
        assert!(
            error
                .to_string()
                .contains("compiled without the liteinst backend"),
            "{options:?}: {error:#}"
        );
    }
    // Other backends are not this refusal's business.
    run_opts_for(&["hermit", "run", "fakeprog"])
        .refuse_liteinst_without_the_feature()
        .unwrap();
}

/// `RunOpts::refuse_unsupervised_log_cap` for `hermit GLOBAL... run OPTIONS...
/// fakeprog`.
#[cfg(test)]
fn log_cap_refusal_for(global: &[&str], options: &[&str]) -> Result<(), Error> {
    let mut argv = vec!["hermit"];
    argv.extend_from_slice(global);
    argv.push("run");
    argv.extend_from_slice(options);
    argv.push("fakeprog");
    let run = run_opts_for(&argv);
    use clap::CommandFactory;
    use clap::FromArgMatches;
    let schema = crate::app::Args::command();
    let raw = argv
        .iter()
        .map(std::ffi::OsString::from)
        .collect::<Vec<_>>();
    let normalized = if let Some(name) = run.application_backend {
        crate::app::run_config::application_arguments(&schema, &raw, name)
            .unwrap()
            .0
    } else {
        raw
    };
    let matches = schema.try_get_matches_from(normalized).unwrap();
    let args = crate::app::Args::from_arg_matches(&matches).unwrap();
    run.refuse_unsupervised_log_cap(args.global.max_log_bytes)
}

/// Round-2 review of https://github.com/rrnewton/hermit/pull/3686, finding 2:
/// the cap ends a run by exiting the hermit process that crossed it, which
/// ends nothing where the guest is not bound to that process.
#[test]
fn log_cap_is_refused_where_the_guest_could_outlive_hermit() {
    let refused: [(&[&str], &[&str], &str); 6] = [
        (
            &["--max-log-bytes=4K", "--backend=liteinst"],
            &["--no-namespace", "--max-timeslice=disabled"],
            "--backend=liteinst and --no-namespace",
        ),
        (
            &["--max-log-bytes=4K", "--backend=kvm"],
            &["--no-namespace"],
            "--backend=kvm and --no-namespace",
        ),
        // Round-4c review, finding 1: the ptrace tracer sets PTRACE_O_EXITKILL
        // only after it has spawned the guest (and started any GDB server),
        // and a charged write can cross the cap in between.
        (
            &["--max-log-bytes=4K", "--backend=ptrace"],
            &["--no-namespace"],
            "--no-namespace",
        ),
        (
            &["--max-log-bytes=4K"],
            &["--no-namespace"],
            "--no-namespace",
        ),
        (
            &["--max-log-bytes=4K", "--backend=ptrace"],
            &["--no-namespace", "--gdbserver"],
            "--no-namespace",
        ),
        (
            &["--max-log-bytes=4K"],
            &["--gdbserver", "--no-namespace"],
            "--no-namespace",
        ),
    ];
    for (global, options, named) in refused {
        let error = log_cap_refusal_for(global, options).unwrap_err();
        assert!(
            error.downcast_ref::<PolicyRefusal>().is_some(),
            "{global:?} {options:?}: {error:#}"
        );
        let message = error.to_string();
        let expected = format!("--max-log-bytes cannot be enforced with {named}: ");
        assert!(
            message.starts_with(&expected),
            "{global:?} {options:?}: {message}"
        );
        assert!(!message.contains('\n'), "one line: {message}");
        assert!(!message.contains("  "), "no runs of spaces: {message}");
        // The same configuration without the flag is not this check's business.
        log_cap_refusal_for(&global[1..], options).unwrap();
    }
}

/// Inside hermit's PID namespace, whose death takes every guest process with
/// it, each backend is accepted. `--no-namespace` is refused on every
/// backend (the test above).
#[test]
fn log_cap_is_accepted_where_hermit_takes_the_guest_down_with_it() {
    for backend in ["--backend=ptrace", "--backend=kvm", "--backend=liteinst"] {
        log_cap_refusal_for(&["--max-log-bytes=4K", backend], &[])
            .unwrap_or_else(|error| panic!("{backend}: {error:#}"));
    }
    // LiteInst is accepted inside hermit's PID namespace, whose death takes the
    // untraced in-guest LiteInst guest with it.
    log_cap_refusal_for(
        &["--max-log-bytes=4K", "--backend=liteinst"],
        &["--max-timeslice=disabled"],
    )
    .unwrap();
    // The default backend, without --backend.
    log_cap_refusal_for(&["--max-log-bytes=4K"], &[]).unwrap();
    // --gdbserver inside hermit's PID namespace, whose death takes the guest
    // with it even before PTRACE_O_EXITKILL is set.
    log_cap_refusal_for(
        &["--max-log-bytes=4K", "--backend=ptrace"],
        &["--gdbserver"],
    )
    .unwrap();
}

/// Every backend and namespace mode, so a change to the decision is a change
/// to this table.
#[test]
fn log_cap_refusal_table_covers_every_backend() {
    let backends = [
        Backend::Ptrace,
        Backend::Liteinst,
        Backend::InGuestTrap,
        Backend::Kvm,
    ];
    let mut refused = Vec::new();
    for backend in backends {
        for no_namespace in [false, true] {
            if RunOpts::log_cap_refusal(backend, no_namespace).is_some() {
                refused.push((backend, no_namespace));
            }
        }
    }
    assert_eq!(
        refused,
        [
            (Backend::Ptrace, true),
            (Backend::Liteinst, true),
            (Backend::InGuestTrap, true),
            (Backend::Kvm, true),
        ]
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
    assert_eq!(
        format!("{}", ro),
        " --max-timeslice=100000 --epoch=2026-01-01T00:00:00+00:00 -- fakeprog"
    );
}

#[test]
fn deprecated_preemption_timeout_disabled_values_round_trip_canonically() {
    for value in ["disabled", "0"] {
        let flag = format!("--preemption-timeout={value}");
        let mut ro = RunOpts::parse_from(["fakehermit", &flag, "fakeprog"]);
        ro.validate_args_with_perf_support(true).unwrap();

        assert_eq!(ro.det_opts.det_config.max_timeslice, None);
        assert_eq!(
            format!("{}", ro),
            " --max-timeslice=disabled --epoch=2026-01-01T00:00:00+00:00 -- fakeprog"
        );
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

    // Under a chaos slowdown one RCB is NANOS_PER_RCB * (clock multiplier *
    // largest factor), multiplied in that order as Detcore's PMU epsilon is:
    // 110 virtual nanoseconds here, which left to right comes to one unit in
    // the last place more and a minimum of 111.
    let slowed = |maximum: &str| {
        let mut ro = RunOpts::parse_from([
            "fakehermit",
            "--chaos",
            "--chaos-per-thread-slowdown",
            "--chaos-slowdown-max-factor=4.4",
            "--clock-multiplier=2.5",
            maximum,
            "fakeprog",
        ]);
        ro.validate_args_with_perf_support(true).map(|()| ro)
    };
    let ro = slowed("--max-timeslice=110").unwrap();
    assert_eq!(
        ro.det_opts.det_config.max_timeslice,
        std::num::NonZeroU64::new(110)
    );
    let error = slowed("--max-timeslice=109").unwrap_err().to_string();
    assert!(
        error.contains("at least one RCB (110 virtual nanoseconds"),
        "{error}"
    );

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
fn strict_refuses_an_epoch_or_seed_drawn_from_the_host() {
    let parse = |arguments: &[&str]| {
        let mut ro = RunOpts::parse_from(arguments);
        if !arguments
            .iter()
            .any(|argument| argument.starts_with("--epoch"))
        {
            ro.capture_default_epoch(|| {
                SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000)
            });
        }
        ro.validate_args_with_perf_support(true).unwrap();
        ro.adopt_replayed_schedule_epoch().unwrap();
        ro
    };

    let error = parse(&["fakehermit", "--strict", "fakeprog"])
        .refuse_strict_with_host_drawn_inputs()
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("--strict requires every input")
            && error.contains("the virtual-time epoch, from the host clock")
            && error.contains("--epoch=2026-09-21T14:13:20Z")
            && error.contains("HERMIT_EPOCH"),
        "{error}"
    );

    let error = parse(&[
        "fakehermit",
        "--strict",
        "--epoch=2026-01-01T00:00:00Z",
        "--seed-from=SystemRandom",
        "fakeprog",
    ])
    .refuse_strict_with_host_drawn_inputs()
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("the seed, from --seed-from=SystemRandom")
            && error.contains("--seed=N")
            && !error.contains("epoch"),
        "{error}"
    );

    for arguments in [
        &[
            "fakehermit",
            "--strict",
            "--epoch=2026-01-01T00:00:00Z",
            "fakeprog",
        ][..],
        &[
            "fakehermit",
            "--strict",
            "--epoch=2026-01-01T00:00:00Z",
            "--seed-from=Args",
            "fakeprog",
        ],
        // Without --strict the same draws are recorded instead.
        &["fakehermit", "fakeprog"],
        &["fakehermit", "--seed-from=SystemRandom", "fakeprog"],
    ] {
        parse(arguments)
            .refuse_strict_with_host_drawn_inputs()
            .unwrap_or_else(|error| panic!("{arguments:?}: {error:#}"));
    }

    // A replay supplies its recorded epoch, which is not drawn from the host.
    let dir = tempfile::tempdir().unwrap();
    let trace = dir.path().join("net.trace");
    detcore_model::network_engine::NetworkEngine::new_record(
        "2026-01-01T00:00:00Z".parse().unwrap(),
    )
    .finish()
    .unwrap()
    .write_framed(File::create(&trace).unwrap())
    .unwrap();
    let flag = format!("--replay-networking={}", trace.display());
    parse(&["fakehermit", "--strict", &flag, "fakeprog"])
        .refuse_strict_with_host_drawn_inputs()
        .unwrap();
}

#[test]
fn strict_rejects_strace_only() {
    // `--strace-only`'s own doc comment says it is shorthand for, among other
    // things, `--no-sequentialize-threads --no-deterministic-io` -- the exact
    // two flags `--strict` already refuses by name. Accepting the shorthand
    // while refusing its parts is the inconsistency this closes.
    let error = RunOpts::try_parse_from(["fakehermit", "--strict", "--strace-only", "fakeprog"])
        .unwrap_err();
    assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    let message = error.to_string();
    assert!(message.contains("--strict"));
    assert!(message.contains("--strace-only"));
}

/// The options `--strace-only`'s help says it is shorthand for.
#[cfg(test)]
const STRACE_ONLY_EXPANSION: [&str; 8] = [
    "--tmp=/tmp",
    "--network=host",
    "--no-virtualize-cpuid",
    "--no-virtualize-time",
    "--no-virtualize-metadata",
    "--no-sequentialize-threads",
    "--no-deterministic-io",
    "--no-rcb-time",
];

/// Validates `--strace-only SHORT_EXTRA fakeprog` and the documented expansion
/// with `LONG_EXTRA`, and lists every way the two resolved runs differ: a
/// refusal, thread pinning, network, `/tmp`, each top-level Detcore
/// configuration field, and the rendered command. Empty when they are the same
/// run. Collected rather than asserted one by one, so a failure names every
/// difference at once.
#[cfg(test)]
fn strace_only_differences(
    short_extra: &[&str],
    long_extra: &[&str],
) -> (RunOpts, RunOpts, Vec<String>) {
    let resolve = |options: &[&str], extra: &[&str]| {
        let mut args = vec!["fakehermit"];
        args.extend_from_slice(options);
        args.extend_from_slice(extra);
        args.push("fakeprog");
        let mut opts = RunOpts::parse_from(args);
        let validated = opts.validate_args_with_perf_support(true);
        (opts, validated.map_err(|error| format!("{error:#}")))
    };
    let (shorthand, shorthand_validated) = resolve(&["--strace-only"], short_extra);
    let (expansion, expansion_validated) = resolve(&STRACE_ONLY_EXPANSION, long_extra);
    let mut differences = Vec::new();
    if let Err(error) = &shorthand_validated {
        differences.push(format!("--strace-only is refused: {error}"));
    }
    if let Err(error) = &expansion_validated {
        differences.push(format!("the expansion is refused: {error}"));
    }
    if !differences.is_empty() {
        return (shorthand, expansion, differences);
    }
    let mut differ = |what: &str, short: String, long: String| {
        if short != long {
            differences.push(format!("{what}: --strace-only {short}, expansion {long}"));
        }
    };
    differ(
        "pin_threads",
        shorthand.pin_threads.to_string(),
        expansion.pin_threads.to_string(),
    );
    differ(
        "network",
        format!("{:?}", shorthand.network),
        format!("{:?}", expansion.network),
    );
    differ(
        "tmp",
        format!("{:?}", shorthand.tmp),
        format!("{:?}", expansion.tmp),
    );
    let fields = |opts: &RunOpts| match serde_json::to_value(&opts.det_opts.det_config).unwrap() {
        serde_json::Value::Object(fields) => fields,
        other => panic!("the Detcore configuration serialized as {other}"),
    };
    let (short_fields, long_fields) = (fields(&shorthand), fields(&expansion));
    for (name, long) in &long_fields {
        let short = short_fields.get(name).cloned().unwrap_or_default();
        differ(
            &format!("Detcore {name}"),
            short.to_string(),
            long.to_string(),
        );
    }
    // A validated `--strace-only` used to reach the rendering's
    // `assert!(dop.deterministic_io)` with deterministic I/O cleared. A panic
    // on either side is a difference of its own, so two panics never compare
    // equal; only two successful renderings are compared.
    let render = |opts: &RunOpts| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| opts.to_string()))
    };
    match (render(&shorthand), render(&expansion)) {
        (Ok(short), Ok(long)) => differ("rendered command", short, long),
        (short, long) => {
            if short.is_err() {
                differences.push("rendering --strace-only panicked".to_owned());
            }
            if long.is_err() {
                differences.push("rendering the expansion panicked".to_owned());
            }
        }
    }
    (shorthand, expansion, differences)
}

#[test]
fn strace_only_resolves_like_its_documented_expansion() {
    // `--strace-only`'s help names the options it is shorthand for. Resolved,
    // the two spellings must be the same run: the same Detcore configuration,
    // thread pinning, network and `/tmp`, and the same rendered command, also
    // with an extra option whose handling reads the lowered ones. Applied
    // after those readers, the shorthand pinned every guest thread to one CPU
    // (the pin decision still saw sequentialized threads), was refused with
    // `--no-virtualize-time` (the time/metadata check still saw virtualized
    // metadata), and kept the options `Config::validate` drops without
    // sequentialized threads.
    let cases: [(&[&str], &[&str]); 5] = [
        (&[], &[]),
        // Already part of the expansion, which clap refuses to repeat.
        (&["--no-virtualize-time"], &[]),
        (&["--stop-after-turn=5"], &["--stop-after-turn=5"]),
        (&["--stop-after-iter=5"], &["--stop-after-iter=5"]),
        (
            &["--debug-externalize-sockets"],
            &["--debug-externalize-sockets"],
        ),
    ];
    let mut failures = Vec::new();
    for (short_extra, long_extra) in cases {
        let (shorthand, expansion, differences) = strace_only_differences(short_extra, long_extra);
        if !differences.is_empty() {
            failures.push(format!("{short_extra:?}: {differences:#?}"));
            continue;
        }
        let config = &shorthand.det_opts.det_config;
        assert!(!shorthand.pin_threads, "{short_extra:?}");
        assert!(config.max_timeslice.is_some(), "{short_extra:?}");
        assert!(!config.sequentialize_threads, "{short_extra:?}");
        assert!(config.stop_after_turn.is_none(), "{short_extra:?}");
        assert!(config.stop_after_iter.is_none(), "{short_extra:?}");
        assert!(!config.debug_externalize_sockets, "{short_extra:?}");
        assert_eq!(shorthand.tmp.as_deref(), Some(Path::new(TMP_DIR)));
        assert_eq!(expansion.network, NetworkingMode::Host);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn dap_supervisor_maps_guest_paths_like_the_strace_only_expansion() {
    // `--dap-listen` supervises the run from `main`, before `validate_args`,
    // and checks the host file it names to the debugger through the guest-path
    // mapping. With `--strace-only` that mapping must see the lowered
    // `--tmp=/tmp`, as the expansion does; otherwise a program under /tmp is
    // treated as hidden and the supervisor ends the run. A path a source-less
    // mount hides must stay hidden in both spellings.
    let settle = |options: &[&str]| {
        let mut args = vec!["fakehermit"];
        args.extend_from_slice(options);
        args.extend([
            "--dap-listen=127.0.0.1:0",
            "--gdbserver-port=1234",
            "--mount=type=tmpfs,target=/hidden",
            "/tmp/guest",
        ]);
        let mut opts = RunOpts::parse_from(args);
        // The prelude `main` runs; without a supervisor marker it hands off
        // to the supervisor, which uses these options from here on.
        assert_eq!(
            opts.dispatch_before_validation(None, None).unwrap(),
            RunDispatch::DapSupervisor
        );
        opts
    };
    let shorthand = settle(&["--strace-only"]);
    let expansion = settle(&STRACE_ONLY_EXPANSION);
    assert_eq!(shorthand.tmp.as_deref(), Some(Path::new(TMP_DIR)));
    assert_eq!(shorthand.tmp, expansion.tmp);
    assert_eq!(shorthand.network, NetworkingMode::Host);
    assert_eq!(shorthand.network, expansion.network);
    assert!(shorthand.no_sequentialize_threads && expansion.no_sequentialize_threads);
    assert!(shorthand.no_deterministic_io && expansion.no_deterministic_io);
    assert!(shorthand.det_opts.det_config.gdbserver);
    assert_eq!(
        serde_json::to_value(&shorthand.det_opts.det_config).unwrap(),
        serde_json::to_value(&expansion.det_opts.det_config).unwrap()
    );
    for path in ["/tmp/guest", "/hidden/guest", "/usr/bin/true"] {
        let path = Path::new(path);
        assert_eq!(
            shorthand.mapped_host_program(path),
            expansion.mapped_host_program(path),
            "{}",
            path.display()
        );
    }
    assert_eq!(
        shorthand.mapped_or_visible_host_program(Path::new("/tmp/guest")),
        Some(PathBuf::from("/tmp/guest"))
    );
    assert_eq!(
        shorthand.mapped_host_program(Path::new("/hidden/guest")),
        GuestPathMapping::Hidden
    );
    assert_eq!(
        shorthand.mapped_or_visible_host_program(Path::new("/hidden/guest")),
        None
    );
}

#[test]
fn strict_rejects_every_route_to_host_networking() {
    // Explicit, and the route that reaches host networking as a side effect of
    // a flag that is not about networking. Both were accepted silently before:
    // measured from inside the guest, each showed interfaces `eth0 lo` where
    // `--strict` alone shows `lo`. With `--gdbserver` as well, the refusal
    // still names the network flag, the one cause.
    for (args, expected_cause) in [
        (vec!["--strict", "--network=host"], "--network=host"),
        (vec!["--strict", "--no-namespace"], "--no-namespace"),
        (
            vec!["--strict", "--gdbserver", "--network=host"],
            "--network=host",
        ),
    ] {
        let mut argv = vec!["fakehermit"];
        argv.extend(args.iter().copied());
        argv.push("fakeprog");
        let mut opts = match RunOpts::try_parse_from(&argv) {
            Ok(opts) => opts,
            Err(error) => {
                // `--no-namespace` already carries a clap conflict with
                // `network`; a parse-time refusal is an acceptable refusal.
                assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
                continue;
            }
        };
        let error = opts
            .validate_args_with_perf_support(true)
            .expect_err(&format!("{argv:?} must be refused under --strict"));
        let message = error.to_string();
        assert!(
            message.contains("--strict"),
            "{argv:?}: message must name --strict: {message}"
        );
        assert!(
            message.contains(expected_cause),
            "{argv:?}: message must name the cause {expected_cause}: {message}"
        );
    }
}

/// A timeslice of at most `SMALL_TIMESLICE_SKID_MARGINS` skid margins gets one
/// notice line naming the value, the branches per slice, the margin,
/// `--timeout` and `--summary`; a longer one, a disabled maximum, a launch that
/// arms no PMU timer, a preemption replay, `--imprecise-timers` or
/// `--hb-list-events` gets none. Slices are counted at the clock multiplier and,
/// under `--chaos` with a per-thread slowdown, at its largest factor (the
/// slowdown alone, without `--chaos`, changes nothing).
#[test]
fn a_small_max_timeslice_gets_a_notice() {
    fn zen() -> u64 {
        10_000
    }
    fn intel() -> u64 {
        100
    }
    let notice =
        |argv: &[&str], margin: fn() -> u64| run_opts_for(argv).small_max_timeslice_notice(margin);
    for (argv, margin, rcbs, slowed) in [
        (vec!["--max-timeslice=1000"], zen as fn() -> u64, 100, false),
        (vec!["--preemption-timeout=1000"], zen, 100, false),
        (vec!["--max-timeslice=1000000"], zen, 100_000, false),
        (vec!["--max-timeslice=10000"], intel, 1000, false),
        (vec!["--chaos", "--max-timeslice=10"], intel, 1, false),
        (
            vec!["--clock-multiplier=100", "--max-timeslice=1000000"],
            intel,
            1000,
            false,
        ),
        (
            vec![
                "--chaos",
                "--chaos-per-thread-slowdown",
                "--chaos-slowdown-max-factor=10",
                "--max-timeslice=10000000",
            ],
            zen,
            100_000,
            true,
        ),
        (
            vec![
                "--replay-schedule-from=unused-schedule",
                "--max-timeslice=1000",
            ],
            zen,
            100,
            false,
        ),
        // One RCB is 10 * (2.5 * 4.4) = 110 virtual nanoseconds, as Detcore
        // multiplies; left to right it is one unit in the last place more,
        // which would leave 110 ns no whole branch.
        (
            vec![
                "--chaos",
                "--chaos-per-thread-slowdown",
                "--chaos-slowdown-max-factor=4.4",
                "--clock-multiplier=2.5",
                "--max-timeslice=110",
            ],
            intel,
            1,
            true,
        ),
    ] {
        let mut full = vec!["hermit", "run"];
        full.extend(&argv);
        full.push("fakeprog");
        let line = notice(&full, margin).unwrap_or_else(|| panic!("{argv:?} gets a notice"));
        assert!(line.starts_with("NOTICE: --max-timeslice="), "{line}");
        let branches = if rcbs == 1 { "branch" } else { "branches" };
        assert!(
            line.contains(&format!(
                "at most about {rcbs} retired conditional {branches} "
            )),
            "{argv:?}: {line}"
        );
        assert_eq!(
            line.contains("(at the largest chaos slowdown factor)"),
            slowed,
            "{argv:?}: {line}"
        );
        assert!(
            line.contains(&format!("up to {} branches (the skid margin)", margin())),
            "{line}"
        );
        assert!(
            line.contains("can run orders of magnitude slower"),
            "{line}"
        );
        assert!(line.contains("(--max-timeslice=200000000)"), "{line}");
        assert!(line.contains("--timeout that expires"), "{line}");
        assert!(line.contains("--summary"), "{line}");
        assert_eq!(line.matches('\n').count(), 1, "one line: {line:?}");
        assert!(line.ends_with('\n'), "{line:?}");
    }
    for (argv, margin) in [
        (vec![], zen as fn() -> u64),
        (vec!["--max-timeslice=1000010"], zen),
        (vec!["--max-timeslice=10010"], intel),
        (vec!["--max-timeslice=disabled"], zen),
        (
            vec!["--clock-multiplier=0.01", "--max-timeslice=20000"],
            zen,
        ),
        (vec!["--namespace-only", "--max-timeslice=1000"], zen),
        (
            vec![
                "--chaos",
                "--replay-preemptions-from=unused-preemptions",
                "--max-timeslice=1000",
            ],
            zen,
        ),
        (vec!["--imprecise-timers", "--max-timeslice=1000"], zen),
        (
            vec![
                "--happens-before=unused-spec",
                "--hb-list-events",
                "--max-timeslice=1000",
            ],
            zen,
        ),
        (
            vec![
                "--chaos-per-thread-slowdown",
                "--chaos-slowdown-max-factor=10",
                "--max-timeslice=10000000",
            ],
            zen,
        ),
    ] {
        let mut full = vec!["hermit", "run"];
        full.extend(&argv);
        full.push("fakeprog");
        assert_eq!(notice(&full, margin), None, "{argv:?}");
    }
    // KVM places a precise event by stepping too, so its small slices are
    // costly in the same way.
    let kvm = run_opts_for(&[
        "hermit",
        "--backend=kvm",
        "run",
        "--max-timeslice=1000",
        "fakeprog",
    ]);
    assert_eq!(
        kvm.small_max_timeslice_notice(zen),
        notice(&["hermit", "run", "--max-timeslice=1000", "fakeprog"], zen)
    );
    assert!(kvm.small_max_timeslice_notice(zen).is_some());
    // A backend without the timer keeps its inherited maximum and steps
    // nothing.
    for backend in ["--backend=liteinst", "--backend=in-guest-trap"] {
        let untimed = run_opts_for(&["hermit", backend, "run", "fakeprog"]);
        assert_eq!(untimed.small_max_timeslice_notice(zen), None, "{backend}");
    }
}

/// `main`'s branch-counter check: a run without `--strict` on a host whose
/// counter failed validation goes on and writes one warning line, even when
/// Reverie's reason spans lines; with `--strict` it is refused and writes
/// nothing; a counter that passed, or a run without a timeslice, writes
/// nothing (https://github.com/rrnewton/hermit/issues/4515).
#[test]
fn a_run_on_an_inexact_branch_counter_warns_without_strict() {
    let check = |argv: &[&str], inexact_branch_counter: fn() -> Option<String>| {
        let mut options = run_opts_for(argv);
        options.inexact_branch_counter = inexact_branch_counter;
        let mut lines = Vec::new();
        let result = options.admit_branch_counter(&mut |line: &str| lines.push(line.to_owned()));
        (result, lines)
    };
    let multiline: fn() -> Option<String> = || {
        Some(
            "Hardware performance counters seem to not be working.\n  counted 0 branches\n  \
             expected 500"
                .to_string(),
        )
    };
    let (result, lines) = check(&["hermit", "run", "fakeprog"], multiline);
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(lines.len(), 1, "{lines:?}");
    let warning = &lines[0];
    assert!(warning.starts_with("WARNING: "), "{warning}");
    assert!(
        warning.contains("counted 0 branches expected 500"),
        "{warning}"
    );
    assert!(warning.contains("--strict"), "{warning}");
    assert_eq!(warning.matches('\n').count(), 1, "one line: {warning:?}");
    assert!(warning.ends_with('\n'), "{warning:?}");
    let (result, lines) = check(&["hermit", "run", "--strict", "fakeprog"], multiline);
    assert!(
        result.is_err_and(|error| error.downcast_ref::<PolicyRefusal>().is_some()),
        "--strict refuses"
    );
    assert!(lines.is_empty(), "{lines:?}");
    let (result, lines) = check(&["hermit", "run", "fakeprog"], || None);
    assert!(result.is_ok() && lines.is_empty(), "{lines:?}");
    let (result, lines) = check(
        &["hermit", "run", "--max-timeslice=disabled", "fakeprog"],
        multiline,
    );
    assert!(result.is_ok() && lines.is_empty(), "{lines:?}");
}

/// The inexact-counter warning that `run`, `analyze`, `bisect` and `record
/// start` share says, in one line, what the counter drives, what this command
/// would repeat that is not guaranteed to (the run's seed or preemption
/// record, the trials and what they report, a recording's replay), and what to
/// do instead (a host whose counters pass, as `hermit host-capabilities`
/// reports it; `--strict` refuses this one). It ends with Reverie's reason,
/// whose counts vary between processes. A counter that passed gets no warning.
#[test]
fn the_inexact_branch_counter_warning_says_what_may_not_reproduce_and_what_to_do() {
    let inexact: fn() -> Option<String> =
        || Some("SpecLockMap is enabled\n  (91 of 95 locked\ninstructions)".to_string());
    for (reader, not_guaranteed) in [
        (
            CounterReader::Run,
            "so the same arguments and --seed, or a preemption record, are not guaranteed to \
             reproduce this run.",
        ),
        (
            CounterReader::Trials,
            "so analyze and bisect trials can disagree for a hardware reason, and a seed, \
             preemption record or schedule they report is not guaranteed to reproduce its \
             outcome.",
        ),
        (
            CounterReader::Recording,
            "so a replay of this recording is not guaranteed to follow it.",
        ),
    ] {
        assert_eq!(warning_for_an_inexact_branch_counter(reader, || None), None);
        let warning = warning_for_an_inexact_branch_counter(reader, inexact)
            .unwrap_or_else(|| panic!("{reader:?}: an inexact counter warns"));
        for expected in [
            "WARNING: this host's retired-branch counter failed Reverie's performance-counter \
             validation. Hermit times preemptions, and by default virtual time, with that \
             counter, ",
            not_guaranteed,
            " For reproducible results use a host whose counters pass the validation (`hermit \
             host-capabilities` reports exact-branch-counter: PRESENT, validation passed); \
             --strict refuses this host.",
            " Reverie reports: SpecLockMap is enabled (91 of 95 locked instructions)\n",
        ] {
            assert!(
                warning.contains(expected),
                "{reader:?}: {expected:?}: {warning}"
            );
        }
        assert!(warning.starts_with("WARNING: "), "{warning}");
        assert!(warning.ends_with("instructions)\n"), "{warning:?}");
        assert_eq!(warning.matches('\n').count(), 1, "one line: {warning:?}");
    }
}

#[test]
fn strict_refuses_an_inexact_branch_counter_on_the_pmu_backends() {
    for backend in ["ptrace", "kvm"] {
        let backend_flag = format!("--backend={backend}");
        let mut options = run_opts_for(&["hermit", &backend_flag, "run", "--strict", "fakeprog"]);
        options.inexact_branch_counter = || Some("SpecLockMap is enabled".to_string());
        let error = options
            .refuse_strict_with_inexact_branch_counter()
            .expect_err(backend);
        assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
        let message = error.to_string();
        for expected in [
            "--strict",
            &format!("`{backend}` backend"),
            "SpecLockMap is enabled",
            "CPU ",
            "issues/3794",
        ] {
            assert!(
                message.contains(expected),
                "{backend}: {expected:?}: {message}"
            );
        }
    }
}

#[test]
fn strict_refusal_needs_strict_a_pmu_backend_and_an_inexact_counter() {
    let check = |argv: &[&str], inexact_branch_counter: fn() -> Option<String>| {
        let mut options = run_opts_for(argv);
        options.inexact_branch_counter = inexact_branch_counter;
        options
            .refuse_strict_with_inexact_branch_counter()
            .unwrap_or_else(|error| panic!("{argv:?}: {error:#}"));
    };
    let unreachable = || -> Option<String> { panic!("the counter is not consulted") };
    check(
        &["hermit", "--backend=ptrace", "run", "fakeprog"],
        unreachable,
    );
    check(
        &["hermit", "--backend=ptrace", "run", "--strict", "fakeprog"],
        || None,
    );
    for backend in ["liteinst", "in-guest-trap"] {
        let backend_flag = format!("--backend={backend}");
        check(
            &["hermit", &backend_flag, "run", "--strict", "fakeprog"],
            unreachable,
        );
    }
    check(
        &["hermit", "run", "--strict", "--namespace-only", "fakeprog"],
        unreachable,
    );
    // Without a timeslice Detcore uses no branch count, so the relaxed
    // strictness level runs where the counter miscounts
    // (https://github.com/rrnewton/hermit/issues/4291).
    check(
        &[
            "hermit",
            "--backend=ptrace",
            "run",
            "--strict",
            "--no-virtualize-cpuid",
            "--max-timeslice=disabled",
            "fakeprog",
        ],
        unreachable,
    );
}

#[test]
fn strict_run_refuses_an_inexact_branch_counter_before_it_looks_at_the_program() {
    // Through `main`: the refusal must be wired into the run's preflight, not
    // only callable. The program does not exist, so a run that passed the
    // refusal fails on the missing program instead.
    let global = GlobalOpts::parse_from(["hermit"]);
    let mut options = RunOpts::parse_from(["run", "--strict", "/nonexistent-strict-pmu-guest"]);
    assert!(!options.program_path().exists());
    options.inexact_branch_counter = || Some("SpecLockMap is enabled".to_string());
    let error = options.main(&global).unwrap_err();
    assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
    assert!(
        error.to_string().contains("SpecLockMap is enabled"),
        "{error:#}"
    );

    options.inexact_branch_counter = || None;
    let error = options.main(&global).unwrap_err();
    assert!(error.downcast_ref::<PolicyRefusal>().is_none(), "{error:#}");
}

#[test]
#[cfg(feature = "native-exec")]
fn native_inputs_refuse_wrong_backend_before_program_or_declaration_access() {
    for backend in ["ptrace", "liteinst"] {
        let backend_flag = format!("--backend={backend}");
        let global = GlobalOpts::parse_from(["hermit", &backend_flag]);
        let mut options = RunOpts::parse_from([
            "run",
            "--verify",
            "--native-exec-inputs=/nonexistent-native-declaration",
            "/nonexistent-native-guest",
        ]);
        // The dedicated DynamoRIO/E9patch frontdoors own the two foreign
        // cases; they retain their existing, stronger external-adapter refusal.
        // Reaching program/declaration validation or a backend probe instead
        // produces a different error and must fail this control.
        let error = options.main(&global).unwrap_err();
        assert_eq!(
            error.to_string(),
            "native exec currently requires in-guest-trap",
            "{backend}: {error:#}"
        );
        assert_policy_refusal_exit(&error);
        assert!(options.native_exec_capture.is_none());
    }
}

#[test]
fn non_strict_runs_still_allow_host_networking() {
    // The refusal is about `--strict` claiming something it did not enforce,
    // not about forbidding host networking outright.
    let mut opts = RunOpts::parse_from(["fakehermit", "--network=host", "fakeprog"]);
    opts.validate_args_with_perf_support(true)
        .expect("host networking without --strict must remain allowed");
    assert_eq!(opts.network, NetworkingMode::Host);
}

/// `--gdbserver` leaves the network as asked: its port is bound in the host's
/// network namespace before the container exists, so the guest keeps its
/// private network, under `--strict` too, where a debugger can now attach to
/// the strict run itself.
#[test]
fn gdbserver_keeps_the_guest_network() {
    for args in [
        &["--gdbserver"][..],
        &["--strict", "--gdbserver"],
        &["--strict", "--gdbserver", "--gdbserver-port=0"],
    ] {
        let mut argv = vec!["fakehermit"];
        argv.extend(args);
        argv.push("fakeprog");
        let mut opts = RunOpts::parse_from(&argv);
        opts.validate_args_with_perf_support(true)
            .unwrap_or_else(|error| panic!("{argv:?} must be admitted: {error:#}"));
        assert!(opts.det_opts.det_config.gdbserver, "{argv:?}");
        assert_eq!(opts.network, NetworkingMode::Local, "{argv:?}");
    }
}

#[test]
fn gdbserver_respects_explicit_host_networking() {
    let mut opts = RunOpts::parse_from(["fakehermit", "--gdbserver", "--network=host", "fakeprog"]);
    opts.validate_args_with_perf_support(true).unwrap();
    assert_eq!(opts.network, NetworkingMode::Host);
}

/// Detcore's notice for an abstract `AF_UNIX` bind is on only for a run that asked
/// for `--network=host` (given, or through `--strace-only`'s expansion), really
/// shares the host's network namespace, has its own PID namespace, is not under
/// `--gdbserver`, and runs Detcore in Hermit's own process.
#[test]
fn abstract_bind_notice_is_on_only_for_a_host_network_run_on_ptrace() {
    let resolved = |global: &[&str], args: &[&str]| {
        let argv: Vec<&str> = std::iter::once("hermit")
            .chain(global.iter().copied())
            .chain(std::iter::once("run"))
            .chain(args.iter().copied())
            .chain(std::iter::once("fakeprog"))
            .collect();
        let mut opts = run_opts_for(&argv);
        opts.validate_args_with_perf_support(true)
            .unwrap_or_else(|error| panic!("{argv:?}: {error:#}"));
        opts.det_opts.det_config.notice_host_network_abstract_binds
    };
    assert!(resolved(&[], &["--network=host"]));
    assert!(!resolved(&[], &[]));
    assert!(!resolved(&[], &["--network=local"]));
    assert!(!resolved(&[], &["--analyze-networking"]));
    // --analyze-networking isolates the run even under --network=host.
    assert!(!resolved(&[], &["--network=host", "--analyze-networking"]));
    // Host networking that another mode needs, or forces.
    assert!(!resolved(&[], &["--no-namespace"]));
    assert!(!resolved(&[], &["--gdbserver"]));
    assert!(!resolved(&[], &["--gdbserver", "--network=host"]));
    // --strace-only is shorthand for an expansion that includes
    // --network=host, and resolves like it: the notice is on for both.
    assert!(resolved(&[], &["--strace-only"]));
    assert!(resolved(&[], &["--strace-only", "--network=host"]));
    assert!(resolved(
        &[],
        &[
            "--tmp=/tmp",
            "--network=host",
            "--no-virtualize-cpuid",
            "--no-virtualize-time",
            "--no-virtualize-metadata",
            "--no-sequentialize-threads",
            "--no-deterministic-io",
            "--no-rcb-time",
        ]
    ));
    // A recording forces host networking but refuses abstract AF_UNIX
    // addresses outright (and clap refuses it with --network=host).
    let dir = tempfile::tempdir().unwrap();
    let record = format!(
        "--record-networking={}",
        dir.path().join("net.trace").display()
    );
    assert!(!resolved(&[], &[&record]));
    // Only the ptrace backend: KVM is not covered, and on the in-guest
    // backends Detcore's stderr would be the guest's.
    assert!(!resolved(&["--backend", "kvm"], &["--network=host"]));

    let mut application = run_opts_for(&["hermit", "run", "--network=host", "fakeprog"]);
    application.select_application_backend("dynamorio").unwrap();
    application = application.with_adapter_capabilities(reverie::BackendCapabilities::DBT);
    application.validate_args_with_perf_support(true).unwrap();
    assert!(
        !application
            .det_opts
            .det_config
            .notice_host_network_abstract_binds
    );
}

/// `--analyze-networking` needs the isolated namespace, which `--gdbserver`
/// no longer leaves, so the two combine.
#[test]
fn gdbserver_combines_with_analyze_networking() {
    let mut opts = RunOpts::parse_from([
        "fakehermit",
        "--gdbserver",
        "--analyze-networking",
        "fakeprog",
    ]);
    opts.validate_args_with_perf_support(true).unwrap();
    assert!(opts.network.uses_local_namespace(opts.analyze_networking));
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
        " --network=host --no-namespace --tmp=/tmp --epoch=2026-01-01T00:00:00+00:00 -- fakeprog"
    );
}

#[test]
fn ptrace_backend_keeps_uts_assumption_with_namespaces() {
    // The default (ptrace) backend does launch through Reverie's `Container`,
    // which unshares CLONE_NEWUTS and sets the deterministic hostname, so the
    // UTS assumption must stay enabled when namespaces are on. Pins the fix to
    // DBT so it does not regress the ptrace trust-the-namespace path.
    let mut opts = RunOpts::parse_from(["fakehermit", "fakeprog"]);
    opts.validate_args_with_perf_support(true).unwrap();
    assert_eq!(opts.selected_backend(), Backend::Ptrace);
    assert!(!opts.no_namespace);
    assert!(opts.det_opts.det_config.has_uts_namespace);
}

#[test]
fn oci_argv_applies_entrypoint_and_cmd_defaults() {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    // No explicit program: Entrypoint + Cmd.
    assert_eq!(
        RunOpts::oci_argv(&s(&["/entry", "a"]), &s(&["python3", "-V"]), None, vec![]),
        Some((PathBuf::from("/entry"), s(&["a", "python3", "-V"])))
    );
    // Cmd alone supplies the program.
    assert_eq!(
        RunOpts::oci_argv(&[], &s(&["python3"]), None, vec![]),
        Some((PathBuf::from("python3"), vec![]))
    );
    // Explicit program replaces Cmd; Entrypoint persists.
    assert_eq!(
        RunOpts::oci_argv(
            &s(&["/entry"]),
            &s(&["ignored"]),
            Some(PathBuf::from("/bin/sh")),
            s(&["-c", "x"])
        ),
        Some((PathBuf::from("/entry"), s(&["/bin/sh", "-c", "x"])))
    );
    // Nothing to run.
    assert_eq!(RunOpts::oci_argv(&[], &[], None, vec![]), None);
}

#[test]
fn image_conflicts_with_no_namespace_with_explanatory_error() {
    // The clap layer no longer lists `image` in `--no-namespace`'s
    // conflicts_with set, so parsing succeeds; the conflict is reported at
    // validation time with a message that explains *why* they are incompatible.
    let mut opts = RunOpts::parse_from([
        "fakehermit",
        "--image=docker.io/library/busybox@sha256:deadbeef",
        "--no-namespace",
        "/bin/sh",
    ]);
    let message = opts
        .validate_args_with_perf_support(true)
        .unwrap_err()
        .to_string();
    assert!(message.contains("--image"), "message: {message}");
    assert!(message.contains("--no-namespace"), "message: {message}");
    assert!(message.contains("chroot"), "message: {message}");
}

#[test]
fn image_rejects_unqualified_backend_and_namespace_paths() {
    let mut backend = run_opts_for(&[
        "hermit",
        "--backend=liteinst",
        "run",
        "--image=busybox@sha256:deadbeef",
        "/bin/sh",
    ]);
    let message = backend
        .validate_args_with_perf_support(true)
        .unwrap_err()
        .to_string();
    assert!(
        message.contains("supports the ptrace and kvm backends"),
        "message: {message}"
    );

    // KVM is qualified for --image: validation must accept it.
    let mut kvm = run_opts_for(&[
        "hermit",
        "--backend=kvm",
        "run",
        "--image=busybox@sha256:deadbeef",
        "/bin/sh",
    ]);
    kvm.validate_args_with_perf_support(true)
        .expect("kvm must be admitted with --image");

    let mut namespace_only = RunOpts::parse_from([
        "fakehermit",
        "--image=busybox@sha256:deadbeef",
        "--namespace-only",
        "/bin/sh",
    ]);
    let message = namespace_only
        .validate_args_with_perf_support(true)
        .unwrap_err()
        .to_string();
    assert!(message.contains("--namespace-only"), "message: {message}");
}

#[test]
fn image_script_validation_resolves_interpreter_inside_rootfs() {
    let tmp = tempfile::TempDir::new().unwrap();
    let rootfs = tmp.path();
    let script = rootfs.join("bin/image-script");
    let interpreter = rootfs.join("image-only-interpreter");
    std::fs::create_dir_all(script.parent().unwrap()).unwrap();
    std::fs::write(&script, b"#!/image-only-interpreter\nexit 0\n").unwrap();
    std::fs::write(&interpreter, b"image interpreter fixture\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&interpreter, std::fs::Permissions::from_mode(0o755)).unwrap();

    validate_executable(&script, Path::new("/bin/image-script"), Some(rootfs)).unwrap();

    std::fs::write(&script, b"#!/missing-image-interpreter\nexit 0\n").unwrap();
    let error = validate_executable(&script, Path::new("/bin/image-script"), Some(rootfs))
        .expect_err("a missing guest interpreter must be rejected");
    assert!(
        error.to_string().contains("/missing-image-interpreter"),
        "unexpected error: {error:#}"
    );
}

/// End-to-end preflight regression: an executable whose complete contents are a
/// shebang with no interpreter must be *rejected* by `validate_executable`, and
/// must not panic on the way there.
///
/// This is the checked-in discrimination for the shared-parser change. Each of
/// the two pre-fix states fails it, for a different reason:
///
/// * With the old duplicated `shebang_interpreter` helper in this file — the one
///   that did `bytes[2..].iter().position(..)? + 2` — the `?` returns `None` for
///   these inputs, so `validate_executable` skips its shebang block entirely and
///   returns `Ok(())`. Preflight silently accepts the file. The `Ok(())` arm
///   below panics.
/// * With `let mut j = 1 + i;` restored in `Shebang::from_buf`, the shared
///   parser slices `&buf[i..i + 1]` where `i == buf.len()` and panics with
///   "range end index N out of range for slice of length N".
///
/// Scope: this pins one preflight rejection for one input class. It is not a
/// shebang-parser test — interpreter arguments, the 256-byte truncation
/// boundary, and non-UTF-8 interpreter paths are not covered here, and the
/// positive case (a resolvable interpreter) lives in
/// `image_script_validation_resolves_interpreter_inside_rootfs`.
///
/// `#!\n` is carried for completeness only: it already yielded an empty
/// interpreter under the old helper, so on its own it discriminates nothing. The
/// two inputs that carry the regression are `#!` and `#! \t`, the shapes whose
/// interpreter field runs to the end of the header buffer.
#[test]
fn validate_executable_rejects_empty_shebang_interpreter() {
    let tmp = tempfile::TempDir::new().unwrap();
    let script = tmp.path().join("empty-shebang");
    let requested = Path::new("/bin/empty-shebang");

    for contents in [b"#!".as_slice(), b"#! \t", b"#!\n"] {
        std::fs::write(&script, contents).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        // The empty-interpreter rejection precedes guest-root resolution, so the
        // host preflight path and the image path must both reach it.
        for guest_root in [None, Some(tmp.path())] {
            let error = match validate_executable(&script, requested, guest_root) {
                Ok(()) => panic!(
                    "preflight accepted an executable whose contents are {contents:?} \
                     (guest_root: {guest_root:?}); it must be rejected"
                ),
                Err(error) => error,
            };
            assert!(
                error.to_string().contains("empty shebang interpreter"),
                "contents {contents:?} (guest_root: {guest_root:?}): unexpected error: {error:#}"
            );
        }
    }
}

#[test]
fn image_guest_program_resolution_matches_execve_pathname_rules() {
    // Absolute paths pass through unchanged.
    assert_eq!(
        resolve_image_guest_program(Path::new("/bin/sh"), Path::new("/")).unwrap(),
        PathBuf::from("/bin/sh")
    );
    // A relative path containing a '/' resolves against the guest working
    // directory (the image WorkingDir) and is normalized.
    assert_eq!(
        resolve_image_guest_program(Path::new("bin/busybox"), Path::new("/opt/app")).unwrap(),
        PathBuf::from("/opt/app/bin/busybox")
    );
    assert_eq!(
        resolve_image_guest_program(Path::new("./run.sh"), Path::new("/srv")).unwrap(),
        PathBuf::from("/srv/run.sh")
    );
    // Default working directory of `/` resolves relative paths at the root.
    assert_eq!(
        resolve_image_guest_program(Path::new("./bin/sh"), Path::new("/")).unwrap(),
        PathBuf::from("/bin/sh")
    );
    // A bare command name (no '/') would need an unsupported in-image PATH
    // search and is rejected with an actionable message.
    let bare = resolve_image_guest_program(Path::new("sh"), Path::new("/"))
        .unwrap_err()
        .to_string();
    assert!(bare.contains("PATH search"), "message: {bare}");
    assert!(bare.contains("bin/sh"), "message: {bare}");
    // Parent components cannot escape the guest root during validation.
    assert!(resolve_image_guest_program(Path::new("../etc/x"), Path::new("/app")).is_err());
}

#[test]
fn strict_help_describes_compatibility_and_opt_outs() {
    use clap::CommandFactory;

    let help = RunOpts::command().render_long_help().to_string();
    for expected in [
        "--strict",
        "Require Hermit's deterministic defaults",
        "already fail closed in ordinary runs",
        "replayed recording that supplies it",
        "--no-sequentialize-threads",
        "Disable deterministic sequential thread execution",
        "--no-deterministic-io",
        "Disable deterministic I/O behavior",
        "--passthru-opt",
        "optimized partial syscall subscription set",
        "--allow-unsupported-syscalls",
        "This weakens determinism",
        "--panic-on-rbc-overshoot",
        "--max-timeslice",
        "Reverie's validation",
        "--preemption-timeout",
        "--target-timeslice",
        "syscall boundaries",
    ] {
        assert!(
            help.contains(expected),
            "missing {expected:?} in run help:\n{help}"
        );
    }
    // The backend is a global option (`hermit --backend <BACKEND> run`); `run`
    // must not offer a second, subcommand-level spelling. The backend selection
    // and its values are documented in the top-level help instead.
    assert!(
        !help.contains("--backend <BACKEND>"),
        "run help still offers a subcommand-level --backend:\n{help}"
    );
    let top_level = crate::app::Args::command().render_long_help().to_string();
    for expected in [
        "--backend <BACKEND>",
        "Select the process instrumentation backend",
        "This is a global option",
        "ptrace",
        "dbt",
        "kvm",
    ] {
        assert!(
            top_level.contains(expected),
            "missing {expected:?} in top-level help:\n{top_level}"
        );
    }
}

#[test]
fn only_timer_delivering_runs_depend_on_host_perf() {
    let default = RunOpts::parse_from(["fakehermit", "fakeprog"]);
    assert!(default.arms_pmu_preemption_timer());
    let namespace_only = RunOpts::parse_from(["fakehermit", "--namespace-only", "fakeprog"]);
    assert_eq!(namespace_only.selected_backend(), Backend::Ptrace);
    assert!(!namespace_only.arms_pmu_preemption_timer());
    // KVM's timer counts with the same perf events.
    let kvm = run_opts_for(&["hermit", "--backend=kvm", "run", "fakeprog"]);
    assert!(kvm.arms_pmu_preemption_timer());
    let liteinst = run_opts_for(&["hermit", "--backend=liteinst", "run", "fakeprog"]);
    assert!(!liteinst.arms_pmu_preemption_timer());
}

#[test]
fn display_runopts_without_perf_support() {
    let mut ro = RunOpts::parse_from(["fakehermit", "fakeprog", "arg1"]);
    ro.validate_args_with_perf_support(false).unwrap();
    assert_eq!(
        format!("{}", ro),
        " --max-timeslice=disabled --epoch=2026-01-01T00:00:00+00:00 -- fakeprog arg1"
    );
}

/// Working counters on a CPU Reverie has no PMU profile for (a GitHub-hosted
/// runner's Emerald Rapids before Reverie had one) leave the run without the
/// timer, as missing counters do, instead of failing at the first task.
#[test]
fn display_runopts_without_a_pmu_profile() {
    let mut ro = RunOpts::parse_from(["fakehermit", "fakeprog", "arg1"]);
    ro.validate_args_with_timer_support(Err(TimerUnavailable::NoPmuProfile(
        "Unsupported CPU family 0x6, model 0x1: Reverie has no performance-counter profile for it"
            .to_owned(),
    )))
    .unwrap();
    assert_eq!(
        format!("{}", ro),
        " --max-timeslice=disabled --epoch=2026-01-01T00:00:00+00:00 -- fakeprog arg1"
    );
}

/// `--detlog-anon` and `--detlog-anon=pages` parse and render back, and a
/// backend whose `/proc/<tid>/maps` is not the guest's alone refuses them.
#[test]
fn detlog_anon_is_ptrace_only() {
    let validated = |argv: &[&str]| {
        let mut ro = run_opts_for(argv);
        ro.validate_args_with_timer_support(Ok(())).map(|()| ro)
    };
    let pages = validated(&["hermit", "run", "--detlog-anon=pages", "fakeprog"]).unwrap();
    assert_eq!(
        pages.det_opts.det_config.detlog_anon,
        Some(detcore_model::config::DetlogAnon::Pages)
    );
    assert!(
        format!("{pages}").contains(" --detlog-anon=pages "),
        "{pages}"
    );
    let mappings = validated(&["hermit", "run", "--detlog-anon", "fakeprog"]).unwrap();
    assert_eq!(
        mappings.det_opts.det_config.detlog_anon,
        Some(detcore_model::config::DetlogAnon::Mappings)
    );
    assert!(
        format!("{mappings}").contains(" --detlog-anon "),
        "{mappings}"
    );
    assert!(
        validated(&[
            "hermit",
            "--backend",
            "ptrace",
            "run",
            "--detlog-anon",
            "fakeprog"
        ])
        .is_ok()
    );
    for backend in ["kvm", "liteinst", "in-guest-trap"] {
        let error = validated(&[
            "hermit",
            "--backend",
            backend,
            "run",
            "--detlog-anon",
            "fakeprog",
        ])
        .expect_err(backend);
        assert!(
            format!("{error}").contains("--detlog-anon hashes the anonymous mappings"),
            "{backend}: {error}"
        );
    }
    let error = validated(&[
        "hermit",
        "run",
        "--namespace-only",
        "--detlog-anon",
        "fakeprog",
    ])
    .expect_err("--namespace-only");
    assert!(format!("{error}").contains("--namespace-only"), "{error}");
}

fn shebang_interpreter(path: &Path) -> Option<PathBuf> {
    let mut file = File::open(path).ok()?;
    let mut bytes = [0_u8; 256];
    let count = file.read(&mut bytes).ok()?;
    Shebang::interpreter_from_buf(&bytes[..count])
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-696): Review sharing ELF entrypoint detection with record.
#[cfg(test)]
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

/// The guest program itself could not be started — a GUEST-SIDE fact.
///
/// ⚠️ THIS IS NOT A HERMIT FAILURE AND MUST NOT BE REPORTED AS ONE. Hermit worked
/// correctly; the path it was given does not name something it can execute. The
/// distinction matters because every gate and harness on this project decides
/// pass/fail from `$?`, and "hermit is broken" and "you gave me a typo" demand
/// completely different responses from a caller.
///
/// The conventional codes are not invented here: GNU `env`, `chroot` and
/// `timeout` — the same convention hermit's 125 came from — reserve 127 for
/// "command not found" and 126 for "found but not executable".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestProgramFault {
    /// Nothing usable at that path: 127.
    NotFound,
    /// It exists but cannot be executed as given: 126.
    NotExecutable,
}

impl GuestProgramFault {
    pub fn exit_code(self) -> i32 {
        // Named rather than written out, so `tests/cli.rs` can assert the SAME
        // definition instead of a second copy of the number. Copying it here is
        // how the internal-failure code went stale in eight tests.
        match self {
            Self::NotFound => hermit::GUEST_PROGRAM_NOT_FOUND_EXIT,
            Self::NotExecutable => hermit::GUEST_PROGRAM_NOT_EXECUTABLE_EXIT,
        }
    }

    pub fn class(self) -> &'static str {
        match self {
            Self::NotFound => "guest-program-not-found",
            Self::NotExecutable => "guest-program-not-executable",
        }
    }
}

impl std::fmt::Display for GuestProgramFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotFound => "guest program not found",
            Self::NotExecutable => "guest program not executable",
        })
    }
}

impl std::error::Error for GuestProgramFault {}

fn validate_executable(
    path: &Path,
    requested: &Path,
    guest_root: Option<&Path>,
) -> Result<(), Error> {
    let metadata = fs::metadata(path)
        .with_context(|| {
            format!(
                "Program {} does not exist or is not accessible. Check the path and any --mount \
                 or --bind target.",
                requested.display()
            )
        })
        .map_err(|error| error.context(GuestProgramFault::NotFound))?;
    if metadata.is_dir() {
        return Err(anyhow::anyhow!(
            "Program {} is a directory; provide the path to an executable file",
            requested.display()
        )
        .context(GuestProgramFault::NotExecutable));
    }
    if !metadata.is_file() {
        return Err(anyhow::anyhow!(
            "Program {} is not a regular executable file",
            requested.display()
        )
        .context(GuestProgramFault::NotExecutable));
    }
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err(anyhow::anyhow!(
            "Program {} is not executable. Add execute permission (for example, `chmod +x {}`) \
             or select another file.",
            requested.display(),
            requested.display()
        )
        .context(GuestProgramFault::NotExecutable));
    }

    if let Some(interpreter) = shebang_interpreter(path) {
        if interpreter.as_os_str().is_empty() {
            anyhow::bail!(
                "Program {} has an empty shebang interpreter",
                requested.display()
            );
        }
        let interpreter_host = guest_root
            .map(|root| crate::app::image::resolve_in_rootfs(root, &interpreter))
            .unwrap_or_else(|| interpreter.clone());
        let interpreter_metadata = fs::metadata(&interpreter_host).with_context(|| {
            format!(
                "Program {} uses shebang interpreter {}, but that interpreter does not exist in \
                 the selected guest filesystem. Install it or update the script's #! line.",
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

/// Resolve a guest program path to an absolute path *inside the image rootfs*,
/// mirroring execve(2) pathname resolution for the `--image` prototype:
///
///   * an absolute path is used as-is;
///   * a relative path that contains a `/` (e.g. `./bin/sh`, `bin/busybox`) is
///     resolved against `guest_cwd` (the image WorkingDir, or `/`), so the path
///     validated here is the same one the chrooted guest will execve;
///   * a bare command name (no `/`) is PATH-searched by the caller
///     (`validate_program`) before this resolver sees it; seen here, it is rejected.
///
/// Search `path_env` (colon-separated guest dirs) for `name` inside `rootfs`,
/// returning the guest-absolute path of the first existing entry.
fn search_image_path(rootfs: &Path, name: &Path, path_env: &str) -> Option<PathBuf> {
    for dir in path_env.split(':').filter(|d| !d.is_empty()) {
        let guest = Path::new(dir).join(name);
        if crate::app::image::resolve_in_rootfs(rootfs, &guest).exists() {
            return Some(guest);
        }
    }
    None
}

fn resolve_image_guest_program(requested: &Path, guest_cwd: &Path) -> Result<PathBuf, Error> {
    if requested.is_absolute() {
        Ok(requested.to_path_buf())
    } else if requested.to_string_lossy().contains('/') {
        normalize_guest_path(&guest_cwd.join(requested))
    } else {
        anyhow::bail!(
            "With --image, a bare program name ({:?}) would require a PATH search inside the \
             image, which is not yet supported; use a path containing '/' (absolute like \
             `/bin/sh`, or relative to the working directory like `./bin/sh`).",
            requested
        )
    }
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

/// Refuse `--virtual-cpus cpus` on a host whose CPU directory, `directory`
/// (`/sys/devices/system/cpu`), lacks one of `cpu0` to `cpu<cpus - 1>` or
/// has it offline. The guest sees those directories, with their topology and
/// cache files rendered for the virtual CPUs, in place of its own: Detcore
/// can leave a CPU out of the listing, but cannot add one the host does not
/// have, nor the `topology` directory Linux removes from an offline CPU's
/// (`topology_remove_dev`).
fn check_host_lists_virtual_cpus(cpus: u32, directory: &Path) -> Result<(), Error> {
    let listed = (0..cpus)
        .take_while(|cpu| directory.join(format!("cpu{cpu}/topology")).is_dir())
        .count();
    if listed < cpus as usize {
        let fewer = match listed {
            0 => "run without --virtual-cpus".to_owned(),
            listed => format!("pass --virtual-cpus {listed} or fewer"),
        };
        anyhow::bail!(
            "--virtual-cpus {cpus} shows the guest this host's cpu0 to cpu{} in {}, rendered \
             for the virtual CPUs, but only the first {listed} are there and online; {fewer}, \
             or run on a host with at least {cpus} online CPUs",
            cpus - 1,
            directory.display(),
        );
    }
    Ok(())
}

/// `--virtual-cpus` is admitted where the host has an online CPU directory
/// for each virtual CPU, and otherwise refused with the count that would be.
#[test]
fn virtual_cpus_need_a_host_directory_per_cpu() {
    let host = tempfile::TempDir::new().unwrap();
    for cpu in ["cpu0", "cpu1", "cpu2", "cpu4"] {
        std::fs::create_dir_all(host.path().join(cpu).join("topology")).unwrap();
    }
    // An offline CPU keeps its directory but loses its topology.
    std::fs::create_dir(host.path().join("cpu3")).unwrap();
    std::fs::create_dir(host.path().join("cpufreq")).unwrap();
    check_host_lists_virtual_cpus(1, host.path()).unwrap();
    check_host_lists_virtual_cpus(3, host.path()).unwrap();
    let error = check_host_lists_virtual_cpus(4, host.path())
        .unwrap_err()
        .to_string();
    assert!(error.contains("cpu0 to cpu3"), "{error}");
    assert!(
        error.contains("only the first 3 are there and online"),
        "{error}"
    );
    assert!(error.contains("pass --virtual-cpus 3 or fewer"), "{error}");
    let empty = tempfile::TempDir::new().unwrap();
    let error = check_host_lists_virtual_cpus(2, empty.path())
        .unwrap_err()
        .to_string();
    assert!(error.contains("run without --virtual-cpus"), "{error}");
}

/// The largest `--chaos-instruction-offset`: each step is one ptrace stop of
/// the preempted thread, so this bounds what one preemption can add (about
/// 35 us a step on x86_64), and the longer the steps, the more often a
/// host-timed stop lands among them and fails the run closed.
const MAX_CHAOS_INSTRUCTION_OFFSET: u64 = 256;

/// Refuse `--chaos-instruction-offset` where it would be ignored or could not
/// be replayed. (The argument parser already refuses `--chaos` with
/// `--namespace-only`.) `aarch64` is whether this build runs on aarch64,
/// where the single steps the option relies on are not validated yet: that
/// is a policy refusal (exit status 122), not a usage error.
fn refuse_chaos_instruction_offset(
    config: &detcore::Config,
    backend: &str,
    aarch64: bool,
) -> Result<(), Error> {
    let offset = config.chaos_instruction_offset;
    if offset > MAX_CHAOS_INSTRUCTION_OFFSET {
        anyhow::bail!(
            "--chaos-instruction-offset={offset} is above {MAX_CHAOS_INSTRUCTION_OFFSET}: every \
             instruction is one ptrace stop of a preemption"
        );
    }
    if !config.chaos {
        anyhow::bail!(
            "--chaos-instruction-offset moves where a chaos timeslice ends; it needs --chaos"
        );
    }
    if aarch64 {
        return Err(Error::new(PolicyRefusal).context(
            "--chaos-instruction-offset is validated only on x86_64: on aarch64 the ptrace \
             backend's single steps past a branch target are not yet checked to count each \
             instruction exactly once, so a recorded offset could not be trusted to replay",
        ));
    }
    if backend != Backend::Ptrace.as_str() {
        anyhow::bail!(
            "--chaos-instruction-offset single-steps the guest with the ptrace backend's precise \
             timer; backend `{backend}` is refused"
        );
    }
    if config.imprecise_timers {
        anyhow::bail!(
            "--chaos-instruction-offset needs precise timers: --imprecise-timers takes no single \
             steps"
        );
    }
    if config.max_timeslice.is_none() {
        anyhow::bail!(
            "--chaos-instruction-offset needs the PMU timer that --max-timeslice=disabled turns off"
        );
    }
    if config.replay_preemptions_from.is_some() || config.replay_schedule_from.is_some() {
        anyhow::bail!(
            "--chaos-instruction-offset draws new offsets; a replay takes its offsets from the \
             record it replays"
        );
    }
    if config.strict_schedule_boundaries {
        anyhow::bail!(
            "--chaos-instruction-offset is not part of the strict slice-boundary protocol; it \
             cannot be combined with --strict-schedule-boundaries"
        );
    }
    Ok(())
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

/// The descriptors a verification snapshots and restores between its two runs.
///
/// ⚠️ ALL THREE, AND THE FIRST VERSION OF THIS FIX RESTORED ONLY STDERR.
/// The leak is a property of an INHERITED OPEN FILE DESCRIPTION, not of stderr:
/// a guest that sets a status flag on any descriptor hermit passed it leaves
/// that flag set on the description itself, which run 2 then inherits already
/// mutated. Nothing about fd 2 is special, and scoping the fix to the descriptor
/// the reproducing guest happened to touch was the limit of that guest, not the
/// limit of the defect.
///
/// Measured at `aeda16ff7de3`, the head that restored fd 2 alone, with a stock
/// `/usr/bin/perl` guest that conditionally sets `O_APPEND`:
///
/// ```text
/// mutating STDOUT, --backend kvm --strict --verify   rc=125  nondeterministic
/// mutating STDOUT, ptrace (control)                  rc=0    deterministic
/// mutating STDERR, kvm  (the shipped fix works)      rc=0    deterministic
/// mutating STDIN,  kvm                               rc=0    deterministic
/// touching no flags, kvm (negative control)          rc=0    deterministic
/// run 1: fcntl(1, F_GETFL) = Ok(32769)   run 2: fcntl(1, F_GETFL) = Ok(33793)
/// ```
///
/// ⚠️ STDIN IS INCLUDED THOUGH IT DID NOT REPRODUCE, DELIBERATELY. Its probe
/// is green because KVM reserves its own stdin path, which is a property of
/// today's backend rather than of the descriptor. Snapshotting it costs one
/// `F_GETFL` and one comparison per verification, and leaving it out would
/// rebuild the exact gap this change exists to close -- a descriptor omitted
/// because no test currently reaches it.
const VERIFY_RESTORED_FDS: [libc::c_int; 3] =
    [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO];

/// The status flags on one descriptor, or `None` if they cannot be read.
///
/// `None` is not an error: an unreadable descriptor simply means there is
/// nothing to restore, and a verification must not fail over housekeeping.
fn fd_status_flags(fd: libc::c_int) -> Option<libc::c_int> {
    // SAFETY: `F_GETFL` reads flags from a descriptor number and writes no
    // memory. Validity is NOT a precondition here: a closed or never-opened
    // descriptor returns -1 with `EBADF`, which this reads as `None`. Saying
    // "fd 2 is valid for the life of the process" would have been an unfounded
    // guarantee -- stderr can be closed -- so the safety argument rests on the
    // call being memory-safe for any integer, which it is.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 { None } else { Some(flags) }
}

/// The status flags of every descriptor a verification restores, in
/// `VERIFY_RESTORED_FDS` order.
///
/// Read once before the first verification run so the second can be handed the
/// same starting state.
fn standard_fd_status_flags() -> [Option<libc::c_int>; VERIFY_RESTORED_FDS.len()] {
    VERIFY_RESTORED_FDS.map(fd_status_flags)
}

/// Put each descriptor's status flags back to what the first run was handed.
///
/// ⚠️ ONLY WHEN THEY ACTUALLY CHANGED. An unconditional `F_SETFL` would be a
/// write on a descriptor hermit does not own whenever nothing moved, and the
/// common case is that nothing moved. Comparing first keeps this inert on every
/// run except the one it exists for -- and it is compared PER DESCRIPTOR, so a
/// guest that moves one flag does not cause writes on the other two.
fn restore_standard_fd_status_flags(before: [Option<libc::c_int>; VERIFY_RESTORED_FDS.len()]) {
    for (fd, before) in VERIFY_RESTORED_FDS.iter().copied().zip(before) {
        let Some(before) = before else {
            continue;
        };
        let Some(now) = fd_status_flags(fd) else {
            continue;
        };
        if now == before {
            continue;
        }
        // SAFETY: `F_SETFL` sets flags on a descriptor number and writes no
        // memory. The value is one this same descriptor reported earlier, and a
        // descriptor that has become invalid meanwhile fails with `EBADF`
        // rather than doing anything.
        unsafe {
            libc::fcntl(fd, libc::F_SETFL, before);
        }
    }
}

/// `path` with the user's home directory written as `~`, as a user types it.
pub(super) fn home_relative(path: &Path) -> String {
    match std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .and_then(|home| path.strip_prefix(home).ok())
    {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// Create two logging destinations and two global configs. Returns non-zero exit
/// status if there was a difference in any component of the output.
impl RunOpts {
    /// Resolve the one intentionally nondeterministic input of an ordinary run.
    ///
    /// The top-level parser calls this exactly once, before evidence setup or
    /// guest launch. Both halves of `--verify` and every backend then receive the
    /// same concrete `DateTime`; Detcore never reads the host clock itself.
    pub(crate) fn capture_default_epoch(&mut self, capture_now: impl FnOnce() -> SystemTime) {
        let exec_stop_admitted = self.refuse_unready_exec_stop().is_ok();
        self.capture_default_epoch_with_admission(capture_now, exec_stop_admitted);
    }

    // The Boolean is only the existing admission decision, not image authority.
    // Tests supply availability through the same admission predicate below.
    fn capture_default_epoch_with_admission(
        &mut self,
        capture_now: impl FnOnce() -> SystemTime,
        exec_stop_admitted: bool,
    ) {
        self.epoch_omitted = true;
        if self.in_guest_exec_stop
            && self.uses_virtual_time_determinization()
            && (self.exec_stop_epoch_refused || !exec_stop_admitted)
        {
            self.exec_stop_epoch_refused = true;
            return;
        }
        // The opt-in path executes the genuine Tool and receives the SAME
        // resolved epoch as every other run; it must not draw a later clock.
        if self.uses_virtual_time_determinization() {
            self.det_opts
                .det_config
                .capture_epoch_from_host_time(capture_now());
            self.epoch_captured_from_host = true;
        }
    }

    #[cfg(test)]
    pub(crate) fn epoch_capture_for_test(&self) -> (String, bool) {
        (
            self.det_opts.det_config.epoch.to_rfc3339(),
            self.epoch_captured_from_host,
        )
    }

    #[cfg(test)]
    pub(crate) fn reports_virtual_epoch_for_test(&self) -> bool {
        self.uses_virtual_time_determinization()
    }

    /// Whether this execution path actually feeds virtual time to a guest.
    /// Namespace-only bypasses Detcore, while strace-only deliberately turns
    /// all determinization off during validation; neither owns an epoch input.
    fn uses_virtual_time_determinization(&self) -> bool {
        self.det_opts.det_config.virtualize_time
            && !self.namespace_only
            && !self.strace_only
            && !self.hb_list_events
    }

    fn epoch_rfc3339(&self) -> String {
        self.det_opts.det_config.epoch.to_rfc3339()
    }

    /// Refuses a `--replay-preemptions-from` or `--replay-schedule-from`
    /// record that Detcore's loader would reject, before stdin is read or the
    /// container starts: unreadable, not valid JSON, or failing its checks,
    /// for example a thread whose change points are not strictly increasing
    /// in time, as a hand edit or an edited knock-out trial can leave.
    /// Detcore reads the record again in the container, where the same
    /// rejection could only panic the container child
    /// (rrnewton/hermit#4859). The refusal names the record and what is
    /// wrong with it (the thread and change point, for an ordering error).
    /// A record is parsed here once more than before; for a large
    /// `--replay-schedule-from` that is a measurable but one-off cost.
    fn refuse_an_invalid_replayed_record(&self) -> Result<(), Error> {
        if self.namespace_only {
            return Ok(());
        }
        let config = &self.det_opts.det_config;
        if config.replay_edited_preemptions
            && (self.verify
                || self.verify_verbose
                || self.verify_strict
                || self.verify_run.is_some())
        {
            return Err(Error::new(PolicyRefusal).context(
                "--replay-edited-preemptions is exploration-only and cannot be combined with \
                 verification (--verify, --verify-strict, or --verify-run). Replay an unedited, \
                 complete recording without this option to verify it; omit verification to explore.",
            ));
        }
        let replayed = [
            ("--replay-preemptions-from", &config.replay_preemptions_from),
            ("--replay-schedule-from", &config.replay_schedule_from),
        ];
        for (flag, path) in replayed {
            let Some(path) = path else { continue };
            let admitted = detcore::preemptions::boundary_recording::Recording::read(path)
                .and_then(|record| {
                    if flag == "--replay-preemptions-from" && config.replay_edited_preemptions {
                        record.require_strict_mode(false)
                    } else {
                        record.require_faithful_replay()
                    }
                });
            if let Err(error) = admitted {
                return Err(Error::new(PolicyRefusal).context(format!(
                    "{flag}={}: Hermit cannot replay this record: {error:#}. Pass a complete \
                     record Hermit wrote (re-record it), or correct the named entry.",
                    path.display()
                )));
            }
        }
        Ok(())
    }

    /// Start a replay from the virtual-time epoch its recording was made under.
    ///
    /// A preemption record stores absolute logical times, i.e. offsets from the
    /// recording run's epoch
    /// (<https://github.com/rrnewton/hermit/issues/3411>). A replay whose clock
    /// starts anywhere else sees every recorded timeslice end shifted by the
    /// difference, and detcore used to panic on the first one already in the
    /// past (<https://github.com/rrnewton/hermit/issues/3413>).
    ///
    /// So when this invocation omitted the epoch, the recorded epoch replaces
    /// the default. An explicit epoch (`--epoch` or `HERMIT_EPOCH`) that
    /// disagrees is refused before any guest starts. A record written before
    /// epochs were stored carries none and is replayed against this run's epoch
    /// exactly as before.
    ///
    /// A network trace (`--replay-networking`) is reconciled the same way: it
    /// releases each recorded input at an absolute virtual time, so replaying
    /// it from a later epoch would hand the guest every input at once. Two
    /// recordings replayed together must share one epoch.
    ///
    /// This holds with `--no-virtualize-time` too: detcore's logical clock, and
    /// so every recorded timeslice end, still starts at the epoch. Only
    /// `--namespace-only`, which bypasses detcore, has nothing to reconcile.
    fn adopt_replayed_schedule_epoch(&mut self) -> Result<(), Error> {
        if self.namespace_only {
            return Ok(());
        }
        let config = &self.det_opts.det_config;
        let mut recordings = Vec::new();
        if let Some(path) = config
            .replay_preemptions_from
            .as_ref()
            .or(config.replay_schedule_from.as_ref())
            && let Some(recorded) =
                detcore::preemptions::read_recorded_epoch(path).map_err(Error::msg)?
        {
            recordings.push((recorded, path.clone()));
        }
        if let (NetworkTraceMode::Replay, Some(path)) =
            (config.network_trace.mode, &config.network_trace.path)
        {
            let file = File::open(path)
                .with_context(|| format!("cannot open network trace {}", path.display()))?;
            let trace = NetworkTraceV2::read_framed(std::io::BufReader::new(file))
                .with_context(|| format!("{} is not a valid network trace", path.display()))?;
            recordings.push((trace.epoch, path.clone()));
        }
        for (recorded, path) in recordings {
            if self.epoch_omitted && !self.epoch_from_recording {
                self.det_opts.det_config.epoch = recorded;
                self.epoch_captured_from_host = false;
                self.epoch_from_recording = true;
                continue;
            }
            let current = self.det_opts.det_config.epoch;
            if current == recorded {
                continue;
            }
            let recorded = recorded.to_rfc3339();
            let current = current.to_rfc3339();
            let (current, remedy) = if self.epoch_from_recording {
                (
                    format!(
                        "the virtual-time epoch {current}, adopted from another recording \
                         replayed in this run,"
                    ),
                    "Replay recordings made in the same run together, or replay them separately."
                        .to_owned(),
                )
            } else {
                (
                    format!(
                        "the explicit virtual-time epoch {current} (from --epoch or HERMIT_EPOCH)"
                    ),
                    format!(
                        "Omit --epoch to replay from the recorded epoch, or pass \
                         --epoch={recorded}."
                    ),
                )
            };
            return Err(Error::new(PolicyRefusal).context(format!(
                "{current} differs from the epoch {recorded} that {} was recorded under. A \
                 recording's times are absolute virtual times measured from its own epoch, so \
                 replaying them from another epoch cannot reproduce the run. {remedy}",
                path.display(),
            )));
        }
        Ok(())
    }

    /// Replay a preemption or schedule record with the guest random seed it was
    /// recorded under.
    ///
    /// `--seed` also seeds what the guest reads as randomness (getrandom,
    /// `/dev/urandom`), so a replay at another `--seed`, or at the default one,
    /// handed the guest other values than the recording did and desynced from
    /// the schedule it replays. An omitted `--rng-seed` now takes the recorded
    /// one, and a different explicit one is refused before anything is
    /// launched. A record written before the seed was stored is replayed as
    /// before.
    fn adopt_replayed_rng_seed(&mut self) -> Result<(), Error> {
        if self.namespace_only {
            return Ok(());
        }
        let config = &self.det_opts.det_config;
        let Some(path) = config
            .replay_preemptions_from
            .clone()
            .or_else(|| config.replay_schedule_from.clone())
        else {
            return Ok(());
        };
        let Some(recorded) =
            detcore::preemptions::read_recorded_rng_seed(&path).map_err(Error::msg)?
        else {
            return Ok(());
        };
        match self.det_opts.det_config.rng_seed {
            None => {
                self.det_opts.det_config.rng_seed = Some(recorded);
                Ok(())
            }
            Some(explicit) if explicit != recorded => {
                Err(Error::new(PolicyRefusal).context(format!(
                    "the explicit --rng-seed={explicit} differs from the seed {recorded} that {} \
                     was recorded under. The guest would read other random values than the \
                     recorded run did, so the recorded schedule cannot reproduce it. Omit \
                     --rng-seed to replay with the recorded seed, or pass --rng-seed={recorded}.",
                    path.display(),
                )))
            }
            Some(_) => Ok(()),
        }
    }

    /// Refuse to replay a preemption or schedule record made under another
    /// `--pipe-wake-latency` than this run's, including a different latency
    /// (https://github.com/rrnewton/hermit/issues/4380). A record without the
    /// field was made with the option off. As for the epoch, the record is
    /// read before anything is launched.
    /// A replay of a recorded schedule runs under the record's post-crash
    /// bound (`--post-crash-timeslice`): taken when the replay sets none,
    /// refused when it sets another, or sets one for a record made with the
    /// switch off (`post_crash_timeslice_for_replay`). The switch changes
    /// every slice after a crash, so a replay under another bound would leave
    /// the record there.
    fn adopt_replayed_post_crash_timeslice(&mut self) -> Result<(), Error> {
        if self.namespace_only {
            return Ok(());
        }
        let config = &self.det_opts.det_config;
        let mut adopted = None;
        for (option, path) in [
            ("--replay-preemptions-from", &config.replay_preemptions_from),
            ("--replay-schedule-from", &config.replay_schedule_from),
        ] {
            let Some(path) = path else {
                continue;
            };
            let recorded = detcore::preemptions::read_recorded_post_crash_timeslice(path)
                .map_err(Error::msg)?;
            // An explicit bound the run would not use (not above
            // --max-timeslice) is the switch off, not unset: a record made
            // with the switch on refuses it.
            let replay = config.post_crash_timeslice.map(|_| {
                config
                    .post_crash_timeslice_in_effect()
                    .map(std::num::NonZeroU64::get)
            });
            let bound = detcore::preemptions::post_crash_timeslice_for_replay(
                &format!("{option} {}", path.display()),
                recorded,
                adopted.map(Some).or(replay),
            )
            .map_err(|refusal| Error::new(PolicyRefusal).context(refusal))?;
            adopted = adopted.or(bound);
        }
        if let Some(bound) = adopted {
            self.det_opts.det_config.post_crash_timeslice = std::num::NonZeroU64::new(bound);
            // The recorded switch must stay on under this replay's own
            // settings (a bound above its --max-timeslice, with threads
            // sequentialized), or the replay cannot take it.
            if let Some(refusal) = self.det_opts.det_config.post_crash_timeslice_refusal() {
                return Err(Error::new(PolicyRefusal).context(format!(
                    "the replayed record was made with --post-crash-timeslice={bound}: {refusal}"
                )));
            }
        }
        Ok(())
    }

    fn refuse_replay_under_another_pipe_wake_latency(&self) -> Result<(), Error> {
        if self.namespace_only {
            return Ok(());
        }
        let config = &self.det_opts.det_config;
        for (option, path) in [
            ("--replay-preemptions-from", &config.replay_preemptions_from),
            ("--replay-schedule-from", &config.replay_schedule_from),
        ] {
            let Some(path) = path else {
                continue;
            };
            let recorded =
                detcore::preemptions::read_recorded_pipe_wake_latency(path).map_err(Error::msg)?;
            if let Some(refusal) = detcore::preemptions::pipe_wake_latency_mismatch(
                &format!("{option} {}", path.display()),
                recorded,
                config.pipe_wake_latency,
            ) {
                return Err(Error::new(PolicyRefusal).context(refusal));
            }
        }
        Ok(())
    }

    /// Refuse to replay a preemption or schedule record under an explicit
    /// `--expired-wait-charge` other than the rule the record states. A replay
    /// that leaves the option unset takes the record's rule (a record without
    /// the field charged every wait), so it is never refused. As for the
    /// epoch, the record is read before anything is launched.
    fn refuse_replay_under_another_expired_wait_charge(&self) -> Result<(), Error> {
        if self.namespace_only || self.det_opts.det_config.expired_wait_charge.is_none() {
            return Ok(());
        }
        let config = &self.det_opts.det_config;
        for (option, path) in [
            ("--replay-preemptions-from", &config.replay_preemptions_from),
            ("--replay-schedule-from", &config.replay_schedule_from),
        ] {
            let Some(path) = path else {
                continue;
            };
            let recorded = detcore::preemptions::read_recorded_expired_wait_runs_charged_once(path)
                .map_err(Error::msg)?;
            if let Some(refusal) = detcore::preemptions::expired_wait_charge_mismatch(
                &format!("{option} {}", path.display()),
                recorded,
                config.expired_wait_charge,
                false,
            ) {
                return Err(Error::new(PolicyRefusal).context(refusal));
            }
        }
        Ok(())
    }

    /// Refuse to replay a preemption or schedule record under an explicit
    /// `--sleep-registration` other than the rule the record states. A replay
    /// that leaves the option unset takes the record's rule (a record without
    /// the field registered sleeps when chosen), so it is never refused. As
    /// for the epoch, the record is read before anything is launched.
    fn refuse_replay_under_another_sleep_registration(&self) -> Result<(), Error> {
        if self.namespace_only || self.det_opts.det_config.sleep_registration.is_none() {
            return Ok(());
        }
        let config = &self.det_opts.det_config;
        for (option, path) in [
            ("--replay-preemptions-from", &config.replay_preemptions_from),
            ("--replay-schedule-from", &config.replay_schedule_from),
        ] {
            let Some(path) = path else {
                continue;
            };
            let recorded =
                detcore::preemptions::read_recorded_queued_sleeps_registered_at_first_pass(path)
                    .map_err(Error::msg)?;
            if let Some(refusal) = detcore::preemptions::sleep_registration_mismatch(
                &format!("{option} {}", path.display()),
                recorded,
                config.sleep_registration,
                false,
            ) {
                return Err(Error::new(PolicyRefusal).context(refusal));
            }
        }
        Ok(())
    }

    /// Refuse to replay a record whose timeslices
    /// `--chaos-instruction-offset` delivered past their branch targets where
    /// the replay cannot stop at the same instructions: without the ptrace
    /// backend's precise timer, and from its schedule trace, which does not
    /// carry the offsets. The record is read before anything is launched.
    fn refuse_instruction_offset_replay_where_unsupported(&self) -> Result<(), Error> {
        self.refuse_instruction_offset_replay_on(cfg!(target_arch = "aarch64"))
    }

    /// [`Self::refuse_instruction_offset_replay_where_unsupported`], where
    /// `aarch64` is whether this build runs on aarch64, whose single steps
    /// past a branch target are not validated yet (as for recording, see
    /// `refuse_chaos_instruction_offset`).
    fn refuse_instruction_offset_replay_on(&self, aarch64: bool) -> Result<(), Error> {
        if self.namespace_only {
            return Ok(());
        }
        let config = &self.det_opts.det_config;
        for (option, path) in [
            ("--replay-preemptions-from", &config.replay_preemptions_from),
            ("--replay-schedule-from", &config.replay_schedule_from),
        ] {
            let Some(path) = path else {
                continue;
            };
            if !detcore::preemptions::read_recorded_instruction_offsets(path).map_err(Error::msg)? {
                continue;
            }
            let refusal = if config.replay_schedule_from.is_some() {
                Some("its schedule trace does not record the instruction offsets".to_owned())
            } else if aarch64 {
                Some(
                    "single steps past a branch target are validated only on x86_64, not on \
                     aarch64"
                        .to_owned(),
                )
            } else if self.selected_backend() != Backend::Ptrace {
                Some(format!(
                    "backend `{}` cannot single-step to them; replay it with the ptrace backend",
                    self.selected_backend().as_str()
                ))
            } else if config.imprecise_timers {
                Some("--imprecise-timers takes no single steps".to_owned())
            } else if config.max_timeslice.is_none() {
                Some("the PMU timer that steps to them is disabled".to_owned())
            } else {
                None
            };
            if let Some(why) = refusal {
                return Err(Error::new(PolicyRefusal).context(format!(
                    "{option} {} ends timeslices at instruction offsets past their branch \
                     targets (--chaos-instruction-offset), and this replay cannot stop there: {why}",
                    path.display()
                )));
            }
        }
        Ok(())
    }

    /// Refuses a replay of a recorded schedule under the other signal-state
    /// copy setting (`--no-signal-state-copy`), as
    /// `refuse_replay_under_another_pipe_wake_latency` refuses another
    /// latency. A record written before the setting was stored is replayed
    /// under either.
    fn refuse_replay_under_another_signal_state_copy(&self) -> Result<(), Error> {
        if self.namespace_only {
            return Ok(());
        }
        let config = &self.det_opts.det_config;
        for (option, path) in [
            ("--replay-preemptions-from", &config.replay_preemptions_from),
            ("--replay-schedule-from", &config.replay_schedule_from),
        ] {
            let Some(path) = path else {
                continue;
            };
            let recorded =
                detcore::preemptions::read_recorded_signal_state_copy(path).map_err(Error::msg)?;
            if let Some(refusal) = detcore::preemptions::signal_state_copy_mismatch(
                &format!("{option} {}", path.display()),
                recorded,
                config.signal_state_copy,
            ) {
                return Err(Error::new(PolicyRefusal).context(refusal));
            }
        }
        Ok(())
    }

    /// Whether the record `--replay-preemptions-from` names carries callback
    /// checkpoints (`Config::annotated_priority_replay`), refusing a
    /// configuration that cannot replay such a record
    /// (`preemption_admission::refuse_unqualified_annotated_replay`). As for
    /// the epoch, the record is read before anything is launched. Preparing a
    /// run sets the configuration from this answer, whatever it carried.
    fn annotated_priority_replay_admission(&self) -> Result<bool, Error> {
        if self.namespace_only {
            return Ok(false);
        }
        let config = &self.det_opts.det_config;
        let Some(path) = &config.replay_preemptions_from else {
            return Ok(false);
        };
        let annotated =
            detcore::preemptions::read_recorded_callback_checkpoints(path).map_err(Error::msg)?;
        if annotated {
            let admission = if let Some(name) = self.application_backend {
                hermit::preemption_admission::refuse_unqualified_annotated_replay_with_capabilities(
                    config,
                    name,
                    self.adapter_capabilities.ok_or_else(|| {
                        Error::new(PolicyRefusal).context(
                            "executable-selected backend is missing immutable physical capabilities",
                        )
                    })?,
                )
            } else {
                hermit::preemption_admission::refuse_unqualified_annotated_replay(
                    config,
                    self.selected_backend(),
                )
            };
            admission.map_err(|refusal| Error::new(PolicyRefusal).context(refusal))?;
        }
        Ok(annotated)
    }

    /// Refuses a replay of a recorded schedule under the other
    /// `--cap-preemption-charge` setting, as
    /// `refuse_replay_under_another_pipe_wake_latency` refuses another
    /// latency. A record without the field was made with the option off.
    fn refuse_replay_under_another_cap_preemption_charge(&self) -> Result<(), Error> {
        if self.namespace_only {
            return Ok(());
        }
        let config = &self.det_opts.det_config;
        for (option, path) in [
            ("--replay-preemptions-from", &config.replay_preemptions_from),
            ("--replay-schedule-from", &config.replay_schedule_from),
        ] {
            let Some(path) = path else {
                continue;
            };
            let recorded = detcore::preemptions::read_recorded_cap_preemption_charge(path)
                .map_err(Error::msg)?;
            if let Some(refusal) = detcore::preemptions::cap_preemption_charge_mismatch(
                &format!("{option} {}", path.display()),
                recorded,
                config.cap_preemption_charge,
            ) {
                return Err(Error::new(PolicyRefusal).context(refusal));
            }
        }
        Ok(())
    }

    /// Refuse a replay whose guest would be numbered otherwise than the run
    /// its record came from (`detcore::preemptions::guest_ids_mismatch`): a
    /// record of a `--no-namespace` run names its threads by IDs that a
    /// replay in a PID namespace does not give them. A replay runs in a PID
    /// namespace: `--no-namespace` does not take either replay option.
    fn refuse_replay_under_other_guest_ids(&self) -> Result<(), Error> {
        if self.namespace_only || self.no_namespace {
            return Ok(());
        }
        let config = &self.det_opts.det_config;
        for (option, path) in [
            ("--replay-preemptions-from", &config.replay_preemptions_from),
            ("--replay-schedule-from", &config.replay_schedule_from),
        ] {
            let Some(path) = path else {
                continue;
            };
            let recorded =
                detcore::preemptions::read_recorded_guest_ids(path).map_err(Error::msg)?;
            if let Some(refusal) = detcore::preemptions::guest_ids_mismatch(
                &format!("{option} {}", path.display()),
                recorded,
                detcore::GuestIds::Namespace,
            ) {
                return Err(Error::new(PolicyRefusal).context(refusal));
            }
        }
        Ok(())
    }

    /// Refuses a replay of a record under another exclusive rewrite --
    /// another mode, another rewrite format version, a record of the
    /// removed stripped-file heuristic, or a record that states no
    /// provenance when this run rewrites -- as
    /// `refuse_replay_under_another_signal_state_copy` refuses the other
    /// copy setting: the rewrite changes the instructions the guest
    /// retires while the files stay the same.
    fn refuse_replay_under_another_exclusive_rewrite(&self) -> Result<(), Error> {
        if self.namespace_only {
            return Ok(());
        }
        let config = &self.det_opts.det_config;
        for (option, path) in [
            ("--replay-preemptions-from", &config.replay_preemptions_from),
            ("--replay-schedule-from", &config.replay_schedule_from),
        ] {
            let Some(path) = path else {
                continue;
            };
            let recorded =
                detcore::preemptions::read_recorded_rewrite_exclusives(path).map_err(Error::msg)?;
            if let Some(refusal) = detcore::preemptions::rewrite_exclusives_mismatch(
                &format!("{option} {}", path.display()),
                recorded,
                self.rewrite_exclusives_provenance(),
            ) {
                return Err(Error::new(PolicyRefusal).context(refusal));
            }
        }
        Ok(())
    }

    /// Point this run at an OCI image rootfs, as `--image` does.
    ///
    /// Used by `hermit oci run`, which resolves the user's reference to the
    /// store's canonical image id first. A tag is a mutable pointer, so passing
    /// the resolved id — not the reference — is what keeps the rootfs cache from
    /// aliasing two different images that shared a tag.
    pub(crate) fn set_image(&mut self, image: String) {
        self.image = Some(image);
    }

    /// The `--image` reference this run was given, if any.
    pub(crate) fn image(&self) -> Option<&str> {
        self.image.as_deref()
    }

    /// The guest program, after [`Self::resolve_image_default_program`] has
    /// filled an omitted one from the image's Entrypoint/Cmd.
    fn program_path(&self) -> &Path {
        self.program
            .as_deref()
            .expect("guest program resolved by validate_args")
    }

    /// Apply OCI Entrypoint/Cmd semantics when `--image` is set.
    ///
    /// With an explicit program, the image Entrypoint (if any) is prepended and
    /// the explicit program replaces Cmd. With no program, Entrypoint+Cmd
    /// becomes the program. Without an image this only enforces that a program
    /// was given, as clap did when PROGRAM was required.
    fn resolve_image_default_program(&mut self) -> Result<(), Error> {
        if self.image_program_resolved {
            return Ok(());
        }
        self.image_program_resolved = true;
        // Pin the image identity once, here, so config reads, PATH search,
        // and rootfs materialization all key against the same resolved ID
        // even if a tag moves later in the run.
        if let Some(image) = &self.image {
            self.image = Some(crate::app::image::canonical_image_ref(image));
        }
        let Some(image) = self.image.clone() else {
            if self.program.is_none() {
                anyhow::bail!("no guest program given; pass a program after `--`");
            }
            return Ok(());
        };
        // Local image inputs answer Entrypoint/Cmd from their own config and
        // must never reach the podman-store fallback below; a bare-relative
        // prefixed value is refused by name here so even a run with no
        // explicit program names the problem instead of consulting a store.
        let config = match crate::app::image_source::parse_image_source(&image) {
            crate::app::image_source::ImageSourceRef::BareRelative { kind, remainder } => {
                return Err(crate::app::image_source::bare_relative_error(
                    kind, &remainder,
                ));
            }
            crate::app::image_source::ImageSourceRef::Local { kind, path } => {
                Some(crate::app::image_source::peek_local_config(kind, &path)?)
            }
            crate::app::image_source::ImageSourceRef::StoreReference => {
                // Prefer the config persisted at materialization
                // (digest-pinned); fall back to a single store probe for
                // not-yet-materialized images.
                let persisted = crate::app::image::read_image_config(&image).unwrap_or_default();
                if !persisted.cmd.is_empty() || !persisted.entrypoint.is_empty() {
                    Some(crate::app::podman_store::ImageRunConfig {
                        env: persisted.env.clone(),
                        workdir: persisted.workdir.clone(),
                        cmd: persisted.cmd.clone(),
                        entrypoint: persisted.entrypoint.clone(),
                    })
                } else {
                    crate::app::podman_store::PodmanStore::probe()
                        .ok()
                        .flatten()
                        .and_then(|store| {
                            store
                                .resolve(&image)
                                .ok()
                                .and_then(|resolved| store.config(&resolved).ok())
                        })
                }
            }
        };
        let (entrypoint, cmd) = match config {
            Some(c) => (c.entrypoint, c.cmd),
            None => (Vec::new(), Vec::new()),
        };
        match Self::oci_argv(
            &entrypoint,
            &cmd,
            self.program.take(),
            std::mem::take(&mut self.args),
        ) {
            Some((program, args)) => {
                self.program = Some(program);
                self.args = args;
                Ok(())
            }
            None => anyhow::bail!(
                "image {image} declares no Entrypoint or Cmd to run by default; \
                 pass an explicit guest program after `--`"
            ),
        }
    }

    /// OCI argv composition: Entrypoint is always prepended; an explicit program
    /// replaces Cmd, otherwise Cmd supplies the default program. Returns `None`
    /// when neither yields a program.
    fn oci_argv(
        entrypoint: &[String],
        cmd: &[String],
        program: Option<PathBuf>,
        args: Vec<String>,
    ) -> Option<(PathBuf, Vec<String>)> {
        if let Some(program) = program {
            if entrypoint.is_empty() {
                return Some((program, args));
            }
            let mut out: Vec<String> = entrypoint[1..].to_vec();
            out.push(program.to_string_lossy().into_owned());
            out.extend(args);
            return Some((PathBuf::from(&entrypoint[0]), out));
        }
        let mut argv: Vec<String> = entrypoint.to_vec();
        argv.extend(cmd.iter().cloned());
        if argv.is_empty() {
            return None;
        }
        let program = PathBuf::from(&argv[0]);
        let mut out: Vec<String> = argv[1..].to_vec();
        out.extend(args);
        Some((program, out))
    }

    fn selected_backend(&self) -> Backend {
        self.backend.unwrap_or_default()
    }

    pub(super) fn execution_backend_name(&self) -> &str {
        self.application_backend
            .unwrap_or_else(|| self.selected_backend().as_str())
    }

    pub(super) fn adapter_program(&self) -> &Path {
        self.program_path()
    }

    pub(super) fn adapter_arguments(&self) -> &[String] {
        &self.args
    }

    pub(super) fn adapter_working_directory(&self) -> Option<&Path> {
        self.workdir.as_deref().map(Path::new)
    }

    pub(super) fn adapter_saved_directory(&self) -> Option<&Path> {
        self.verify_run.as_deref()
    }

    pub(super) fn adapter_engagement_path(&self) -> Option<&Path> {
        self.backend_engagement_json.as_deref()
    }

    pub(super) fn adapter_verbose_verification(&self) -> bool {
        self.verify_verbose
    }

    pub(super) fn adapter_verification_json(&self) -> Option<&Path> {
        self.verify_json.as_deref()
    }

    pub(super) fn adapter_uses_namespace(&self) -> bool {
        !self.no_namespace
    }

    /// Apply the global backend selection for a `RunOpts` built outside
    /// [`RunOpts::main`], such as `hermit analyze`'s trial options, so that
    /// validation and reproducers see the backend the run will use.
    pub(crate) fn set_backend(&mut self, backend: Option<Backend>) {
        self.backend = backend;
    }

    /// An extension executable selects its one linked adapter. It cannot be
    /// redirected to a built-in backend by a contradictory global option.
    pub(super) fn select_application_backend(&mut self, name: &'static str) -> Result<(), Error> {
        super::backend_hook::validate_name(name)?;
        anyhow::ensure!(
            self.backend.is_none(),
            "this executable selects backend `{name}`; do not also pass --backend"
        );
        self.application_backend = Some(name);
        if let Some(config) = &mut self.config_capture {
            config.global.insert("backend".to_owned(), name.into());
        }
        Ok(())
    }

    /// A new executable does not inherit a ptrace-specific qualification just
    /// because the built-in default enum still exists in this compatibility
    /// step. Refuse unsupported core runtime switches before reading stdin.
    fn refuse_unqualified_application_options(
        &self,
        cpu_profile_refusal: &'static str,
    ) -> Result<(), Error> {
        let Some(name) = self.application_backend else {
            return Ok(());
        };
        let config = &self.det_opts.det_config;
        let options = [
            (self.namespace_only, "--namespace-only"),
            (self.strace_only, "--strace-only"),
            (self.image.is_some(), "--image"),
            (self.dap_listen.is_some(), "--dap-listen"),
            (config.gdbserver, "--gdbserver"),
            (self.in_guest_exec_stop, "--in-guest-exec-stop"),
            (self.skid_margin.is_some(), "--skid-margin"),
            (self.fatal_core_dir.is_some(), "--fatal-core-dir"),
            (self.happens_before.is_some(), "--happens-before"),
            (config.futex_wake_yields, "--futex-wake-yields"),
            (config.cap_preemption_charge, "--cap-preemption-charge"),
            (
                config.untrap_passthrough_syscalls,
                "--untrap-passthrough-syscalls",
            ),
            (config.signal_at.is_some(), "--signal-at"),
            (config.pipe_wake_latency.is_some(), "--pipe-wake-latency"),
            (config.work_span_report.is_some(), "--work-span-report"),
            (self.record_networking.is_some(), "--record-networking"),
            (self.replay_networking.is_some(), "--replay-networking"),
            (
                config.network_trace.mode != NetworkTraceMode::Off,
                "network tracing",
            ),
        ];
        for (requested, option) in options {
            if requested {
                return Err(Error::new(PolicyRefusal).context(format!(
                    "{option} is not qualified for executable-selected backend `{name}`"
                )));
            }
        }
        // `auto` is the unset default, not a named nondefault profile: it
        // resolves (or is refused by name) at launch admission, where a
        // profile it resolves to that this backend is not qualified for is
        // refused there. Refusing it here would preempt every other
        // unqualified-option refusal for a run that named no profile.
        if !config.cpu_profile.is_default()
            && config.cpu_profile != detcore_model::cpu_profile::CpuProfile::Auto
        {
            return Err(Error::new(PolicyRefusal).context(
                hermit::cpu_profile_admission::CpuProfileRefusal::new(
                    config.cpu_profile,
                    format!("--backend={name}: {cpu_profile_refusal}"),
                ),
            ));
        }
        if config.virtual_cpus.is_some() {
            return Err(Error::new(PolicyRefusal).context(format!(
                "--virtual-cpus is not qualified for executable-selected backend `{name}`"
            )));
        }
        #[cfg(any(feature = "native-exec", feature = "native-exec-input-admission"))]
        if self.native_exec_inputs.is_some() {
            return Err(Error::new(PolicyRefusal)
                .context("drop --native-exec-inputs, or rebuild hermit-run with --features native-exec and select --backend=in-guest-trap")
                .context("an external physical adapter cannot consume a native exec enrollment"));
        }
        Ok(())
    }

    /// An admission-only build can recognize the request, but cannot silently
    /// run it as ordinary execution. The existing full native-exec build keeps
    /// its original implementation and refusal order.
    #[cfg(all(feature = "native-exec-input-admission", not(feature = "native-exec")))]
    fn refuse_uncompiled_native_exec(&self, backend: Backend) -> Result<(), Error> {
        if self.native_exec_inputs.is_none() {
            return Ok(());
        }
        if self.application_backend.is_some() {
            return Err(Error::new(PolicyRefusal)
                .context("drop --native-exec-inputs, or rebuild hermit-run with --features native-exec and select --backend=in-guest-trap")
                .context("an external physical adapter cannot consume a native exec enrollment"));
        }
        if backend != Backend::InGuestTrap {
            return Err(Error::new(PolicyRefusal)
                .context("select --backend=in-guest-trap and rebuild hermit-run with --features native-exec, or drop --native-exec-inputs")
                .context("native exec currently requires in-guest-trap"));
        }
        Err(Error::new(PolicyRefusal)
            .context("rebuild hermit-run with --features native-exec, or drop --native-exec-inputs")
            .context("native exec execution is not compiled into this executable"))
    }

    /// Lower `--strace-only` onto exactly the options its help names: `--tmp=/tmp
    /// --network=host --no-virtualize-cpuid --no-virtualize-time
    /// --no-virtualize-metadata --no-sequentialize-threads --no-deterministic-io
    /// --no-rcb-time` (a `--tmp` given explicitly is kept). Every reader of those
    /// options then sees the spelled-out configuration: thread pinning, Detcore's
    /// own flag implications in `Config::validate`, the time/metadata check, and
    /// the `--dap-listen` supervisor's guest-path mapping, which runs before
    /// validation. Applied late, after the pin decision, the shorthand used to pin
    /// every guest thread to one CPU, which the expansion does not do.
    ///
    /// Idempotent, and a no-op without `--strace-only`.
    pub(crate) fn lower_strace_only(&mut self) {
        if !self.strace_only {
            return;
        }
        self.tmp.get_or_insert_with(|| PathBuf::from(TMP_DIR));
        self.network = NetworkingMode::Host;
        self.no_sequentialize_threads = true;
        self.no_deterministic_io = true;
        let config = &mut self.det_opts.det_config;
        config.virtualize_cpuid = false;
        config.virtualize_time = false;
        config.virtualize_metadata = false;
        config.no_rcb_time = true;
    }

    /// `main`'s prelude: what it settles before it can dispatch anywhere, and
    /// where it dispatches. The `--dap-listen` supervisor never reaches
    /// `validate_args`, so the global backend, the `--strace-only` lowering,
    /// the exec-stop admission, and the `--gdbserver` a DAP listener implies
    /// (set before validation, so every `--gdbserver` refusal applies) are all
    /// settled before the choice is made. `inner` is the value of
    /// `DAP_LISTEN_INNER_ENV`, which marks the run a supervisor started.
    fn dispatch_before_validation(
        &mut self,
        backend: Option<Backend>,
        inner: Option<std::ffi::OsString>,
    ) -> Result<RunDispatch, Error> {
        self.set_backend(backend);
        self.lower_strace_only();
        self.refuse_virtual_timeout_with_debugger()?;
        self.prepare_mapped_verification_inputs()?;
        self.refuse_unready_exec_stop()?;
        // Before stdin is read or any backend is probed, so a backend that
        // cannot even start here gets the same verdict, and before
        // `--dap-listen` binds its port and starts the run it supervises.
        self.refuse_gdbserver_without_a_gdbserver()?;
        if self.dap_listen.is_none() {
            return Ok(RunDispatch::Run);
        }
        self.det_opts.det_config.gdbserver = true;
        match inner {
            None => Ok(RunDispatch::DapSupervisor),
            // The run a supervising `hermit run --dap-listen` started.
            Some(owner) if started_by_a_dap_supervisor(&owner) => Ok(RunDispatch::Run),
            Some(owner) => anyhow::bail!(
                "{DAP_LISTEN_INNER_ENV}={owner:?} is set, but its parent did not start this \
                 hermit run --dap-listen; unset {DAP_LISTEN_INNER_ENV}"
            ),
        }
    }

    /// The global `--backend` option that selected this run's backend, rendered
    /// for the position before the subcommand (`hermit{this} run ...`). Empty
    /// when no backend was requested explicitly.
    pub(crate) fn global_backend_arg(&self) -> String {
        self.application_backend
            .or_else(|| self.backend.map(|backend| backend.as_str()))
            .map(|name| format!(" --backend={name}"))
            .unwrap_or_default()
    }
    fn runtime_backend(&self) -> Backend {
        self.selected_backend()
    }

    /// Whether this run's Tool writes its DETLOG records to the guest's standard
    /// error for `--verify` to move into the run's log: SaBRe's plugin and the
    /// in-guest LiteInst runtime (`liteinst` and `in-guest-trap`) both do.
    fn forwards_in_guest_detlogs(&self) -> bool {
        let backend = self.selected_backend();
        backend.is_in_guest_liteinst()
    }

    /// Select forwarding with the same backend predicate before common setup.
    fn forwarded_detlog_plan(&self) -> Result<ForwardedDetlogPlan, Error> {
        if !self.uses_in_guest_liteinst() {
            return Ok(ForwardedDetlogPlan::None);
        }
        Ok(ForwardedDetlogPlan::Socket { opened: false })
    }

    fn forwarded_detlog_plan_for_transport(
        &self,
        transport: Option<super::backend_hook::VerificationLogTransport>,
    ) -> Result<ForwardedDetlogPlan, Error> {
        use super::backend_hook::VerificationLogTransport;
        match transport {
            Some(VerificationLogTransport::InGuestSocket) => {
                Ok(ForwardedDetlogPlan::Socket { opened: false })
            }
            Some(
                VerificationLogTransport::CoordinatorTracing
                | VerificationLogTransport::NativeEvidence,
            ) => Ok(ForwardedDetlogPlan::None),
            None => self.forwarded_detlog_plan(),
        }
    }

    /// A Unix `SOCK_SEQPACKET` pair for one verification run's forwarded
    /// DETLOG records, when the run hosts Detcore inside the guest (in-guest
    /// LiteInst or SaBRe): the guest's runtime sends each record as one message
    /// on the sending end, a descriptor Reverie protects from the guest, so the
    /// guest's own output never mixes with them
    /// (`hermit::forward_in_guest_detlogs_to`). Returns the sending end and the
    /// receiving end; inside the run's container, the coordinator writes what
    /// arrives on the receiving end into the run's log in order
    /// (`detcore::detlog::set_forwarded_source`).
    pub(super) fn open_forwarded_detlog_channel()
    -> Result<Option<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)>, Error> {
        let mut pair = [-1; 2];
        // SAFETY: socketpair writes two descriptors into `pair` on success.
        let created = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                pair.as_mut_ptr(),
            )
        };
        if created != 0 {
            return Err(Error::new(std::io::Error::last_os_error())
                .context("creating the in-guest DETLOG forwarding socket pair"));
        }
        // SAFETY: socketpair returned two new descriptors this function owns.
        let [sender, receiver] = pair
            .map(|fd| unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) });
        // Room for the largest record; the kernel caps the request at its limit.
        let size: libc::c_int = 4 << 20;
        for (fd, option) in [(&sender, libc::SO_SNDBUF), (&receiver, libc::SO_RCVBUF)] {
            // SAFETY: a valid socket and an int option value.
            unsafe {
                libc::setsockopt(
                    fd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    option,
                    (&raw const size).cast(),
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                )
            };
        }
        Ok(Some((sender, receiver)))
    }

    /// Moves the DETLOG records this run's in-guest Tool forwarded into the
    /// run's log and returns how many are syscall records, or `None` when the
    /// backend forwards none. In-guest LiteInst and SaBRe send them on their
    /// own socket (`socket`), whose records the coordinator already wrote into
    /// the log in order during the run; they are only counted here, and a run
    /// whose socket carried anything but records is refused. Without the
    /// socket, SaBRe's plugin forwards them on the guest's standard error, from
    /// which they are cut out and appended.
    fn take_forwarded_detlogs_for_transport(
        &self,
        log: &Path,
        stderr: &mut Vec<u8>,
        socket: bool,
        transport: Option<super::backend_hook::VerificationLogTransport>,
    ) -> Result<Option<usize>, Error> {
        use super::backend_hook::VerificationLogTransport;
        let forwards = transport.map_or_else(
            || self.forwards_in_guest_detlogs(),
            |source| source == VerificationLogTransport::InGuestSocket,
        );
        if !forwards {
            return Ok(None);
        }
        if socket {
            return forwarded_records_in_log(log).map(Some);
        }
        let limit = log_max_bytes().map_err(Error::msg)?;
        extract_forwarded_detlogs(log, stderr, limit, false).map(Some)
    }

    /// Whether this run hosts Detcore inside the guest through LiteInst
    /// (`liteinst` or `in-guest-trap`). `--namespace-only` runs no backend, so
    /// it never does.
    fn uses_in_guest_liteinst(&self) -> bool {
        !self.namespace_only && self.selected_backend().is_in_guest_liteinst()
    }

    /// Native launches qualify their embedded artifact catalog in
    /// `Capture::prepare` before either physical verification run starts.
    fn uses_native_exec(&self) -> bool {
        #[cfg(feature = "native-exec")]
        {
            self.native_exec_inputs.is_some()
        }
        #[cfg(not(feature = "native-exec"))]
        {
            false
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-3635): Review the in-guest LiteInst option refusals.
    /// Refuses options that in-guest LiteInst cannot honour yet, so a run never
    /// silently drops one of them.
    ///
    /// These refusals are static facts about the backend, so they hold in
    /// every build and come before the availability check: a command gets the
    /// same verdict whether or not the build has the `liteinst` feature
    /// (https://github.com/rrnewton/hermit/issues/3418). The maximum-timeslice
    /// advice is the exception. The timeslice is on by default, so in a build
    /// without the feature that advice would answer every run, and following
    /// it would only reach the build-feature refusal one run later. Such a
    /// build skips it, and `refuse_liteinst_without_the_feature` names the
    /// missing feature instead.
    /// Admit a nondefault CPU profile, with its own required host checks.
    /// v3 binds qualified XSTATE facts; host-no-rdrand requires CPUID faulting
    /// without running the v3 XSTATE qualification. A v2 run is untouched.
    fn admit_cpu_profile(&mut self) -> Result<(), Error> {
        self.resolve_auto_cpu_profile()?;
        let config = &self.det_opts.det_config;
        // Still auto only where it could not resolve, which a later check or
        // the launch refuses: nothing to admit.
        if config.cpu_profile.is_default()
            || config.cpu_profile == detcore_model::cpu_profile::CpuProfile::Auto
        {
            return Ok(());
        }
        let backend = self.selected_backend();
        let request = hermit::cpu_profile_admission::AdmissionRequest {
            backend: self.execution_backend_name(),
            ptrace: self.application_backend.is_none() && backend == Backend::Ptrace,
            virtualize_cpuid: config.virtualize_cpuid,
            virtualize_time: config.virtualize_time,
            strace_only: self.strace_only,
            gdbserver: config.gdbserver,
            namespace_only: self.namespace_only,
        };
        if config.cpu_profile == detcore_model::cpu_profile::CpuProfile::HostNoRdrand {
            // Network traces, like syscall recordings, do not bind this
            // profile's host CPUID state. Refuse before host admission,
            // manifest creation or opening/truncating the trace. A network
            // manifest restored by `run --from` also sets replay_networking.
            if self.record_networking.is_some()
                || self.replay_networking.is_some()
                || config.network_trace.mode != NetworkTraceMode::Off
            {
                hermit::cpu_profile_admission::check_record_or_replay(config.cpu_profile)
                    .map_err(|refusal| Error::new(PolicyRefusal).context(refusal.to_string()))?;
            }
            let source = hermit::cpu_profile_admission::admit_host_no_rdrand(&request).map_err(
                |refusal| {
                    Error::new(PolicyRefusal)
                        .context(format!("--cpu-profile host-no-rdrand refused: {refusal}"))
                },
            )?;
            self.det_opts.det_config.cpu_profile_source = Some(source);
            tracing::info!("--cpu-profile host-no-rdrand admitted: CPUID faulting verified");
            eprintln!("{}", detcore_model::cpu_profile::HOST_NO_RDRAND_RESIDUAL);
            return Ok(());
        }
        match hermit::cpu_profile_admission::admit(&request) {
            Ok((probe, host)) => {
                log_v3_admission(&probe);
                self.det_opts.det_config.cpu_profile_host = Some(host);
                super::verify::CPU_PROFILE_V3_RAN.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
            Err(refusal) => Err(Error::new(PolicyRefusal)
                .context(format!("--cpu-profile x86-64-v3 refused: {refusal}"))),
        }
    }

    /// Refuse before a pending DAP listener is lowered to the GDB configuration,
    /// and before direct callers can bind a debugger port or launch a container.
    fn refuse_virtual_timeout_with_debugger(&self) -> Result<(), Error> {
        let config = &self.det_opts.det_config;
        detcore_model::virtual_timeout::refuse_debugger(
            config.virtual_timeout,
            config.gdbserver || self.dap_listen.is_some(),
        )?;
        Ok(())
    }

    /// Refuses `--gdbserver`, and `--dap-listen`, which implies it, where the
    /// run would have no gdbserver. Only Reverie's ptrace tracer serves gdb:
    /// `--backend=ptrace`, the default, and `--backend=e9patch`, whose
    /// rewritten guest runs under it. The KVM, DBT and SaBRe backends used to
    /// run the guest without a gdbserver and without a word, so the debugger
    /// waited on a port nothing served, and a `--strict` run took the flag
    /// once `--gdbserver` stopped forcing host networking. `--namespace-only`
    /// runs no tracer at all. The in-guest LiteInst backends refuse it on
    /// their own (`refuse_unqualified_liteinst_in_guest_options`).
    fn refuse_gdbserver_without_a_gdbserver(&self) -> Result<(), Error> {
        let option = if self.dap_listen.is_some() {
            "--dap-listen"
        } else if self.det_opts.det_config.gdbserver {
            "--gdbserver"
        } else {
            return Ok(());
        };
        let (cause, remedy) = if self.namespace_only {
            (
                "--namespace-only runs the guest without a tracer".to_owned(),
                "drop --namespace-only".to_owned(),
            )
        } else {
            match self.selected_backend() {
                backend @ Backend::Kvm => (
                    format!("the {} backend has none", backend.as_str()),
                    format!(
                        "run it with the ptrace backend: drop --backend={} (ptrace is the \
                         default) or pass --backend=ptrace",
                        backend.as_str()
                    ),
                ),
                _ => return Ok(()),
            }
        };
        Err(Error::new(PolicyRefusal).context(format!(
            "{option} needs a gdbserver, which only the ptrace backend runs: {cause}, so no \
             debugger could attach; {remedy}"
        )))
    }

    /// Resolve `--cpu-profile auto` to the named profile the guest program
    /// and its program interpreter need
    /// (`hermit::cpu_profile_admission::resolve_auto`), before that profile
    /// is admitted like an explicit one. The configuration, and with it any
    /// recording and the second run of `--verify`, then holds only the named
    /// profile. A program auto cannot read or resolve is refused by name at
    /// the launch.
    fn resolve_auto_cpu_profile(&mut self) -> Result<(), Error> {
        use detcore_model::cpu_profile::CpuProfile;
        if self.det_opts.det_config.cpu_profile != CpuProfile::Auto {
            return Ok(());
        }
        // A program that cannot run keeps its own fault (127 or 126), which
        // `validate_program` reports in its place among the run's checks:
        // auto stays unresolved, and nothing is admitted or launched for it.
        if self.validate_program().is_err() {
            return Ok(());
        }
        let (program, _, rootfs) = self.guest_program_on_host()?;
        let to_host = |path: &Path| match &rootfs {
            Some(rootfs) => Some(crate::app::image::resolve_in_rootfs(rootfs, path)),
            None => match self.mapped_host_program(path) {
                GuestPathMapping::Mapped(host) => Some(host),
                GuestPathMapping::Unchanged => Some(path.to_path_buf()),
                GuestPathMapping::Hidden => None,
            },
        };
        // Where auto cannot resolve (an unreadable program, a level no
        // profile presents), it stays auto: the run's own checks keep their
        // place and their refusals, and the launch then refuses auto by name
        // (`resolve_launch_auto`), before any guest runs. Never a guess.
        let Ok((profile, why)) = hermit::cpu_profile_admission::resolve_auto(&program, to_host)
        else {
            return Ok(());
        };
        self.auto_resolution = Some(format!("--cpu-profile auto resolved to {profile}: {why}"));
        self.det_opts.det_config.cpu_profile = profile;
        Ok(())
    }

    /// Log what `--cpu-profile auto` resolved to, once tracing has started,
    /// where the run's log goes (`--log=debug` and finer). Not at info: a
    /// rerun from the run's manifest names the resolved profile, resolves
    /// nothing, and must log what the original logged at info.
    fn log_auto_resolution(&self) {
        if let Some(resolution) = &self.auto_resolution {
            tracing::debug!("{resolution}");
        }
    }

    fn refuse_unqualified_liteinst_in_guest_options(&self) -> Result<(), Error> {
        if !self.uses_in_guest_liteinst() {
            return Ok(());
        }
        let config = &self.det_opts.det_config;
        let reason = if cfg!(feature = "liteinst") && config.max_timeslice.is_some() {
            "it cannot deliver Detcore's preemption timer yet (the in-guest Tool host refuses \
             set_timer with ENOSYS), so the run would fail at its first timeslice; pass \
             --max-timeslice=disabled"
        } else if self.timeout.is_some() {
            "--timeout has not been qualified for the in-guest runtime, whose run future does \
             not kill the guest when it is dropped"
        } else if self.skid_margin.is_some() {
            "--skid-margin configures the PMU preemption timer, which the in-guest runtime \
             does not arm"
        } else if config.gdbserver {
            "--gdbserver needs the ptrace tracer, which the in-guest runtime does not attach"
        } else {
            return Ok(());
        };
        Err(Error::new(PolicyRefusal).context(format!(
            "--backend={} (in-guest LiteInst) refuses this run: {reason}",
            self.selected_backend().as_str()
        )))
    }

    /// In a build without the `liteinst` feature, refuses `--backend=liteinst`
    /// for the missing feature ("this build was compiled without the liteinst
    /// backend"), in place of the inherited in-guest maximum-timeslice advice.
    /// Explicit unsupported timer requests are refused before this check.
    ///
    /// `main` calls it after every refusal that holds in every build: the
    /// in-guest option refusals, the `--max-log-bytes` refusal, the `--verify`
    /// log-level check, argument validation and replay-epoch reconciliation.
    /// So an unqualified `--timeout`, an unenforceable `--max-log-bytes` or a
    /// conflicting `--epoch` gets the same verdict whatever the build. It is
    /// also before `--verify` reads stdin, so this refusal does not
    /// wait for input either. Builds with the feature are unchanged: they
    /// check whether the LiteInst runtime is installed later, with the other
    /// backends.
    fn refuse_liteinst_without_the_feature(&self) -> Result<(), Error> {
        if cfg!(feature = "liteinst") || !self.uses_in_guest_liteinst() {
            return Ok(());
        }
        self.selected_backend().ensure_available()
    }

    /// Why `--max-log-bytes` cannot end a run of this backend in this namespace
    /// mode, or `None` when the guest runs inside hermit's PID namespace, which
    /// dies with the hermit process that would end the run.
    ///
    /// The cap ends a run by `_exit(123)` in whichever hermit process crossed
    /// it: the outer process or a container init. That ends the RUN only if the
    /// guest tree cannot outlive that process. The one mechanism that makes sure
    /// of it from before the guest exists is a PID namespace whose init is
    /// hermit's container init: the namespace dies with its init, and the init
    /// dies with the outer process through `container::arm_parent_death_signal`,
    /// armed before the init starts the guest. A crossing that lands before that
    /// arming, when the death signal can no longer come, is caught right after
    /// it by `tracing::exit_if_log_cap_already_crossed`.
    /// A ptrace tracee with `PTRACE_O_EXITKILL` also dies with its tracer, but
    /// Reverie sets that option only after the guest exists (below), so it
    /// binds the guest for the rest of the run, not for all of it.
    ///
    /// So every `--no-namespace` configuration is refused, and DBT is refused in
    /// both modes:
    ///
    /// - DBT with `--no-namespace`: `backends::run_dbt` starts `drrun` as a
    ///   plain child of the outer process, and Reverie's DBT launcher sets only
    ///   a process group and the personality: no parent-death signal and no
    ///   ptrace attachment. With namespaces, the guest is inside hermit's PID
    ///   namespace, but the cap has not yet been tested ending such a run.
    /// - ptrace with `--no-namespace`, and `--backend=e9patch` with it (e9patch
    ///   preprocessing, whose rewritten guest runs on the ptrace backend,
    ///   `runtime_backend`): Reverie's
    ///   `TracerBuilder::spawn` clones the guest, starts the `--gdbserver`
    ///   server if one is asked for, and only then, in `postspawn`, sets
    ///   `PTRACE_O_EXITKILL`. Linux clears the inherited parent-death signal in
    ///   the clone, so in between the guest is bound to nothing, and a charged
    ///   write can cross the cap there: the outer process's asynchronous file
    ///   appender draining a queued record, or the GDB server's charged
    ///   accepted-client event (round-4c review of
    ///   https://github.com/rrnewton/hermit/pull/3686, finding 1).
    /// - LiteInst with `--no-namespace` (LiteInst runs only in-guest at this
    ///   base): Reverie spawns the guest directly and does not trace it, and
    ///   no PID namespace contains it.
    /// - SaBRe with `--no-namespace`: `sabre_ptrace` detaches the guest with
    ///   SIGSTOP to hand it to the supervisor worker, which re-attaches and
    ///   only then sets `PTRACE_O_EXITKILL`. In between the guest is stopped,
    ///   untraced and in no PID namespace, and the worker's charged trace event
    ///   can cross the cap there, which would leave a stopped guest behind.
    /// - KVM with `--no-namespace`: reverie-kvm creates host processes at
    ///   several sites, and only some of them are shown to arm a parent-death
    ///   signal. Unproven is refused until each site is audited; inside
    ///   hermit's PID namespace the question does not arise.
    ///
    /// The match has no wildcard arm, so a new backend must be classified here
    /// before it compiles. Not every subcommand that accepts the global flag
    /// passes through this check, and each one stands as follows (round-3
    /// review of https://github.com/rrnewton/hermit/pull/3686, finding 6):
    ///
    /// - `run` and `oci run` call it through `RunOpts::main`.
    /// - `analyze` and `bisect` send their trials straight to `RunOpts::run`
    ///   and call it through `AnalyzeOpts::refuse_unsupervised_log_cap` before
    ///   any trial. `Subcommand::validate_backend_scope` admits only KVM there
    ///   besides ptrace, so `--no-namespace` given through `--run-arg` or after
    ///   `--`, with either, is the case it catches. They add no
    ///   `--no-namespace` of their own.
    /// - `record` and `replay` start their containers through
    ///   `owned_container::run` and never call it. They have no
    ///   `--no-namespace` mode: every one of those containers asks for a
    ///   private PID namespace, and their GDB servers (`replay` and `record
    ///   start --verify-with-gdbex`) run inside it.
    /// - `hermit --backend=sabre strace` starts no container: `StraceOpts::main`
    ///   refuses the flag itself, and `strace` with any other backend fails
    ///   before it starts anything.
    /// - `instruction-map` and `record list`, `remove` and `clean` charge their
    ///   tracing but run no guest. `version`, `host-capabilities`, `log-diff`
    ///   and `oci ls`, `inspect` and `pull` initialize no tracing at all.
    fn log_cap_refusal(backend: Backend, no_namespace: bool) -> Option<&'static str> {
        match backend {
            // In its container the guest is inside hermit's PID namespace, but
            // no run has yet shown the cap ending a DBT run there
            // (https://github.com/rrnewton/hermit/issues/4009).
            Backend::Liteinst if no_namespace => Some(
                "--backend=liteinst and --no-namespace: the in-guest LiteInst guest is neither \
                 a ptrace tracee nor inside a PID namespace hermit owns, so exiting 123 at the \
                 cap would leave the guest running; drop --no-namespace",
            ),
            Backend::InGuestTrap if no_namespace => Some(
                "--backend=in-guest-trap and --no-namespace: the in-guest LiteInst guest is \
                 neither a ptrace tracee nor inside a PID namespace hermit owns, so exiting 123 \
                 at the cap would leave the guest running; drop --no-namespace",
            ),

            Backend::Kvm if no_namespace => Some(
                "--backend=kvm and --no-namespace: no PID namespace contains the KVM backend's \
                 host processes, and they are not shown to die with hermit, so exiting 123 at \
                 the cap might leave the guest running; drop --no-namespace",
            ),
            Backend::Ptrace if no_namespace => Some(
                "--no-namespace: the ptrace tracer installs PTRACE_O_EXITKILL only after the \
                 guest exists, and after it starts any --gdbserver server, so a log write can \
                 cross the cap before the guest is bound to hermit; no PID namespace contains \
                 the guest, so exiting 123 there would leave it running; drop --no-namespace",
            ),
            Backend::Ptrace | Backend::Liteinst | Backend::InGuestTrap | Backend::Kvm => None,
        }
    }

    /// Refuses `--max-log-bytes` (exit 122) where [`Self::log_cap_refusal`]
    /// says the cap could end hermit and leave the guest running: DBT, and
    /// `--no-namespace` with any backend (`--gdbserver` included, which used
    /// to be a separate rule here and is now a case of `--no-namespace`).
    /// Hermit does not supervise the guest tree itself, so the honest answer
    /// is to refuse the combination rather than report a run as stopped when
    /// it is not.
    pub(crate) fn refuse_unsupervised_log_cap(
        &self,
        max_log_bytes: Option<u64>,
    ) -> Result<(), Error> {
        if max_log_bytes.is_none() {
            return Ok(());
        }
        if let Some(name) = self.application_backend {
            return Err(Error::new(PolicyRefusal).context(format!(
                "--max-log-bytes is not qualified to stop executable-selected backend `{name}`"
            )));
        }
        let refusal = Self::log_cap_refusal(self.selected_backend(), self.no_namespace);
        match refusal {
            None => Ok(()),
            Some(reason) => Err(Error::new(PolicyRefusal)
                .context(format!("--max-log-bytes cannot be enforced with {reason}"))),
        }
    }

    /// Which comparator a `--verify` run uses, as a function of the request
    /// ALONE.
    ///
    /// DELIBERATELY BACKEND-INDEPENDENT, AND THAT IS THE PROPERTY UNDER GUARD.
    /// This used to read `let kvm_output_only = self.selected_backend() ==
    /// Backend::Kvm`, so plain KVM verification bypassed internal-log comparison
    /// entirely and `--verify-strict` could not reach the canonical comparator on
    /// that backend at all. The special case was not narrowed, it was removed:
    /// every backend now compares both captured logs and picks its comparator
    /// here from `verify_verbose`/`verify_strict` only.
    ///
    /// Removing a branch leaves nothing behind to notice its return, so
    /// `comparator_choice_does_not_depend_on_the_backend` pins the absence
    /// across every `Backend` variant. Re-introducing a backend term here is the
    /// regression it exists to catch.
    ///
    /// `--verify-verbose` historically implied a bitwise compare (it flipped
    /// `strip_lines` off and `FullTrace` on); that is preserved, and
    /// `--verify-strict` selects the same comparison quietly.
    pub(super) fn verification_strictness(&self) -> LogCompareStrictness {
        if self.verify_verbose || self.verify_strict {
            LogCompareStrictness::Canonical
        } else {
            LogCompareStrictness::Stripped
        }
    }

    fn verification_comparison_options(&self) -> ComparisonOptions {
        // Use the configuration the selected runtime backend actually receives.
        // `run_with_output_backend` applies this same normalization before
        // constructing Detcore. Reading the pre-normalization CLI value here
        // would let a backend disable an observation while the verdict still
        // reported that it participated.
        let config = if self.application_backend.is_some() {
            self.effective_det_config()
        } else {
            hermit::prepare_backend_config(self.effective_det_config(), self.runtime_backend())
        };
        ComparisonOptions {
            verbose: self.verify_verbose,
            strictness: self.verification_strictness(),
            // The original KVM defect made this false for one backend, reducing
            // verification to guest-output comparison. Keep the actual options
            // consumed by compare_two_runs behind the all-backend regression
            // bracket above.
            compare_logs: true,
            diagnostic_full_trace: self.verify_verbose,
            compare_io_buffers: config.detlog_io_buffers,
            keep_logs: self.keep_logs,
            // `verify` sets this when a skid overshoot overrides the match.
            match_overridden: false,
            failed_log_retention: (!self.keep_logs).then(default_failed_verify_log_retention),
            record_envelope: RecordEnvelope::all_records_v1(),
            // Read from the LIVE config, not a constant: `--no-virtualize-time`
            // makes this a genuine runtime choice on the run path, so a hard-coded
            // `true` would publish a time policy the run did not use. The
            // record/replay path is a fixed decision instead and names it once, as
            // `RECORD_REPLAY_VIRTUALIZES_TIME`.
            virtualize_time: config.virtualize_time,
            caller_owns_logs: false,
        }
    }

    /// The `--verify-json` path this invocation will publish a verdict to, if
    /// any. Exposed so the top-level dispatcher can stamp the invocation-bound
    /// NO-RESULT record before ANY fallible preflight runs — several of this
    /// function's own early exits, and the DBT / `--namespace-only` bypasses
    /// below, never reach `verify()` at all.
    pub(crate) fn verify_json_path(&self) -> Option<&Path> {
        self.verify.then_some(self.verify_json.as_deref()).flatten()
    }

    fn retained_verify_log_dir(&self) -> Result<Option<PathBuf>, Error> {
        if !self.keep_logs {
            return Ok(None);
        }
        let directory = match &self.verify_log_dir {
            Some(directory) => directory.clone(),
            None => dirs::state_dir()
                .ok_or_else(|| {
                    Error::msg(
                        "--keep-logs needs --verify-log-dir because no user state directory is available",
                    )
                })?
                .join("hermit")
                .join("verify-logs"),
        };
        fs::create_dir_all(&directory).with_context(|| {
            format!(
                "could not create verification log directory {}",
                directory.display()
            )
        })?;
        Ok(Some(fs::canonicalize(&directory).with_context(|| {
            format!(
                "could not resolve verification log directory {}",
                directory.display()
            )
        })?))
    }

    // Pure option/availability admission. Availability comes from an already
    // retained package or initial discovery; actual file checks remain required.
    #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
    fn exec_stop_common_domain(&self, package_available: bool) -> bool {
        self.selected_backend() == Backend::InGuestTrap
            && package_available
            && !self.namespace_only
            && !self.strace_only
            && self.dap_listen.is_none()
            && !self.det_opts.det_config.gdbserver
            && self.det_opts.det_config.network_trace == NetworkTraceConfig::default()
            && self
                .guest_command()
                .is_ok_and(|command| command.find_program().is_ok())
    }

    #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
    fn exec_stop_verification_domain(&self, package_available: bool) -> bool {
        self.exec_stop_common_domain(package_available)
            && ((self.verify && self.verify_run.is_none())
                || (!self.verify
                    && self.verify_run.is_some()
                    && self.native_exec_shared_inputs.is_some()))
            && !self.no_namespace
    }

    #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
    fn mapped_command_config(
        &self,
        identity_sources: Option<&IdentityGuard>,
    ) -> Result<(Command, detcore::Config), Error> {
        let command = self.guest_command()?;
        let config = complete_container_config(
            self.effective_det_config(),
            identity_sources,
            self.launch_workdir(),
        )?;
        let mut config = hermit::prepare_in_guest_loader_route_config(config)?;
        // The encoded ForwardPolicy cannot carry span/field-dependent answers.
        // Refuse those actual RUST_LOG directives before enabling plain INFO.
        let directives = match std::env::var("RUST_LOG") {
            Ok(value) => value,
            Err(std::env::VarError::NotPresent) => String::new(),
            Err(error) => return Err(Error::msg(error)),
        };
        detcore::detlog::validate_plain_info_directives(&directives).map_err(Error::msg)?;
        config.in_guest_detlog_forward_policy =
            Some(hermit::in_guest_detlog_forward_policy_for_native().encode());
        Ok((command, config))
    }

    fn verification_run_log(
        &self,
        log_file: &mut Option<fs::File>,
        global: &GlobalOpts,
    ) -> Result<(SharedLog, super::tracing::TracingGuard), Error> {
        // HACK: Use interior mutability to workaround not being able to pass
        // `log_file` by value. Guaranteed by caller to never panic.
        let log_file = log_file.take().unwrap();

        let strictness = self.verification_strictness();
        let level = verification_log_level(global.log, strictness, self.verify_verbose);

        // Bound this log too. `hermit run --verify` opens its own file and
        // calls `init_file_tracing` directly instead of going through
        // `GlobalOpts::init_tracing`, so without this wrapper the bound that
        // was measured on exactly these `--verify` comparison logs would not
        // apply to the path that produces them: a livelocked run here could
        // still fill the disk.
        let limit = log_max_bytes().map_err(Error::msg)?;
        // SYNCHRONOUS, so a fatal diagnostic in the tail survives the run that
        // emitted it. The non-blocking appender loses whatever has not drained
        // when a fail-closed guest dies, which is precisely the line naming the
        // cause -- measured 0 of 18 runs versus 4 of 4. See
        // `init_sync_file_tracing`.
        // The coordinator writes forwarded records through this same writer, so
        // a record and a tracing event never share a line.
        //
        // `--max-log-bytes` counts these logs too, forwarded records as well
        // as tracing events, and its counter is shared across the verify runs,
        // so the cap is a total for the invocation.
        let log = verification_log(log_file, limit, global.log_budget());
        let guard = init_sync_file_tracing(Some(level), log.clone(), self.runtime_backend());
        Ok((log, guard))
    }

    #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
    fn exec_stop_direct_domain(&self, package_available: bool) -> bool {
        self.exec_stop_common_domain(package_available) && !self.verify && self.verify_run.is_none()
    }

    /// Admit the opt-in packaged first-image direct-run or verification domain. Resource
    /// availability does not replace the retained image and actual startup checks.
    pub(crate) fn refuse_unready_exec_stop(&self) -> Result<(), Error> {
        #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
        let package_available = self.in_guest_exec_stop
            && !self.exec_stop_epoch_refused
            && self.selected_backend() == Backend::InGuestTrap
            && (self.mapped_shared_image.is_some()
                || super::mapped_first_admission::packaged_image_declared());
        #[cfg(not(all(feature = "exec-stop-launcher", feature = "liteinst")))]
        let package_available = false;
        self.refuse_unready_exec_stop_with_availability(package_available)
    }

    // Availability DATA cannot bypass the sticky epoch refusal or real options.
    fn refuse_unready_exec_stop_with_availability(
        &self,
        package_available: bool,
    ) -> Result<(), Error> {
        #[cfg(not(all(feature = "exec-stop-launcher", feature = "liteinst")))]
        let _ = package_available;
        if self.in_guest_exec_stop {
            if self.verify_run.is_some() && self.epoch_omitted {
                return Err(Error::new(PolicyRefusal)
                    .context("mapped-shared-verification-requires-one-explicit-pair-epoch"));
            }
            #[cfg(any(feature = "native-exec", feature = "native-exec-input-admission"))]
            if self.native_exec_inputs.is_some() {
                return Err(Error::new(PolicyRefusal).context(
                    "--in-guest-exec-stop: exec-stop-runtime-not-ready: native successor verification is not admitted",
                ));
            }
            #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
            if !self.exec_stop_epoch_refused
                && (self.exec_stop_direct_domain(package_available)
                    || self.exec_stop_verification_domain(package_available))
            {
                return Ok(());
            }
            return Err(Error::new(PolicyRefusal).context(
                "--in-guest-exec-stop: exec-stop-runtime-not-ready: requires the packaged private runtime, \
                 in-guest-trap, and supported direct-run or singleton verification options",
            ));
        }
        Ok(())
    }

    pub fn main(&mut self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        self.main_with_backend(global, &super::core_adapter::CoreApplicationAdapter, ())
    }

    pub(super) fn main_with_backend<A: super::backend_hook::BackendAdapter>(
        &mut self,
        global: &GlobalOpts,
        adapter: &A,
        adapter_options: A::Options,
    ) -> Result<ExitStatus, Error> {
        if self.application_backend.is_some() {
            self.adapter_capabilities = Some(A::CAPABILITIES);
            adapter.validate_mode(if self.verify_run.is_some() {
                super::backend_hook::Mode::SavedVerification
            } else if self.verify {
                super::backend_hook::Mode::VerificationPair
            } else {
                super::backend_hook::Mode::Ordinary
            })?;
        }
        // An executable-selected adapter has no DAP/namespace-only dispatch.
        // Refuse those switches before the built-in dispatcher can launch a
        // supervisor, lower a shorthand or read any input.
        self.refuse_unqualified_application_options(A::NONDEFAULT_CPU_PROFILE_REFUSAL)?;
        // The newly recognized admission-only request must not reach DAP or
        // another dispatcher before discovering that no execution exists.
        #[cfg(all(feature = "native-exec-input-admission", not(feature = "native-exec")))]
        self.refuse_uncompiled_native_exec(global.backend.unwrap_or_default())?;
        // Before the dispatch below, which can serve a debug adapter: that
        // path binds a listener and reads the image's configuration, and
        // the refusal needs neither.
        self.refuse_exclusive_rewrite_in_an_image()?;
        // Admission must see the actual global backend before any launch.
        match self
            .dispatch_before_validation(global.backend, std::env::var_os(DAP_LISTEN_INNER_ENV))?
        {
            RunDispatch::DapSupervisor => return self.serve_dap(),
            RunDispatch::Run => {}
        }
        // The caller exits without dropping these options, and a run that
        // times out unwinds past this frame, so the property snapshot is
        // removed by this guard, whatever the result.
        let _remove_property_snapshot = super::property_snapshot::RemoveOnExit;
        let result = self.main_without_core_cleanup(global, adapter, adapter_options);
        drop(self.property_snapshot.take());
        if let Ok(status) = &result {
            self.finish_manifest(*status);
        }
        if matches!(result, Ok(ExitStatus::Exited(0))) {
            self.discard_fatal_cores_after_success();
        }
        result
    }

    /// `--dap-listen`: this process supervises two children, the run itself
    /// (this same command, as a plain `--gdbserver` run) and, once the run's
    /// gdbserver is listening, `hermit-dap` on a socket this process bound.
    /// A run that is refused, or whose program is not found, ends before any
    /// adapter starts, with its own message and status. Before the adapter
    /// starts, the host file named to the debugger is checked against the
    /// file the waiting guest process sees (see
    /// [`super::dap_adapter::identify_debuggee`]). If the adapter ends
    /// for any reason while the gdbserver still waits for its one connection,
    /// no debugger can ever attach, so this ends the run instead of leaving it
    /// waiting. Both children die with this process.
    fn serve_dap(&self) -> Result<ExitStatus, Error> {
        let argument = self.dap_listen.as_deref().unwrap_or_default();
        let gdbserver_port = self.det_opts.det_config.gdbserver_port;
        anyhow::ensure!(
            gdbserver_port != 0,
            "--dap-listen needs a fixed --gdbserver-port, so it can tell when the run's \
             gdbserver listens and give that port to the debugger; pass another port than 0"
        );
        let listener = super::dap_adapter::bind(argument)?;
        // Where the guest command finds its program (guest working
        // directory, PATH), searched later in the guest's own view. A program
        // that cannot be found is left to the run, which reports it with its
        // own status (127 if it is not found); hermit-dap is looked for only
        // once the run gets that far.
        let guest = self.guest_program().ok();
        // Held until this function returns, while the run is supervised.
        let (_supervisor_socket, marker) = bind_dap_supervisor_socket()?;
        let run = super::dap_adapter::spawn_owned(
            std::process::Command::new(std::env::current_exe().context("Failed to locate hermit")?)
                .args(std::env::args_os().skip(1))
                .env(DAP_LISTEN_INNER_ENV, marker),
        )
        .context("Failed to start the run")?;
        super::dap_adapter::supervise(
            run,
            listener,
            gdbserver_port,
            |guest_pid| {
                let guest = guest.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "--dap-listen cannot tell which file the guest runs for {}",
                        self.program_path().display()
                    )
                })?;
                super::dap_adapter::identify_debuggee(guest_pid, guest, |path| {
                    // Rebuilt from its components: a file mounted at exactly
                    // `path` maps to its source with a trailing '/'.
                    self.mapped_or_visible_host_program(path)
                        .map(|host| host.components().collect())
                })
            },
            self.dap_gdb.as_deref(),
        )
    }

    pub(crate) fn adopt_verification_inputs(&mut self) -> Result<(), Error> {
        // Before stdin or any backend: a pair whose shared inputs are missing,
        // unsealed or not the manifest's objects is refused before it starts.
        #[cfg(feature = "native-exec")]
        anyhow::ensure!(
            self.native_exec_inputs.is_none()
                || self.verify_run.is_none()
                || self.native_exec_shared_inputs.is_some(),
            "native --verify-run requires the holder's shared-input manifest"
        );
        // Explicit mapped entry may already hold this owner. Omitted routes
        // reach this at the unchanged post-policy-check point in main.
        let retained = self.shared_inputs.is_some();
        #[cfg(feature = "native-exec")]
        let retained = retained || self.native_exec_capture.is_some();
        if retained {
            return Ok(());
        }
        if let Some(fd) = self.native_exec_shared_inputs {
            let adopted = super::verify_inputs::take_adopted(fd)?;
            self.adopt_shared_ld_so_cache(&adopted)?;
            #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
            let adopted = {
                let mut adopted = adopted;
                if self.in_guest_exec_stop {
                    self.mapped_shared_image =
                        Some(std::sync::Arc::new(adopted.take_mapped_package()?));
                } else {
                    anyhow::ensure!(
                        !adopted.has_mapped_package(),
                        "shared mapped package requires the mapped launch mechanism"
                    );
                }
                adopted
            };
            #[cfg(feature = "native-exec")]
            if self.native_exec_inputs.is_some() {
                self.native_exec_capture = Some(std::sync::Arc::new(
                    adopted.into_inputs::<super::native_exec_cli::Capture>()?,
                ));
            } else {
                self.shared_inputs = Some(std::sync::Arc::new(adopted));
            }
            #[cfg(not(feature = "native-exec"))]
            {
                self.shared_inputs = Some(std::sync::Arc::new(adopted));
            }
        }
        Ok(())
    }

    pub(crate) fn prepare_mapped_verification_inputs(&mut self) -> Result<(), Error> {
        #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
        if self.in_guest_exec_stop {
            // Reject unsupported options before consuming the holder's entry.
            // Availability here is only a precheck: each caller still performs
            // the real package-availability admission after adoption.
            self.refuse_unready_exec_stop_with_availability(true)?;
            self.adopt_verification_inputs()?;
        }
        Ok(())
    }

    fn main_without_core_cleanup<A: super::backend_hook::BackendAdapter>(
        &mut self,
        global: &GlobalOpts,
        adapter: &A,
        adapter_options: A::Options,
    ) -> Result<ExitStatus, Error> {
        // Set up an early tracing option before we're ready to set the global default:

        // The backend is a global option (`hermit --backend X run ...`), the only
        // place it can be given.
        self.set_backend(global.backend);
        self.refuse_unqualified_application_options(A::NONDEFAULT_CPU_PROFILE_REFUSAL)?;
        // Static native-option policy belongs before stdin reservation,
        // availability probes, or backend-specific preprocessing/dispatch.
        #[cfg(any(feature = "native-exec", feature = "native-exec-input-admission"))]
        if self.native_exec_inputs.is_some() && self.selected_backend() != Backend::InGuestTrap {
            return Err(Error::new(PolicyRefusal)
                .context("select --backend=in-guest-trap, or drop --native-exec-inputs")
                .context("native exec currently requires in-guest-trap"));
        }
        self.refuse_unapplied_config()?;
        self.resolve_preemption_settings()?;
        if let Some(path) = &self.backend_engagement_json {
            clear_machine_record(path, "backend engagement")?;
        }
        // Before `--verify` reads stdin to its end below: a run that in-guest
        // LiteInst refuses must not wait for its input. `validate_args` repeats
        // the check for its other callers.
        self.refuse_unqualified_liteinst_in_guest_options()?;
        // Beside the refusals above, and before stdin is read for the same
        // reason: a refused run must not consume its input. It is also above the
        // DBT arm below, which returns without reaching `RunOpts::run`.
        self.refuse_unsupervised_log_cap(global.max_log_bytes)?;
        // Also before stdin is read and before any backend is probed: a
        // happens-before spec this launch cannot enforce needs neither, so it
        // is refused at once rather than after input or availability checks.
        self.refuse_unenforceable_happens_before()?;
        self.adopt_verification_inputs()?;
        // Also before stdin is read: a host or run the x86-64-v3 profile
        // cannot qualify is refused at once.
        self.admit_cpu_profile()?;
        if self.verify || self.verify_run.is_some() {
            validate_log_level(global)?;
        }

        // TODO(T124429978): temporarily disabling this because it inexplicably clobbers our
        // subsequent tracing_subscriber::fmt::init() call.
        // tracing::subscriber::with_default(super::tracing::stderr_subscriber(global.log), || {

        // Argument validation and epoch reconciliation also come before stdin
        // is read: neither needs the input, and a run they refuse must not wait
        // for it.
        self.validate_args()?;
        // Right after validation, which may have turned the preemption timer
        // off, and still before stdin is read or anything is launched.
        self.refuse_happens_before_without_preemption()?;
        // Before the epoch read below, so an unreadable, unparsable or
        // invalid replayed record is refused one way, whichever flag names it.
        self.refuse_an_invalid_replayed_record()?;
        self.adopt_replayed_schedule_epoch()?;
        self.adopt_replayed_rng_seed()?;
        // After the replayed epoch is adopted, which makes an omitted epoch
        // explicit, and still before stdin is read or anything is launched.
        self.refuse_strict_with_host_drawn_inputs()?;
        self.refuse_replay_under_another_pipe_wake_latency()?;
        self.refuse_replay_under_another_signal_state_copy()?;
        self.adopt_replayed_post_crash_timeslice()?;
        // Refused here too, before stdin is read; each run's preparation
        // below sets the configuration from the same record.
        self.annotated_priority_replay_admission()?;
        self.refuse_instruction_offset_replay_where_unsupported()?;
        // As `--seed-from` prints the seed it chose: the value pins this
        // run's maximum with `--max-timeslice`.
        if let Some(drawn) = self.seeded_chaos_timeslice() {
            crate::app::tracing::diagnostic_eprintln!(
                "[hermit] --chaos-seeded-timeslice: scheduler seed {} drew --max-timeslice={}",
                self.det_opts.det_config.sched_seed(),
                drawn
            );
        }
        self.refuse_replay_under_another_expired_wait_charge()?;
        self.refuse_replay_under_another_cap_preemption_charge()?;
        self.refuse_replay_under_other_guest_ids()?;
        self.refuse_replay_under_another_sleep_registration()?;
        self.refuse_replay_under_another_exclusive_rewrite()?;
        if !self.namespace_only
            && (self.verify_verbose
                || global
                    .log
                    .is_some_and(|level| level >= tracing::metadata::LevelFilter::DEBUG))
        {
            global.write_controller_diagnostic(format_args!(
                "hermit: {}",
                self.preemption_summary()
            ))?;
        }
        // How the guest's process IDs are numbered is an implicit input too: a
        // base chosen from the free windows is saved and reported with the
        // others, and a refusal here comes before stdin is read.
        self.resolve_guest_ids()?;
        // Once every implicit input it records is resolved, and before the
        // guest starts, so a run that fails or hangs still leaves it.
        self.write_saved_config()?;
        // For the same reason, the run manifest too. It is a convenience
        // beside the warning that already prints the inputs, so a data
        // directory that cannot be written does not stop the run.
        let recording = self.begin_manifest(global).unwrap_or_else(|error| {
            crate::app::tracing::diagnostic_eprintln!(
                "hermit: WARNING: this run's manifest was not written: {error:#}"
            );
            None
        });
        self.warn_of_host_drawn_inputs(global, recording)?;
        // After every refusal above, which holds in every build, so a build
        // without the feature refuses those runs the way a feature build does;
        // and before stdin is read.
        self.refuse_liteinst_without_the_feature()?;
        let verification = self.verify || self.verify_run.is_some();
        let application_stdin = if self.application_backend.is_some() {
            // DBT streams this reservation during live or saved-run execution.
            // The common live owner replays it without pre-draining stdin;
            // library output-capturing backends reserve their input below.
            super::startup_stdin()?
        } else if verification {
            // `--verify` runs the guest twice through the output-capturing
            // backend, which otherwise feeds the guest an empty stdin. Snapshot
            // the real stdin now so both runs replay identical input; without
            // this, piped input (e.g. `echo prog | hermit run --verify -- gcc
            // -x c -`) is dropped and hermit reports a false deterministic pass.
            hermit::reserve_output_stdin_snapshot(super::startup_stdin()?)?;
            None
        } else if self.selected_backend() == Backend::Kvm {
            hermit::reserve_kvm_stdin(super::startup_stdin()?)?;
            None
        } else {
            None
        };

        if self.allow_unsupported_syscalls {
            crate::app::tracing::diagnostic_eprintln!(
                "WARNING: --allow-unsupported-syscalls permits unmodeled syscalls to reach the \
                 host; a successful exit does not establish complete deterministic execution."
            );
        }
        let backend = self.selected_backend();

        if self.namespace_only
            && let Some(explicit_backend) = self.backend
        {
            anyhow::bail!(
                "--backend={} cannot be used with --namespace-only because namespace-only mode \
                 bypasses instrumentation",
                explicit_backend.as_str()
            );
        }
        // ⚠️ HERE, NOT IN `run()`. The DBT single-run arm below returns through
        // its physical launcher, so a check placed in `run()` covers every
        // backend except the one measured furthest from working. Same shape as
        // the `--namespace-only` second launch path documented in `run()`: the
        // first placement worked and was not yet complete. Measured: with the
        // check in `run()`, `--backend dbt --timeout 3` still accepted the flag
        // and ran unbounded.
        //
        // AND BEFORE THE AVAILABILITY PROBE. Whether `--timeout` is qualified on
        // a backend is a static policy fact; whether the backend is compiled in
        // depends on the build's features. Probing availability first made a
        // build without `sabre` answer "backend unavailable" (125) where every
        // other build answers "refused" (122), so the same command got a
        // different verdict per build
        // (https://github.com/rrnewton/hermit/issues/3418).
        self.ensure_timeout_supported()?;
        if !self.namespace_only
            && backend != Backend::Kvm
            && !self.uses_native_exec()
            && self.application_backend.is_none()
        {
            // E9patch's own availability check covers both its ptrace runtime and
            // the `e9patch` cargo feature (it reports "not included in this build"
            // when the feature is disabled), so it no longer needs a special case.
            // A `--no-namespace` run creates no namespace, so it is not refused
            // where only unshare(CLONE_NEWUSER) is denied
            // (https://github.com/rrnewton/hermit/issues/4515).
            let namespaces = if self.no_namespace {
                hermit::NamespaceUse::None
            } else {
                hermit::NamespaceUse::Creates
            };
            if self.in_guest_exec_stop {
                // This launcher needs ptrace, seccomp and the selected namespace
                // mechanisms, but not an adjacent preload DSO. Its exact-bound
                // image is opened and retained before container entry in run().
                self.refuse_unready_exec_stop()?;
                hermit::validate_tracing_environment(namespaces).map_err(|error| {
                    Error::new(hermit::BackendUnavailable::new(backend, error.to_string()))
                })?;
            } else {
                backend.ensure_available_for(namespaces)?;
            }
        }
        // Native exec uses the embedded, producer-qualified catalog rather
        // than the ordinary preload. Its complete Capture::prepare below
        // remains mandatory after program/config validation and before either
        // physical run; an adjacent preload cannot establish its availability.
        self.install_pmu_config()?;
        // After the PMU configuration, which Reverie's validation reads, and
        // before every check below that can still refuse the run: the pending
        // report is rewritten with the verdict, so that a run refused here,
        // by the strict counter check or a later one, still says which
        // counter it would have read.
        if let Some(path) = self.verify_json.as_deref()
            && self.arms_pmu_preemption_timer()
        {
            crate::app::verify::record_exact_branch_counter(branch_counter_verdict());
            crate::app::verify::write_pending_verification_json(path)?;
        }
        self.admit_branch_counter(&mut crate::app::tracing::write_stderr_diagnostic)?;
        // After the PMU configuration, whose skid margin it reads, and before
        // every check below that can still refuse the run, like the warning
        // above, so the cost is stated once whatever happens next.
        if let Some(notice) = self.small_max_timeslice_notice(installed_skid_margin) {
            crate::app::tracing::write_stderr_diagnostic(&notice);
        }
        // The KVM backend reaches real reverie-kvm code from its dispatch path
        // and reports an accurate, program-specific error there, so it is not
        // pre-empted by the generic availability probe above. E9patch is a CLI
        // preprocessor and probes its ptrace runtime and tool separately.
        self.validate_mount_sources()?;
        validate_host_scratch(HostLayout::of_host(), std::env::var_os("TMPDIR").is_some())?;
        self.validate_program()?;
        if self.hb_list_events {
            return self.list_happens_before_events();
        }
        // After the event listing, which runs no guest and writes no summary.
        self.validate_summary_json_visibility()?;
        self.validate_work_span_report_visibility()?;
        self.validate_access_trace_visibility()?;
        if self.happens_before.is_some() {
            // Resolve the spec against the guest binary's debug info now and cache
            // it so every subsequent `effective_det_config()` (including both
            // `--verify` runs) hands the scheduler the identical resolved program.
            let program = self.load_and_resolve_happens_before()?;
            self.resolved_happens_before = Some(program);
        }

        if self.rewrite_exclusives {
            self.prepare_exclusive_rewrite(backend)?;
        }

        self.take_property_snapshot()?;
        let private_engagement_summary = if self.backend_engagement_json.is_some()
            && backend == Backend::Ptrace
            && self.application_backend.is_none()
            && self.summary_json.is_none()
        {
            let file = private_backend_engagement_summary()?;
            // The container is handed the descriptor: see
            // private_summary_descriptor_path.
            self.summary_json = Some(private_summary_descriptor_path(&file));
            Some(file)
        } else {
            None
        };
        // });

        // Last, so that a run refused above creates no directory.
        self.open_fatal_core_dir()?;

        // Ordinary and saved operations retain the existing physical launcher's
        // preparation/writer order. Live pairs join the one common verifier.
        if self.application_backend.is_some()
            && !self.verify
            && let Some(directory) = &self.verify_run
        {
            self.warn_no_namespace();
            return self.verify_run_with_adapter(
                directory,
                global,
                application_stdin,
                adapter,
                adapter_options,
            );
        }
        if self.application_backend.is_some() && !self.verify {
            self.warn_no_namespace();
            self.log_auto_resolution();
            let mode = if self.verify_run.is_some() {
                super::backend_hook::Mode::SavedVerification
            } else {
                super::backend_hook::Mode::Ordinary
            };
            let physical = self
                .application_backend
                .map(|_| super::backend_hook::PreparedPhysical::new(self, A::CAPABILITIES));
            let mut session = adapter.prepare(
                adapter_options,
                super::backend_hook::Prepare {
                    options: self,
                    global,
                    mode,
                    stdin: application_stdin,
                    physical: physical.as_ref(),
                },
            )?;
            return adapter
                .execute(
                    &mut session,
                    super::backend_hook::Execute {
                        mode,
                        verification: None,
                    },
                )?
                .finished(mode);
        }

        if backend == Backend::Liteinst && self.uses_in_guest_liteinst() {
            // Nothing has started the guest yet, so this names the
            // configuration, not an outcome.
            crate::app::tracing::diagnostic_eprintln!(
                "hermit: [liteinst in-guest] selected: the guest preload is to host the Detcore Tool"
            );
        }
        if backend == Backend::InGuestTrap
            && self.uses_in_guest_liteinst()
            && !self.in_guest_exec_stop
        {
            if self.uses_native_exec() {
                crate::app::tracing::diagnostic_eprintln!(
                    "hermit: [in-guest-trap] selected: the embedded native runtime is to host \
                     the Detcore Tool, with syscall site patching off"
                );
            } else {
                crate::app::tracing::diagnostic_eprintln!(
                    "hermit: [in-guest-trap] selected: the guest preload is to host the Detcore Tool, \
                     with syscall site patching off (REVERIE_LITEINST_SITE_PATCHING=0)"
                );
            }
        }

        #[cfg(feature = "native-exec")]
        if let Some(declaration) = &self.native_exec_inputs {
            anyhow::ensure!(
                !self.no_namespace && !self.namespace_only && self.image.is_none(),
                "native exec requires the common declared-input namespace"
            );
            anyhow::ensure!(
                self.timer_downgrade.is_none(),
                "native exec requires an explicit no-timer configuration"
            );
            detcore::native_exec::validate_native_exec_host_configuration(
                &self.effective_det_config(),
            )?;
            let command = self.guest_command()?;
            let capture = if let Some(adopted) = self.native_exec_capture.take() {
                adopted
            } else {
                anyhow::ensure!(
                    self.native_exec_shared_inputs.is_none() && self.verify_run.is_none(),
                    "native shared enrollment cannot fall back to fresh capture"
                );
                super::native_exec_cli::Capture::prepare(declaration, &command)?
            };
            capture.require_initial(&command)?;
            self.native_exec_capture = Some(capture);
        }
        self.warn_no_namespace();

        if self.namespace_only {
            self.run_with_namespace_only(global)
        } else if self.verify {
            self.verify(global, application_stdin, adapter, adapter_options)
        } else if let Some(directory) = &self.verify_run {
            self.verify_run(directory, global)
        } else {
            let (status, _, _) = self.run(global, false)?;
            self.write_backend_engagement_after_run()?;
            drop(private_engagement_summary);
            Ok(status)
        }
    }

    /// Some arguments imply others. This is the place where that validation occurs.
    /// Also this performs side effects like accessing system randomness to implement --seed-from=SystemArgs
    pub fn validate_args(&mut self) -> Result<(), Error> {
        #[cfg(all(feature = "native-exec-input-admission", not(feature = "native-exec")))]
        self.refuse_uncompiled_native_exec(self.selected_backend())?;
        self.refuse_virtual_timeout_with_debugger()?;
        self.refuse_unready_exec_stop()?;
        self.refuse_gdbserver_without_a_gdbserver()?;
        refuse_signal_state_check_env(std::env::var_os(SIGNAL_STATE_CHECK_ENV).as_deref())
            .map_err(Error::msg)?;
        let timer = if self.arms_pmu_preemption_timer() {
            reverie_timer_support()
        } else {
            Ok(())
        };
        self.validate_args_with_timer_support(timer)?;
        self.refuse_unqualified_liteinst_in_guest_options()
    }

    /// Whether this run would arm a PMU preemption timer for
    /// `--max-timeslice` (reverie-ptrace's, or reverie-kvm's on KVM), and so
    /// depends on host `perf_event_open` support.
    ///
    /// `--namespace-only` execs the guest directly without any instrumentation
    /// backend (see `run_with_namespace_only`), so it never arms that timer even
    /// though `selected_backend()` still reports the ptrace default. Probing perf
    /// for it would print a `--max-timeslice` downgrade warning about a timer the
    /// run never uses.
    ///
    /// Runtimes without a timer refuse explicit timer requests before this
    /// host-counter probe. Their inherited scheduling configuration is retained.
    fn arms_pmu_preemption_timer(&self) -> bool {
        !self.namespace_only && self.backend_arms_pmu_preemption_timer()
    }

    /// Whether the selected backend's own runtime arms a PMU preemption timer
    /// (its `delivers_preemption_timer_events` capability), before
    /// `--namespace-only` is considered. `RunOpts::run` launches
    /// that runtime even with `--namespace-only`: only `main` dispatches the
    /// flag to `run_with_namespace_only`.
    fn backend_arms_pmu_preemption_timer(&self) -> bool {
        self.application_backend.map_or_else(
            || hermit::backend_delivers_preemption_timer(self.selected_backend()),
            |_| {
                self.adapter_capabilities
                    .is_some_and(|capabilities| capabilities.delivers_preemption_timer_events)
            },
        )
    }

    /// [`Self::validate_args_with_timer_support`] for tests, with a missing
    /// timer reported as missing perf counters.
    #[cfg(test)]
    pub(crate) fn validate_args_with_perf_support(
        &mut self,
        perf_supported: bool,
    ) -> Result<(), Error> {
        self.validate_args_with_timer_support(if perf_supported {
            Ok(())
        } else {
            Err(TimerUnavailable::NoPerfEvents)
        })
    }

    fn validate_args_with_timer_support(
        &mut self,
        timer: Result<(), TimerUnavailable>,
    ) -> Result<(), Error> {
        // Callers other than `main` (analyze, oci, tests) reach validation
        // without `dispatch_before_validation`; lowering again is a no-op.
        self.lower_strace_only();
        // Before the image is resolved, which this refusal does not need.
        self.refuse_exclusive_rewrite_in_an_image()?;
        self.resolve_image_default_program()?;
        self.refuse_virtual_timeout_with_debugger()?;
        self.refuse_unready_exec_stop()?;
        // Before the timer settings are checked, which read the timeslice.
        self.resolve_chaos_timeslice_range()?;
        self.resolve_preemption_settings()?;
        let backend = self.selected_backend();
        // The margin configures the PMU preemption timer of every backend that
        // delivers its events (reverie-ptrace's, which e9patch preprocessing
        // runs on, and reverie-kvm's: both read Reverie's PMU configuration).
        // LiteInst passes here so that the in-guest refusal, which names why
        // it cannot honour `--skid-margin`, is the one that stops the run.
        if self.skid_margin.is_some()
            && (self.namespace_only
                || !(hermit::backend_delivers_preemption_timer(backend)
                    || matches!(backend, Backend::Liteinst | Backend::InGuestTrap)))
        {
            anyhow::bail!(
                "--skid-margin configures the PMU preemption timer and requires a backend that \
                 delivers its events"
            );
        }
        if self.fatal_core_dir.is_some()
            && (self.namespace_only || !matches!(backend, Backend::Ptrace))
        {
            anyhow::bail!(
                "--fatal-core-dir captures cores at the ptrace backend's exit stop, so it \
                 needs --backend=ptrace or --backend=e9patch, without --namespace-only"
            );
        }
        // `--detlog-anon` lists the guest task's own `/proc/<tid>/maps`. An
        // in-guest tool (DBT, SaBRe, liteinst, the in-guest trap) maps its
        // own memory into that table, KVM's describes the VMM, and
        // `--namespace-only` intercepts nothing.
        if self.det_opts.det_config.detlog_anon.is_some()
            && (self.namespace_only || !matches!(backend, Backend::Ptrace))
        {
            anyhow::bail!(
                "--detlog-anon hashes the anonymous mappings in the guest's own /proc/<tid>/maps, \
                 so it needs --backend=ptrace or --backend=e9patch, without --namespace-only: \
                 backend `{}` shares that table with its tool or keeps the guest elsewhere",
                backend.as_str()
            );
        }
        if self.image.is_some() && self.no_namespace {
            anyhow::bail!(
                "--image chroots the guest into a materialized OCI rootfs, which requires mount \
                 and PID namespaces; it is incompatible with --no-namespace"
            );
        }
        if self.pid_base.is_some() && self.runtime_backend() != Backend::Ptrace {
            anyhow::bail!(
                "--pid-base needs the ptrace backend, which creates the guest's tasks with the \
                 chosen IDs; backend `{}` cannot",
                backend.as_str()
            );
        }
        if self.pid_base.is_some() && (self.no_sequentialize_threads || self.strace_only) {
            anyhow::bail!(
                "--pid-base needs sequentialized threads: without them, the order the guest's \
                 tasks are created in, and so their IDs, is the host's"
            );
        }
        if self.image.is_some() && !matches!(backend, Backend::Ptrace | Backend::Kvm) {
            anyhow::bail!(
                "--image supports the ptrace and kvm backends; backend `{}` has separate \
                 launcher/runtime-file requirements that have not been qualified inside the OCI rootfs",
                backend.as_str()
            );
        }

        if self.backend_engagement_json.is_some() && !matches!(backend, Backend::Ptrace) {
            anyhow::bail!(
                "--backend-engagement-json is not available for backend `{}`; SaBRe publishes \
                 HERMIT_SABRE_PATH_EVIDENCE, and liteinst/KVM expose no engagement value",
                backend.as_str()
            );
        }
        if self.image.is_some() && self.namespace_only {
            anyhow::bail!("--image cannot be combined with --namespace-only");
        }
        if self.image.is_some() && !self.mount.is_empty() {
            anyhow::bail!(
                "--image does not yet compose with custom --mount targets inside the OCI rootfs; \
                 use --bind <HOST-DIR>:<GUEST-DIR> for a work directory"
            );
        }

        // What the command line asked for, before the modes below that force
        // host networking.
        let host_network_requested = self.network == NetworkingMode::Host;
        let config = &mut self.det_opts.det_config;

        // Every backend, DBT included, launches the guest through Reverie's
        // `Container` (see `container::default_container`) unless
        // `--no-namespace` is given. It unshares `CLONE_NEWUTS` and applies the
        // deterministic hostname `hermetic-container.local`; without it,
        // Detcore's `handle_uname` rewrites the nodename instead.
        config.has_uts_namespace = !self.no_namespace;

        if self.no_namespace {
            self.network = NetworkingMode::Host;
            self.tmp = Some(PathBuf::from(TMP_DIR));
        }

        if self.analyze_networking {
            config.warn_non_zero_binds = true;
        }

        config.sequentialize_threads = self.strict || !self.no_sequentialize_threads;
        // A signal delivery the kernel contradicts ends a strict run (A5 step
        // 3.2); elsewhere it is logged as a divergence.
        config.signal_delivery_check_fatal = self.strict;
        // `--strict`'s binder policy (`Config::strict_binder`).
        config.strict_binder = self.strict;
        // `--futex-wake-yields` is accepted only where Detcore's precise futex
        // scheduler runs the guest with sequentialized threads, on the ptrace
        // backend (with or without e9patch preprocessing). Every other launch
        // would accept the flag and ignore it, so it is refused before launch.
        if config.futex_wake_yields {
            if self.namespace_only {
                anyhow::bail!(
                    "--futex-wake-yields needs Detcore's scheduler; --namespace-only (--lite) \
                     runs the guest natively, without it"
                );
            }
            if self.strace_only {
                anyhow::bail!(
                    "--futex-wake-yields reorders the turns of sequentialized threads; \
                     --strace-only turns sequentialization off"
                );
            }
            if !config.sequentialize_threads {
                anyhow::bail!(
                    "--futex-wake-yields reorders the turns of sequentialized threads; it \
                     cannot be combined with --no-sequentialize-threads"
                );
            }
            if config.debug_futex_mode != detcore::BlockingMode::Precise {
                anyhow::bail!(
                    "--futex-wake-yields needs --debug-futex-mode=precise, the only futex \
                     mode in which Detcore knows how many waiters a FUTEX_WAKE woke"
                );
            }

            if !matches!(backend, Backend::Ptrace) {
                anyhow::bail!(
                    "--futex-wake-yields has been tested only on the ptrace backend (and e9patch \
                     preprocessing, which runs on it); backend `{}` is refused until its \
                     handoff is tested",
                    backend.as_str()
                );
            }
        }
        // `--chaos-instruction-offset` asks the backend's precise timer for
        // single steps past a branch target, which only the ptrace backend
        // takes; elsewhere the request would be ignored or refused mid-run.
        // An external adapter (e9patch preprocessing, DBT, SaBRe) is named by
        // the execution backend (`execution_backend_name`), whatever runtime
        // it runs on.
        if config.chaos_instruction_offset > 0 {
            refuse_chaos_instruction_offset(
                config,
                self.application_backend.unwrap_or(backend.as_str()),
                cfg!(target_arch = "aarch64"),
            )?;
        }
        // `--signal-at` is sent by Detcore's scheduler, through the signal
        // path of the ptrace backend (with or without e9patch preprocessing).
        // Every other launch would accept it and never send it, or send it
        // somewhere other than the guest: on KVM the root is virtual and a
        // host kill reaches the sandbox monitor. So they refuse it before
        // launch.
        if let Some(at) = config.signal_at {
            if self.namespace_only {
                anyhow::bail!(
                    "--signal-at={at} needs Detcore's scheduler; --namespace-only (--lite) runs \
                     the guest natively, without it"
                );
            }
            if self.strace_only {
                anyhow::bail!(
                    "--signal-at={at} counts the turns of sequentialized threads; --strace-only \
                     turns sequentialization off"
                );
            }
            if !config.sequentialize_threads {
                anyhow::bail!(
                    "--signal-at={at} counts the turns of sequentialized threads; it cannot be \
                     combined with --no-sequentialize-threads"
                );
            }
            if !matches!(backend, Backend::Ptrace) {
                anyhow::bail!(
                    "--signal-at is supported only on the ptrace backend (and e9patch \
                     preprocessing, which runs on it); backend `{}` is refused until it sends \
                     the signal to the guest",
                    backend.as_str()
                );
            }
        }
        // `--pipe-wake-latency` reorders the turns of sequentialized threads
        // by priority (https://github.com/rrnewton/hermit/issues/4380). As for
        // `--futex-wake-yields`, every launch that would accept it and ignore
        // it refuses it before launch.
        if let Some(latency) = config.pipe_wake_latency {
            if self.namespace_only {
                anyhow::bail!(
                    "--pipe-wake-latency={latency} needs Detcore's scheduler; --namespace-only \
                     (--lite) runs the guest natively, without it"
                );
            }
            if self.strace_only {
                anyhow::bail!(
                    "--pipe-wake-latency={latency} reorders the turns of sequentialized threads; \
                     --strace-only turns sequentialization off"
                );
            }
            if !config.sequentialize_threads {
                anyhow::bail!(
                    "--pipe-wake-latency={latency} reorders the turns of sequentialized threads; \
                     it cannot be combined with --no-sequentialize-threads"
                );
            }
            if !config.sched_heuristic.is_priority_ordered() {
                anyhow::bail!(
                    "--pipe-wake-latency={latency} hands the turn over by priority order; \
                     --sched-heuristic={:?} does not select by priority, so it would be ignored",
                    config.sched_heuristic
                );
            }

            if !matches!(backend, Backend::Ptrace) {
                anyhow::bail!(
                    "--pipe-wake-latency has been tested only on the ptrace backend (and e9patch \
                     preprocessing, which runs on it) and on sabre; backend `{}` is refused \
                     until its hand-off is tested",
                    backend.as_str()
                );
            }
            if config.debug_externalize_sockets {
                anyhow::bail!(
                    "--pipe-wake-latency={latency} hands off between threads polling \
                     scheduler-managed pipes; --debug-externalize-sockets makes them block in the \
                     kernel instead, so it would be ignored"
                );
            }
            // Whatever else makes the option inert is refused too, rather than
            // accepted and ignored (Codex review of
            // https://github.com/rrnewton/hermit/pull/4424).
            if config.pipe_wake_latency_in_effect().is_none() {
                anyhow::bail!(
                    "--pipe-wake-latency={latency} would have no effect with these options"
                );
            }
        }
        // `--work-span-report` reads the scheduler's commits; a launch that
        // has none would write a report of nothing, so it is refused.
        if let Some(report) = &config.work_span_report {
            let report = report.display();
            if self.namespace_only {
                anyhow::bail!(
                    "--work-span-report={report} needs Detcore's scheduler; --namespace-only \
                     (--lite) runs the guest natively, without it"
                );
            }
            if self.strace_only {
                anyhow::bail!(
                    "--work-span-report={report} reads the turns of sequentialized threads; \
                     --strace-only turns sequentialization off"
                );
            }
            if !config.sequentialize_threads {
                anyhow::bail!(
                    "--work-span-report={report} reads the turns of sequentialized threads; it \
                     cannot be combined with --no-sequentialize-threads"
                );
            }
            if self.image.is_some() {
                anyhow::bail!(
                    "--work-span-report={report} is written from inside the run container, and \
                     --image (a prototype) replaces its root, so the report would be lost"
                );
            }
            if !matches!(backend, Backend::Ptrace) {
                anyhow::bail!(
                    "--work-span-report weighs turns by the guest CPU time the ptrace backend's \
                     tool measures, and has been checked on that backend only (and on e9patch \
                     preprocessing, which runs on it); backend `{}` is refused",
                    backend.as_str()
                );
            }
        }
        // `--virtual-timeout` is checked where Detcore's scheduler commits
        // virtual time; a launch without that scheduler could never stop on
        // it, so it is refused rather than accepted and ignored.
        if let Some(bound) = config.virtual_timeout {
            let without = if self.namespace_only {
                Some("--namespace-only (--lite) runs the guest natively, without it")
            } else if self.strace_only {
                Some("--strace-only turns sequentialization off")
            } else if !config.sequentialize_threads {
                Some("it cannot be combined with --no-sequentialize-threads")
            } else if !config.virtualize_time {
                Some(
                    "with --no-virtualize-time the run's summary reports no elapsed virtual time, \
                     which is what the bound measures",
                )
            } else {
                None
            };
            if let Some(without) = without {
                anyhow::bail!(
                    "--virtual-timeout={bound} bounds the virtual time Detcore's scheduler commits \
                     to sequentialized threads; {without}. Drop that option, or bound wall-clock \
                     time with --timeout instead"
                );
            }
            // Fail-closed, as `--timeout` is: a backend is accepted once a
            // run of the bound on it has been measured, not because it shares
            // the scheduler.
            const UNMEASURED: &str = "the bound has been measured only on the ptrace and kvm \
                                      backends, and is refused elsewhere until a run on that \
                                      backend qualifies it";
            let unqualified = if self.application_backend.is_some() {
                Some(UNMEASURED)
            } else {
                match backend {
                    Backend::Ptrace | Backend::Kvm => None,
                    Backend::Liteinst | Backend::InGuestTrap => Some(UNMEASURED),
                }
            };
            if let Some(unqualified) = unqualified {
                anyhow::bail!(
                    "--virtual-timeout={bound} is not supported on the `{}` backend: \
                     {unqualified}. Run it with --backend=ptrace or --backend=kvm, or bound \
                     wall-clock time with --timeout instead",
                    self.execution_backend_name()
                );
            }
            // Only the ptrace runtime unwinds a guest the bound stopped and
            // finishes its run with output and log; KVM's scheduler exits the
            // container child, which leaves a comparison without a verdict.
            let verify_flag = if self.verify {
                Some("--verify")
            } else {
                self.verify_run.as_ref().map(|_| "--verify-run")
            };
            if let Some(flag) = verify_flag
                && !(self.application_backend.is_none() && matches!(backend, Backend::Ptrace))
            {
                anyhow::bail!(
                    "--virtual-timeout={bound} with {flag} is supported only on the ptrace \
                     backend, which finishes a run the bound stopped with its output and log; \
                     backend `{}` ends that run from its scheduler, so the comparison could \
                     reach no verdict. Run it with --backend=ptrace, or drop {flag}",
                    self.execution_backend_name()
                );
            }
        }
        // `--record-access-trace-to` names each record by its position in the
        // run's recorded schedule, which the scheduler of sequentialized
        // threads records; a launch without one would write a trace of
        // nothing, so it is refused.
        if let Some(trace) = &config.access_trace {
            let trace = trace.display();
            if self.namespace_only {
                anyhow::bail!(
                    "--record-access-trace-to={trace} needs Detcore's scheduler; \
                     --namespace-only (--lite) runs the guest natively, without it; drop \
                     --namespace-only, or drop --record-access-trace-to"
                );
            }
            if self.strace_only {
                anyhow::bail!(
                    "--record-access-trace-to={trace} reads the events of sequentialized \
                     threads; --strace-only turns sequentialization off; drop --strace-only, or \
                     drop --record-access-trace-to"
                );
            }
            if !config.sequentialize_threads {
                anyhow::bail!(
                    "--record-access-trace-to={trace} reads the events of sequentialized \
                     threads; drop --no-sequentialize-threads, or drop --record-access-trace-to"
                );
            }
            if !config.record_preemptions && config.record_preemptions_to.is_none() {
                anyhow::bail!(
                    "--record-access-trace-to={trace} names each record by its position in the \
                     run's recorded schedule, and this run records none; add \
                     --record-preemptions-to=PATH"
                );
            }
            if self.image.is_some() {
                anyhow::bail!(
                    "--record-access-trace-to={trace} is written from inside the run container, \
                     and --image (a prototype) replaces its root, so the trace would be lost; drop \
                     --image, or drop --record-access-trace-to"
                );
            }
            if self.application_backend.is_some() || !matches!(backend, Backend::Ptrace) {
                anyhow::bail!(
                    "--record-access-trace-to has been checked on the ptrace backend only; \
                     backend `{}` is refused: use --backend=ptrace",
                    self.execution_backend_name()
                );
            }
        } else if let Some(shaping) = [
            (config.access_trace_from != 0)
                .then(|| format!("--access-trace-from={}", config.access_trace_from)),
            config
                .access_trace_max_records
                .map(|records| format!("--access-trace-max-records={records}")),
            config
                .access_trace_max_bytes
                .map(|bytes| format!("--access-trace-max-bytes={bytes}")),
        ]
        .into_iter()
        .flatten()
        .next()
        {
            anyhow::bail!(
                "{shaping} only shapes an access trace; add --record-access-trace-to=PATH, or \
                 drop it"
            );
        }
        if let Some(refusal) = watch_refusal(
            config,
            hermit::backend_capabilities(backend).hardware_watchpoints,
            backend.as_str(),
        ) {
            anyhow::bail!(refusal);
        }
        if let Some(cpus) = config.virtual_cpus {
            if self.namespace_only {
                anyhow::bail!(
                    "--virtual-cpus is presented by Detcore, and --namespace-only (--lite) runs \
                     the guest natively, without it, so the guest would see the host's CPUs; \
                     drop one of the two"
                );
            }
            if !matches!(backend, Backend::Ptrace) {
                anyhow::bail!(
                    "--virtual-cpus has been checked on the ptrace backend only (and on e9patch \
                     preprocessing, which runs on it); backend `{}` is refused: run with the \
                     ptrace backend (the default) or without --virtual-cpus",
                    backend.as_str()
                );
            }
            check_host_lists_virtual_cpus(cpus.get(), Path::new("/sys/devices/system/cpu"))?;
        }
        config.deterministic_io = self.strict || !self.no_deterministic_io;
        // An unmodeled host syscall makes a successful result unqualified.
        // Ordinary execution therefore fails closed; compatibility passthrough
        // is available only through the explicit, noisy opt-out.
        config.panic_on_unsupported_syscalls = !self.allow_unsupported_syscalls;
        if config.passthru_opt && config.panic_on_unsupported_syscalls {
            anyhow::bail!(
                "--passthru-opt cannot be combined with fail-closed unsupported-syscall handling \
                 (the default; pass --allow-unsupported-syscalls to opt out)"
            );
        }
        config.shutdown_on_unsupported_syscall = config.panic_on_unsupported_syscalls;
        if config.untrap_passthrough_syscalls {
            // The untrapped calls never reach Detcore, so nothing that counts,
            // logs or compares Detcore's events sees them.
            let conflict = if self.strict {
                Some("--strict, which promises that Detcore observes every syscall")
            } else if self.namespace_only || backend != Backend::Ptrace {
                Some(
                    "a backend other than ptrace, whose syscall interception it has not been \
                     checked against",
                )
            } else if let Err(refusal @ detcore::UntrapPassthroughRefusal::PreemptionRecord) =
                detcore::untrap_passthrough_admission(config)
            {
                // Recording one, or replaying one: a record made without the
                // flag holds the untrapped calls' callbacks, their charges and
                // their preemption points, which a replay with it never
                // reaches, so its checkpoints and priority changes would land
                // at different guest progress. The rule and its message are
                // Detcore's, which every launch checks again.
                return Err(Error::new(refusal));
            } else if self.happens_before.is_some() {
                Some("--happens-before, whose syscall anchors count the calls it untraps")
            } else if self.verify_run.is_some() {
                Some("--verify-run, whose record does not say the calls were untrapped")
            } else {
                None
            };
            if let Some(conflict) = conflict {
                anyhow::bail!("--untrap-passthrough-syscalls cannot be combined with {conflict}");
            }
        }

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
        if let Err(error) = config.scheduler_turn_charge() {
            anyhow::bail!("{error}");
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
            crate::app::tracing::diagnostic_eprintln!(
                "[hermit] auto setting --seed {0:?} --sched-seed {0:?}",
                seed
            );
            config.seed = seed;
            self.seed_from_resolved = Some(seed);
        }

        // Once the seed is final, and before the checks below, so that they
        // check the maximum the run will use.
        self.resolve_seeded_chaos_timeslice()?;
        let config = &mut self.det_opts.det_config;
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

        // A range draws PMU-backed slices; without the timer it would do
        // nothing, and the run's saved command would contradict itself. A
        // replayed record's drawn timeslice is as binding: without the timer
        // the replay would lose the slice ends it was recorded with.
        if (config.chaos_timeslice_range.is_some() || config.chaos_timeslice.is_some())
            && let Err(unavailable) = &timer
        {
            let reason = match unavailable {
                TimerUnavailable::NoPerfEvents => {
                    "user-space perf counters are unavailable (perf_event_open)".to_owned()
                }
                TimerUnavailable::NoPmuProfile(reason) => reason.to_string(),
            };
            let request = match config.chaos_timeslice_range {
                Some(range)
                    if config.chaos_timeslice_draw
                        == detcore_model::config::ChaosTimesliceDraw::Run =>
                {
                    format!("--chaos-timeslice-draw=run (--chaos-timeslice-range={range})")
                }
                Some(range) => format!("--chaos-timeslice-range={range}"),
                None => "the replayed record's drawn --max-timeslice".to_owned(),
            };
            return Err(anyhow::Error::new(PolicyRefusal).context(format!(
                "{request} needs PMU-backed preemption, but {reason}. Remove \
                 --chaos-timeslice-range and --chaos-timeslice-draw=run, or run/replay on a \
                 host with a usable PMU."
            )));
        }

        // This is a Detcore Config-internal matter, but relies on reverie_ptrace, which detcore is
        // allowed to depend on:
        if config.max_timeslice.is_some()
            && let Err(unavailable) = &timer
        {
            // TODO(T124429978): this could change back to tracing::warn! when the bug is fixed.
            // With --max-log-bytes this must not wait on a stderr nobody reads: it runs before
            // the container starts, so a blocked write would keep the run from ever reaching
            // the cap.
            match unavailable {
                TimerUnavailable::NoPerfEvents => crate::app::tracing::write_stderr_diagnostic(
                    "WARNING: --max-timeslice requires user-space perf counters, but \
                     perf_event_open is unavailable; continuing with \
                     --max-timeslice=disabled. Check the host perf_event_paranoid value and \
                     container seccomp policy.\n",
                ),
                TimerUnavailable::NoPmuProfile(reason) => {
                    crate::app::tracing::write_stderr_diagnostic(&format!(
                        "WARNING: --max-timeslice requires a performance-counter profile for \
                         this CPU, but {reason}; continuing with --max-timeslice=disabled.\n"
                    ))
                }
            }
            config.max_timeslice = None;
            self.timer_downgrade = Some(unavailable.clone());
        }
        // Without RCB time, which needs the preemption timer, syscall charges
        // are the only progress of virtual time and trapped syscalls the only
        // points where a thread yields, so a loop on the calls this untraps
        // would never yield or let virtual time advance. Checked once a host
        // without usable performance counters has turned the timer off. The
        // rule and its message are Detcore's
        // (`detcore::untrap_passthrough_admission`), which every launch,
        // the library's included, checks again in `facility_admission`.
        detcore::untrap_passthrough_admission(config).map_err(Error::new)?;

        // `--chaos-timeslice-draw=run` draws from the effective scheduler seed,
        // which `--seed-from` has just settled, unless a replayed record gave
        // the timeslice. Drawing again on a later validation gives the same value.
        if !self.chaos_timeslice_adopted
            && config.chaos_timeslice_draw == detcore_model::config::ChaosTimesliceDraw::Run
            && let Some(range) = config.chaos_timeslice_range
        {
            let drawn = detcore::preemptions::draw_chaos_timeslice(range, config.sched_seed());
            config.max_timeslice = Some(drawn);
            config.chaos_timeslice = Some(drawn);
        }

        // Deterministic RCB counts requires thread pinning.  But this only matters if
        // we're expecting full determinstic execution (sequentialize_threads).
        if config.max_timeslice.is_some() && config.sequentialize_threads {
            self.pin_threads = true;
        }

        // `--cap-preemption-charge` lowers the charge of turns that end an
        // expired timeslice under Detcore's scheduler. A launch where no such
        // turn can happen would accept the flag and ignore it, so it is
        // refused before launch. Checked after the timer downgrade above, which
        // can take the timeslices away, and after `lower_strace_only`.
        if config.cap_preemption_charge {
            let unscheduled = if self.namespace_only {
                Some("--namespace-only")
            } else if self.strace_only {
                Some("--strace-only")
            } else if !config.sequentialize_threads {
                Some("--no-sequentialize-threads")
            } else {
                None
            };
            if let Some(option) = unscheduled {
                anyhow::bail!(
                    "--cap-preemption-charge lowers the charge of the scheduler turns that end \
                     timeslices; {option} runs the guest without them. Drop {option} or \
                     --cap-preemption-charge"
                );
            }
            if config.max_timeslice.is_none() && config.target_timeslice.is_none() {
                let cause = match &self.timer_downgrade {
                    Some(_) => "this host's PMU timer is unavailable (see the warning above)",
                    None => "--max-timeslice=disabled",
                };
                anyhow::bail!(
                    "--cap-preemption-charge has no effect: no timeslice ends by expiry because \
                     {cause}. Give a --max-timeslice (or --target-timeslice), or drop \
                     --cap-preemption-charge"
                );
            }
        }

        // Happens-before enforcement parks the "after" thread until its "before"
        // anchor fires. That deterministic parking is only meaningful when the
        // scheduler owns thread selection; with threads running in parallel there
        // is no single serial order to constrain. Fail closed rather than silently
        // ignore the spec.
        if self.happens_before.is_some() && !config.sequentialize_threads {
            anyhow::bail!(
                "--happens-before requires deterministic sequential thread execution; remove \
                 --no-sequentialize-threads (and --strace-only) so the scheduler can enforce \
                 ordering edges"
            );
        }

        // `--strict` calls itself fail-closed strict deterministic mode, so it
        // must refuse the settings hermit itself documents as
        // determinism-compromising rather than accept them silently. Before
        // this, `--strict --network=host` exited 0 with no warning, even though
        // `--network`'s own help says `host` "compromises isolation and
        // deterministic reproducibility".
        //
        // Checked HERE, after every override above, because one of the two
        // routes to host networking is a side effect of a flag that is not
        // about networking at all: `--no-namespace` sets it above, silently.
        // Measured from inside the guest, `--strict` alone sees interfaces
        // `lo`, while `--strict --network=host`, `--strict --no-namespace` and
        // `--strict --strace-only` all see `eth0 lo`. `--gdbserver` is not a
        // route: its port is bound in the host's namespace before the
        // container exists (`hermit::gdbserver`), so the guest keeps `lo`.
        //
        // This forbids rather than warns because the directive is that a hole
        // must be opened deliberately: drop `--strict`, or ask for the hole
        // explicitly without also claiming strictness.
        if self.strict && self.network == NetworkingMode::Host {
            let cause = if self.no_namespace {
                "--no-namespace, which forces host networking"
            } else {
                "--network=host"
            };
            anyhow::bail!(
                "--strict is fail-closed deterministic mode and cannot be combined with {}: \
                 host networking exposes the guest to external traffic that hermit does not \
                 determinize. Re-run without --strict to allow it deliberately.",
                cause
            );
        }

        // Set after the strict check: a recording shares the host network
        // namespace, so the strict refusal of host networking does not apply.
        // An IPv4 or IPv6 operation either becomes an outbound TCP channel,
        // whose inputs are captured and released deterministically, or ends
        // the run with a refusal. So do the other routes into that namespace
        // that detcore's `syscalls::network_trace` knows: other socket
        // families, abstract AF_UNIX addresses, interface ioctls, and opens
        // of /proc/net, /proc/sys/net and /sys network interfaces. Its module
        // documentation lists the routes that remain open. Replay applies the
        // same refusals in its private namespace.
        self.det_opts.det_config.network_trace = self.network_trace_config()?;
        // Detcore's notice for an abstract AF_UNIX bind, on only where its
        // advice holds and it reaches Hermit's stderr rather than the guest's:
        // --network=host, given or as part of --strace-only's documented
        // expansion (lowered before this, so both spellings resolve alike),
        // whose run really shares the host's namespace (--analyze-networking
        // isolates it), in a PID namespace (every guest's first process is
        // pid 3), not taken over by --gdbserver, which needs host networking
        // (a recording, which refuses abstract addresses outright, cannot be
        // combined with --network=host), and with Detcore in Hermit's own
        // process (the ptrace backend, e9patch preprocessing included).
        self.det_opts.det_config.notice_host_network_abstract_binds = host_network_requested
            && !self.network.uses_local_namespace(self.analyze_networking)
            && !self.no_namespace
            && !self.det_opts.det_config.gdbserver
            && self.application_backend.is_none()
            && matches!(backend, Backend::Ptrace);
        if self.record_networking.is_some() {
            self.network = NetworkingMode::Host;
        }

        // Advise when running a VMM (e.g. QEMU) under host-time virtualization,
        // whose emulated guest clock calibration this corrupts (issue #6).
        // Checked last so it reflects any overrides above that disable virtual
        // time (e.g. --strace-only).
        let virtualize_time = self.det_opts.det_config.virtualize_time;
        if let Some(warning) =
            vmm_time_virtualization_warning(self.program_path(), &self.args, virtualize_time)
        {
            // TODO(T124429978): this could change back to tracing::warn! when the bug is fixed:
            crate::app::tracing::diagnostic_eprintln!("{warning}");
        }

        Ok(())
    }

    /// The run options `hermit analyze` and `hermit bisect` cannot pass to
    /// their trials, each with the reason, as given. Every trial would either
    /// leave the deterministic, isolated setup the trials are compared in,
    /// write the same output path as every other trial, or replay one fixed
    /// recording where each run needs its own. Options a trial sets for
    /// itself afterwards (analyze's own preemption record, schedules and
    /// stack-trace events) are not seen here.
    pub(crate) fn options_trials_refuse(&self) -> Vec<(&'static str, &'static str)> {
        let config = &self.det_opts.det_config;
        const SHARED: &str = "every trial writes that same path, so only the last trial's output \
                              would remain";
        [
            (
                self.config.is_some(),
                "--config",
                "the trials parse their run arguments directly and would ignore the file; give \
                 its options as run arguments",
            ),
            (
                self.strace_only,
                "--strace-only",
                "it turns off thread sequentialization and the rest of determinization, and the \
                 trials' schedule search needs both",
            ),
            (
                self.no_sequentialize_threads,
                "--no-sequentialize-threads",
                "the trials' schedule search needs sequentialized threads",
            ),
            (
                self.network == NetworkingMode::Host,
                "--network=host",
                "it puts every trial on the host network, so the trials are no longer reproducible",
            ),
            (
                // Trials replay one another's records, which name every thread
                // by its ID: under --no-namespace only IDs Hermit chooses stay
                // the same from trial to trial (`prepare_trial_guest_ids`).
                self.no_namespace && super::guest_ids::may_choose_ids().is_err(),
                "--no-namespace",
                "Hermit cannot choose the guest's process IDs here, so every trial's guest would \
                 get the host's next free IDs, and schedule and preemption replay, which the \
                 trials are, need IDs that do not change from run to run",
            ),
            (
                config.gdbserver,
                "--gdbserver",
                "it makes every trial wait for a debugger on the same port",
            ),
            (
                self.dap_listen.is_some(),
                "--dap-listen",
                "it makes every trial wait for a debugger, and only the first trial could take \
                 the adapter's port",
            ),
            (
                config.record_preemptions_to.is_some(),
                "--record-preemptions-to",
                SHARED,
            ),
            (
                config.replay_schedule_from.is_some(),
                "--replay-schedule-from",
                "analyze's target and baseline runs would replay that one schedule, and every \
                 trial replays its own; pass a recording to analyze as --run1-preemptions or \
                 --run2-preemptions, and to bisect as --good or --bad",
            ),
            (
                config.replay_preemptions_from.is_some(),
                "--replay-preemptions-from",
                "analyze's target and baseline runs would replay that one record, and a trial, \
                 which replays a schedule of its own, cannot replay both; pass a recording to \
                 analyze as --run1-preemptions or --run2-preemptions, and to bisect as --good or \
                 --bad",
            ),
            (
                config
                    .stacktrace_event
                    .iter()
                    .any(|(_, path)| path.is_some()),
                "--stacktrace-event with a path",
                SHARED,
            ),
            (
                config.preemption_stacktrace_log_file.is_some(),
                "--preemption-stacktrace-log-file",
                SHARED,
            ),
            (
                config.access_trace.is_some(),
                "--record-access-trace-to",
                SHARED,
            ),
        ]
        .into_iter()
        .filter_map(|(given, option, reason)| given.then_some((option, reason)))
        .collect()
    }

    /// The trace `--record-networking` names, as given. `hermit analyze` and
    /// `hermit bisect` refuse it in their run arguments
    /// (`AnalyzeOpts::refuse_outputs_trials_overwrite`).
    pub(crate) fn record_networking_trace(&self) -> Option<&Path> {
        self.record_networking.as_deref()
    }

    /// The trace `--replay-networking` names, as given. `hermit analyze` and
    /// `hermit bisect` reconcile its epoch before their trials start
    /// (`AnalyzeOpts::adopt_recorded_epoch`).
    pub(crate) fn replay_networking_trace(&self) -> Option<&Path> {
        self.replay_networking.as_deref()
    }

    /// The network trace mode the record and replay flags select, `Off` with
    /// neither. A replay trace is read and checked here so a bad path fails
    /// before the guest starts.
    fn network_trace_config(&self) -> Result<NetworkTraceConfig, Error> {
        let (mode, path) = match (&self.record_networking, &self.replay_networking) {
            (None, None) => return Ok(NetworkTraceConfig::default()),
            (Some(path), _) => (NetworkTraceMode::Record, path),
            (None, Some(path)) => (NetworkTraceMode::Replay, path),
        };
        // The trace is written by the tracer when the run ends; resolve it
        // against the directory the user named it from.
        let path = std::path::absolute(path)
            .with_context(|| format!("cannot resolve network trace path {}", path.display()))?;
        if mode == NetworkTraceMode::Record {
            let parent = path.parent().unwrap_or(Path::new("/"));
            if !parent.is_dir() {
                anyhow::bail!(
                    "--record-networking: directory {} does not exist; create it or choose a \
                     trace path in an existing directory",
                    parent.display()
                );
            }
        } else {
            let file = File::open(&path).with_context(|| {
                format!(
                    "--replay-networking: cannot open network trace {}; pass a trace written by \
                     --record-networking",
                    path.display()
                )
            })?;
            NetworkTraceV2::read_framed(std::io::BufReader::new(file)).with_context(|| {
                format!(
                    "--replay-networking: {} is not a valid network trace; record a new one with \
                     --record-networking",
                    path.display()
                )
            })?;
        }
        Ok(NetworkTraceConfig {
            mode,
            path: Some(path),
            network_perturb_seed: None,
        })
    }

    /// Open the network trace before the container starts and point the
    /// config at the inherited descriptor. Detcore reads and writes the trace
    /// inside the container, where the private `/tmp` and any chroot hide the
    /// host path. A record truncates now, so a run that fails before writing
    /// leaves an empty file that `--replay-networking` refuses.
    fn open_network_trace(&mut self) -> Result<Option<File>, Error> {
        let config = &mut self.det_opts.det_config.network_trace;
        let file = match (config.mode, &config.path) {
            (NetworkTraceMode::Off, _) | (_, None) => return Ok(None),
            (NetworkTraceMode::Record, Some(path)) => OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(path)
                .with_context(|| {
                    format!(
                        "--record-networking: cannot create network trace {}; choose a writable \
                         path",
                        path.display()
                    )
                })?,
            (NetworkTraceMode::Replay, Some(path)) => File::open(path).with_context(|| {
                format!(
                    "--replay-networking: cannot open network trace {}; pass a trace written by \
                     --record-networking",
                    path.display()
                )
            })?,
        };
        config.path = Some(PathBuf::from(format!(
            "/proc/self/fd/{}",
            std::os::fd::AsRawFd::as_raw_fd(&file)
        )));
        Ok(Some(file))
    }

    /// Records `--skid-margin` for Reverie's PMU configuration. Recording reads
    /// nothing from the host's CPU, so it holds wherever the configuration is
    /// built later, and a CPU with no profile is handled where a timer is armed.
    pub(crate) fn install_pmu_config(&self) -> Result<(), Error> {
        let Some(skid_margin) = self.skid_margin else {
            return Ok(());
        };
        #[cfg(test)]
        crate::app::test_isolation::assert_alone_before_setting_the_skid_margin();
        reverie_ptrace::set_skid_margin_override(skid_margin).map_err(|_| {
            anyhow::anyhow!(
                "Reverie PMU configuration was initialized before --skid-margin could be applied"
            )
        })
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
        validate_mount_targets(&self.mount, Path::exists, Path::is_dir)
    }

    fn validate_executable_overlay_mount_targets(&self) -> Result<(), Error> {
        for bind in &self.bind {
            let target = Path::new(OsStr::from_bytes(bind.target.to_bytes()));
            validate_e9patch_mount_target(target)?;
        }
        for mount in &self.mount {
            validate_e9patch_mount_target(mount.get_target())?;
        }
        Ok(())
    }

    fn resolve_executable_overlay_target(
        &self,
        guest: &Path,
        host: &Path,
    ) -> Result<PathBuf, Error> {
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
                        || (self.has_private_dev_shm()
                            && path_resolution_visits_prefix(&host, Path::new(GUEST_DEV_SHM))?)
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
        let private_dev_shm = self.has_private_dev_shm();
        for bind in self.bind.iter().rev() {
            if !bind_is_applied(bind, private_dev_shm) {
                continue;
            }
            let (source, target) = bind_paths(bind);
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
        if private_dev_shm && is_in_dev_shm(program) {
            return GuestPathMapping::Hidden;
        }
        GuestPathMapping::Unchanged
    }

    /// The environment and working directory the guest starts with, as its
    /// command gives them: every variable it inherits or is given (the
    /// `--base-env` one, an `--image`'s, then `--env`, a bare `--env NAME`
    /// read from Hermit's own environment), and the directory (`--workdir`,
    /// an `--image`'s, else Hermit's own).
    pub(super) fn guest_inputs(
        &self,
    ) -> Result<
        (
            std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString>,
            PathBuf,
        ),
        Error,
    > {
        let command = self.guest_command()?;
        let directory = self.guest_current_dir(&command)?;
        Ok((command.get_captured_envs(), directory))
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

    /// Load the `--happens-before` spec, resolve its anchors against the guest
    /// program's debug info, print the resolved anchor/edge program, and return
    /// success without running the guest. Scheduler enforcement of these edges is
    /// a separate, forthcoming change; this path is pure introspection.
    /// Load the `--happens-before` spec and resolve its anchor code locations
    /// against the guest binary's debug info, returning the resolved program the
    /// scheduler will enforce. Unresolved code locations are reported but do not
    /// fail the run: a count-based anchor (after N syscalls / M RCBs) needs no
    /// debug info, and a purely deferred RIP anchor that never resolves simply
    /// never fires. This shares the same load/resolve path as
    /// `list_happens_before_events` so the preview and the enforced program agree.
    fn load_and_resolve_happens_before(&self) -> Result<HappensBeforeProgram, Error> {
        let spec_path = self
            .happens_before
            .as_ref()
            .expect("load_and_resolve_happens_before requires --happens-before");
        let mut program = Self::load_happens_before_spec(spec_path)?;

        let (_, host) = self.resolve_guest_and_host_program()?;
        match DebugInfoResolver::open(&host) {
            Ok(resolver) if !resolver.is_empty() => {
                let unresolved = resolve_program(&mut program, &resolver);
                if !unresolved.is_empty() {
                    crate::app::tracing::diagnostic_eprintln!(
                        "hermit: {} happens-before anchor(s) with unresolved code locations \
                         (they will never fire): {}",
                        unresolved.len(),
                        unresolved.join(", ")
                    );
                }
            }
            Ok(_) => {
                let unresolved: Vec<String> = program
                    .unresolved_locations()
                    .map(|a| a.name.clone())
                    .collect();
                if !unresolved.is_empty() {
                    crate::app::tracing::diagnostic_eprintln!(
                        "hermit: {} has no usable symbol/debug info; {} code-location anchor(s) \
                         will never fire: {}",
                        host.display(),
                        unresolved.len(),
                        unresolved.join(", ")
                    );
                }
            }
            Err(err) => {
                crate::app::tracing::diagnostic_eprintln!(
                    "hermit: could not read debug info from {}: {:#}; code-location anchors will \
                     never fire",
                    host.display(),
                    err
                );
            }
        }
        Ok(program)
    }

    /// Load a `--happens-before` spec for a run or for `--hb-list-events`.
    ///
    /// A spec that is missing (including a path through a non-directory),
    /// unreadable for permission, a directory, not UTF-8, not valid JSON or not
    /// a valid program (`nth: 0`, `fd` on the wrong syscall, ...) is the user's
    /// to fix: a policy refusal, exit 122
    /// (https://github.com/rrnewton/hermit/issues/3943). Any other failure to read
    /// it (`EIO`, ...) is not the user's, and stays a Hermit failure, exit 125.
    fn load_happens_before_spec(path: &Path) -> Result<HappensBeforeProgram, Error> {
        load_program(path).map_err(|error| {
            let host_read_failure = error.downcast_ref::<std::io::Error>().is_some_and(|io| {
                !matches!(
                    io.kind(),
                    std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::PermissionDenied
                        | std::io::ErrorKind::IsADirectory
                        | std::io::ErrorKind::NotADirectory
                        | std::io::ErrorKind::InvalidData
                )
            });
            if host_read_failure {
                error
            } else {
                error.context(super::container::PolicyRefusal)
            }
        })
    }

    /// Refuse a `--happens-before` spec that does not load
    /// ([`Self::load_happens_before_spec`]) or that this launch cannot enforce
    /// (`refuse_unenforceable_anchors`): a phase other than the prehook, an
    /// unobservable syscall, or a count or syscall-occurrence anchor on a launch
    /// whose runtime is not ptrace (`--backend=e9patch` runs under ptrace and is
    /// enforced). Needs only the spec, not the guest program, so it runs before
    /// stdin is read or a backend is probed.
    fn refuse_unenforceable_happens_before(&self) -> Result<(), Error> {
        let Some(path) = self.happens_before.as_deref() else {
            return Ok(());
        };
        if self.hb_list_events {
            return Ok(());
        }
        let program = Self::load_happens_before_spec(path)?;
        let backend_name = format!("{:?}", self.selected_backend());
        refuse_unenforceable_anchors(
            &program,
            path,
            HbLaunch {
                backend: &backend_name,
                // e9patch rewrites the program and then runs it under the
                // ptrace runtime, which receives and enforces the program.
                ptrace: self.application_backend.map_or_else(
                    || self.runtime_backend() == Backend::Ptrace,
                    |_| {
                        self.adapter_capabilities.is_some_and(|capabilities| {
                            capabilities.delivers_preemption_timer_events
                        })
                    },
                ),
                namespace_only: self.namespace_only,
                passthru_opt: self.det_opts.det_config.passthru_opt,
                // The requested timer, before validation; an implicit loss is
                // refused later (`refuse_happens_before_without_preemption`).
                preemption: self.det_opts.det_config.max_timeslice.is_some(),
                replay_schedule: self.det_opts.det_config.replay_schedule_from.is_some(),
            },
        )
        // A spec the launch cannot enforce is a policy refusal (exit 122), not
        // a Hermit failure.
        .map_err(|error| error.context(super::container::PolicyRefusal))
    }

    /// Refuse a version-2 `--happens-before` spec when argument validation
    /// left the run without a preemption timer
    /// (`hermit::happens_before::refuse_hold_without_preemption`). The early
    /// check (`refuse_unenforceable_happens_before`) sees only the requested
    /// timer; this one sees the effective timer, which validation turns off
    /// when the host cannot arm it (`TimerUnavailable`), and names that
    /// reason. Nothing assigns `max_timeslice` after validation on the run
    /// path.
    fn refuse_happens_before_without_preemption(&self) -> Result<(), Error> {
        let Some(path) = self.happens_before.as_deref() else {
            return Ok(());
        };
        if self.hb_list_events || self.det_opts.det_config.max_timeslice.is_some() {
            return Ok(());
        }
        let program = Self::load_happens_before_spec(path)?;
        let reason = match &self.timer_downgrade {
            Some(TimerUnavailable::NoPerfEvents) => {
                "this host cannot arm it: perf_event_open is unavailable".to_owned()
            }
            Some(TimerUnavailable::NoPmuProfile(reason)) => {
                format!("this host cannot arm it: {reason}")
            }
            None => "--max-timeslice disabled turns the preemption timer off".to_owned(),
        };
        hermit::happens_before::refuse_hold_without_preemption(&program, path, &reason)
            .map_err(|error| error.context(super::container::PolicyRefusal))
    }

    fn list_happens_before_events(&self) -> Result<ExitStatus, Error> {
        let spec_path = self
            .happens_before
            .as_ref()
            .expect("--hb-list-events requires --happens-before");
        let mut program = Self::load_happens_before_spec(spec_path)?;

        let (_, host) = self.resolve_guest_and_host_program()?;
        let resolver = match DebugInfoResolver::open(&host) {
            Ok(r) if !r.is_empty() => Some(r),
            Ok(_) => {
                crate::app::tracing::diagnostic_eprintln!(
                    "hermit: {} has no usable symbol/debug info; code-location anchors will not \
                     resolve",
                    host.display()
                );
                None
            }
            Err(err) => {
                crate::app::tracing::diagnostic_eprintln!(
                    "hermit: could not read debug info from {}: {:#}",
                    host.display(),
                    err
                );
                None
            }
        };

        let unresolved = match &resolver {
            Some(r) => resolve_program(&mut program, r),
            None => program
                .unresolved_locations()
                .map(|a| a.name.clone())
                .collect(),
        };

        println!(
            "happens-before program: {} anchor(s), {} edge(s)",
            program.anchors.len(),
            program.edges.len()
        );
        println!("anchors:");
        for (name, anchor) in &program.anchors {
            println!(
                "  {} = {}",
                name,
                describe_anchor(anchor, resolver.as_ref())
            );
        }
        println!("edges:");
        for edge in &program.edges {
            let op = match edge.strength {
                Strength::Hard => "<",
                Strength::Soft => "<~",
            };
            println!("  {} {} {}", edge.before, op, edge.after);
        }
        if !unresolved.is_empty() {
            crate::app::tracing::diagnostic_eprintln!(
                "hermit: {} anchor(s) with unresolved code locations: {}",
                unresolved.len(),
                unresolved.join(", ")
            );
        }
        Ok(ExitStatus::SUCCESS)
    }

    /// Where the guest command finds its program, as guest paths: the path
    /// it names, made absolute against its working directory, or, for a bare
    /// name, each `PATH` candidate in order, as
    /// [`Self::resolve_guest_and_host_program`] tries them.
    fn guest_program(&self) -> Result<super::dap_adapter::GuestProgram, Error> {
        use super::dap_adapter::GuestProgram;
        let command = self.guest_command()?;
        let requested = Path::new(command.get_program());
        if requested.is_absolute() {
            return Ok(GuestProgram::Named(normalize_guest_path(requested)?));
        }
        let current_dir = self.guest_current_dir(&command)?;
        if requested.components().count() > 1 {
            return Ok(GuestProgram::Named(normalize_guest_path(
                &current_dir.join(requested),
            )?));
        }
        let path = command
            .get_captured_envs()
            .get(OsStr::new("PATH"))
            .cloned()
            .unwrap_or_default();
        let candidates = path
            .as_bytes()
            .split(|byte| *byte == b':')
            .map(|bytes| Path::new(OsStr::from_bytes(bytes)))
            .map(|directory| {
                normalize_guest_path(&if directory.is_absolute() {
                    directory.join(requested)
                } else {
                    current_dir.join(directory).join(requested)
                })
            })
            .collect::<Result<_, _>>()?;
        Ok(GuestProgram::Search(candidates))
    }

    /// Refuse a program at `requested` under host `/dev/shm`, which the
    /// guest's private `/dev/shm` hides.
    fn refuse_program_under_dev_shm(&self, requested: &Path) -> Result<(), Error> {
        if requested.exists() && is_in_dev_shm(requested) && self.has_private_dev_shm() {
            anyhow::bail!(
                "Program {} is under host /dev/shm, but Hermit gives the guest a private \
                 /dev/shm. Pass --bind /dev/shm to keep the host's, or move the program.",
                requested.display()
            );
        }
        Ok(())
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
            self.refuse_program_under_dev_shm(&requested)?;
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

    /// Refuse an absolute `--summary-json` that the run container cannot see.
    ///
    /// Detcore writes the summary from inside the container, at the end of the
    /// run, and the container mounts a private directory over `/tmp`. A host
    /// path under `/tmp` therefore names nothing there: the write failed with
    /// NotFound and panicked at teardown, after the whole guest had run, or --
    /// when its parent happened to exist in the private `/tmp`, as for
    /// `/tmp/summary.json` -- succeeded into a directory that is deleted with
    /// the container, losing the summary without a word. Hermit's own private
    /// summaries no longer take this route (private_summary_descriptor_path);
    /// this is the same fault for a path the caller chose, so it is refused
    /// before anything runs, like a program under host `/tmp`. See
    /// https://github.com/rrnewton/hermit/issues/3260.
    ///
    /// "Cannot see" is decided by the same guest-path mapping the program check
    /// uses, so `--tmp=/tmp`, a `--bind` into `/tmp` and a sourced `--mount`
    /// keep working, and a source-less `--mount` (a tmpfs, equally discarded)
    /// is refused for the same reason. `--tmp=<dir>` other than `/tmp` is
    /// refused for a `/tmp` path too: the guest's `/tmp` is then `<dir>`, so
    /// the write would go to `<dir>/...` -- not the requested host path, and a
    /// NotFound panic when that subdirectory is absent. A relative path is not
    /// refused: the container inherits the caller's working directory itself,
    /// not its name, so a relative summary reaches the host directory even
    /// when that directory is under `/tmp`. Modes that never write the summary
    /// are not refused: DBT, `--namespace-only` and `--hb-list-events` (checked
    /// earlier). `--no-namespace` has no private mounts, and `--image` (a
    /// prototype) replaces the whole root, which this mapping does not model.
    fn validate_summary_json_visibility(&self) -> Result<(), Error> {
        if self.namespace_only || self.image.is_some() {
            return Ok(());
        }
        self.validate_output_visibility("--summary-json", "summary", self.summary_json.as_deref())
    }

    /// The `--work-span-report` counterpart of
    /// `validate_summary_json_visibility`. Every mode that would not write
    /// the report (`--namespace-only`, `--image`, backends other than ptrace)
    /// is refused earlier, so none is exempt here.
    ///
    /// The path is checked as the container will resolve it: with private
    /// mounts, a `..` component is refused (the container's mounts can
    /// resolve it into a hidden directory, as `/var/../tmp` resolves into the
    /// private `/tmp`), and an absolute path is checked again through its
    /// parent directory's host resolution, so a symlinked parent directory
    /// into a hidden one is refused too.
    fn validate_work_span_report_visibility(&self) -> Result<(), Error> {
        self.validate_scheduler_output(
            "--work-span-report",
            "report",
            self.det_opts.det_config.work_span_report.as_deref(),
        )
    }

    /// The `--record-access-trace-to` counterpart of
    /// `validate_work_span_report_visibility`: the scheduler writes the trace
    /// from inside the container too, and every mode that would not write it
    /// is refused earlier.
    fn validate_access_trace_visibility(&self) -> Result<(), Error> {
        self.validate_scheduler_output(
            "--record-access-trace-to",
            "trace",
            self.det_opts.det_config.access_trace.as_deref(),
        )
    }

    /// Refuse an output the scheduler writes from inside the run container
    /// (`flag`'s `what`) at a path the container would resolve elsewhere: a
    /// `..` component, a hidden directory, or a symlinked parent into one.
    fn validate_scheduler_output(
        &self,
        flag: &str,
        what: &str,
        path: Option<&Path>,
    ) -> Result<(), Error> {
        let Some(path) = path else {
            return Ok(());
        };
        if !self.no_namespace
            && path
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            anyhow::bail!(
                "{flag} {} has a `..` component, which the run container's mounts can \
                 resolve into a directory it hides, losing the {what}. Name the directory directly.",
                path.display()
            );
        }
        self.validate_output_visibility(flag, what, Some(path))?;
        if path.is_absolute()
            && let (Some(parent), Some(name)) = (path.parent(), path.file_name())
            && let Ok(resolved) = std::fs::canonicalize(parent)
            && resolved != parent
        {
            self.validate_output_visibility(flag, what, Some(&resolved.join(name)))?;
        }
        Ok(())
    }

    /// Refuse an output file the run writes from inside its container
    /// (`flag`'s `what`) at a path the container hides.
    fn validate_output_visibility(
        &self,
        flag: &str,
        what: &str,
        path: Option<&Path>,
    ) -> Result<(), Error> {
        let Some(path) = path else {
            return Ok(());
        };
        if self.no_namespace || !path.is_absolute() {
            return Ok(());
        }
        let hidden = match self.mapped_host_program(path) {
            GuestPathMapping::Hidden => true,
            // Mapped through `--tmp=<dir>` rather than a bind or mount.
            GuestPathMapping::Mapped(host) => self.tmp.as_deref().is_some_and(|tmp| {
                tmp != Path::new(TMP_DIR)
                    && path
                        .strip_prefix(TMP_DIR)
                        .is_ok_and(|suffix| host == tmp.join(suffix))
            }),
            GuestPathMapping::Unchanged => false,
        };
        if hidden {
            let (cause, remedy) = if path.starts_with(TMP_DIR) {
                (
                    "it is under host /tmp, but Hermit replaces guest /tmp with an isolated \
                     directory (or the --tmp directory)",
                    "Write it outside /tmp, or pass --tmp=/tmp to expose host /tmp.",
                )
            } else if path.starts_with(GUEST_DEV_SHM) {
                (
                    "it is under host /dev/shm, but Hermit gives the guest a private /dev/shm",
                    "Write it outside /dev/shm, or pass --bind /dev/shm to keep the host's.",
                )
            } else {
                (
                    "a --mount without a source hides it",
                    "Write it outside that mount.",
                )
            };
            anyhow::bail!(
                "{flag} {} is not visible inside the run container: {cause}. The {what} \
                 is written from inside the container, so it would be lost. {remedy}",
                path.display()
            );
        }
        Ok(())
    }

    fn validate_program(&self) -> Result<(), Error> {
        let (host, requested, rootfs) = self.guest_program_on_host()?;
        validate_executable(&host, &requested, rootfs.as_deref())
    }

    /// Where the guest program is on the host, as [`Self::validate_program`]
    /// checks it: its host path, the path as the command names it, and the
    /// image rootfs it resolves in, if any.
    fn guest_program_on_host(&self) -> Result<(PathBuf, PathBuf, Option<PathBuf>), Error> {
        // PROTOTYPE (--image): the guest program is interpreted inside the
        // materialized OCI rootfs, not on the host filesystem. Resolve and
        // validate it against the rootfs so that image-only binaries (e.g.
        // busybox's `/bin/busybox`, absent from the host) are accepted, and so
        // host binaries that merely happen to share a path are never used.
        // materialize_rootfs is idempotent/cached, so this does not re-pull the
        // image later in `container()`.
        if let Some(image) = &self.image {
            // Materialize before building the command: on a cold cache this is
            // what makes the image's Env/WorkingDir available for relative-path
            // resolution during first-run validation.
            let rootfs = crate::app::image::materialize_rootfs(image)?;
            let command = self.guest_command()?;
            let requested = Path::new(command.get_program());
            // `guest_command()` sets the image working directory (WorkingDir, or
            // `/`) as the guest cwd; resolve a relative program path against it.
            let guest_cwd = command
                .get_current_dir()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("/"));
            // A bare name (the common shape of an image Cmd, e.g. `python3`)
            // is searched along the image's own PATH inside the rootfs. The
            // guest's execve repeats the same search at run time against the
            // image Env, so validation only has to prove one entry exists.
            let searched;
            let requested =
                if !requested.is_absolute() && !requested.to_string_lossy().contains('/') {
                    let cfg = crate::app::image::read_image_config(image)?;
                    let path_env = cfg
                        .env
                        .iter()
                        .find(|(k, _)| k == "PATH")
                        .map(|(_, v)| v.as_str())
                        .unwrap_or("/usr/local/bin:/usr/local/sbin:/usr/bin:/usr/sbin:/bin:/sbin");
                    match search_image_path(&rootfs, requested, path_env) {
                        Some(found) => {
                            searched = found;
                            &searched
                        }
                        None => anyhow::bail!(
                            "With --image, program {:?} was not found on the image PATH \
                         ({path_env}); use an absolute path like `/bin/sh`, or a path \
                         containing '/' relative to the working directory.",
                            requested
                        ),
                    }
                } else {
                    requested
                };
            let guest_abs = resolve_image_guest_program(requested, &guest_cwd)?;
            // A program under a --bind target lives in the bind source, not
            // the rootfs; validate it there. The longest matching target
            // wins (binds may nest, e.g. /work and /work/repo).
            let mut best: Option<(PathBuf, PathBuf)> = None;
            for bind in &self.bind {
                use std::os::unix::ffi::OsStringExt as _;
                let target = PathBuf::from(std::ffi::OsString::from_vec(
                    bind.target.as_bytes().to_vec(),
                ));
                if let Ok(rel) = guest_abs.strip_prefix(&target)
                    && best.as_ref().is_none_or(|(current, _)| {
                        target.as_os_str().len() > current.as_os_str().len()
                    })
                {
                    let source = PathBuf::from(std::ffi::OsString::from_vec(
                        bind.source.as_bytes().to_vec(),
                    ));
                    best = Some((target, source.join(rel)));
                }
            }
            if let Some((_, host)) = best {
                validate_executable(&host, &guest_abs, None)?;
                return Ok((host, guest_abs, None));
            }
            // Resolve chroot-aware: images (nixos/nix especially) expose
            // `/bin/sh` as a symlink to an absolute `/nix/store/...` target that
            // only resolves under the image root, so a naive host `stat` would
            // follow it against the host `/` and fail.
            let in_rootfs = crate::app::image::resolve_in_rootfs(&rootfs, &guest_abs);
            validate_executable(&in_rootfs, &guest_abs, Some(&rootfs))?;
            return Ok((in_rootfs, guest_abs, Some(rootfs)));
        }

        let command = self.guest_command()?;
        let requested = Path::new(command.get_program());
        if requested.is_absolute() {
            if let GuestPathMapping::Mapped(host) = self.mapped_host_program(requested) {
                return Ok((host, requested.to_path_buf(), None));
            }
            if requested.starts_with(TMP_DIR) && self.tmp.is_none() && requested.exists() {
                anyhow::bail!(
                    "Program {} is under host /tmp, but Hermit replaces guest /tmp with an \
                     isolated directory. Pass --tmp=/tmp to expose host /tmp or bind the program \
                     into guest /tmp.",
                    requested.display()
                );
            }
            self.refuse_program_under_dev_shm(requested)?;
            validate_executable(requested, requested, None)?;
            return Ok((requested.to_path_buf(), requested.to_path_buf(), None));
        }

        // ⚠️ SAME FAULT AS THE ABSOLUTE-PATH BRANCH ABOVE, SO THE SAME CODE.
        // The guest program is not there. Whether the caller spelled it as an
        // absolute path or as a bare name is a property of the COMMAND LINE, not
        // of the failure, and it was deciding the exit code: `/nope/x` gave 127
        // with class=guest-program-not-found while `nope-x` gave 125 with
        // class=cli-error. A caller scripting on the code got either for the same
        // mistake with no way to predict which.
        //
        // ⚠️ AND 125 IS NOT FREE. `bin/safehermit` sets rc=125 when a run is
        // cgroup-killed at the log byte cap -- "A distinct code, because 255 tells
        // a caller nothing about WHY". Under that wrapper a mistyped program name
        // was indistinguishable from a run killed for producing too much output.
        // 127 is the GNU convention for command-not-found and is what the
        // absolute branch already returns, so this joins it rather than minting a
        // third code.
        let resolved = command
            .find_program()
            .with_context(|| {
                format!(
                    "Could not resolve program {:?} in the guest PATH. Check PATH or use an absolute \
                     executable path.",
                    requested
                )
            })
            .map_err(|error| error.context(GuestProgramFault::NotFound))?;
        Ok((resolved, requested.to_path_buf(), None))
    }

    fn validate_executable_overlay_source_visibility(&self, source: &Path) -> Result<(), Error> {
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

    /// Refuse `--rewrite-exclusives` for a guest that runs from an OCI image
    /// (`--image`, and `hermit oci run`, which sets it). The rewritten copies
    /// are bind-mounted over the host files the guest program resolves to,
    /// and an image launch builds its own container in the image's
    /// materialized rootfs, which those mounts do not reach: the guest would
    /// run its original exclusive loops while the run reported a rewrite.
    /// The in-memory rewrite writes the guest's own mapped pages, whatever
    /// file they come from, so it is not refused.
    pub(crate) fn refuse_exclusive_rewrite_in_an_image(&self) -> Result<(), Error> {
        self.refuse_exclusive_rewrite_for_image_launch(self.image.is_some())
    }

    /// [`Self::refuse_exclusive_rewrite_in_an_image`] where the caller knows
    /// the guest runs from an image before `--image` says so: `hermit oci
    /// run` takes its image as a positional argument and resolves it to an
    /// id later, so its refusal must not wait for that.
    pub(crate) fn refuse_exclusive_rewrite_for_image_launch(
        &self,
        from_image: bool,
    ) -> Result<(), Error> {
        anyhow::ensure!(
            !self.rewrite_exclusives || self.rewrites_exclusives_in_memory() || !from_image,
            "--rewrite-exclusives-mode=overlay bind-mounts rewritten copies over the host files \
             the guest loads, and a guest run from an OCI image (--image, hermit oci run) loads \
             the files of the image's rootfs, which those mounts do not reach, so it would run \
             its original exclusive loops; rewrite in the guest's memory instead \
             (--rewrite-exclusives-mode=memory), or rewrite the files the image holds with \
             `hermit rewrite-exclusives --output-dir`"
        );
        Ok(())
    }

    /// Applies to a trial the preprocessing `RunOpts::main` does before a run
    /// and `RunOpts::run` does not: the exclusive-loop rewrite of
    /// `--rewrite-exclusives`. `hermit analyze` and `hermit bisect` start every
    /// trial through `RunOpts::run`, so without this a trial ran the original
    /// images while its printed reproducer, which carries the option, ran the
    /// rewritten ones.
    ///
    /// Every trial of one analysis shares the inputs the preparation reads
    /// ([`Self::trial_preprocessing_key`]), so the overlays are prepared once
    /// per process for those inputs and reused: one rewrite and one report
    /// per analysis, not one per trial. The key is all of them and whether
    /// the rewrite is in memory, so a second analysis in one process (as
    /// tests run) with other mounts, another `/tmp` or working directory, a
    /// changed file or the other mode prepares, and checks, its own.
    pub(crate) fn prepare_trial_preprocessing(&mut self) -> Result<(), Error> {
        if !self.rewrite_exclusives {
            return Ok(());
        }
        // Every trial is checked, before anything prepared is reused: the
        // cache holds artifacts, not permission.
        self.validate_exclusive_rewrite(self.selected_backend())?;
        let key = self.trial_preprocessing_key();
        let mut prepared = PREPARED_TRIAL_PREPROCESSING
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(position) = prepared.iter().position(|(seen, ..)| *seen == key) {
            let (_, overlays, covered, unmarked, fingerprints) = &prepared[position];
            // A remembered path is reused only while the artifact at that
            // path still holds exactly the bytes and mode the preparation
            // made. The cache directory stays writable by the invoking
            // user between trials, so a deleted or replaced artifact must
            // never be mounted: drop the stale entry and prepare again,
            // which rewrites and repairs the cache entry.
            if prepared_overlays_current(overlays, fingerprints) {
                self.exclusive_rewrite_overlays = overlays.clone();
                self.exclusive_rewrite_covered = covered.clone();
                self.exclusive_rewrite_unmarked = unmarked.clone();
                return Ok(());
            }
            prepared.remove(position);
        }
        self.exclusive_rewrite_overlays.clear();
        self.exclusive_rewrite_covered.clear();
        self.exclusive_rewrite_unmarked.clear();
        self.prepare_exclusive_rewrite(self.selected_backend())?;
        let fingerprints = self
            .exclusive_rewrite_overlays
            .iter()
            .filter_map(|overlay| {
                overlay_fingerprint(&overlay.source)
                    .map(|(digest, mode)| (overlay.source.clone(), digest, mode))
            })
            .collect();
        prepared.push((
            key,
            self.exclusive_rewrite_overlays.clone(),
            self.exclusive_rewrite_covered.clone(),
            self.exclusive_rewrite_unmarked.clone(),
            fingerprints,
        ));
        Ok(())
    }

    /// What decides the overlays one analysis prepares (see
    /// [`Self::prepare_trial_preprocessing`]): every input the preparation
    /// reads. The options that place the guest's files and decide what is
    /// rewritten: the program, the `--rewrite-exclusives-also` files, the
    /// backend, the mounts and binds, `--tmp`, `--workdir`, `--image`, the
    /// guest environment, and the host's working directory and `PATH`. And
    /// the files those resolve to, as the preparation resolves them, each
    /// with a digest of its bytes and its permissions: a file that changed
    /// between two analyses gets a new copy, not the one made from its old
    /// bytes, and one whose mode changed gets a copy with that mode, which
    /// the rewritten file carries (`elf::write_rewritten`). A file that does
    /// not resolve or read keys as its error, which the preparation then
    /// reports. The cache the copies live in is part of the key too: it
    /// follows this process's environment
    /// (`hermit::aarch64_lse::cache_path`), so an overlay prepared under
    /// another `HOME` or `XDG_CACHE_HOME` names a path this run would not
    /// write.
    fn trial_preprocessing_key(&self) -> String {
        let mut files = vec![
            self.resolve_guest_and_host_program()
                .and_then(|(guest, host)| {
                    Ok((self.resolve_executable_overlay_target(&guest, &host)?, host))
                }),
        ];
        files.extend(self.rewrite_exclusives_also.iter().map(|also| {
            fs::canonicalize(also)
                .map(|path| (path.clone(), path))
                .map_err(Error::from)
        }));
        let files: Vec<String> = files
            .into_iter()
            .map(|file| match file {
                Ok((guest, host)) => match (fs::read(&host), fs::metadata(&host)) {
                    (Ok(bytes), Ok(metadata)) => format!(
                        "{} at {}: {} mode {:o}",
                        guest.display(),
                        host.display(),
                        detcore::Digest::new(&bytes),
                        metadata.permissions().mode()
                    ),
                    (Err(error), _) | (_, Err(error)) => {
                        format!("{}: {error}", host.display())
                    }
                },
                Err(error) => format!("{error:#}"),
            })
            .collect();
        format!(
            "{:?}",
            (
                (
                    &self.program,
                    &self.rewrite_exclusives_also,
                    self.rewrites_exclusives_in_memory(),
                    self.selected_backend(),
                ),
                (
                    &self.mount,
                    &self.bind,
                    &self.tmp,
                    &self.workdir,
                    &self.image
                ),
                (
                    &self.base_env,
                    &self.env,
                    std::env::current_dir().ok(),
                    std::env::var_os("PATH"),
                    hermit::aarch64_lse::cache_path(),
                ),
                files,
            )
        )
    }

    /// Rewrite the guest executable's canonical exclusive loops (and those of
    /// any file named by `--rewrite-exclusives-also`) into content-addressed
    /// copies, and overlay each copy on the path the guest loads it from.
    ///
    /// The rewrite is a pure function of the input bytes, and each copy is
    /// named by the bytes it holds
    /// (`aarch64_lse::cached_rewrite`): two runs of one `--verify` pair, and
    /// a replay of a recording, map identical bytes, and runs that would map
    /// different bytes never share a copy.
    fn prepare_exclusive_rewrite(&mut self, backend: Backend) -> Result<(), Error> {
        // Every preparation, however it was reached, including a library
        // caller that validated nothing.
        self.refuse_exclusive_rewrite_in_an_image()?;
        self.validate_exclusive_rewrite(backend)?;
        if self.rewrites_exclusives_in_memory() {
            // Detcore rewrites each executable file mapping as the guest maps
            // it (`rewrite_exclusives_in_memory` in its config), so there is
            // nothing to prepare here.
            crate::app::tracing::diagnostic_eprintln!(
                ":: Exclusive-loop rewrite: in the guest's memory, as each executable file is \
                 mapped; the files are unchanged"
            );
            return Ok(());
        }
        self.prepare_exclusive_rewrite_overlays()
    }

    /// Refuse `--rewrite-exclusives` where it cannot apply: the flag
    /// combinations first, which are wrong on any host, then the backend and
    /// the host. Every run that rewrites checks this, including each analyze
    /// trial before it looks for prepared overlays.
    fn validate_exclusive_rewrite(&self, backend: Backend) -> Result<(), Error> {
        let in_memory = self.rewrites_exclusives_in_memory();
        anyhow::ensure!(
            !in_memory || self.rewrite_exclusives_also.is_empty(),
            "--rewrite-exclusives-also names files for the overlay; rewriting in memory covers \
             every executable file the guest maps"
        );
        anyhow::ensure!(
            in_memory || !self.no_namespace,
            "--rewrite-exclusives-mode=overlay bind-mounts rewritten files over the originals, \
             and a --no-namespace guest has no mount namespace to carry them; rewriting in \
             memory (the default under --no-namespace) needs none"
        );
        // An image launch builds its own container, without the overlay
        // mounts, so the guest would run the original code.
        self.refuse_exclusive_rewrite_in_an_image()?;
        // Detcore does the rewrite; --namespace-only runs the guest without it.
        anyhow::ensure!(
            !in_memory || !self.namespace_only,
            "--rewrite-exclusives in the guest's memory is done by Detcore, which \
             --namespace-only does not run; use --rewrite-exclusives-mode=overlay"
        );
        // Detcore writes a new mapping's code while the guest's other threads
        // are stopped; with threads running concurrently, one could be inside
        // that code during the write.
        anyhow::ensure!(
            !in_memory || self.strict || !self.no_sequentialize_threads,
            "--rewrite-exclusives in the guest's memory writes code that another thread could \
             be running under --no-sequentialize-threads; run with threads sequentialized, or \
             with --rewrite-exclusives-mode=overlay"
        );
        anyhow::ensure!(
            backend == Backend::Ptrace,
            "--rewrite-exclusives is preprocessing for the ptrace backend; \
             backend `{backend:?}` would run the rewritten image under a different runtime"
        );
        anyhow::ensure!(
            hermit::aarch64_lse::host_has_lse_atomics(),
            "--rewrite-exclusives rewrites exclusive loops into FEAT_LSE atomic instructions, \
             which this host does not execute (no HWCAP_ATOMICS); the rewritten image would \
             raise SIGILL"
        );
        Ok(())
    }

    /// The overlay half of [`Self::prepare_exclusive_rewrite`]: rewrite the
    /// files into the cache and overlay each copy on its guest path.
    fn prepare_exclusive_rewrite_overlays(&mut self) -> Result<(), Error> {
        let (guest, host) = self.resolve_guest_and_host_program()?;
        let guest = self.resolve_executable_overlay_target(&guest, &host)?;
        let mut files = vec![(guest, host)];
        for also in &self.rewrite_exclusives_also {
            let path = fs::canonicalize(also).with_context(|| {
                format!(
                    "--rewrite-exclusives-also {} cannot be resolved",
                    also.display()
                )
            })?;
            // The overlay binds the rewritten copy over this path, so it is
            // both the host path read and the guest path mounted: a file the
            // guest reaches under a different path would be rewritten here
            // and loaded unrewritten there.
            anyhow::ensure!(
                self.mapped_or_visible_host_program(&path).as_deref() == Some(path.as_path()),
                "--rewrite-exclusives-also {} must name a path the guest loads unchanged; \
                 this run's mounts map it elsewhere",
                path.display()
            );
            files.push((path.clone(), path));
        }
        // The overlay mechanism is e9patch's, so its checks apply here too:
        // a custom mount must not hide the target or the cached artifact.
        self.validate_executable_overlay_mount_targets()?;
        let cache = hermit::aarch64_lse::cache_dir()?;
        self.exclusive_rewrite_covered = files.iter().map(|(guest, _)| guest.clone()).collect();
        for (guest_path, host_path) in files {
            let input = fs::read(&host_path)
                .with_context(|| format!("reading {} to rewrite", host_path.display()))?;
            let name = host_path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "image".to_owned());
            // Rewrite into memory first, always: the cache entry is named by
            // the bytes it must hold (`aarch64_lse::cached_rewrite`), so a
            // cached file is mounted only if it is exactly what this rewriter
            // made of this source.
            let mut rewritten = input.clone();
            let report = hermit::aarch64_lse::elf::rewrite_bytes(&mut rewritten)?;
            let Some(report) = report else {
                crate::app::tracing::diagnostic_eprintln!(
                    ":: Exclusive-loop rewrite: {} is not an aarch64 ELF image; left unchanged",
                    guest_path.display()
                );
                continue;
            };
            crate::app::tracing::diagnostic_eprintln!(
                "{}",
                exclusive_rewrite_planned_line(&guest_path, &report)
            );
            // The first few, as examples; the listing names every one.
            const LOGGED: usize = 16;
            for (addr, why) in report.refused_sites.iter().take(LOGGED) {
                tracing::info!("exclusive loop left alone at {addr:#x}: {why}");
            }
            if let Some(more) = report.refused_sites.len().checked_sub(LOGGED)
                && more != 0
            {
                tracing::info!(
                    "{more} more exclusive loops left alone in {}; `hermit rewrite-exclusives \
                     --list-refusals` lists every one",
                    guest_path.display()
                );
            }
            // A file nothing marks is named, never left silently: the run's
            // summary lists it too (`Config::rewrite_exclusives_unmarked`).
            if report.unmarked() != 0 {
                crate::app::tracing::diagnostic_eprintln!(
                    ":: Exclusive-loop rewrite: {} has no mapping symbols ($x, $d) marking its \
                     code, so its {} exclusive loop(s) there were left as they are",
                    guest_path.display(),
                    report.unmarked()
                );
                self.exclusive_rewrite_unmarked.push(guest_path.clone());
            }
            if report.unsupported() != 0 {
                crate::app::tracing::diagnostic_eprintln!(
                    ":: Exclusive-loop rewrite: {} has a mapping symbol other than $x and $d, so \
                     its {} exclusive loop(s) in the sections it marks were left as they are; \
                     `hermit rewrite-exclusives --list-refusals` names the symbol",
                    guest_path.display(),
                    report.unsupported()
                );
                if report.unmarked() == 0 {
                    self.exclusive_rewrite_unmarked.push(guest_path.clone());
                }
            }
            if report.patched_total() != 0 {
                let output =
                    hermit::aarch64_lse::cached_rewrite(&cache, &rewritten, &host_path, &name)?;
                self.validate_executable_overlay_source_visibility(&output)?;
                self.exclusive_rewrite_overlays
                    .push(ExecutableOverlayMount {
                        source: output,
                        target: guest_path,
                        rewritten: Some(report.patched_total()),
                    });
            }
        }
        Ok(())
    }

    pub(super) fn adapter_executable_overlay_plan(
        &self,
    ) -> Result<super::backend_hook::ExecutableOverlay, Error> {
        let (guest, host) = self.resolve_guest_and_host_program()?;
        let target = self.resolve_executable_overlay_target(&guest, &host)?;
        Ok(super::backend_hook::ExecutableOverlay {
            guest,
            host,
            target,
        })
    }

    pub(super) fn adapter_with_executable_overlay(
        &self,
        plan: &super::backend_hook::ExecutableOverlay,
        source: Option<&Path>,
    ) -> Result<Self, Error> {
        anyhow::ensure!(
            self.adapter_executable_overlay_plan()? == *plan,
            "executable overlay no longer matches the admitted guest/mount resolution"
        );
        let overlay = if let Some(source) = source {
            self.validate_executable_overlay_mount_targets()?;
            self.validate_executable_overlay_source_visibility(source)?;
            Some(ExecutableOverlayMount {
                source: source.to_owned(),
                target: plan.target.clone(),
                rewritten: None,
            })
        } else {
            None
        };
        let mut options = self.clone();
        options.prepared_program = Some(plan.guest.clone());
        options.executable_overlay = overlay;
        Ok(options)
    }

    fn write_backend_engagement_after_run(&self) -> Result<(), Error> {
        let Some(path) = &self.backend_engagement_json else {
            return Ok(());
        };
        let engagement = match self.selected_backend() {
            Backend::Ptrace => {
                let summary_path = self.summary_json.as_deref().ok_or_else(|| {
                    Error::msg("ptrace backend engagement requires a typed run summary")
                })?;
                let summary = fs::read(summary_path)
                    .with_context(|| {
                        format!(
                            "reading ptrace backend engagement from {}",
                            summary_path.display()
                        )
                    })
                    .and_then(|bytes| {
                        serde_json::from_slice::<RunSummary>(&bytes).with_context(|| {
                            format!(
                                "parsing ptrace backend engagement from {}",
                                summary_path.display()
                            )
                        })
                    })?;
                BackendEngagement::Ptrace {
                    scheduler_turns: summary.sched_turns,
                }
            }

            Backend::Liteinst | Backend::InGuestTrap | Backend::Kvm => {
                return Err(Error::msg(format!(
                    "backend `{}` does not expose an engagement value",
                    self.selected_backend().as_str()
                )));
            }
        };
        write_backend_engagement(path, engagement)
    }

    fn tmpfs(&self) -> Result<Tmpfs, Error> {
        match self.tmp.as_ref() {
            Some(path) => {
                let path = path.as_path();
                fs::create_dir_all(path)?;
                Ok(Tmpfs::Path(path.to_path_buf()))
            }
            None => Ok(Tmpfs::Temp(tempfile::TempDir::new()?)),
        }
    }

    /// Run the guest once. With `capture_output` the guest's output is
    /// returned with the scheduler terminal outcome the run ended with, if any
    /// (a deadlock under Hermit's model, a virtual timeout), whose status is
    /// then not the guest's own; without it such an outcome is returned as its
    /// error.
    pub fn run(
        &self,
        global: &GlobalOpts,
        capture_output: bool,
    ) -> Result<(ExitStatus, Option<Output>, Option<detcore::TerminalOutcome>), Error> {
        #[cfg(all(feature = "native-exec-input-admission", not(feature = "native-exec")))]
        self.refuse_uncompiled_native_exec(self.selected_backend())?;
        self.refuse_unready_exec_stop()?;
        // Analyze and other in-process callers bypass the outer CLI entry.
        self.refuse_exclusive_rewrite_in_an_image()?;
        self.refuse_an_invalid_replayed_record()?;
        detcore::preemptions::validate_config_replay_inputs(&self.effective_det_config())?;
        <detcore::Detcore as reverie::Tool>::facility_admission(&self.det_opts.det_config)?;
        let mut options = self.clone();
        options.capture_launch_namespace_mounts();
        options.det_opts.det_config.annotated_priority_replay =
            options.annotated_priority_replay_admission()?;
        // `hermit run` took the snapshot in `main`; a run started another way
        // (each trial of `hermit analyze`) takes its own here, and a trial
        // that reads the same properties shares that copy and its path.
        options.take_property_snapshot()?;
        let network_trace = options.open_network_trace()?;
        let global = global.clone();
        #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
        let mapped_image = if self.in_guest_exec_stop {
            Some(super::mapped_first_admission::RetainedImage::open()?)
        } else {
            None
        };
        #[cfg(not(all(feature = "exec-stop-launcher", feature = "liteinst")))]
        let mapped_image: Option<()> = None;
        let resources = if network_trace.is_some() {
            "network trace descriptor"
        } else {
            "no descriptors"
        };
        let (value, (_, mapped_image)) = self.launch_in_container(
            (network_trace, mapped_image),
            resources,
            self.run_timeout(),
            move |(_, mapped_image), identity, native| {
                #[cfg(feature = "native-exec")]
                if let Some(native) = native {
                    let _ = native;
                    return Err(Error::msg(
                        "native exec currently requires the verification run path",
                    ));
                }
                #[cfg(not(feature = "native-exec"))]
                let _ = native;
                #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
                if let Some(image) = mapped_image.as_ref() {
                    return options.run_mapped_in_container(
                        &global,
                        capture_output,
                        identity,
                        image,
                    );
                }
                #[cfg(not(all(feature = "exec-stop-launcher", feature = "liteinst")))]
                let _ = mapped_image;
                options.run_in_container(&global, capture_output, identity)
            },
        )?;
        #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
        if let Some(image) = mapped_image.as_ref() {
            image.verify()?;
        }
        #[cfg(not(all(feature = "exec-stop-launcher", feature = "liteinst")))]
        let _ = mapped_image;
        Ok(value)
    }

    /// Decide how the guest's process IDs are numbered (`guest_ids::resolve`).
    /// Detcore reports it, at INFO level and in the run summary. Only the ptrace
    /// backend can create a guest's tasks with chosen IDs; another backend
    /// without a PID namespace gives its guest host IDs.
    fn resolve_guest_ids(&mut self) -> Result<(), Error> {
        // A replay under --no-namespace needs the record's IDs: its base,
        // required as if given by --pid-base, so that a replay that cannot
        // have them is refused, naming the record. A record no replay can
        // follow is refused before anything is reserved or probed.
        let replayed = self.no_namespace_replayed_guest_ids()?;
        if let Some((source, recorded)) = &replayed
            && let Some(refusal) = replay_guest_ids_refusal(source, *recorded, None)
        {
            return Err(Error::new(PolicyRefusal).context(refusal));
        }
        let required = match (&replayed, self.pid_base) {
            (_, Some(base)) => Some(super::guest_ids::Required::PidBase(base)),
            (Some((source, detcore_model::pid::GuestIds::Chosen { base, .. })), None) => {
                Some(super::guest_ids::Required::Replay {
                    source,
                    base: *base,
                })
            }
            _ => None,
        };
        let backend = self.runtime_backend();
        let (ids, warning) =
            if backend == Backend::Ptrace && self.det_opts.det_config.sequentialize_threads {
                super::guest_ids::resolve(self.no_namespace, required, self.strict)?
            } else if self.no_namespace
                && matches!(
                    backend,
                    Backend::Ptrace | Backend::Liteinst | Backend::InGuestTrap
                )
            {
                // The guest's tasks are host tasks numbered by the host: unless
                // threads are sequentialized, the order IDs would be chosen in is
                // the host's too. KVM and DBT number the guest themselves.
                (detcore_model::pid::GuestIds::Host, None)
            } else {
                (detcore_model::pid::GuestIds::Namespace, None)
            };
        if let Some((source, recorded)) = &replayed
            && let Some(refusal) = replay_guest_ids_refusal(source, *recorded, Some(ids))
        {
            return Err(Error::new(PolicyRefusal).context(refusal));
        }
        if let Some(warning) = warning {
            // Not `eprintln!`, which panics when stderr's reader has gone.
            use std::io::Write as _;
            let _ = writeln!(std::io::stderr(), "{warning}");
        }
        // A base the command line or a replayed record required is not drawn
        // from the host.
        self.guest_ids_drawn_from_host =
            guest_ids_drawn_from_host(self.no_namespace, ids, required.is_some() || self.strict);
        self.det_opts.det_config.guest_ids = ids;
        // Namespace-only execs the command directly without Detcore replay or
        // Chosen provenance admission. Inherited option-metadata validation
        // still runs; only this execution-specific replay preflight is bypassed.
        if !self.namespace_only {
            detcore::preemptions::validate_config_replay_inputs(&self.effective_det_config())?;
        }
        Ok(())
    }

    /// Numbers a trial's guest as `RunOpts::main` does (`resolve_guest_ids`),
    /// which a trial, started through `RunOpts::run`, would otherwise skip.
    ///
    /// Under `--no-namespace` the IDs are resolved once per process, by the
    /// first trial, and every later trial reuses them: the window they start
    /// in stays reserved for this process (`guest_ids::Reservation`), so a
    /// second resolution would find it taken by this very analysis. Trials
    /// replay one another's records, which name every thread by its ID, so
    /// the IDs must be chosen: a first trial that would get host IDs is
    /// refused, and so is any task that cannot have its chosen ID
    /// (`refuse_host_ids`), instead of desyncing every replay after it. A
    /// trial that replays a record with other IDs is refused as
    /// `hermit run` refuses it.
    ///
    /// The memo (`TRIAL_GUEST_IDS`) is normally filled before the first
    /// trial, from the analysis's input records
    /// (`prepare_trial_guest_ids_for_inputs`). The window is not checked
    /// again for host tasks; one that takes an ID in it during the analysis
    /// gets the trial whose task needed that ID refused (`refuse_host_ids`),
    /// which ends the analysis.
    ///
    /// The trial's options name the base (`--pid-base`), so that its printed
    /// reproducer replays its record under the same IDs.
    pub(crate) fn prepare_trial_guest_ids(&mut self) -> Result<(), Error> {
        if !self.no_namespace {
            return Ok(());
        }
        let mut resolved = TRIAL_GUEST_IDS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(ids) = *resolved {
            if let Some((source, recorded)) = self.no_namespace_replayed_guest_ids()?
                && let Some(refusal) = replay_guest_ids_refusal(&source, recorded, Some(ids))
            {
                return Err(Error::new(PolicyRefusal).context(refusal));
            }
            self.det_opts.det_config.guest_ids = ids;
            self.pid_base = ids.base();
            return Ok(());
        }
        self.resolve_guest_ids()?;
        let ids = trial_guest_ids(self.det_opts.det_config.guest_ids)
            .map_err(|refusal| Error::new(PolicyRefusal).context(refusal))?;
        self.det_opts.det_config.guest_ids = ids;
        self.pid_base = ids.base();
        *resolved = Some(ids);
        Ok(())
    }

    /// Numbers the trials of an analysis or bisection before its first trial
    /// (`prepare_trial_guest_ids`), from its input `recordings`, so that every
    /// record it writes for a trial can state the trials' numbering
    /// (`AnalyzeOpts::stamp_trial_record`). The inputs number their guests
    /// alike (`AnalyzeOpts::refuse_recordings_numbered_apart`), so the first
    /// speaks for all.
    ///
    /// Under `--no-namespace` the trials are numbered as a replay of the first
    /// input would be (`resolve_guest_ids`): from its base
    /// (`guest_ids::Required::Replay`), and an input whose IDs Hermit did not
    /// choose is refused, naming it, as `hermit run` refuses its replay. The
    /// run arguments' own replayed record or `--pid-base` takes precedence,
    /// and the trials replaying the inputs must then agree with it. Without
    /// inputs, as for a seeded analysis, a free window is chosen. In a PID
    /// namespace an input numbered otherwise is refused: every trial numbers
    /// its guest by its namespace, so none could follow it.
    pub(crate) fn prepare_trial_guest_ids_for_inputs(
        &mut self,
        recordings: &[(&str, &Path)],
    ) -> Result<(), Error> {
        if !self.no_namespace {
            for (option, path) in recordings {
                let ids =
                    detcore::preemptions::read_recorded_guest_ids(path).map_err(Error::msg)?;
                if ids != detcore_model::pid::GuestIds::Namespace {
                    return Err(Error::new(PolicyRefusal).context(format!(
                        "{option} {} was recorded with guest process IDs {ids}, and every trial \
                         numbers its guest by its own PID namespace, so no trial could follow \
                         it. Analyze it under --no-namespace (with the same --pid-base) instead",
                        path.display()
                    )));
                }
            }
            return Ok(());
        }
        // As each trial does before it is numbered (`RunData::launch`): this
        // settles the configuration the numbering reads, such as whether
        // threads are sequentialized.
        self.validate_args()?;
        let config = &mut self.det_opts.det_config;
        if config.replay_preemptions_from.is_none()
            && config.replay_schedule_from.is_none()
            && let Some((_, path)) = recordings.first()
        {
            config.replay_preemptions_from = Some(path.to_path_buf());
        }
        self.prepare_trial_guest_ids()
    }

    /// Under `--no-namespace`, the replayed record (`--replay-preemptions-from`
    /// or `--replay-schedule-from`) and the guest IDs it was recorded with.
    fn no_namespace_replayed_guest_ids(
        &self,
    ) -> Result<Option<(String, detcore_model::pid::GuestIds)>, Error> {
        if !self.no_namespace {
            return Ok(None);
        }
        let config = &self.det_opts.det_config;
        let mut replayed = None;
        for (option, path) in [
            ("--replay-preemptions-from", &config.replay_preemptions_from),
            ("--replay-schedule-from", &config.replay_schedule_from),
        ] {
            let Some(path) = path else { continue };
            let record = detcore::preemptions::boundary_recording::Recording::read(path)?;
            let source = format!("{option} {}", path.display());
            let recorded = record.guest_ids();
            // Before host-ID probes/container setup, use the same full record
            // to check its required layout and numbering. Final loaders repeat
            // this on the immutable bytes they actually install.
            let runtime_ids =
                self.pid_base
                    .map_or(recorded, |base| detcore_model::pid::GuestIds::Chosen {
                        base,
                        refuse_host_ids: true,
                    });
            record.validate_replay_inputs(&source, runtime_ids)?;
            if let Some((_, previous)) = &replayed
                && let Some(refusal) =
                    detcore::preemptions::guest_ids_mismatch(&source, *previous, recorded)
            {
                return Err(Error::new(PolicyRefusal).context(refusal));
            }
            if replayed.is_none() {
                replayed = Some((source, recorded));
            }
        }
        Ok(replayed)
    }

    fn warn_no_namespace(&self) {
        if self.no_namespace {
            // Not `eprintln!`, which panics when stderr's reader has gone
            // (EPIPE): a warning must not end the run with 101.
            use std::io::Write as _;
            let _ = writeln!(
                std::io::stderr(),
                "WARNING: --no-namespace is not a sandbox; run trusted guests only. The guest \
                 inherits the caller UID/GID/capabilities and shares host /proc, filesystem, /tmp, \
                 localhost/network, ports, Unix sockets, and mutable state between runs. Unsupported \
                 syscalls can mutate host state; --verify may be less deterministic due to shared state."
            );
        }
    }

    /// Run `work` as the init of this run's container: the PID, IPC, UTS, mount
    /// and network namespaces, private /tmp, /dev/shm and /dev/mqueue and
    /// identity mounts that every backend shares, or a plain child under
    /// `--no-namespace`. `work` receives the parent-held `guards`, the
    /// container's identity mounts, and any explicitly enrolled native run. An
    /// adapter that cannot consume a native run must refuse it.
    pub(super) fn launch_in_container<G, F, T>(
        &self,
        guards: G,
        resources: &str,
        timeout: Option<Duration>,
        mut work: F,
    ) -> Result<(T, G), Error>
    where
        G: 'static,
        F: FnMut(&mut G, Option<&IdentityGuard>, Option<NativeContainerRun>) -> Result<T, Error>
            + 'static,
        T: serde::Serialize + serde::de::DeserializeOwned + 'static,
    {
        #[cfg(all(feature = "native-exec-input-admission", not(feature = "native-exec")))]
        self.refuse_uncompiled_native_exec(self.selected_backend())?;
        self.refuse_virtual_timeout_with_debugger()?;
        self.refuse_unready_exec_stop()?;
        if !self.namespace_only {
            detcore::preemptions::validate_config_replay_inputs(&self.effective_det_config())?;
            <detcore::Detcore as reverie::Tool>::facility_admission(&self.det_opts.det_config)?;
        }
        let gdbserver_port = self.bind_gdbserver_port()?;
        let (mut container, tmpfs, identity, resources) = if self.no_namespace {
            let mut container = Container::new();
            apply_affinity(
                &mut container,
                self.pin_threads,
                self.det_opts.det_config.cpu_profile_source,
            );
            (
                container,
                None,
                None,
                format!("held {resources}; no PID namespace"),
            )
        } else {
            let tmpfs = self.tmpfs()?;
            let (container, identity) = self.container(tmpfs.path())?;
            let resources = format!(
                "private tmp {}, identity mounts and {resources}",
                tmpfs.path().display()
            );
            (container, Some(tmpfs), Some(identity), resources)
        };
        #[cfg(feature = "native-exec")]
        let native_plan = self
            .native_exec_capture
            .as_ref()
            .map(|capture| capture.plan(&mut container))
            .transpose()?;
        #[cfg(not(feature = "native-exec"))]
        let native_plan: Option<()> = None;
        // Inherited at the clone: where the child's scheduler leaves a virtual
        // timeout it exits 124 with instead of reporting it
        // (`classify_container_result`).
        if self.det_opts.det_config.virtual_timeout.is_some() {
            super::terminal_outcome_note::arm()?;
        }
        // Inherited at the clone too: where the child's scheduler leaves the
        // late timer events behind a replay refusal it exits 122 with
        // (`classify_container_result`).
        if self.det_opts.det_config.replay_preemptions_from.is_some()
            || self.det_opts.det_config.replay_schedule_from.is_some()
        {
            super::replay_refusal_note::arm()?;
        }
        super::owned_container::run(
            &mut container,
            (tmpfs, identity, guards, native_plan, gdbserver_port),
            resources,
            !self.no_namespace,
            "with_container",
            timeout,
            move |(_, identity, guards, native_plan, gdbserver_port)| {
                // What the mount plan made, before anything else uses the
                // container's mounts (a no-op for a plan with no record), and
                // whether the placeholder directory is still where the guest
                // path reaches it.
                if let Some(identity) = identity.as_mut() {
                    identity.record_plan()?;
                    identity.confirm_directory_placeholder();
                }
                if let Some(port) = gdbserver_port.take() {
                    port.serve_next_gdbserver();
                }
                #[cfg(feature = "native-exec")]
                let native = native_plan
                    .take()
                    .map(|plan| unsafe { plan.materialize() })
                    .transpose()?;
                #[cfg(not(feature = "native-exec"))]
                let native = native_plan.take();
                work(guards, identity.as_ref(), native)
            },
        )
        .map(|(value, (_, _, guards, _, _))| (value, guards))
    }

    /// For a `--gdbserver` run on the ptrace tracer, the one that serves gdb,
    /// binds the gdbserver's port here, in the host's network namespace,
    /// before the container exists, so a gdb client on the host reaches it
    /// whatever network the guest has ([`hermit::gdbserver`]), and says where
    /// to attach.
    fn bind_gdbserver_port(&self) -> Result<Option<hermit::gdbserver::GdbPort>, Error> {
        let config = &self.det_opts.det_config;
        if !config.gdbserver || self.runtime_backend() != Backend::Ptrace {
            return Ok(None);
        }
        let port = hermit::gdbserver::GdbPort::bind(config.gdbserver_port).with_context(|| {
            // `--dap-listen` must know the port, so it refuses 0.
            let remedy = if self.dap_listen.is_some() {
                "pass another --gdbserver-port"
            } else {
                "pass another --gdbserver-port, or --gdbserver-port=0 to let the system pick one"
            };
            format!(
                "Hermit cannot serve gdb on 127.0.0.1:{}: {remedy}",
                config.gdbserver_port
            )
        })?;
        // A `--dap-listen` run's supervisor names the attach request instead.
        if self.dap_listen.is_none() {
            crate::app::tracing::diagnostic_eprintln!(
                "hermit run: the guest waits for one gdb client on 127.0.0.1:{0} before it \
                 starts; attach with: gdb -ex 'target remote 127.0.0.1:{0}' {1}",
                port.port(),
                self.program_path().display()
            );
        }
        Ok(Some(port))
    }

    fn run_with_namespace_only(&self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        // TODO: Make this use detcore instead after detcore is capable of being
        // "lightweight".
        let _guard = global.init_tracing_for_backend(self.runtime_backend());

        let tmpfs = self.tmpfs()?;
        let PreparedMounts {
            mounts,
            identity_sources: _identity_sources,
        } = self.mounts(tmpfs.path())?;

        let mut command = self.namespace_only_guest_command()?;
        // `--namespace-only` does NOT go through `with_container`: it unshares
        // the guest's PID and IPC namespaces and execs the guest directly, so the guest process
        // ITSELF becomes PID 1 of the new namespace and inherits the same
        // undeliverable-signal protection. An adversarial review found this
        // second launch path after the `with_container` guards were in place --
        // the fix worked and was not yet complete.
        //
        // There is no closure to arm here, so the guard goes in `pre_exec`,
        // which reverie runs in the forked child before the guest image is
        // loaded (`reverie-process/src/container.rs:738`). `PR_SET_PDEATHSIG`
        // is preserved across `execve`, so it is still armed once the guest is
        // running; reverie uses the same idiom for exec'd untraced members in
        // `safeptrace/src/notifier.rs`.
        //
        // Only the death signal is armed, deliberately. The SIGTERM/SIGINT/
        // SIGHUP handlers installed for the `with_container` path cannot help
        // here: `execve` resets caught signals to `SIG_DFL`, so any handler set
        // before exec is gone by the time the guest runs. Installing one would
        // also be wrong on its own terms -- in this mode PID 1 is the user's own
        // program, and hermit changing its signal dispositions would alter the
        // behaviour being observed.
        //
        // SAFETY: the arming (prctl, an atomic read-modify-write and `_exit`)
        // and the proc diagnostic use only Linux syscall wrappers and
        // signal-set operations, without allocation or stdio locks.
        unsafe {
            command.pre_exec(|| {
                super::container::arm_namespace_only_guest()?;
                hermit::proc_mount::warn_if_readonly_proc();
                Ok(())
            });
        }
        // Every guest `kill` stops for Hermit, which refuses a `kill(0)` that
        // would reach its own process group: nothing else intercepts the guest
        // in this mode (https://github.com/rrnewton/hermit/issues/4065). The
        // guest installs the filter last in its setup and sends the listener
        // back over this socket (`install_group_kill_filter`).
        let (listener_socket, guest_socket) = {
            use std::os::fd::FromRawFd;
            let mut pair = [-1; 2];
            // SAFETY: socketpair writes two descriptors into `pair`.
            if unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    pair.as_mut_ptr(),
                )
            } < 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            // SAFETY: both descriptors were just opened and are owned here.
            unsafe { (OwnedFd::from_raw_fd(pair[0]), OwnedFd::from_raw_fd(pair[1])) }
        };
        let filter = super::container::group_kill_filter();
        let guest_end = guest_socket.as_raw_fd();
        // SAFETY: `install_group_kill_filter` is async-signal-safe; the
        // guest's end of the socket stays open in Hermit until `spawn` returns.
        unsafe {
            command
                .pre_exec(move || super::container::install_group_kill_filter(&filter, guest_end));
        }
        command
            .unshare(GUEST_NAMESPACES)
            .map_root()
            .hostname("hermetic-container.local")
            .domainname("local")
            .mount(Mount::proc().allow_readonly_fallback())
            .mounts(mounts.into_iter().map(|planned| planned.mount));

        match &self.network {
            NetworkingMode::Local => {
                command.local_networking_only();
            }
            NetworkingMode::Host => {}
        }

        let mut child = match command.spawn() {
            Ok(child) => {
                // The child's mounts, the overlays among them, were made
                // before spawn returned.
                for line in exclusive_rewrite_applied_lines(&self.exclusive_rewrite_overlays) {
                    crate::app::tracing::diagnostic_eprintln!("{line}");
                }
                child
            }
            Err(error) => {
                let Some(errno) = super::container::group_kill_filter_failure(&listener_socket)
                else {
                    return Err(error.into());
                };
                // Failing closed is deliberate: without the filter a guest's
                // kill(0) reaches Hermit's group again.
                return Err(Error::new(super::container::PolicyRefusal).context(format!(
                    "hermit could not install the --namespace-only kill filter in the guest: \
                     seccomp(SECCOMP_SET_MODE_FILTER, SECCOMP_FILTER_FLAG_NEW_LISTENER) failed \
                     with {:?}. The filter keeps a guest's kill(0) from reaching Hermit and the \
                     processes that started it \
                     (https://github.com/rrnewton/hermit/issues/4065), so hermit refuses to start \
                     the guest without it. A host or container policy that denies seccomp, a \
                     seccomp filter of the caller's that already has a listener (another \
                     --namespace-only run, a seccomp-notify sandbox), or a kernel without \
                     SECCOMP_FILTER_FLAG_NEW_LISTENER causes it. Run hermit where seccomp is \
                     allowed and no other listener is installed.",
                    reverie::Errno::new(errno)
                )));
            }
        };
        drop(guest_socket);
        let listener = match super::container::receive_group_kill_listener(&listener_socket) {
            Ok(listener) => listener,
            Err(error) => {
                // The guest runs, but unpoliced: stop it rather than let it.
                let _ = child.signal(nix::sys::signal::Signal::SIGKILL);
                let _ = child.wait_blocking();
                return Err(error.into());
            }
        };
        // The thread stops once the guest has exited: the listener does not
        // always report that its last task is gone.
        let (stopped, stop) = {
            use std::os::fd::FromRawFd;
            let mut pipe = [-1; 2];
            // SAFETY: pipe2 writes two descriptors into `pipe`.
            if unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
                let _ = child.signal(nix::sys::signal::Signal::SIGKILL);
                let _ = child.wait_blocking();
                return Err(std::io::Error::last_os_error().into());
            }
            // SAFETY: both descriptors were just opened and are owned here.
            unsafe { (OwnedFd::from_raw_fd(pipe[0]), OwnedFd::from_raw_fd(pipe[1])) }
        };
        let police =
            std::thread::spawn(move || super::container::police_group_kills(listener, stopped));

        let exit_status = child.wait_blocking()?;
        drop(stop);
        let _ = police.join();

        Ok(exit_status)
    }

    // Execution mode corresponding to `run --verify-run DIR`: one of
    // `verify`'s two executions, run the way `verify` runs each (the same
    // `run_verify`, log level, private summary, host-input record, latches and
    // forwarded-record count), with what `verify` would hand its comparison
    // written to DIR instead of compared. `hermit log-diff --verify-json`
    // compares two such directories (`verify_run::compare`).
    fn verify_run(&self, directory: &Path, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        self.verify_run_with_execution(
            directory,
            None,
            false,
            |options, log, path, forwarded| {
                #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
                let mapped_image =
                    if options.in_guest_exec_stop {
                        Some(options.mapped_shared_image.as_ref().context(
                            "mapped --verify-run requires its holder's retained package",
                        )?)
                    } else {
                        None
                    };
                #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
                if let Some(image) = mapped_image {
                    image.verify()?;
                }
                let result = options.run_verify(
                    (log, path),
                    forwarded,
                    global,
                    VerificationRun::Run1,
                    #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
                    mapped_image.map(|image| MappedVerificationRun {
                        image: image.clone(),
                        policy: super::mapped_first_admission::CapturePolicy::Full,
                    }),
                );
                #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
                let result = if let Some(image) = mapped_image {
                    finish_mapped_verification_image(result, image.verify())
                } else {
                    result
                };
                let VerificationRunOutput {
                    output,
                    skid_overshoots,
                    latches,
                    terminal_outcome,
                } = result?;
                Ok(super::backend_hook::PhysicalRun {
                    output,
                    skid_overshoots,
                    latches,
                    terminal_outcome,
                    pending: (),
                })
            },
            |(), _| Ok(None),
        )
    }

    /// The existing saved-run writer and publication owner. Physical callbacks
    /// return observations only; this function retains latches, forwarded-log
    /// checks, summary/host-input files and the synchronized record-last commit.
    fn verify_run_with_execution<P>(
        &self,
        directory: &Path,
        transport: Option<super::backend_hook::VerificationLogTransport>,
        native: bool,
        mut execute: impl FnMut(
            &Self,
            fs::File,
            &Path,
            ForwardedDetlogPlan,
        ) -> Result<super::backend_hook::PhysicalRun<P>, Error>,
        finish: impl FnOnce(
            P,
            &Output,
        ) -> Result<Option<super::backend_hook::SavedNativeEvidence>, Error>,
    ) -> Result<ExitStatus, Error> {
        let writer = verify_run::VerifyRunWriter::create(directory)?;
        let log_path = writer.path(verify_run::LOG);
        let log_file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&log_path)
            .with_context(|| format!("creating the verification log {}", log_path.display()))?;
        let forwarded = self.forwarded_detlog_plan_for_transport(transport)?;
        let socket = forwarded.requires_socket();
        // Both reached through the controller's descriptors, as `verify`
        // reaches its own (see private_summary_descriptor_path).
        let summary_file = (!native).then(private_verify_summary).transpose()?;
        let summary_path = summary_file.as_ref().map(private_summary_descriptor_path);
        let host_inputs_file = if native {
            None
        } else {
            Some(tempfile::tempfile().context("creating the host-input record")?)
        };
        let host_inputs_path = host_inputs_file.as_ref().map(descriptor_path);
        let mut options = self.clone();
        if let Some(path) = &summary_path {
            options.summary_json = Some(path.clone());
        }
        if let Some(path) = &host_inputs_path {
            options.host_input_log = Some(path.clone());
        }
        options.fatal_core_run_label = Some("run");
        let mut comparison =
            verify_run::recorded_comparison(&self.verification_comparison_options());
        if native {
            comparison.strictness = LogCompareStrictness::Canonical;
        }
        let exact_branch_counter =
            (!native && self.arms_pmu_preemption_timer()).then(branch_counter_verdict);

        let completed = execute(&options, log_file, &log_path, forwarded).and_then(|completed| {
            let super::backend_hook::PhysicalRun {
                mut output,
                skid_overshoots,
                latches,
                terminal_outcome,
                pending,
            } = completed;
            latches.refuse_comparison("run")?;
            let forwarded_syscalls = self.take_forwarded_detlogs_for_transport(
                &log_path,
                &mut output.stderr,
                socket,
                transport,
            )?;
            let evidence = finish(pending, &output)?;
            anyhow::ensure!(
                evidence.is_some() == native,
                Error::new(hermit::BackendRunFailure).context(
                    "backend omitted or changed its predeclared saved verification evidence"
                )
            );
            Ok((
                output,
                skid_overshoots,
                forwarded_syscalls,
                terminal_outcome,
                evidence,
            ))
        });
        let (outcome, native_record, result) = match completed {
            Ok((output, skid_overshoots, forwarded_syscalls, terminal_outcome, evidence)) => {
                writer.write(verify_run::STDOUT, &output.stdout)?;
                writer.write(verify_run::STDERR, &output.stderr)?;
                let (guest_deadlock, virtual_timeout_ns) =
                    detcore::TerminalOutcome::to_fields(terminal_outcome);
                let native_record = evidence
                    .map(|evidence| {
                        writer.write_native_evidence(&evidence)?;
                        Ok::<_, Error>(verify_run::DbtRunRecord {
                            initialization_records: evidence.initialization_records(),
                        })
                    })
                    .transpose()?;
                (
                    verify_run::VerifyRunOutcome::Completed {
                        status: verify_run::recorded_status(output.status),
                        skid_overshoots,
                        forwarded_syscalls: forwarded_syscalls.map(|count| count as u64),
                        guest_deadlock,
                        virtual_timeout_ns,
                    },
                    native_record,
                    match terminal_outcome {
                        Some(ended) => Err(Error::new(SchedulerOutcome(ended))),
                        None => Ok(output.status),
                    },
                )
            }
            Err(error) => (verify_run_outcome_of(&error), None, Err(error)),
        };
        if let Some(summary_path) = &summary_path {
            let summary = fs::read(summary_path).unwrap_or_default();
            if !summary.is_empty() {
                writer.write(verify_run::SUMMARY, &summary)?;
                if let Some(public) = &self.summary_json {
                    fs::write(public, &summary)
                        .with_context(|| format!("writing the run summary {}", public.display()))?;
                }
            }
        }
        if let Some(host_inputs_path) = &host_inputs_path {
            let host_inputs =
                fs::read(host_inputs_path).context("reading the host-input record")?;
            writer.write(verify_run::HOST_INPUTS, &host_inputs)?;
        }
        writer.commit(&self.verify_run_record(
            comparison,
            exact_branch_counter,
            outcome,
            native_record,
        ))?;
        result
    }

    /// Prepare the executable's static adapter, then use the same core-owned
    /// saved writer. Native evidence keeps its raw payload and original wire
    /// metadata; Common runs cannot omit or acquire native evidence.
    fn verify_run_with_adapter<A: super::backend_hook::BackendAdapter>(
        &self,
        directory: &Path,
        global: &GlobalOpts,
        stdin: Option<fs::File>,
        adapter: &A,
        adapter_options: A::Options,
    ) -> Result<ExitStatus, Error> {
        use super::backend_hook::EvidenceRequirement;
        use super::backend_hook::Mode;
        anyhow::ensure!(
            self.application_backend == Some(A::NAME),
            "saved application execution requires its exact static adapter identity"
        );
        A::LOG_TRANSPORT.validate(A::EVIDENCE)?;
        let physical = super::backend_hook::PreparedPhysical::new(self, A::CAPABILITIES);
        let mut session = adapter.prepare(
            adapter_options,
            super::backend_hook::Prepare {
                options: self,
                global,
                mode: Mode::SavedVerification,
                stdin,
                physical: Some(&physical),
            },
        )?;
        self.verify_run_with_execution(
            directory,
            Some(A::LOG_TRANSPORT),
            A::EVIDENCE == EvidenceRequirement::NativeCounters,
            |options, log, path, forwarded| {
                adapter
                    .execute(
                        &mut session,
                        super::backend_hook::Execute {
                            mode: Mode::SavedVerification,
                            verification: Some(super::backend_hook::VerificationCapture {
                                log,
                                path,
                                run: VerificationRun::Run1,
                                options,
                                physical: Some(&physical),
                                requested_log: global.log,
                                log_budget: global.log_budget(),
                                log_transport: A::LOG_TRANSPORT,
                                forwarded_detlogs: forwarded,
                            }),
                        },
                    )?
                    .verification()
            },
            |pending, output| {
                let facts = adapter.finish(
                    pending,
                    super::backend_hook::Observation {
                        output,
                        run: VerificationRun::Run1,
                    },
                )?;
                match A::EVIDENCE {
                    EvidenceRequirement::Common => Ok(None),
                    EvidenceRequirement::NativeCounters => {
                        adapter.save_native_evidence(&facts).map(Some)
                    }
                }
            },
        )
    }

    /// The record a `--verify-run` execution of these options commits.
    pub(super) fn verify_run_record(
        &self,
        comparison: verify_run::RecordedComparison,
        exact_branch_counter: Option<hermit::canonical_verdict::BranchCounterVerdict>,
        outcome: verify_run::VerifyRunOutcome,
        dbt: Option<verify_run::DbtRunRecord>,
    ) -> verify_run::VerifyRunRecord {
        verify_run::VerifyRunRecord {
            schema: verify_run::SCHEMA,
            backend: self.execution_backend_name().to_string(),
            comparison,
            passthru_opt: self.det_opts.det_config.passthru_opt,
            epoch: self.epoch_rfc3339(),
            exact_branch_counter,
            dbt,
            role: verify_run::RunRole::Run,
            outcome,
        }
    }

    // Execution mode corresponding to `run --verify`:
    fn verify<A: super::backend_hook::BackendAdapter>(
        &self,
        global: &GlobalOpts,
        stdin: Option<fs::File>,
        adapter: &A,
        adapter_options: A::Options,
    ) -> Result<ExitStatus, Error> {
        // Stamp before fallible snapshot/preparation work, as in the original
        // physical path. Both runs retain this one ld.so.cache snapshot.
        if let Some(path) = &self.verify_json {
            write_pending_verification_json(path)?;
        }
        self.with_verification_ld_so_cache()?
            .verify_sharing_ld_so_cache(global, stdin, adapter, adapter_options)
    }

    pub(super) fn verify_sharing_ld_so_cache<A: super::backend_hook::BackendAdapter>(
        &self,
        global: &GlobalOpts,
        stdin: Option<fs::File>,
        adapter: &A,
        adapter_options: A::Options,
    ) -> Result<ExitStatus, Error> {
        #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
        let mapped_image = if self.in_guest_exec_stop {
            self.refuse_unready_exec_stop()?;
            Some(std::sync::Arc::new(
                super::mapped_first_admission::RetainedImage::open()?,
            ))
        } else {
            None
        };
        self.verify_with_retained_adapters(
            global,
            stdin,
            adapter,
            adapter_options,
            #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
            mapped_image,
        )
    }

    /// Run the common pair after its physical adapter and inputs are retained.
    fn verify_with_retained_adapters<A: super::backend_hook::BackendAdapter>(
        &self,
        global: &GlobalOpts,
        stdin: Option<fs::File>,
        adapter: &A,
        adapter_options: A::Options,
        #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))] mapped_image: Option<
            std::sync::Arc<super::mapped_first_admission::RetainedImage>,
        >,
    ) -> Result<ExitStatus, Error> {
        use super::backend_hook::EvidenceRequirement;
        use super::backend_hook::Mode;
        use super::backend_hook::PairEvidence;
        use super::backend_hook::PhysicalRun;

        // Admission inside the common pair owner and each container must use
        // this same retained package too. Resource paths may disappear after
        // opening; neither run may turn them into a second mutable input.
        #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
        let retained_options = mapped_image.as_ref().map(|image| {
            let mut options = self.clone();
            options.mapped_shared_image = Some(image.clone());
            options
        });
        #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
        let pair_options = retained_options.as_ref().unwrap_or(self);
        #[cfg(not(all(feature = "exec-stop-launcher", feature = "liteinst")))]
        let pair_options = self;
        let selected = self.application_backend.is_some();
        let requirement = if selected {
            A::EVIDENCE
        } else {
            EvidenceRequirement::Common
        };
        if selected {
            A::LOG_TRANSPORT.validate(A::EVIDENCE)?;
        }
        let transport = selected.then_some(A::LOG_TRANSPORT);
        let physical = self
            .application_backend
            .map(|_| super::backend_hook::PreparedPhysical::new(self, A::CAPABILITIES));
        let mut session = if selected {
            Some(adapter.prepare(
                adapter_options,
                super::backend_hook::Prepare {
                    options: self,
                    global,
                    mode: Mode::VerificationPair,
                    stdin,
                    physical: physical.as_ref(),
                },
            )?)
        } else {
            None
        };
        pair_options.verify_with_evidence(
            VerificationEvidence {
                requirement,
                records_run_metadata: !(selected
                    && requirement == EvidenceRequirement::NativeCounters),
                transport,
                native_identity_diagnostic: selected
                    .then_some(A::NATIVE_IDENTITY_DIAGNOSTIC)
                    .flatten(),
            },
            |options, log, path, forwarded, run| {
                if let Some(session) = &mut session {
                    let captured = adapter
                        .execute(
                            session,
                            super::backend_hook::Execute {
                                mode: Mode::VerificationPair,
                                verification: Some(super::backend_hook::VerificationCapture {
                                    log,
                                    path,
                                    run,
                                    options,
                                    physical: physical.as_ref(),
                                    requested_log: global.log,
                                    log_budget: global.log_budget(),
                                    log_transport: A::LOG_TRANSPORT,
                                    forwarded_detlogs: forwarded,
                                }),
                            },
                        )?
                        .verification()?;
                    return Ok(PhysicalRun {
                        output: captured.output,
                        skid_overshoots: captured.skid_overshoots,
                        latches: captured.latches,
                        terminal_outcome: None,
                        pending: Some(captured.pending),
                    });
                }
                #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
                if let Some(image) = mapped_image.as_ref() {
                    image.verify()?;
                }
                let result = options.run_verify(
                    (log, path),
                    forwarded,
                    global,
                    run,
                    #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
                    mapped_image.as_ref().map(|image| MappedVerificationRun {
                        image: image.clone(),
                        policy: super::mapped_first_admission::CapturePolicy::Full,
                    }),
                );
                #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
                let result = if let Some(image) = mapped_image.as_ref() {
                    // Check the same opened image after owned finalization on
                    // either outcome without replacing the execution error.
                    finish_mapped_verification_image(result, image.verify())
                } else {
                    result
                };
                let captured = result?;
                Ok(PhysicalRun {
                    output: captured.output,
                    skid_overshoots: captured.skid_overshoots,
                    latches: captured.latches,
                    terminal_outcome: captured.terminal_outcome,
                    pending: None,
                })
            },
            |pending, output, run| {
                pending
                    .map(|pending| {
                        adapter.finish(pending, super::backend_hook::Observation { output, run })
                    })
                    .transpose()
            },
            |first, second| match (first, second) {
                (Some(first), Some(second)) => adapter.pair_evidence(first, second),
                (None, None) => Ok(PairEvidence::Common),
                _ => Err(Error::msg(
                    "physical backend facts changed kind between runs",
                )),
            },
        )
    }

    // Existing injected physical-run tests use the same common owner and the
    // same default adapter completion. This is not a second comparison path.
    #[cfg(test)]
    pub(super) fn verify_with_execution(
        &self,
        mut execute: impl FnMut(
            &Self,
            fs::File,
            &Path,
            ForwardedDetlogPlan,
            VerificationRun,
        ) -> Result<VerificationRunOutput, Error>,
    ) -> Result<ExitStatus, Error> {
        use super::backend_hook::BackendAdapter;
        use super::backend_hook::EvidenceRequirement;
        let adapter = super::core_adapter::CoreApplicationAdapter;
        self.verify_with_evidence(
            VerificationEvidence {
                requirement: EvidenceRequirement::Common,
                records_run_metadata: true,
                transport: None,
                native_identity_diagnostic: None,
            },
            |options, log, path, forwarded, run| {
                execute(options, log, path, forwarded, run)
                    .map(VerificationRunOutput::into_physical)
            },
            |pending, output, run| {
                adapter.finish(pending, super::backend_hook::Observation { output, run })
            },
            |first, second| adapter.pair_evidence(first, second),
        )
    }

    /// The common two-run owner, after the invocation has been stamped and
    /// its physical adapter prepared. The callback returns a completed run;
    /// disposition, deferred facts, comparison and reporting stay here.
    pub(super) fn verify_with_evidence<P, F>(
        &self,
        evidence: VerificationEvidence,
        mut execute: impl FnMut(
            &Self,
            fs::File,
            &Path,
            ForwardedDetlogPlan,
            VerificationRun,
        ) -> Result<super::backend_hook::PhysicalRun<P>, Error>,
        finish: impl Fn(P, &Output, VerificationRun) -> Result<F, Error>,
        pair_evidence: impl Fn(&F, &F) -> Result<super::backend_hook::PairEvidence, Error>,
    ) -> Result<ExitStatus, Error> {
        let VerificationEvidence {
            requirement,
            records_run_metadata,
            transport,
            native_identity_diagnostic,
        } = evidence;
        self.refuse_unready_exec_stop()?;
        let retained_log_dir = self.retained_verify_log_dir()?;
        let (log1, log2) = temp_log_files_in("run1", "run2", retained_log_dir.as_deref())
            .context("Failed to create verification log files")?;

        let (log1_file, log1_path) = log1.into_parts();
        let (log2_file, log2_path) = log2.into_parts();
        let forwarded1_for_run = self.forwarded_detlog_plan_for_transport(transport)?;
        let forwarded2_for_run = self.forwarded_detlog_plan_for_transport(transport)?;
        let (forwarded1, forwarded2) = (
            forwarded1_for_run.requires_socket(),
            forwarded2_for_run.requires_socket(),
        );

        // Verification historically sent both executions to one --summary-json
        // path, so run 2 overwrote run 1. Keep the public path's run-2 meaning,
        // while capturing run 1 privately. When no public path was requested,
        // the same private file is emptied before run 2 and reused.
        //
        // The private file lives in the checkout because Hermit's isolated /tmp
        // is not the host /tmp. It is consequently guest-visible. Leaving run
        // 1's JSON in it while run 2 executes changes the guest's input: a guest
        // that reads the file sees zero bytes in run 1 and a completed summary
        // in run 2. Emptying it before run 2 preserves the initial contents.
        //
        // Both runs and every read below use the descriptor spelling, never
        // `summary1_file.path()`: that host name does not exist inside the
        // container when the working directory is under the host /tmp. See
        // private_summary_descriptor_path. `summary1_file` stays open, and so
        // the spelling stays valid, until this function returns.
        // The current physical DBT adapter produces typed native facts, not
        // RunSummary or host-input files. Do not read an old public summary or
        // create a new checkout-path dependency on that path's behalf.
        let summary1_file = records_run_metadata
            .then(private_verify_summary)
            .transpose()?;
        let summary1_path = summary1_file.as_ref().map(private_summary_descriptor_path);
        let summary2_path = records_run_metadata
            .then(|| self.summary_json.as_deref().or(summary1_path.as_deref()))
            .flatten();
        // Both runs bind one snapshot of the host's /etc/ld.so.cache: the
        // package manager can replace it between them, and the two loaders'
        // `fstat` of it would then differ (https://github.com/rrnewton/hermit/issues/4693).
        let ld_so_cache = self
            .with_verification_ld_so_cache()?
            .ld_so_cache_snapshot
            .expect("with_verification_ld_so_cache always sets the snapshot");
        let mut run1_options = self.clone();
        run1_options.summary_json = summary1_path.clone();
        run1_options.ld_so_cache_snapshot = Some(ld_so_cache.clone());
        let mut run2_options = self.clone();
        run2_options.ld_so_cache_snapshot = Some(ld_so_cache);
        run2_options.summary_json = summary2_path.map(Path::to_owned);

        // Each run records the host files its guest opens, so that a
        // divergence caused by a host file changing during a run can be named
        // (see `hermit::host_input_change`). Both runs share one unlinked file,
        // reached through the controller's descriptor as the private summary
        // is. Detcore writes it only when a run ends, and it is read and
        // emptied before run 2, so it is empty while either guest runs: a
        // guest that finds it through /proc sees the same thing in both runs.
        let host_inputs_file = records_run_metadata
            .then(tempfile::tempfile)
            .transpose()
            .context("creating the host-input record")?;
        let host_inputs_path = host_inputs_file.as_ref().map(descriptor_path);
        run1_options.host_input_log = host_inputs_path.clone();
        run2_options.host_input_log = host_inputs_path.clone();
        run1_options.fatal_core_run_label = Some("run1");
        run2_options.fatal_core_run_label = Some("run2");

        // Captured BEFORE run 1 so the same values can be put back before run 2.
        // See the restore call below for the measurement this exists for.
        let fd_flags_before_run1 = standard_fd_status_flags();

        crate::app::tracing::diagnostic_eprintln!(":: {}", "Run1...".yellow().bold());

        let super::backend_hook::PhysicalRun {
            output: out1,
            skid_overshoots: skid_overshoots_run1,
            latches: latches1,
            terminal_outcome: terminal_outcome1,
            pending: pending1,
        } = match run_verification_execution(
            self.verify_json.as_deref(),
            VerificationRun::Run1,
            || {
                execute(
                    &run1_options,
                    log1_file,
                    &log1_path,
                    forwarded1_for_run,
                    VerificationRun::Run1,
                )
            },
        ) {
            Ok(result) => result,
            Err(mut error) => {
                if let Some(overshoot) = error.downcast_ref::<SkidOvershootError>()
                    && let Some(path) = &self.verify_json
                {
                    let summary1 = summary1_path.as_deref().and_then(read_verify_summary);
                    if let Err(report_error) = write_skid_overshoot_without_comparison_json(
                        path,
                        overshoot.count(),
                        verification_runtime_from_summaries(summary1.as_ref(), None),
                        // `run_verify` returned only an Error, so this site has
                        // no guest ExitStatus to record.
                        None,
                    ) {
                        error = retain_verification_error(
                            error,
                            "publishing skid overshoot",
                            report_error,
                        );
                    }
                }
                error =
                    publish_run_failure(self.verify_json.as_deref(), VerificationRun::Run1, error);
                // A refused or failed first run names its reason only in its
                // own log (a policy refusal's reason is a Detcore tracing
                // event, and only the exit status leaves the container), so
                // --print-verify-logs echoes it here too, before retention
                // takes the log. Not after the log cap: that exit must stay
                // prompt, and the log is the one that crossed it.
                if self.print_verify_logs
                    && error.downcast_ref::<LogCapExceeded>().is_none()
                    && let Err(echo_error) = echo_first_run_log(&log1_path)
                {
                    error =
                        retain_verification_error(error, "echoing the first-run log", echo_error);
                }
                if self.keep_logs {
                    error = retain_logs_after_verification_error(error, [("run 1", log1_path)]);
                }
                return Err(error);
            }
        };
        let mut out1 = out1;
        let mut signal_reports = SignalTerminationReports::default();
        if !records_run_metadata {
            signal_reports.prepare("run 1", out1.status, &log1_path);
        }
        // The latches first: a determinism loss is named as one, even when its
        // loss notice also reached the forwarded records.
        if let Err(error) = latches1.refuse_comparison("run 1") {
            let error =
                publish_run_failure(self.verify_json.as_deref(), VerificationRun::Run1, error);
            if self.keep_logs {
                retain_verification_logs([("run 1", log1_path)])?;
            }
            return Err(error);
        }
        let forwarded_syscalls1 = match self.take_forwarded_detlogs_for_transport(
            &log1_path,
            &mut out1.stderr,
            forwarded1,
            transport,
        ) {
            Ok(count) => count,
            Err(error) => {
                if self.keep_logs {
                    retain_verification_logs([("run 1", log1_path)])?;
                }
                return Err(error);
            }
        };
        // Prepared now, with the forwarded records in place: the comparison
        // consumes the log. Written if the verification ends unverified.
        if records_run_metadata {
            signal_reports.prepare("run 1", out1.status, &log1_path);
        }

        if self.print_verify_logs {
            echo_first_run_log(&log1_path)?;
        }

        // A terminal outcome (a deadlock under Hermit's model, a virtual
        // timeout) passes `--verify-allow` like any other non-success status
        // (`rejected_first_run`): neither is a guest result.
        if let Some(error) = reject_first_run(
            self.verify_json.as_deref(),
            self.verify_allow,
            &out1,
            skid_overshoots_run1,
            summary1_path
                .as_deref()
                .and_then(read_verify_summary)
                .as_ref(),
        ) {
            let error = rejected_first_run(error, terminal_outcome1);
            // Queued here; `main` writes it after everything else.
            drop(signal_reports);
            if self.keep_logs {
                retain_verification_logs([("run 1", log1_path)])?;
            }
            return Err(error);
        }

        let facts1 = match finish(pending1, &out1, VerificationRun::Run1) {
            Ok(facts) => facts,
            Err(error) => {
                let mut error =
                    publish_run_failure(self.verify_json.as_deref(), VerificationRun::Run1, error);
                if self.keep_logs {
                    error = retain_logs_after_verification_error(error, [("run 1", log1_path)]);
                }
                return Err(error);
            }
        };

        let summary1 = summary1_path
            .as_deref()
            .map(take_verify_summary_before_next_run)
            .transpose()?
            .flatten();
        #[cfg(feature = "dbt")]
        let dbt_host_inputs1 = dbt1.as_ref().and_then(|facts| {
            facts.host_inputs.as_ref().map(|inputs| match inputs {
                Ok(inputs) => Ok(inputs.clone()),
                Err(error) => Err(std::io::Error::other(error.clone())),
            })
        });
        let host_inputs1 = host_inputs_path.as_deref().map(read_host_inputs);
        #[cfg(feature = "dbt")]
        let host_inputs1 = dbt_host_inputs1.or(host_inputs1);
        if let Some(file) = &host_inputs_file {
            file.set_len(0)
                .context("emptying the host-input record before run 2")?;
        }
        if skid_overshoots_run1 > 0
            && let Some(path) = &self.verify_json
        {
            write_skid_overshoot_without_comparison_json(
                path,
                skid_overshoots_run1,
                verification_runtime_from_summaries(summary1.as_ref(), None),
                Some(out1.status),
            )?;
        }

        // ⚠️ THE TWO RUNS MUST START FROM IDENTICAL fd STATE, AND WITHOUT THIS
        // THEY DO NOT. Both runs inherit hermit's OWN stderr, so a guest that
        // mutates its status flags leaves run 2 starting from state run 1
        // created -- and the comparison then reports the guest as
        // nondeterministic when the guest was identical both times.
        //
        // MEASURED 2026-08-26 on `hermit --backend kvm run --strict --verify --
        // /usr/bin/awk 'BEGIN { print 42 }'`: awk sets O_APPEND on fd 2 only
        // when it is not already set, so
        //     run 1  fcntl(2, F_GETFL) = 32769  -> fcntl(2, F_SETFL, 33793)
        //     run 2  fcntl(2, F_GETFL) = 33793  -> no SETFL at all
        // One extra syscall in run 1 (161 vs 160), which shifts every later
        // record by one and reported TWENTY mismatches for TWO real differences.
        //
        // ⚠️ TWO, NOT ONE. An earlier version of this comment said "drop that
        // single record and the sequences are byte-identical". The INBOUND
        // records are -- measured, 0 of 160 differ once run 1's `F_SETFL` is
        // removed -- but the `F_GETFL` RESULT differs as well, `Ok(32769)`
        // against `Ok(33793)`, because that is the flag word this defect is
        // about. Caught by `agent(hermit-dbg)`. The distinction matters: the
        // claim as written invited a reader to check inbound records only, find
        // them clean, and conclude the harness was blameless.
        //
        // The one-line demonstration, before this fix: `2>file` gave rc=125
        // and 20 mismatches; `2>>file` -- the same run with O_APPEND already
        // set, so awk skips the SETFL in BOTH runs -- gave rc=0 and 0
        // mismatches. One bit on one descriptor decided the verdict.
        //
        // ⚠️ RESTORE, DO NOT SANITISE. Clearing the flags outright would change
        // what the guest observes; this puts back exactly what run 1 was
        // handed, so run 2 sees the same starting state and nothing else moves.
        // Best-effort by construction: if the flags cannot be read or restored
        // we leave the descriptor alone rather than fail a verification over
        // housekeeping, and the comparison reports the divergence as before.
        restore_standard_fd_status_flags(fd_flags_before_run1);

        crate::app::tracing::diagnostic_eprintln!(":: {}", "Run2...".yellow().bold());
        let super::backend_hook::PhysicalRun {
            output: out2,
            skid_overshoots: skid_overshoots_run2,
            latches: latches2,
            terminal_outcome: terminal_outcome2,
            pending: pending2,
        } = match run_verification_execution(
            self.verify_json.as_deref(),
            VerificationRun::Run2,
            || {
                execute(
                    &run2_options,
                    log2_file,
                    &log2_path,
                    forwarded2_for_run,
                    VerificationRun::Run2,
                )
            },
        ) {
            Ok(result) => result,
            Err(mut error) => {
                if let Some(overshoot) = error.downcast_ref::<SkidOvershootError>()
                    && let Some(path) = &self.verify_json
                {
                    let summary2 = summary2_path.and_then(read_verify_summary);
                    let count = skid_overshoots_run1.saturating_add(overshoot.count());
                    if let Err(report_error) = write_skid_overshoot_without_comparison_json(
                        path,
                        count,
                        verification_runtime_from_summaries(summary1.as_ref(), summary2.as_ref()),
                        Some(out1.status),
                    ) {
                        error = retain_verification_error(
                            error,
                            "publishing skid overshoot",
                            report_error,
                        );
                    }
                }
                error =
                    publish_run_failure(self.verify_json.as_deref(), VerificationRun::Run2, error);
                if self.keep_logs {
                    error = retain_logs_after_verification_error(
                        error,
                        [("run 1", log1_path), ("run 2", log2_path)],
                    );
                }
                return Err(error);
            }
        };
        let mut out2 = out2;
        if !records_run_metadata {
            signal_reports.prepare("run 2", out2.status, &log2_path);
        }
        if let Err(error) = latches2.refuse_comparison("run 2") {
            let error =
                publish_run_failure(self.verify_json.as_deref(), VerificationRun::Run2, error);
            if self.keep_logs {
                retain_verification_logs([("run 1", log1_path), ("run 2", log2_path)])?;
            }
            return Err(error);
        }
        if let Some(forwarded_syscalls1) = forwarded_syscalls1 {
            let backend = "LiteInst";
            let forwarded_syscalls2 = match self
                .take_forwarded_detlogs_for_transport(
                    &log2_path,
                    &mut out2.stderr,
                    forwarded2,
                    transport,
                )
                .and_then(|count| {
                    count.context("run 2 forwarded no DETLOG records where run 1 did")
                }) {
                Ok(count) => count,
                Err(error) => {
                    if self.keep_logs {
                        retain_verification_logs([("run 1", log1_path), ("run 2", log2_path)])?;
                    }
                    return Err(error);
                }
            };
            if forwarded_syscalls1 == 0 || forwarded_syscalls2 == 0 {
                if self.keep_logs {
                    retain_verification_logs([("run 1", log1_path), ("run 2", log2_path)])?;
                }
                return Err(Error::msg(format!(
                    "{backend} verification captured no syscall DETLOG records: run1={forwarded_syscalls1}, run2={forwarded_syscalls2}"
                )));
            }
            eprintln!(
                ":: {backend} syscall DETLOG records included: run1={forwarded_syscalls1}, run2={forwarded_syscalls2}"
            );
        }

        if records_run_metadata {
            signal_reports.prepare("run 2", out2.status, &log2_path);
        }

        let facts2 = match finish(pending2, &out2, VerificationRun::Run2) {
            Ok(facts) => facts,
            Err(error) => {
                let mut error =
                    publish_run_failure(self.verify_json.as_deref(), VerificationRun::Run2, error);
                if self.keep_logs {
                    error = retain_logs_after_verification_error(
                        error,
                        [("run 1", log1_path), ("run 2", log2_path)],
                    );
                }
                return Err(error);
            }
        };
        let evidence = match pair_evidence(&facts1, &facts2)
            .and_then(|evidence| requirement.validate(evidence))
        {
            Ok(super::backend_hook::PairEvidence::Common) => None,
            Ok(super::backend_hook::PairEvidence::NativeCounters { first, second }) => {
                Some(super::verify::NativeEvidenceComparison::new(
                    self.execution_backend_name(),
                    &first,
                    &second,
                ))
            }
            Err(error) => {
                let mut error =
                    publish_run_failure(self.verify_json.as_deref(), VerificationRun::Run2, error);
                if self.keep_logs {
                    error = retain_logs_after_verification_error(
                        error,
                        [("run 1", log1_path), ("run 2", log2_path)],
                    );
                }
                return Err(error);
            }
        };

        let summary2 = summary2_path.and_then(read_verify_summary);
        let outcome = conclude_verification(
            VerificationConclusion {
                verify_json: self.verify_json.as_deref(),
                comparison_options: self.verification_comparison_options(),
                evidence,
                native_identity_diagnostic,
                backend: self.selected_backend(),
                application_backend: self.application_backend,
                passthru_opt: self.det_opts.det_config.passthru_opt,
                rewrite_exclusives: Some(self.rewrite_exclusives),
            },
            CompletedVerificationRun {
                output: &out1,
                log: log1_path,
                skid_overshoots: skid_overshoots_run1,
                summary: summary1,
                host_inputs: host_inputs1,
            },
            CompletedVerificationRun {
                output: &out2,
                log: log2_path,
                skid_overshoots: skid_overshoots_run2,
                summary: summary2,
                host_inputs: {
                    #[cfg(feature = "dbt")]
                    {
                        let dbt_inputs = dbt2.as_ref().and_then(|facts| {
                            facts.host_inputs.as_ref().map(|inputs| match inputs {
                                Ok(inputs) => Ok(inputs.clone()),
                                Err(error) => Err(std::io::Error::other(error.clone())),
                            })
                        });
                        dbt_inputs.or_else(|| host_inputs_path.as_deref().map(read_host_inputs))
                    }
                    #[cfg(not(feature = "dbt"))]
                    {
                        host_inputs_path.as_deref().map(read_host_inputs)
                    }
                },
            },
        )?;
        drop(host_inputs_file);

        // On divergence, still return the nonzero status and skip
        // the backend banner — but EMIT THE GUEST'S OUTPUT FIRST when both runs
        // agreed on it.
        //
        // ⚠️ THE HISTORICAL BEHAVIOUR DESTROYED THE EVIDENCE IT EXISTED TO
        // PRESENT. Dropping the output made a diverging run indistinguishable at
        // the CLI from a run that produced nothing, which is a far more alarming
        // and completely different failure. Measured 2026-08-25: a KVM
        // divergence on `awk 'BEGIN { print 42 }'` was investigated, escalated,
        // and reported to the project owner as "the guest produced no output".
        // The guest produced `42`; both runs produced `42`; the harness threw it
        // away. A diagnostic that hides the guest's behaviour misleads precisely
        // when someone is investigating, and it did.
        //
        // WHY EMITTING IS UNAMBIGUOUS HERE, and why this is narrow: the
        // comparator ALREADY prints a labelled diff of both runs whenever
        // stdout or stderr differ. So the only case reaching this point with
        // output to show is the one where BOTH RUNS AGREED — there is no
        // question of which to print, and nothing is being chosen on the
        // reader's behalf. Where they disagree the diff above is the report and
        // this stays silent, exactly as before.
        //
        // The exit status is unchanged, so anything keying on the process
        // result is unaffected; only the diagnostic gains the bytes it was
        // discarding.
        if !outcome.verified() {
            emit_compared_guest_output(false, &out1, &out2)?;
            return outcome.into_exit_status();
        }
        signal_reports.verified();
        let status = outcome.guest_status;

        let backend_banner = match self.selected_backend() {
            Backend::Kvm => Some("KVM (reverie-kvm KvmGuest<Detcore>)"),
            Backend::Liteinst | Backend::InGuestTrap | Backend::Ptrace => None,
        };
        if let Some(backend_banner) = backend_banner {
            eprintln!(":: Backend: {backend_banner}");
        }
        emit_compared_guest_output(true, &out1, &out2)?;
        // Both runs ended with the same terminal outcome at the same point,
        // which `main` reports by its class line and status. Only an explicit
        // `--verify-allow` of failure or both reaches here with one, and then
        // `verified` says the outcome repeats: for a deadlock, not that it is
        // native; for a virtual timeout, that both runs stopped at the same
        // turn.
        if let Some(outcome) = terminal_outcome1
            && terminal_outcome1 == terminal_outcome2
        {
            return Err(Error::new(SchedulerOutcome(outcome)));
        }
        Ok(status)
    }

    /// The `--bind`s the DBT adapter checks, as (source, target) pairs: those
    /// [`Self::mounts`] applies for every backend ([`bind_is_applied`]).
    /// It warns about any other bind, which has no effect.
    pub(super) fn adapter_binds(&self) -> Vec<(PathBuf, PathBuf)> {
        let private_dev_shm = self.has_private_dev_shm();
        self.bind
            .iter()
            .filter(|bind| bind_is_applied(bind, private_dev_shm))
            .map(|bind| {
                let (source, target) = bind_paths(bind);
                (source.to_path_buf(), target.to_path_buf())
            })
            .collect()
    }

    /// These options with the DBT adapter's own container inputs added after
    /// the caller's, only for this physical launch.
    pub(super) fn with_adapter_mounts(&self, binds: &[Bind], mounts: Vec<MountArg>) -> Self {
        let mut options = self.clone();
        options.backend_mounts.0 += mounts.len();
        options.backend_mounts.1 += binds.len();
        options.bind.extend_from_slice(binds);
        options.mount.extend(mounts);
        options
    }

    /// These options with `mount` added after the caller's own, as one of
    /// them (shown in the guest's mount table): the DBT adapter's tmpfs at
    /// the test workdir, which the guest uses as its working directory.
    pub(super) fn with_guest_mount(&self, mount: MountArg) -> Self {
        let mut options = self.clone();
        options.mount.push(mount);
        options
    }

    /// Whether the caller's `--mount`s install a fresh tmpfs at guest path
    /// `target` (a mount of type tmpfs: setting that type changes nothing).
    pub(super) fn mounts_tmpfs_at(&self, target: &Path) -> bool {
        self.mount
            .iter()
            .map(|arg| &arg.mount)
            .any(|mount| mount.get_target() == target && mount.clone().fstype("tmpfs") == *mount)
    }

    /// Take the `/etc/ld.so.cache` role the `hermit verify-inputs` holder
    /// sealed, if it sealed one, as this run's snapshot: both runs of the pair
    /// then bind the same bytes whatever the host's package manager does
    /// between them (https://github.com/rrnewton/hermit/issues/4693).
    fn adopt_shared_ld_so_cache(
        &mut self,
        adopted: &super::verify_inputs::AdoptedInputs,
    ) -> Result<(), Error> {
        if let Some((bytes, origin)) =
            adopted.read_role(LD_SO_CACHE_ROLE, Path::new(LD_SO_CACHE_PATH))?
        {
            self.ld_so_cache_snapshot = Some(super::container::LdSoCacheSnapshot::from_captured(
                bytes, origin,
            ));
        }
        Ok(())
    }

    /// Whether these options carry the `/etc/ld.so.cache` snapshot both runs
    /// of a verification bind.
    pub(super) fn has_ld_so_cache_snapshot(&self) -> bool {
        self.ld_so_cache_snapshot.is_some()
    }

    /// These options with the `/etc/ld.so.cache` snapshot both runs of an
    /// in-process verification bind: the one already taken, else the host's
    /// cache now.
    fn with_verification_ld_so_cache(&self) -> Result<Self, Error> {
        let mut shared = self.clone();
        if shared.ld_so_cache_snapshot.is_none() {
            shared.ld_so_cache_snapshot = Some(super::container::LdSoCacheSnapshot::of_host()?);
        }
        Ok(shared)
    }

    /// The guest paths at which the caller's mounts can cover `/dev/shm`:
    /// every `--mount` target, and a `--bind` at `/dev/shm` itself.
    fn dev_shm_covering_targets(&self) -> impl Iterator<Item = &Path> {
        self.mount.iter().map(|arg| arg.mount.get_target()).chain(
            self.bind
                .iter()
                .map(|bind| bind_paths(bind).1)
                .filter(|target| is_dev_shm(target)),
        )
    }

    /// Whether the guest's `/dev/shm` is the container's private one
    /// ([`private_dev_shm`]), which hides the host's.
    fn has_private_dev_shm(&self) -> bool {
        !self.no_namespace
            && self.image.is_none()
            && private_dev_shm(self.dev_shm_covering_targets()).is_some()
    }

    /// Returns the mounts to be used with the container, in the order they are
    /// installed: the identity-hardening mounts; the caller's `--mount`s and
    /// `--bind`s not below `/dev/shm` (a backend adapter's own last, marked as
    /// a backend's); the e9patch overlay (a backend's own); the private `/tmp`
    /// here only when its directory is in the host's `/dev/shm`; the private
    /// `/dev/shm`; the caller's mounts and binds below it; the private `/tmp`;
    /// and the private `/dev/mqueue`.
    fn mounts(&self, tmpfs: &Path) -> Result<PreparedMounts, Error> {
        self.mounts_on(tmpfs, HostLayout::of_host())
    }

    /// [`Self::mounts`] on a host with `layout`. A host without `/tmp`
    /// (Android) gets no private `/tmp`, since there is no host `/tmp` for it
    /// to hide and nothing to mount it onto; a mount or bind the caller aimed
    /// into `/tmp` is refused there, naming why.
    fn mounts_on(&self, tmpfs: &Path, layout: HostLayout) -> Result<PreparedMounts, Error> {
        if !layout.tmp {
            if let Some(tmp) = &self.tmp {
                if self.strace_only {
                    anyhow::bail!(
                        "--strace-only shares the host's {TMP_DIR} with the guest, and this host \
                         has none"
                    );
                }
                anyhow::bail!(
                    "this host has no {TMP_DIR}, so there is none for --tmp {} to go over",
                    tmp.display()
                );
            }
            let backend_mounts_from = self.mount.len().saturating_sub(self.backend_mounts.0);
            let backend_binds_from = self.bind.len().saturating_sub(self.backend_mounts.1);
            let into_tmp = self
                .mount
                .iter()
                .enumerate()
                .map(|(index, mount)| {
                    (
                        mount.get_target().to_path_buf(),
                        index >= backend_mounts_from,
                    )
                })
                .chain(self.bind.iter().enumerate().map(|(index, bind)| {
                    (
                        PathBuf::from(OsStr::from_bytes(bind.target.to_bytes())),
                        index >= backend_binds_from,
                    )
                }))
                .find(|(target, _)| target.starts_with(TMP_DIR));
            match into_tmp {
                Some((target, true)) => anyhow::bail!(
                    "this host has no {TMP_DIR}, where the {} backend places {}: that backend \
                     cannot run here",
                    self.runtime_backend().as_str(),
                    target.display()
                ),
                Some((target, false)) => anyhow::bail!(
                    "this host has no {TMP_DIR}, so Hermit gives the guest none, and cannot place \
                     {} there. Choose a target outside {TMP_DIR}.",
                    target.display()
                ),
                None => {}
            }
        }
        // Each run of an in-process `--verify` must bind the one snapshot its
        // owner took for both; reading the host here would let a cache
        // replaced between the runs reach only one of them
        // (https://github.com/rrnewton/hermit/issues/4693).
        anyhow::ensure!(
            !matches!(self.fatal_core_run_label, Some("run1" | "run2"))
                || self.ld_so_cache_snapshot.is_some(),
            "verification run {} has no shared /etc/ld.so.cache snapshot",
            self.fatal_core_run_label.unwrap_or_default()
        );
        // And a `--verify-run` whose holder sealed the cache binds that copy,
        // never the host's (`adopt_shared_ld_so_cache`).
        anyhow::ensure!(
            self.ld_so_cache_snapshot.is_some()
                || !self
                    .shared_inputs
                    .as_ref()
                    .is_some_and(|inputs| inputs.has_role(LD_SO_CACHE_ROLE)),
            "the shared inputs seal /etc/ld.so.cache, but this run did not adopt it"
        );
        let (mut identity_mounts, identity_sources) =
            identity_hardening_mounts_on(layout, self.ld_so_cache_snapshot.as_ref())?;
        let mut identity_sources = identity_sources;
        let mut explicit_tmp_mount = false;
        let mut user_mounts = Vec::with_capacity(self.mount.len() + self.bind.len());
        let backend_mounts_from = self.mount.len().saturating_sub(self.backend_mounts.0);
        let backend_binds_from = self.bind.len().saturating_sub(self.backend_mounts.1);
        let planned = |mount: Mount, backend: bool| {
            if backend {
                PlannedMount::backend(mount)
            } else {
                PlannedMount::hermit(mount)
            }
        };
        let dev_shm = private_dev_shm(self.dev_shm_covering_targets());
        // The caller's mounts below `/dev/shm`, installed into the private one.
        let mut into_dev_shm = Vec::new();
        // The private /tmp is bound before the private `/dev/shm` when its own
        // directory is in the host's `/dev/shm` (`$TMPDIR` or `--tmp` there),
        // and then covers host /tmp before the binds into `/dev/shm`.
        let tmp_first = is_in_dev_shm(tmpfs);
        let refuse_into_dev_shm = |mount: &Mount| -> Result<(), Error> {
            refuse_bind_from_host_dev_shm(mount)?;
            if tmp_first {
                refuse_covered_bind_source(
                    mount,
                    Path::new(TMP_DIR),
                    "the guest's private /tmp is kept in host /dev/shm (TMPDIR or --tmp), so it \
                     covers host /tmp before the bind is made; bind the source from elsewhere, \
                     or keep the private /tmp outside /dev/shm",
                )?;
            }
            Ok(())
        };

        for (index, mount) in self.mount.iter().map(|arg| &arg.mount).enumerate() {
            let backend = index >= backend_mounts_from;
            let user_target = mount.get_target();
            if dev_shm.is_some() && is_below_dev_shm(user_target) {
                refuse_into_dev_shm(mount)?;
                if mount.get_source().is_some() && super::container::is_bind_mount(mount) {
                    identity_sources.add_user_bind(user_target, None);
                }
                into_dev_shm.push(planned(mount.clone().touch_target(), backend));
                continue;
            }
            identity_sources.discard_mounts_shadowed_by(&mut identity_mounts, user_target);
            let staging = mount
                .get_target()
                .strip_prefix(TMP_DIR)
                .ok()
                .map(|path| tmpfs.join(path));
            if mount.get_source().is_some() && super::container::is_bind_mount(mount) {
                identity_sources.add_user_bind(user_target, staging.as_deref());
            }
            if let Ok(path) = mount.get_target().strip_prefix(TMP_DIR) {
                explicit_tmp_mount |= path.as_os_str().is_empty();
                // If the target is in /tmp, change it so it goes to our
                // temporary /tmp instead.
                user_mounts.push(planned(
                    mount.clone().target(tmpfs.join(path)).touch_target(),
                    backend,
                ));
            } else {
                user_mounts.push(planned(mount.clone(), backend));
            }
        }

        for (index, bind) in self.bind.iter().enumerate() {
            let backend = index >= backend_binds_from;
            let mount = Mount::from(bind.clone()).rshared();

            // Bind mounts only make sense in the directories the container
            // replaces: `/tmp`, staged in the private tmp directory, and
            // `/dev/shm`, whose private tmpfs they go into.
            if !bind_is_applied(bind, dev_shm.is_some()) {
                if !keeps_host_dev_shm(bind) {
                    warn_unapplied_bind(bind);
                }
            } else if let Ok(relative_path) = mount.get_target().strip_prefix(TMP_DIR) {
                // Discard provenance only for a bind we actually retain. An
                // ignored outside-/tmp bind must not suppress the real
                // identity-hardening mount at that target.
                explicit_tmp_mount |= relative_path.as_os_str().is_empty();
                identity_sources
                    .discard_mounts_shadowed_by(&mut identity_mounts, mount.get_target());
                let target = tmpfs.join(relative_path);
                identity_sources.add_user_bind(mount.get_target(), Some(&target));
                user_mounts.push(planned(mount.target(target).touch_target(), backend));
            } else if is_dev_shm(mount.get_target()) {
                // Another host directory as the guest's `/dev/shm`.
                identity_sources.add_user_bind(mount.get_target(), None);
                user_mounts.push(planned(mount, backend));
            } else {
                refuse_into_dev_shm(&mount)?;
                identity_sources.add_user_bind(mount.get_target(), None);
                into_dev_shm.push(planned(mount.touch_target(), backend));
            }
        }

        if self.tmp.is_none() && layout.tmp {
            if explicit_tmp_mount {
                // The user mount or bind is installed on the generated staging
                // path, then exposed at `/tmp` by the final bind below. Preserve
                // the user's root and translate only that internal mountpoint.
                identity_sources.add_translated_tmp_mountpoint(tmpfs);
            } else {
                // Add this after explicit mounts. A user mount at `/` shadows
                // group/nscd, but the final `/tmp` bind below is later and
                // remains active, so its provenance must survive.
                identity_sources.add_private_tmp(tmpfs)?;
            }
        }

        let mut mounts = identity_mounts
            .into_iter()
            .map(PlannedMount::hermit)
            .collect::<Vec<_>>();
        mounts.extend(user_mounts);

        for overlay in self
            .exclusive_rewrite_overlays
            .iter()
            .chain(self.executable_overlay.iter())
        {
            let target = if let Ok(relative_path) = overlay.target.strip_prefix(TMP_DIR) {
                tmpfs.join(relative_path)
            } else {
                overlay.target.clone()
            };
            mounts.push(PlannedMount::backend(
                Mount::bind(&overlay.source, &target)
                    .readonly()
                    .touch_target(),
            ));
            mounts.push(PlannedMount::backend(Mount::new(target).flags(
                MountFlags::MS_BIND | MountFlags::MS_REMOUNT | MountFlags::MS_RDONLY,
            )));
        }
        // Bind the /tmp/tmpXXXXXX tmpfs mount over /tmp to hide it. This way,
        // we still preserve the files or directories bind-mounted inside of it
        // while hiding the real /tmp. A host without /tmp has none to hide,
        // and nothing to mount onto (refused above for a caller's mount).
        let mut private_tmp = if layout.tmp {
            Some(PlannedMount::hermit(Mount::bind(tmpfs, TMP_DIR).rshared()))
        } else {
            tracing::info!(
                "[hermit] this host has no {TMP_DIR}, so the guest has none either: Hermit's \
                 private {TMP_DIR} goes over the host's"
            );
            None
        };
        // The private `/dev/shm` follows every other mount sourced from a host
        // path, so that a source in the host's `/dev/shm` is still the host's,
        // and precedes the mounts that go into it. It precedes the private
        // /tmp too, so that a bind into it can come from host /tmp, unless the
        // private /tmp's own directory is in the host's `/dev/shm`.
        if tmp_first {
            mounts.extend(private_tmp.take());
        }
        mounts.extend(dev_shm.map(PlannedMount::hermit));
        mounts.extend(into_dev_shm);
        mounts.extend(private_tmp);
        mounts.extend(
            private_mqueue(self.mount.iter().map(|arg| arg.mount.get_target()))?
                .map(PlannedMount::hermit),
        );

        Ok(PreparedMounts {
            mounts,
            identity_sources,
        })
    }

    pub(super) fn with_adapter_capabilities(
        &self,
        capabilities: reverie::BackendCapabilities,
    ) -> Self {
        let mut options = self.clone();
        options.adapter_capabilities = Some(capabilities);
        options
    }

    /// Apply only the common owner's per-run provenance sink. This does not
    /// reobserve the terminal or change the prepared physical capabilities.
    pub(super) fn apply_run_metadata(&self, config: &mut DetConfig) {
        // Host inputs are recorded for `--verify` (`host_input_log`), and for
        // every run whose output a later run reproduces or is compared with:
        // a recorded schedule keeps the run's host-input set, a schedule
        // replay compares its opens against the recorded one, and a summary
        // reports the set (`detcore_model::host_input::HostInputSet`). An
        // application backend's runtime owns where its host-input records go,
        // so only its explicit requests apply here.
        config.record_host_inputs = self.record_host_inputs
            || self.host_input_log.is_some()
            || (self.application_backend.is_none()
                && (config.record_preemptions_to.is_some()
                    || config.replay_preemptions_from.is_some()
                    || config.replay_schedule_from.is_some()
                    || self.summary_json.is_some()));
        config.host_input_log = self.host_input_log.clone();
    }

    pub(super) fn mounts_directory_placeholder(&self) -> bool {
        let capabilities = self
            .adapter_capabilities
            .unwrap_or_else(|| hermit::backend_capabilities(self.runtime_backend()));
        detcore::needs_directory_placeholder(&capabilities)
    }

    /// Returns a configured container to run a function in.
    ///
    /// The container's setup makes its namespaces, (for `--network=local`)
    /// its sysfs, and then its mount plan, the container's `/proc` first,
    /// which the container's init records for the guest's container mount
    /// table (`IdentityGuard::record_plan`). A native exec domain's container
    /// follows the plan with its declared-input mounts, and has no table.
    fn container(&self, tmpfs: &Path) -> Result<(Container, IdentityGuard), Error> {
        // PROTOTYPE: when an OCI image is requested, the guest runs against the
        // image's materialized rootfs (deterministic file inputs) rather than
        // the host filesystem. This replaces the default namespace+mounts setup
        // with a chroot into the pinned image root.
        // Detcore's placeholder directory, only where the guest's procfs is
        // not the kernel's (`detcore::needs_directory_placeholder`): where it
        // is, Detcore serves its `/proc` directories on that procfs's root,
        // and no tmpfs of Hermit's is there for a guest to reach.
        let mounts_placeholder = self.mounts_directory_placeholder();
        if let Some(image) = &self.image {
            let rootfs = crate::app::image::materialize_rootfs(image)?;
            let workdir_binds: Vec<(PathBuf, PathBuf)> = self
                .bind
                .iter()
                .map(|bind| {
                    use std::os::unix::ffi::OsStringExt as _;
                    (
                        PathBuf::from(std::ffi::OsString::from_vec(
                            bind.source.as_bytes().to_vec(),
                        )),
                        PathBuf::from(std::ffi::OsString::from_vec(
                            bind.target.as_bytes().to_vec(),
                        )),
                    )
                })
                .collect();
            for (_, target) in &workdir_binds {
                let rel = target.strip_prefix("/").unwrap_or(target);
                let _ = std::fs::create_dir_all(rootfs.join(rel));
            }
            let (mut container, identity_sources) = image_container(
                &rootfs,
                tmpfs,
                self.pin_threads,
                self.det_opts.det_config.cpu_profile_source,
                self.tmp.is_none(),
                mounts_placeholder,
                crate::app::container::ImageContainerOpts {
                    bind_kvm: self.runtime_backend() == Backend::Kvm,
                    workdir_binds: &workdir_binds,
                },
            )?;
            self.network
                .configure(&mut container, self.analyze_networking);
            return Ok((container, identity_sources));
        }

        let mut container = namespace_container(
            self.pin_threads,
            self.det_opts.det_config.cpu_profile_source,
        );
        self.network
            .configure(&mut container, self.analyze_networking);

        let PreparedMounts {
            mounts,
            mut identity_sources,
        } = self.mounts(tmpfs)?;
        // Detcore's placeholder directory, where the backend needs one, right
        // after the container's own `/proc`, which holds its mount point,
        // unless a later mount of the plan covers it, as the setup resolves
        // each target in turn (a `--mount` at `/proc/fs` or the directory
        // itself, by any spelling that resolves there:
        // `container::later_mount_covers`): then Detcore serves no
        // placeholder directory, as without a container. The container's init
        // checks again once every mount is in
        // (`IdentityGuard::confirm_directory_placeholder`).
        let placeholder = PathBuf::from(super::container::DIRECTORY_PLACEHOLDER);
        let mut plan = vec![PlannedMount::hermit(container_proc_mount())];
        if mounts_placeholder {
            plan.push(PlannedMount::placeholder(
                super::container::directory_placeholder_mount(&placeholder),
            ));
        }
        plan.extend(mounts);
        let placeholder = if !mounts_placeholder {
            None
        } else if super::container::later_mount_covers(&plan, 1) {
            plan.remove(1);
            None
        } else {
            Some(placeholder)
        };
        identity_sources.set_plan(&mut container, plan)?;
        identity_sources.set_directory_placeholder(placeholder);
        #[cfg(feature = "native-exec")]
        if self.native_exec_capture.is_some() {
            identity_sources.forget_plan();
        }

        Ok((container, identity_sources))
    }

    fn run_verify(
        &self,
        log: (fs::File, &Path),
        forwarded_detlogs: ForwardedDetlogPlan,
        global: &GlobalOpts,
        _run: VerificationRun,
        #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))] mapped: Option<
            MappedVerificationRun,
        >,
    ) -> Result<VerificationRunOutput, Error> {
        let (log_file, _log_path) = log;
        #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
        anyhow::ensure!(
            self.in_guest_exec_stop == mapped.is_some(),
            "mapped verification requires its retained image context"
        );
        let mut options = self.clone();
        options.capture_launch_namespace_mounts();
        options.det_opts.det_config.annotated_priority_replay =
            options.annotated_priority_replay_admission()?;
        // Each run counts its store-exclusives (aarch64), so that a
        // divergence a retried load/store-exclusive loop explains is named
        // (`conclude_verification`).
        options.det_opts.det_config.count_store_exclusives = true;
        let network_trace = options.open_network_trace()?;
        let global = global.clone();
        let resources = format!(
            "verification log{}{} descriptors",
            if network_trace.is_some() {
                ", network trace"
            } else {
                ""
            },
            if forwarded_detlogs.requires_socket() {
                ", forwarded DETLOG"
            } else {
                ""
            },
        );
        #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
        let guards = (Some(log_file), network_trace, forwarded_detlogs, mapped);
        #[cfg(not(all(feature = "exec-stop-launcher", feature = "liteinst")))]
        let guards = (Some(log_file), network_trace, forwarded_detlogs);
        self.launch_in_container(guards, &resources, None, move |guards, identity, native| {
            #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
            let (log, _, forwarded, mapped) = guards;
            #[cfg(not(all(feature = "exec-stop-launcher", feature = "liteinst")))]
            let (log, _, forwarded) = guards;
            options.run_verify_in_container(
                log,
                forwarded,
                &global,
                identity,
                native,
                #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
                mapped.as_ref(),
            )
        })
        .map(
            |((output, skid_overshoots, latches, terminal_outcome), _guards)| {
                VerificationRunOutput {
                    output,
                    skid_overshoots,
                    latches,
                    terminal_outcome,
                }
            },
        )
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

    pub(super) fn guest_command(&self) -> Result<Command, Error> {
        self.build_guest_command(true)
    }

    fn namespace_only_guest_command(&self) -> Result<Command, Error> {
        // Namespace-only bypasses instrumentation, so preserve the caller's
        // sanitizer environment instead of installing the leak-disable overrides.
        self.build_guest_command(false)
    }

    /// The guest's working directory as `--workdir` names it, if given
    /// (relative to Hermit's own, as the guest command takes it).
    pub(super) fn launch_workdir(&self) -> Option<&Path> {
        self.workdir.as_deref().map(Path::new)
    }

    fn build_guest_command(
        &self,
        disable_sanitizer_leak_detection_for_guest: bool,
    ) -> Result<Command, Error> {
        let program = self
            .prepared_program
            .as_deref()
            .unwrap_or_else(|| self.program_path());
        let mut command = Command::new(program);
        command.args(&self.args);
        // The guest's first process has the base ID; Detcore numbers the rest.
        if let Some(base) = self.det_opts.det_config.guest_ids.base() {
            command.pid(reverie::process::Pid::from_raw(base));
        }
        if self.prepared_program.is_some() {
            command.arg0(self.program_path());
        }
        // PROTOTYPE (--image): the guest lives inside the OCI rootfs, so the
        // guest working directory must be one that exists under the image root.
        // Default it to the image's declared `WorkingDir`, else `/` (a bare
        // chroot leaves cwd pointing at the — now unreachable — host cwd, which
        // makes `getcwd` fail inside the guest). An explicit `--workdir` wins.
        if let Some(current_dir) = &self.workdir {
            command.current_dir(current_dir);
        } else if let Some(image) = &self.image {
            let cfg = crate::app::image::read_image_config(image)?;
            command.current_dir(cfg.workdir.as_deref().unwrap_or("/"));
        }

        // PROTOTYPE (--image): an OCI rootfs is a self-contained filesystem, so
        // leaking the host environment (a host `PATH` full of paths absent from
        // the image) both breaks usability and undermines determinism. Instead
        // start from the `--base-env=minimal` defaults, apply the image's *own*
        // declared `Env` over them — pinned by the image digest, hence
        // deterministic — so each value the image declares replaces the
        // default it names, then merge user `--env` on top.
        if let Some(image) = &self.image {
            let cfg = crate::app::image::read_image_config(image)?;
            apply_image_base_environment(&mut command, &cfg.env);
            self.merge_from_env_settings(&mut command)?;
            return Ok(command);
        }

        apply_base_and_explicit_environment(&mut command, &self.base_env, &self.env)?;
        if disable_sanitizer_leak_detection_for_guest {
            disable_sanitizer_leak_detection(&mut command);
        }

        Ok(command)
    }

    /// Whether this invocation asked for `--save-config`.
    pub(crate) fn saves_config(&self) -> bool {
        self.save_config.is_some()
    }

    /// The run config option this invocation gave, if any, for the commands
    /// that embed `run`'s options but cannot honour them.
    pub(crate) fn run_config_option(&self) -> Option<&'static str> {
        if self.config.is_some() {
            Some("--config")
        } else if self.save_config.is_some() {
            Some("--save-config")
        } else {
            None
        }
    }

    /// The top-level parser replaces `hermit run --config FILE` with the
    /// file's options before parsing (`crate::app::run_config::expand_argv`), so a
    /// `--config` still present here was never applied.
    fn refuse_unapplied_config(&self) -> Result<(), Error> {
        match &self.config {
            Some(path) => Err(Error::msg(format!(
                "--config {} was not applied: only a top-level `hermit run` invocation loads a \
                 run config. Run `hermit [GLOBAL OPTIONS] run --config {} [OPTIONS]`.",
                path.display(),
                path.display()
            ))),
            None => Ok(()),
        }
    }

    /// Hands over the options the top-level parser captured for
    /// `--save-config`.
    pub(crate) fn set_config_capture(&mut self, config: crate::app::run_config::RunConfig) {
        self.config_capture = Some(config);
    }

    /// Records that `--cpu-profile auto` resolved to `profile`, as
    /// [`Self::resolve_auto_cpu_profile`] does from the program, for tests
    /// of what a resolved run saves.
    #[cfg(test)]
    pub(crate) fn set_auto_resolution(&mut self, profile: detcore_model::cpu_profile::CpuProfile) {
        self.auto_resolution = Some(format!("--cpu-profile auto resolved to {profile}"));
        self.det_opts.det_config.cpu_profile = profile;
    }

    /// Retain source identity independently of YAML capture, which may fail
    /// for a non-UTF-8 argument and does not run for embedded OCI commands.
    pub(crate) fn capture_max_timeslice_source(&mut self, matches: &clap::ArgMatches) {
        self.max_timeslice_explicit =
            matches.value_source("max_timeslice") != Some(clap::parser::ValueSource::DefaultValue);
    }

    /// Resolve the maximum a non-chaos analysis replay will use. Keep this
    /// on the same resolution path as launch validation: a range supplies
    /// its MAX and a seeded draw supplies its non-chaos fallback.
    pub(crate) fn analysis_replay_max_timeslice(
        &mut self,
    ) -> Result<detcore_model::config::MaybeTimeslice, Error> {
        self.det_opts.det_config.chaos = false;
        self.resolve_chaos_timeslice_range()?;
        self.resolve_seeded_chaos_timeslice()?;
        Ok(self.det_opts.det_config.max_timeslice)
    }

    /// Freeze an analysis prefix trial's chosen continuation. The prefix
    /// record carries the matching maximum; retaining a range or seeded draw
    /// here would select a different maximum when launch validates again.
    pub(crate) fn set_analysis_continuation_timeslice(
        &mut self,
        continuation: detcore_model::config::MaybeTimeslice,
    ) {
        self.chaos_seeded_timeslice = false;
        self.max_timeslice_explicit = true;
        self.chaos_timeslice_adopted = false;
        let config = &mut self.det_opts.det_config;
        config.chaos = false;
        config.max_timeslice = continuation;
        config.chaos_timeslice = None;
        config.chaos_timeslice_range = None;
        config.chaos_timeslice_draw = detcore_model::config::ChaosTimesliceDraw::Slice;
    }

    /// The maximum timeslice `--chaos-seeded-timeslice` drew from the
    /// scheduler seed, or `None` when this run does not draw one: the flag is
    /// off, the run is not in chaos mode or replays preemptions or a schedule
    /// (the recording supplies its preemption points), or the maximum is
    /// disabled because the host cannot arm the timer.
    fn seeded_chaos_timeslice(&self) -> Option<NonZeroU64> {
        let config = &self.det_opts.det_config;
        let draws = self.chaos_seeded_timeslice
            && config.chaos
            && config.replay_preemptions_from.is_none()
            && config.replay_schedule_from.is_none()
            && config.max_timeslice.is_some();
        draws.then(|| {
            hermit::chaos_timeslice::seeded_max_timeslice(
                config.sched_seed(),
                config.minimum_max_timeslice_nanos(),
            )
        })
    }

    /// Lower `--chaos-seeded-timeslice` to `--max-timeslice`: the value drawn
    /// from the scheduler seed, or else the default the flag stands in for,
    /// raised to one RCB where that is longer
    /// ([`hermit::chaos_timeslice::seeded_fallback_max_timeslice`]).
    /// Validation runs again whenever a caller changes the options, as `hermit
    /// analyze` does when it gives a trial a new seed, turns chaos off for its
    /// baseline or replays a trial's preemptions, so this derives the value
    /// afresh from the current options every time rather than from what an
    /// earlier call stored. A disabled maximum, given or set because the host
    /// cannot arm the timer, leaves nothing to draw and stays disabled.
    fn resolve_seeded_chaos_timeslice(&mut self) -> Result<(), Error> {
        if !self.chaos_seeded_timeslice {
            return Ok(());
        }
        // `--chaos`, which the flag requires, already refuses `--namespace-only`.
        if !self.backend_arms_pmu_preemption_timer() {
            anyhow::bail!(
                "--chaos-seeded-timeslice draws the maximum timeslice that the PMU timer \
                 enforces, and --backend={} has no such timer: run it on --backend=ptrace, or \
                 give chaos a fixed cooperative deadline with --target-timeslice=N",
                self.execution_backend_name()
            );
        }
        let config = &self.det_opts.det_config;
        if self.max_timeslice_explicit
            && let Some(given) = config.max_timeslice
        {
            anyhow::bail!(
                "--chaos-seeded-timeslice draws --max-timeslice from the scheduler seed, so it \
                 cannot be combined with --max-timeslice={given}: drop --max-timeslice to draw \
                 the value from the seed, or drop --chaos-seeded-timeslice to keep \
                 --max-timeslice={given} for every seed"
            );
        }
        if let Some(target) = config.target_timeslice {
            anyhow::bail!(
                "--chaos-seeded-timeslice draws --max-timeslice from the scheduler seed, but \
                 --target-timeslice={target} would set the mean of chaos's per-slice draw \
                 instead: drop --target-timeslice to draw the maximum from the seed, or drop \
                 --chaos-seeded-timeslice to keep --target-timeslice={target}"
            );
        }
        if self.det_opts.det_config.max_timeslice.is_some() {
            let fallback = hermit::chaos_timeslice::seeded_fallback_max_timeslice(
                self.det_opts.det_config.minimum_max_timeslice_nanos(),
            );
            self.det_opts.det_config.max_timeslice =
                Some(self.seeded_chaos_timeslice().unwrap_or(fallback));
        }
        Ok(())
    }

    /// Decide the run's `--max-timeslice` under `--chaos-timeslice-range`, or
    /// when a replayed record states the one its run drew
    /// (`PreemptionRecord::chaos_timeslice`).
    ///
    /// A replay takes the recorded timeslice: from a range it gives, which it
    /// does not draw from again, or in place of the default maximum when it
    /// gives none. An explicit `--max-timeslice` that differs is refused, as an
    /// explicit epoch that differs from the recorded one is. Otherwise a range
    /// sets the maximum to its MAX, from which each slice draws its mean
    /// (`--chaos-timeslice-draw=slice`), or draws the maximum once from the
    /// scheduler seed (`run`, `detcore::preemptions::draw_chaos_timeslice`). A
    /// drawn or recorded timeslice is the run's `--max-timeslice` in every
    /// respect, and `Config::chaos_timeslice` keeps it for the run's own record
    /// (and turns the per-slice draw off). An explicit `--max-timeslice` or
    /// `--chaos-seeded-timeslice` with a range is refused rather than letting
    /// one silently win, and so is `--chaos-seeded-timeslice` on a replay of a
    /// record that states a drawn timeslice. Repeating this (analyze
    /// revalidates its trials' options) gives the same result.
    fn resolve_chaos_timeslice_range(&mut self) -> Result<(), Error> {
        // The explicit run-draw shorthand is the same request as the
        // recommended range written out. Keep the default Slice/no-range
        // configuration untouched, and let the existing replay, conflict and
        // PMU checks consume the resolved range.
        let config = &mut self.det_opts.det_config;
        if config.chaos_timeslice_draw == detcore_model::config::ChaosTimesliceDraw::Run
            && config.chaos_timeslice_range.is_none()
        {
            config.chaos_timeslice_range =
                Some(detcore_model::config::RECOMMENDED_CHAOS_TIMESLICE_RANGE);
        }
        let config = &self.det_opts.det_config;
        let mut recorded = None;
        for (option, path) in [
            ("--replay-preemptions-from", &config.replay_preemptions_from),
            ("--replay-schedule-from", &config.replay_schedule_from),
        ] {
            let Some(path) = path else {
                continue;
            };
            if let Some(timeslice) =
                detcore::preemptions::read_recorded_chaos_timeslice(path).map_err(Error::msg)?
            {
                recorded = Some((timeslice, format!("{option} {}", path.display())));
            }
        }
        // `--chaos-seeded-timeslice` would replace the maximum with its own
        // draw once the seed is settled, so the run would report one maximum
        // and run under the other.
        if let Some(range) = config.chaos_timeslice_range
            && self.chaos_seeded_timeslice
        {
            return Err(Error::new(PolicyRefusal).context(format!(
                "--chaos-timeslice-range={range} sets the run's --max-timeslice (to its MAX, {} \
                 ns, or to a value drawn from it), and --chaos-seeded-timeslice draws \
                 --max-timeslice from the scheduler seed; give one of them, not both",
                range.max
            )));
        }
        if let Some(range) = config.chaos_timeslice_range
            && self.max_timeslice_explicit
        {
            return Err(Error::new(PolicyRefusal).context(format!(
                "--chaos-timeslice-range={range} sets the run's --max-timeslice (to its MAX, {} \
                 ns, or to a value drawn from it); give one of them, not both",
                range.max
            )));
        }
        let timeslice = match (recorded, config.chaos_timeslice_range) {
            (Some((recorded, source)), _) => {
                // `--chaos-seeded-timeslice` draws nothing on a replay and
                // gives it the flag's fallback maximum instead, which would
                // replace the recorded one.
                if self.chaos_seeded_timeslice {
                    return Err(Error::new(PolicyRefusal).context(format!(
                        "{source} was recorded under a --max-timeslice of {recorded} ns, drawn \
                         by --chaos-timeslice-draw=run, and --chaos-seeded-timeslice would \
                         replay it under that flag's fallback maximum instead. Chaos slices and \
                         the PMU maximum follow the timeslice, so the replay cannot reproduce \
                         the run. Omit --chaos-seeded-timeslice to replay under the recorded \
                         one."
                    )));
                }
                if self.max_timeslice_explicit
                    && config.max_timeslice.map(std::num::NonZeroU64::get) != Some(recorded)
                {
                    return Err(Error::new(PolicyRefusal).context(format!(
                        "{source} was recorded under a --max-timeslice of {recorded} ns, drawn \
                         by --chaos-timeslice-draw=run, and would be replayed under {}. Chaos \
                         slices and the PMU maximum follow the timeslice, so the replay cannot \
                         reproduce the run. Omit --max-timeslice to replay under the recorded \
                         one, or pass --max-timeslice={recorded}.",
                        config
                            .max_timeslice
                            .map_or_else(|| "none".to_owned(), |maximum| format!("{maximum} ns")),
                    )));
                }
                std::num::NonZeroU64::new(recorded).ok_or_else(|| {
                    Error::msg(format!("{source} records a --max-timeslice of 0 ns"))
                })?
            }
            // The per-run draw waits for the effective seed (`--seed-from`),
            // which validation settles later (`draw_chaos_timeslice_for_run`);
            // until then the range's MAX stands in for the timer checks.
            (None, Some(range)) => {
                let config = &mut self.det_opts.det_config;
                config.max_timeslice = Some(config.chaos_timeslice.unwrap_or(range.max));
                return Ok(());
            }
            (None, None) => return Ok(()),
        };
        let config = &mut self.det_opts.det_config;
        config.max_timeslice = Some(timeslice);
        config.chaos_timeslice = Some(timeslice);
        self.chaos_timeslice_adopted = true;
        Ok(())
    }

    fn resolve_preemption_settings(&self) -> Result<(), Error> {
        // Namespace-only runs do not dispatch an instrumentation backend.
        if self.namespace_only {
            return Ok(());
        }
        if let Some(name) = self.application_backend {
            return hermit::preemption_admission::refuse_unsupported_preemption_with_capabilities(
                &self.det_opts.det_config,
                name,
                self.adapter_capabilities.ok_or_else(|| {
                    Error::new(PolicyRefusal).context(
                        "executable-selected backend is missing immutable physical capabilities",
                    )
                })?,
                self.max_timeslice_explicit,
                self.skid_margin.is_some(),
            )
            .map_err(|refusal| Error::new(PolicyRefusal).context(refusal));
        }
        // The inherited maximum also chooses Detcore's clock accounting and
        // cooperative deadline. Checking a timer request must not rewrite it.
        hermit::preemption_admission::refuse_unsupported_preemption(
            &self.det_opts.det_config,
            self.selected_backend(),
            self.max_timeslice_explicit,
            self.skid_margin.is_some(),
        )
        .map_err(|refusal| Error::new(PolicyRefusal).context(refusal))
    }

    fn preemption_summary(&self) -> detcore_model::summary::PreemptionSummary {
        let maximum = self.det_opts.det_config.max_timeslice;
        detcore_model::summary::PreemptionSummary {
            max_timeslice_ns: maximum.map(|maximum| maximum.get()),
            timer_enabled: Some(
                !self.namespace_only
                    && maximum.is_some()
                    && self.backend_arms_pmu_preemption_timer(),
            ),
            // Counted by the run itself, which reports it in its summary.
            stopless_slice_ends: None,
        }
    }

    /// The run config `--save-config` writes: the captured options with the
    /// implicit inputs this run resolved written out, so that loading it
    /// reproduces the run rather than resolving them again.
    pub(crate) fn saved_run_config(&self) -> Result<crate::app::run_config::RunConfig, Error> {
        use crate::app::run_config::CPU_PROFILE_KEY;
        use crate::app::run_config::EPOCH_KEY;
        use crate::app::run_config::PID_BASE_KEY;
        use crate::app::run_config::SEED_FROM_KEY;
        use crate::app::run_config::SEED_KEY;

        let mut config = self.config_capture.clone().ok_or_else(|| {
            Error::msg(
                "--save-config is written only by a `hermit run` invocation, whose parser \
                 captures the options it was given",
            )
        })?;
        if self.epoch_captured_from_host || self.epoch_from_recording {
            config
                .run
                .insert(EPOCH_KEY.to_owned(), self.epoch_rfc3339().into());
        }
        if let Some(seed) = self.seed_from_resolved {
            config.run.remove(SEED_FROM_KEY);
            config.run.insert(SEED_KEY.to_owned(), seed.into());
        }
        // Freeze the range resolved for a bare run draw, so loading the saved
        // config does not consult a possibly changed recommendation.
        if self.det_opts.det_config.chaos_timeslice_draw
            == detcore_model::config::ChaosTimesliceDraw::Run
            && let Some(range) = self.det_opts.det_config.chaos_timeslice_range
        {
            config
                .run
                .entry("chaos-timeslice-range".to_owned())
                .or_insert_with(|| range.to_string().into());
        }
        // A base Hermit chose for this run (from a free window) is saved as if
        // given, so the saved run takes the same IDs or is refused, instead of
        // choosing another base.
        if self.pid_base.is_none()
            && let Some(base) = self.det_opts.det_config.guest_ids.base()
        {
            config.run.insert(PID_BASE_KEY.to_owned(), base.into());
        }
        // The profile auto resolved to, given or not: loading the config
        // (`--config`, or `--from` a manifest) runs that profile and never
        // derives one again.
        if self.auto_resolution.is_some() {
            config.run.insert(
                CPU_PROFILE_KEY.to_owned(),
                self.det_opts.det_config.cpu_profile.name().into(),
            );
        }
        Ok(config)
    }

    fn write_saved_config(&self) -> Result<(), Error> {
        match &self.save_config {
            Some(path) => crate::app::run_config::write(path, &self.saved_run_config()?),
            None => Ok(()),
        }
    }

    /// The inputs this run drew from the host, by option long name, each
    /// with where it came from: an epoch from the host clock, a seed from
    /// `--seed-from=SystemRandom`, and the preemption timer turned off
    /// because this host has no usable performance counter. A seed derived
    /// from the arguments (`--seed-from=Args`) is not drawn from the host.
    fn host_drawn_inputs(&self) -> BTreeMap<String, String> {
        let mut drawn = BTreeMap::new();
        if self.timer_downgrade.is_some() {
            drawn.insert(
                crate::app::manifest::MAX_TIMESLICE_KEY.to_owned(),
                "no-usable-performance-counter".to_owned(),
            );
        }
        if self.epoch_captured_from_host {
            drawn.insert(
                crate::app::run_config::EPOCH_KEY.to_owned(),
                "host-now".to_owned(),
            );
        }
        if self.seed_from_resolved.is_some()
            && matches!(self.seed_from, Some(SeedFrom::SystemRandom))
        {
            drawn.insert(
                crate::app::run_config::SEED_KEY.to_owned(),
                "SystemRandom".to_owned(),
            );
        }
        // A `--no-namespace` guest's IDs, when the host decided them: a base
        // taken from the windows free on this host, or the host's own IDs.
        if let Some(source) = self.guest_ids_drawn_from_host {
            drawn.insert(
                crate::app::run_config::PID_BASE_KEY.to_owned(),
                source.to_owned(),
            );
        }
        drawn
    }

    /// `--strict` promises that the arguments alone determine the run, so a
    /// strict run that would draw an input from the host clock or the host's
    /// randomness is refused before the guest starts: it could be reproduced
    /// only from a recording, which is what `--strict` says the run does not
    /// need. Each refusal names the option that supplies the input. An epoch
    /// adopted from the recording a run replays is not drawn from the host and
    /// is not refused.
    ///
    /// The preemption timer a host without a usable performance counter turns
    /// off is also recorded as host-drawn, but is not refused here: every
    /// strict run on such a host (the hosted CI runners among them) would stop,
    /// and a run on one host draws the same value every time.
    fn refuse_strict_with_host_drawn_inputs(&self) -> Result<(), Error> {
        if !self.strict {
            return Ok(());
        }
        let mut drawn = self.host_drawn_inputs();
        drawn.remove(crate::app::manifest::MAX_TIMESLICE_KEY);
        if drawn.is_empty() {
            return Ok(());
        }
        let remedies = drawn
            .keys()
            .map(|key| match key.as_str() {
                crate::app::run_config::EPOCH_KEY => {
                    // The host sample this run took, to whole seconds, as an
                    // example a user can paste.
                    let example = self.det_opts.det_config.epoch.format("%Y-%m-%dT%H:%M:%SZ");
                    format!(
                        "the virtual-time epoch, from the host clock: pass --epoch (for example \
                         --epoch={example}) or set HERMIT_EPOCH"
                    )
                }
                crate::app::run_config::SEED_KEY => {
                    "the seed, from --seed-from=SystemRandom: pass \
                     --seed=N, or --seed-from=Args"
                        .to_owned()
                }
                other => format!("{other}: pass it explicitly"),
            })
            .collect::<Vec<_>>()
            .join("; ");
        anyhow::bail!(
            "--strict requires every input of the run on the command line, but this run would \
             draw {remedies}"
        )
    }

    /// Warns, in one line, that this run drew inputs from the host and so
    /// cannot be rerun from its command line, and says how to rerun it: from
    /// the recording that saved them, or with the options that pass them.
    /// A network recording's own line already names its replay. Nothing is
    /// said of an input the command line gave, such as an explicit epoch, or
    /// one a replayed recording supplied.
    fn warn_of_host_drawn_inputs(
        &self,
        global: &GlobalOpts,
        recording: Option<(hermit::Id, crate::app::manifest::Kind)>,
    ) -> Result<(), Error> {
        use crate::app::manifest;
        use crate::app::run_config::EPOCH_KEY;
        use crate::app::run_config::SEED_KEY;

        let drawn = self.host_drawn_inputs();
        let what = match drawn.keys().map(String::as_str).collect::<Vec<_>>()[..] {
            [] => return Ok(()),
            [EPOCH_KEY] => "its epoch",
            [SEED_KEY] => "its seed",
            [manifest::MAX_TIMESLICE_KEY] => "its preemption timer setting",
            [crate::app::run_config::PID_BASE_KEY] => "its guest process ID base",
            _ => "settings",
        };
        match recording {
            Some((_, manifest::Kind::Network)) => Ok(()),
            Some((id, _)) => global.report_event(
                ::tracing::Level::WARN,
                format_args!(
                    // `hermit show` alone shows `last`, which a run manifest does not
                    // move, so the warning names `last-run`.
                    "this run drew {what} from the host; its inputs are saved as a recording in \
                     {}/; rerun it with: hermit replay {id} (hermit show last-run)",
                    home_relative(&manifest::data_dir())
                ),
            ),
            None => {
                let options = drawn
                    .keys()
                    .map(|key| match key.as_str() {
                        EPOCH_KEY => format!("--epoch={}", self.epoch_rfc3339()),
                        SEED_KEY => format!("--seed={}", self.seed_from_resolved.unwrap_or(0)),
                        manifest::MAX_TIMESLICE_KEY => "--max-timeslice=disabled".to_owned(),
                        crate::app::run_config::PID_BASE_KEY => {
                            match self.det_opts.det_config.guest_ids.base() {
                                Some(base) => format!("--pid-base={base}"),
                                None => String::new(),
                            }
                        }
                        other => format!("--{other}"),
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                global.report_event(
                    ::tracing::Level::WARN,
                    format_args!(
                        "this run drew {what} from the host and saved no recording; rerun it \
                         with: {options}"
                    ),
                )
            }
        }
    }

    /// Writes this run's manifest when the policy asks for one (see
    /// `crate::app::manifest`), and remembers its path for `finish_manifest`.
    /// Returns the recording's id and kind when it wrote one. Only a
    /// top-level `hermit run`, whose parser captured the options it was
    /// given, writes one.
    fn begin_manifest(
        &mut self,
        global: &GlobalOpts,
    ) -> Result<Option<(hermit::Id, crate::app::manifest::Kind)>, Error> {
        use crate::app::manifest;

        if self.config_capture.is_none() {
            return Ok(None);
        }
        let user_config = match manifest::user_config_path() {
            Some(path) => manifest::read_user_config(&path)?,
            None => manifest::UserConfig::default(),
        };
        let policy = manifest::resolve_policy(
            self.manifest,
            &user_config,
            std::env::var(manifest::POLICY_ENV).ok().as_deref(),
        )?;
        let host_drawn = self.host_drawn_inputs();
        // A network recording is one the user asked for: it always gets its
        // manifest, which names the trace and holds the seeds and command
        // the trace itself does not (the design note's section 5).
        let network_trace = self
            .record_networking
            .as_deref()
            .map(std::path::absolute)
            .transpose()
            .context("cannot resolve the network trace path")?;
        let kind = match (&network_trace, policy, host_drawn.is_empty()) {
            (Some(_), _, _) => manifest::Kind::Network,
            (None, manifest::ManifestPolicy::Never, _)
            | (None, manifest::ManifestPolicy::NonreproducibleOnly, true) => return Ok(None),
            (None, _, false) => manifest::Kind::InputsOnly,
            (None, manifest::ManifestPolicy::Always, true) => manifest::Kind::History,
        };
        let config = self.saved_run_config()?;
        let (mut global_inputs, mut run_inputs) =
            crate::app::run_config::to_manifest_parts(&config)?;
        manifest::drop_outputs(&mut global_inputs, &mut run_inputs);
        if self.timer_downgrade.is_some() {
            run_inputs.insert(
                manifest::MAX_TIMESLICE_KEY.to_owned(),
                serde_json::Value::from("disabled"),
            );
        }
        let id = hermit::Id::unique();
        let mut document = manifest::Manifest::new(
            id,
            kind,
            manifest::Command {
                program: config.program.clone(),
                args: config.args.clone(),
                cwd: std::env::current_dir()
                    .ok()
                    .and_then(|dir| dir.to_str().map(str::to_owned)),
            },
            manifest::Inputs {
                global: global_inputs,
                run: run_inputs,
                host_drawn,
            },
        );
        if !self.namespace_only {
            document.preemption = Some(self.preemption_summary());
        }
        let path = match &network_trace {
            // The trace stays where its user named it, with its manifest
            // beside it (a coordinator default for the design note's open
            // choice 3).
            Some(trace) => {
                document.artifacts = Some(manifest::Artifacts {
                    network_trace: Some(manifest::NetworkTrace {
                        path: trace
                            .file_name()
                            .and_then(|name| name.to_str())
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "network trace name {} is not UTF-8",
                                    trace.display()
                                )
                            })?
                            .to_owned(),
                        format: manifest::NETWORK_TRACE_FORMAT.to_owned(),
                        version: None,
                        sha256: None,
                        bytes: None,
                    }),
                    syscall_recording: None,
                });
                manifest::network_manifest_path(trace)
            }
            None => manifest::data_dir()
                .join(id.to_string())
                .join(manifest::MANIFEST_NAME),
        };
        // A network recording's manifest is written once its trace is: a
        // run that fails before writing one must not replace the manifest of
        // a trace still at that path.
        if kind == manifest::Kind::Network {
            self.pending_network_manifest = Some(document.clone());
        } else {
            manifest::write(&path, &document)?;
        }
        if kind.pruned() {
            manifest::prune(&manifest::data_dir(), manifest::KEEP_MANIFEST_ONLY);
        }
        if kind == manifest::Kind::Network {
            global.write_controller_diagnostic(format_args!(
                "hermit: the network recording's manifest is written to {} when the run ends, \
                 as recording {id}; replay it offline with: hermit replay {id}",
                path.display(),
            ))?;
        }
        self.manifest_path = Some(path);
        Ok(Some((id, kind)))
    }

    /// Records how the guest ended in this run's manifest, if it wrote one.
    /// Best effort: the manifest without an exit already reproduces the run.
    fn finish_manifest(&mut self, status: ExitStatus) {
        let Some(path) = self.manifest_path.take() else {
            return;
        };
        let exit = crate::app::manifest::Exit::from_status(status);
        let pending = self.pending_network_manifest.take();
        let document = match pending {
            Some(document) => Ok(document),
            None => crate::app::manifest::read(&path),
        };
        let result = document.and_then(|mut document| {
            document.exit = Some(exit);
            // The tracer writes a network trace once the run ends; record
            // what it wrote, so a rerun can tell a truncated or replaced one.
            if let Some(trace) = document
                .artifacts
                .as_mut()
                .and_then(|artifacts| artifacts.network_trace.as_mut())
            {
                let file = path.with_file_name(&trace.path);
                let (sha256, bytes) = crate::app::manifest::digest_and_size(&file)?;
                trace.version = Some(crate::app::manifest::network_trace_version(&file)?);
                trace.sha256 = Some(sha256);
                trace.bytes = Some(bytes);
            }
            crate::app::manifest::write(&path, &document)?;
            Ok(document)
        });
        let document = match result {
            Ok(document) => document,
            Err(error) => {
                crate::app::tracing::diagnostic_eprintln!(
                    "hermit: could not record the exit status in {}: {error:#}",
                    path.display()
                );
                return;
            }
        };
        if document.kind != crate::app::manifest::Kind::Network {
            // A run's manifest is the newest run, `last-run`, once the run
            // has ended; `last` stays with the recording the user made.
            if let Ok(id) = document.id.parse() {
                note_latest(
                    &crate::app::manifest::data_dir(),
                    id,
                    document.kind.moves_last(),
                );
            }
            return;
        }
        // A copy in the data directory, naming the trace by its absolute
        // path, makes the recording one that `hermit replay`, `hermit list`
        // and `last` know by its ID.
        let data_dir = crate::app::manifest::data_dir();
        if let Err(error) = store_network_manifest(&data_dir, &path, document) {
            crate::app::tracing::diagnostic_eprintln!(
                "hermit: could not add the network recording to {}: {error:#}; replay it with: \
                 hermit run --from {}",
                data_dir.display(),
                path.display()
            );
        }
    }

    /// `hermit analyze` keeps a debugging dump of each trial's options in
    /// its workspace. Trials are built by the program, not parsed from a
    /// command line, so this is Rust's `Debug` rendering and not a run config
    /// that `--config` can load.
    pub(crate) fn write_debug_options_dump(&self) -> Result<(), Error> {
        if let Some(path) = &self.save_config {
            let mut file = File::create(path)?;
            file.write_all(format!("{:#?}\n", self).as_bytes())?;
        }
        Ok(())
    }

    /// Read the mounts of the namespace this process runs in, for
    /// `detcore::Config::launch_namespace_mounts`, unless they were read
    /// already. Called on the options a run launches with, before its
    /// container is created, so the mounts are those of the namespace the
    /// guest's inherited descriptors come from, not the container's copies.
    fn capture_launch_namespace_mounts(&mut self) {
        if self.launch_namespace_mounts.0.is_empty()
            && let Ok(mountinfo) = fs::read_to_string("/proc/self/mountinfo")
        {
            self.launch_namespace_mounts = detcore_model::host_input::LaunchNamespaceMounts(
                detcore_model::host_input::mount_types(&mountinfo),
            );
        }
    }

    pub(super) fn effective_det_config(&self) -> DetConfig {
        let mut config = self.det_opts.det_config.clone();
        config.launch_namespace_mounts = self.launch_namespace_mounts.clone();
        if !self.allow_unsupported_syscalls
            && std::env::var(FAIL_CLOSED_ENV).is_ok_and(|value| value == "1")
        {
            config.panic_on_unsupported_syscalls = true;
        }
        config.shutdown_on_unsupported_syscall = config.panic_on_unsupported_syscalls;
        // Hand the scheduler the resolved happens-before program (already resolved
        // against the guest binary in `main()`). `#[serde(skip)]` on the field means
        // this is in-process only; it reaches the ptrace backend directly and is not
        // carried through the DBT JSON config or `--save-config`.
        config.happens_before = self.resolved_happens_before.clone();
        self.apply_run_metadata(&mut config);
        config.untraced_code_range = self
            .application_backend
            .is_none()
            .then(|| untraced_code_range(self.selected_backend()))
            .flatten();
        config.fatal_core_capture = self.fatal_core_capture();
        // The files `--rewrite-exclusives` left unchanged for want of
        // mapping symbols, for the run's summary.
        config.rewrite_exclusives_unmarked = self.exclusive_rewrite_unmarked.clone();
        // A pure function of the flags, so every trial of an analysis and
        // both runs of a `--verify` pair rewrite alike. Replay admission
        // compares a record's rewrite provenance against these fields, so
        // its CLI sites validate this effective config, never the raw
        // parsed one whose rewrite fields are still off.
        config.rewrite_exclusives_in_memory = self.rewrites_exclusives_in_memory();
        config.property_snapshot = self
            .property_snapshot
            .as_ref()
            .map(|snapshot| snapshot.config());
        config.rewrite_exclusives_overlay =
            if self.rewrite_exclusives && !config.rewrite_exclusives_in_memory {
                self.exclusive_rewrite_covered.clone()
            } else {
                Vec::new()
            };
        // The requested overlay, from the flags alone: early replay
        // admission runs before `prepare_exclusive_rewrite` populates
        // the covered files above, and must still state this run's
        // rewrite provenance.
        config.rewrite_exclusives_overlay_requested =
            self.rewrite_exclusives && !self.rewrites_exclusives_in_memory();
        config
    }

    /// Whether `--rewrite-exclusives` rewrites the guest's memory rather than
    /// overlaying rewritten files.
    pub(crate) fn rewrites_exclusives_in_memory(&self) -> bool {
        self.rewrite_exclusives
            && match self.rewrite_exclusives_mode {
                RewriteExclusivesMode::Auto => self.no_namespace,
                RewriteExclusivesMode::Overlay => false,
                RewriteExclusivesMode::Memory => true,
            }
    }

    /// Take the run's copy of the Android system property area
    /// (`super::property_snapshot`) before the guest's first exec, on a host
    /// that has one. Where it cannot be copied the guest would read the live
    /// area: `--strict` refuses that, and any other run says so once.
    fn take_property_snapshot(&mut self) -> Result<(), Error> {
        // One snapshot per invocation: every run of it (both runs of a
        // `--verify`, every trial of `hermit analyze`) reads the same copy.
        static INVOCATION: std::sync::Mutex<
            Option<std::sync::Arc<super::property_snapshot::PropertySnapshotDir>>,
        > = std::sync::Mutex::new(None);
        if self.property_snapshot.is_some() {
            return Ok(());
        }
        let mut invocation = INVOCATION
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(shared) = invocation.as_ref() {
            self.property_snapshot = Some(shared.clone());
            return Ok(());
        }
        // SAFETY: `geteuid` takes no arguments and cannot fail.
        let euid = unsafe { libc::geteuid() };
        // Bind an absolute copy root before the visibility check, capture
        // and config publication: a relative `TMPDIR` would otherwise make
        // the published copy path resolve against the guest's dirfd/cwd.
        let under =
            std::path::absolute(std::env::temp_dir()).unwrap_or_else(|_| std::env::temp_dir());
        let taken = if !self.no_namespace
            && under.starts_with("/tmp")
            && Path::new(super::property_snapshot::PROPERTY_AREA).is_dir()
        {
            // The guest and its tracer see the container's own /tmp.
            super::property_snapshot::Taken::Refused(format!(
                "its copy would be under {}, which the run's container replaces with its own \
                 /tmp; set TMPDIR to another directory",
                under.display()
            ))
        } else {
            super::property_snapshot::take(
                Path::new(super::property_snapshot::PROPERTY_AREA),
                &under,
                euid,
            )
        };
        match taken {
            super::property_snapshot::Taken::NoPropertyArea => Ok(()),
            super::property_snapshot::Taken::Snapshot(snapshot) => {
                tracing::info!(
                    "the guest reads a copy of the system property area taken now ({} files, {})",
                    snapshot.copied(),
                    if snapshot.consistent_cut() {
                        "one consistent cut"
                    } else {
                        "coherent per file"
                    }
                );
                if !snapshot.consistent_cut() {
                    use std::io::Write as _;
                    let _ = writeln!(
                        std::io::stderr(),
                        "WARNING: HERMIT_PROPERTY_SNAPSHOT_PER_FILE: the copy of the system \
                         property area could not be confirmed as one consistent cut (properties \
                         changed during each attempt, or properties_serial could not be read), \
                         so each file is coherent but two files may come from different moments"
                    );
                }
                let inherited = snapshot.inherited_live_descriptors();
                if !inherited.is_empty() {
                    let reason = format!(
                        "the guest would inherit descriptors {inherited:?}, open on live \
                         property files"
                    );
                    if self.strict {
                        return Err(Error::new(PolicyRefusal).context(format!(
                            "--strict refuses this run: it cannot freeze the system property \
                             area for the guest, because {reason}"
                        )));
                    }
                    use std::io::Write as _;
                    let _ = writeln!(
                        std::io::stderr(),
                        "WARNING: HERMIT_PROPERTY_AREA_LIVE: {reason}; reads through them are \
                         not frozen"
                    );
                }
                let snapshot = std::sync::Arc::new(snapshot);
                *invocation = Some(snapshot.clone());
                self.property_snapshot = Some(snapshot);
                Ok(())
            }
            super::property_snapshot::Taken::Refused(reason) if self.strict => {
                Err(Error::new(PolicyRefusal).context(format!(
                    "--strict refuses this run: it cannot freeze the system property area \
                     for the guest, because {reason}"
                )))
            }
            super::property_snapshot::Taken::Refused(reason) => {
                // Once per invocation: each trial of `hermit analyze` tries
                // again. Written directly, as `warn_no_namespace` is: this
                // process's own tracing events are not shown at the default
                // level, and `eprintln!` would panic when stderr's reader has
                // gone.
                static WARNED: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                if WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    return Ok(());
                }
                use std::io::Write as _;
                let _ = writeln!(
                    std::io::stderr(),
                    "WARNING: HERMIT_PROPERTY_AREA_LIVE: the guest reads the live system \
                     property area, which processes outside the run rewrite while it runs, \
                     because {reason}"
                );
                Ok(())
            }
        }
    }

    /// The rewrite `--rewrite-exclusives` applies in this run, as a
    /// preemption record states it: its mode and this Hermit's rewrite
    /// format version; `None` when the flag is off.
    pub(crate) fn rewrite_exclusives_provenance(
        &self,
    ) -> Option<detcore::preemptions::RewriteExclusives> {
        self.rewrite_exclusives.then(|| {
            detcore::preemptions::RewriteExclusives::current(
                if self.rewrites_exclusives_in_memory() {
                    detcore::preemptions::RewriteExclusivesMode::Memory
                } else {
                    detcore::preemptions::RewriteExclusivesMode::Overlay
                },
            )
        })
    }

    /// Where and within what bounds the tracer keeps fatal cores, or `None`,
    /// the default, to keep none.
    fn fatal_core_capture(&self) -> Option<detcore::FatalCoreCapture> {
        let handle = self.fatal_core_dir_handle.as_ref()?;
        Some(detcore::FatalCoreCapture {
            dir: descriptor_path(handle),
            file_prefix: self.fatal_core_file_prefix(),
            max_core_bytes: self.fatal_core_max_bytes,
            max_total_bytes: self.fatal_core_total_max_bytes,
            time_limit: FATAL_CORE_TIME_LIMIT,
        })
    }

    /// The name prefix of every core this run writes.
    fn fatal_core_file_prefix(&self) -> String {
        match self.fatal_core_run_label {
            Some(label) => format!("hermit-{}-{label}-", self.fatal_core_owner),
            None => format!("hermit-{}-", self.fatal_core_owner),
        }
    }

    /// Opens `--fatal-core-dir`, creating it, so that the guest's run can
    /// reach it. Called once every refusal that needs no guest has passed.
    fn open_fatal_core_dir(&mut self) -> Result<(), Error> {
        if let Some(dir) = &self.fatal_core_dir {
            fs::create_dir_all(dir)
                .with_context(|| format!("creating --fatal-core-dir {}", dir.display()))?;
            let handle = File::open(dir)
                .with_context(|| format!("opening --fatal-core-dir {}", dir.display()))?;
            self.fatal_core_dir_handle = Some(std::sync::Arc::new(handle));
            self.fatal_core_owner = fatal_core_owner_token();
        }
        Ok(())
    }

    /// A run that succeeded keeps no cores: removes every file in
    /// `--fatal-core-dir` that this invocation named, including any temporary
    /// file a capture left behind.
    fn discard_fatal_cores_after_success(&self) {
        let (Some(dir), Some(_)) = (&self.fatal_core_dir, &self.fatal_core_dir_handle) else {
            return;
        };
        let removed = remove_fatal_core_files(dir, &self.fatal_core_owner);
        if removed > 0 {
            eprintln!("hermit: the run exited 0, so its {removed} fatal core file(s) were removed");
        }
    }

    /// The `--timeout` bound, if the caller asked for one.
    ///
    /// Mirrors `StartOpts::record_timeout` so the two spellings of a hermit
    /// deadline are read the same way.
    pub(super) fn run_timeout(&self) -> Option<Duration> {
        self.timeout
            .map(|seconds| Duration::from_secs(seconds.get()))
    }

    /// Whether this run's backend enforces `--timeout` (see
    /// [`Self::ensure_timeout_supported`]). in-guest-trap runs the in-guest
    /// LiteInst runtime, so it takes liteinst's answer here; both are refused
    /// `--timeout` earlier by `refuse_unqualified_liteinst_in_guest_options`.
    pub(crate) fn backend_enforces_timeout(&self) -> bool {
        matches!(
            self.runtime_backend(),
            Backend::Ptrace | Backend::Liteinst | Backend::InGuestTrap
        )
    }

    /// Whether `hermit analyze` can bound this run with a `--timeout`: its
    /// backend enforces one, and it does not run the in-guest runtime, which
    /// refuses one (`refuse_unqualified_liteinst_in_guest_options`).
    pub(crate) fn accepts_run_timeout(&self) -> bool {
        self.backend_enforces_timeout() && !self.uses_in_guest_liteinst()
    }

    /// Bound the run at `seconds` of wall-clock time, or at its own
    /// `--timeout` where that is shorter: `hermit analyze`'s per-trial bound.
    pub(crate) fn bound_run_timeout(&mut self, seconds: NonZeroU64) {
        self.timeout = Some(self.timeout.map_or(seconds, |own| own.min(seconds)));
    }

    /// The `--timeout` bound in whole seconds, as [`Self::set_run_timeout`]
    /// takes it back: `hermit analyze` shortens a run's bound to the time its
    /// budget has left and restores the run's own afterwards.
    pub(crate) fn run_timeout_seconds(&self) -> Option<NonZeroU64> {
        self.timeout
    }

    pub(crate) fn set_run_timeout(&mut self, seconds: Option<NonZeroU64>) {
        self.timeout = seconds;
    }

    /// The guest program, as the run arguments name it: none when `--image`
    /// leaves it to the image's Entrypoint/Cmd.
    pub(crate) fn program(&self) -> Option<&Path> {
        self.program.as_deref()
    }

    /// `--strict` promises a deterministic run, and on the ptrace and e9patch
    /// backends the virtual clock and the preemption points are the guest's
    /// retired conditional branches, read from a performance counter. Where
    /// that counter miscounts, two runs of one program diverge by a few
    /// branches: measured on an AMD EPYC 9D64 (family 0x19, model 0xA0), around
    /// locked instructions (<https://github.com/rrnewton/hermit/issues/3794>).
    /// Refuse rather than run a strict guest on a clock that is not exact.
    /// Runs without `--strict` go ahead, and Reverie logs the failed
    /// validation.
    ///
    /// A run with `--max-timeslice=disabled` does not depend on the counter
    /// being exact: Detcore reads it only to advance logical time and to place
    /// preemption points, and does neither without a timeslice
    /// (`DetConfig::use_rcb_time`, `Detcore::update_logical_time_rcbs`).
    /// Reverie still opens, calibrates and incidentally reads the counters,
    /// but none of those reads reaches logical time, a preemption point or the
    /// guest (the post-facto review of
    /// <https://github.com/rrnewton/hermit/issues/4291>). So
    /// such a run is not refused; that is what lets a test run at the relaxed
    /// strictness level on a host whose counter miscounts
    /// (<https://github.com/rrnewton/hermit/issues/4291>).
    fn refuse_strict_with_inexact_branch_counter(&self) -> Result<(), Error> {
        if !self.strict || !self.reads_the_branch_counter() {
            return Ok(());
        }
        refuse_an_inexact_branch_counter(self.selected_backend(), self.inexact_branch_counter)
    }

    /// The one-line stderr warning for a run that goes on without `--strict`
    /// while reading a retired-branch counter that failed Reverie's validation,
    /// which `--strict` refuses: the run proceeds on a degraded capability, so
    /// it says so ([`warning_for_an_inexact_branch_counter`]). Reverie
    /// validates the counters once per process, on its first timer, so asking
    /// here costs nothing more.
    fn inexact_branch_counter_warning(&self) -> Option<String> {
        // A `--strict` run never gets here with such a counter:
        // `admit_branch_counter` refuses it first.
        if !self.reads_the_branch_counter() {
            return None;
        }
        warning_for_an_inexact_branch_counter(CounterReader::Run, self.inexact_branch_counter)
    }

    /// The one-line stderr notice for a timeslice of at most
    /// [`SMALL_TIMESLICE_SKID_MARGINS`] skid margins (`skid_margin`, normally
    /// [`installed_skid_margin`]). A thread that computes without returning to
    /// Detcore is stopped by a timer event at the end of every timeslice, and on
    /// the ptrace and KVM backends a precise timer is placed by stepping the guest
    /// through up to the margin, so at least a tenth of such a slice is stepped: a
    /// CPU-bound guest runs orders of magnitude slower, and a run cut off by
    /// `--timeout` may not be hung.
    ///
    /// A slice is counted in RCBs at [`detcore::Config::largest_nanos_per_rcb`],
    /// the fewest any thread gets. `None` where the maximum does not decide how
    /// much is stepped: where the run arms no PMU timer, as
    /// [`Self::reads_the_branch_counter`] decides (a disabled maximum, given or
    /// set because the host has no usable counters, or a backend without the
    /// timer); in a preemption replay, whose recording sets the slices; with
    /// `--imprecise-timers`, which steps nothing; and with `--hb-list-events`,
    /// which runs no guest.
    pub(crate) fn small_max_timeslice_notice(&self, skid_margin: fn() -> u64) -> Option<String> {
        let config = &self.det_opts.det_config;
        if !self.reads_the_branch_counter()
            || config.replay_preemptions_from.is_some()
            || config.imprecise_timers
            || self.hb_list_events
        {
            return None;
        }
        let maximum = config.max_timeslice?.get();
        let rcbs = (maximum as f64 / config.largest_nanos_per_rcb()).floor() as u64;
        let margin = skid_margin();
        (rcbs <= SMALL_TIMESLICE_SKID_MARGINS.saturating_mul(margin)).then(|| {
            let branches = if rcbs == 1 { "branch" } else { "branches" };
            let slowed = if config.chaos && config.chaos_per_thread_slowdown {
                " (at the largest chaos slowdown factor)"
            } else {
                ""
            };
            format!(
                "NOTICE: --max-timeslice={maximum} lets a thread that computes without a syscall \
                 run at most about {rcbs} retired conditional {branches}{slowed} before a timer \
                 event stops it, and the guest is stepped through up to {margin} branches (the \
                 skid margin) to place each event, so a CPU-bound guest can run orders of \
                 magnitude slower than at the default (--max-timeslice={default}), and a \
                 --timeout that expires may mean a slow run rather than a hung one. Omit \
                 --max-timeslice, or raise it, for a faster run; --summary reports how many \
                 timeslices the run had.\n",
                default = detcore::Config::default()
                    .max_timeslice
                    .map_or_else(|| "disabled".to_owned(), |maximum| maximum.to_string())
            )
        })
    }

    /// [`Self::small_max_timeslice_notice`] for the trials of `hermit analyze`
    /// and `hermit bisect`, which take these options without `main`'s
    /// validation: where the host cannot arm the timer, the trials run with the
    /// maximum disabled, so there is no notice.
    pub(crate) fn small_trial_max_timeslice_notice(
        &self,
        skid_margin: fn() -> u64,
    ) -> Option<String> {
        reverie_timer_support().ok()?;
        self.small_max_timeslice_notice(skid_margin)
    }

    /// `main`'s branch-counter check: a `--strict` run on a counter that failed
    /// Reverie's validation is refused, and any other run that reads it passes
    /// [`Self::inexact_branch_counter_warning`] to `warn` (stderr in `main`).
    fn admit_branch_counter(&self, warn: &mut dyn FnMut(&str)) -> Result<(), Error> {
        self.refuse_strict_with_inexact_branch_counter()?;
        if let Some(warning) = self.inexact_branch_counter_warning() {
            warn(&warning);
        }
        Ok(())
    }

    /// Whether this run reads the retired-conditional-branch counter that
    /// [`Self::refuse_strict_with_inexact_branch_counter`] guards: on a
    /// backend that arms a PMU preemption timer, with a timeslice.
    fn reads_the_branch_counter(&self) -> bool {
        self.arms_pmu_preemption_timer() && self.det_opts.det_config.max_timeslice.is_some()
    }

    /// [`Self::reads_the_branch_counter`] for an `analyze` or `bisect` trial.
    /// Trials start through `RunOpts::run` and never pass through `main`
    /// (<https://github.com/rrnewton/hermit/issues/3810>), so `--namespace-only`
    /// (alias `--lite`) does not reach `run_with_namespace_only`: the trial
    /// still runs on the backend's runtime and its counter, and is not exempt.
    fn trial_reads_the_branch_counter(&self) -> bool {
        self.backend_arms_pmu_preemption_timer() && self.det_opts.det_config.max_timeslice.is_some()
    }

    /// [`Self::refuse_strict_with_inexact_branch_counter`] for an `analyze` or
    /// `bisect` trial (`AnalyzeOpts::refuse_strict_with_inexact_branch_counter`),
    /// under [`Self::trial_reads_the_branch_counter`].
    pub(crate) fn refuse_strict_trial_with_inexact_branch_counter(&self) -> Result<(), Error> {
        if !self.strict || !self.trial_reads_the_branch_counter() {
            return Ok(());
        }
        refuse_an_inexact_branch_counter(self.selected_backend(), self.inexact_branch_counter)
    }

    /// [`Self::inexact_branch_counter_warning`] for the trials of one
    /// `analyze` or `bisect` (`AnalyzeOpts::branch_counter_warning`): wherever
    /// [`Self::refuse_strict_trial_with_inexact_branch_counter`] refuses
    /// `--strict`, trials without it warn.
    pub(crate) fn trial_inexact_branch_counter_warning(&self) -> Option<String> {
        if !self.trial_reads_the_branch_counter() {
            return None;
        }
        warning_for_an_inexact_branch_counter(CounterReader::Trials, self.inexact_branch_counter)
    }

    /// The run options set here that only `RunOpts::main` applies, as the
    /// spellings a user would write. A trial started through `RunOpts::run`,
    /// as `hermit analyze` and `hermit bisect` start theirs, would accept them
    /// and run without them, while its printed reproducer still carried them:
    ///
    /// - `--namespace-only`: only `main` dispatches to `run_with_namespace_only`;
    ///   `run` launches the backend's runtime.
    /// - `--verify` (and every option that requires it): only `main`
    ///   dispatches to `verify()`; `run` is a single run.
    /// - `--happens-before` (and `--hb-list-events`): only `main` resolves the
    ///   specification, so `run` hands the scheduler none.
    /// - `--verify-run`: only `main` dispatches to `verify_run()`.
    /// - `--backend-engagement-json`: only `main` writes it.
    /// - Nondefault `--cpu-profile`: only `main` runs the required host checks
    ///   and admits the profile.
    ///
    /// `--skid-margin` is not listed: `hermit analyze` and `hermit bisect`
    /// install it for their trials themselves (`install_pmu_config`).
    pub(crate) fn options_only_main_applies(&self) -> Vec<&'static str> {
        [
            (self.namespace_only, "--namespace-only"),
            (self.verify, "--verify"),
            // Only `main` runs the host qualification the profile admits with.
            // Not auto: each launch resolves that itself.
            (
                !self.det_opts.det_config.cpu_profile.is_default()
                    && self.det_opts.det_config.cpu_profile
                        != detcore_model::cpu_profile::CpuProfile::Auto,
                match self.det_opts.det_config.cpu_profile {
                    detcore_model::cpu_profile::CpuProfile::HostNoRdrand => {
                        "--cpu-profile host-no-rdrand"
                    }
                    _ => "--cpu-profile x86-64-v3",
                },
            ),
            (self.verify_run.is_some(), "--verify-run"),
            (self.happens_before.is_some(), "--happens-before"),
            (
                self.backend_engagement_json.is_some(),
                "--backend-engagement-json",
            ),
        ]
        .into_iter()
        .filter_map(|(set, option)| set.then_some(option))
        .collect()
    }

    /// Replace the host's retired-branch-counter probe, for a `RunOpts` built
    /// outside clap's parse of `run`, such as `hermit analyze`'s trial options.
    pub(crate) fn set_inexact_branch_counter(&mut self, probe: fn() -> Option<String>) {
        self.inexact_branch_counter = probe;
    }

    /// Refuse `--timeout` on a backend where it has not been shown to bound the
    /// run, instead of accepting a flag that does nothing.
    ///
    /// ⚠️ MEASURED PER BACKEND, 2026-08-26, `--timeout 3` against a guest that
    /// never exits, two runs each and reproducible:
    ///
    /// | backend    | elapsed | marker                        |
    /// |------------|---------|-------------------------------|
    /// | `ptrace`   | 3s      | `class=run-timeout`           |
    /// | `liteinst` | 3s      | `class=run-timeout`           |
    /// | `kvm`      | 13s     | `HERMIT_RUN_TIMEOUT_FALLBACK` |
    /// | `sabre`    | 40s     | none -- killed by the harness |
    /// | `dbt`      | 20s     | none -- killed by the harness |
    ///
    /// The `liteinst` row measured the ptrace-hosted hybrid, which no longer
    /// exists. In-guest LiteInst refuses `--timeout` earlier, in
    /// `refuse_unqualified_liteinst_in_guest_options`, so it never reaches this
    /// check.
    ///
    /// ⚠️ `sabre` AND `dbt` ARE NOT MERELY UNTESTED -- THEY STRUCTURALLY CANNOT
    /// HONOUR THE FLAG TODAY, and the difference decides what fixing them means.
    /// Neither BOUNDED THE RUN AT ALL: the elapsed times are the outer harness's
    /// own deadline, and the absence of a marker is precisely the "exit 124 with
    /// no marker means no inner bound fired" reading in docs/TIMEOUT_LADDER.md.
    /// Both run fine WITHOUT the flag, so it is the bound that fails.
    ///
    /// The mechanism is visible on `sabre`, which panicked in
    /// `reverie-rpc-transport`'s blocking client after 69 seconds with a broken
    /// pipe: a BLOCKING call cannot yield to the single `current_thread` tokio
    /// runtime that `tokio::time::timeout` needs in order to fire, so the primary
    /// path is never reached. `dbt` shows the same total absence of a bound and
    /// is a launch adapter around DynamoRIO, so it is plausibly the same class --
    /// but that has NOT been traced and must not be asserted.
    ///
    /// So do not "qualify" one of these by running the flag once and seeing it
    /// accepted; acceptance is exactly what already happens and it does nothing.
    ///
    /// `kvm` is excluded for a different and softer reason: it does bound the
    /// run, but ONLY through the hard `_exit` fallback, ten seconds late and
    /// with no unwind. Allowing it would mean the marker that is supposed to
    /// mean "the unwind failed" fires on every single KVM timeout, which
    /// destroys the signal the marker exists to carry. Qualify it by finding out
    /// why the runtime never reaches the timer, not by widening this list.
    ///
    /// Fail-closed, so a NEW backend must be qualified deliberately rather than
    /// inheriting a guarantee nobody measured for it.
    pub(crate) fn ensure_timeout_supported(&self) -> Result<(), Error> {
        if self.timeout.is_none() || self.backend_enforces_timeout() {
            return Ok(());
        }
        if let Some(name) = self.application_backend {
            return Err(Error::new(PolicyRefusal).context(format!(
                "--timeout is not qualified to bound executable-selected backend `{name}`; use an outer bound"
            )));
        }
        let backend = self.runtime_backend();
        Err(Error::new(PolicyRefusal).context(format!(
            "--timeout is not qualified on the `{backend:?}` backend and hermit will not \
             accept a bound it cannot enforce. Measured 2026-08-26 with `--timeout 3` on a \
             guest that never exits: ptrace and liteinst stopped at 3s and reported \
             `class=run-timeout`; kvm stopped only via the hard fallback at 13s; sabre and \
             dbt did not stop the run at all. Use an outer bound (the cell's \
             `timeout_seconds`, or `bin/safehermit --sh-deadline`) on this backend, and see \
             docs/TIMEOUT_LADDER.md."
        )))
    }

    #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
    fn run_mapped_in_container(
        &self,
        global: &GlobalOpts,
        capture_output: bool,
        identity_sources: Option<&IdentityGuard>,
        image: &super::mapped_first_admission::RetainedImage,
    ) -> Result<(ExitStatus, Option<Output>, Option<detcore::TerminalOutcome>), Error> {
        anyhow::ensure!(
            !capture_output,
            "exec-stop direct-run path requires external bounded stdout capture"
        );
        let handle = global.log_file_handle.as_ref()
            .ok_or_else(|| Error::msg("--in-guest-exec-stop requires an actual --log-file for complete INFO/DETLOG evidence"))?;
        let log = verification_log(
            handle.try_clone()?,
            log_max_bytes().map_err(Error::msg)?,
            global.log_budget(),
        );
        let _guard =
            super::tracing::init_sync_file_tracing(global.log, log.clone(), Backend::InGuestTrap);
        hermit::proc_mount::warn_if_readonly_proc();
        let command = self.guest_command()?;
        let config = complete_container_config(
            self.effective_det_config(),
            identity_sources,
            self.launch_workdir(),
        )?;
        let mut config = hermit::prepare_in_guest_loader_route_config(config)?;
        // The encoded ForwardPolicy cannot carry span/field-dependent answers.
        // Refuse those actual RUST_LOG directives before enabling plain INFO.
        let directives = match std::env::var("RUST_LOG") {
            Ok(value) => value,
            Err(std::env::VarError::NotPresent) => String::new(),
            Err(error) => return Err(Error::msg(error)),
        };
        detcore::detlog::validate_plain_info_directives(&directives).map_err(Error::msg)?;
        config.in_guest_detlog_forward_policy =
            Some(hermit::in_guest_detlog_forward_policy_for_native().encode());
        // SAFETY: explicit bounded intermediate opt-in, internally exact-bound
        // actual phase4 artifact, same original ptracer and retained lexical
        // cleanup root. Full landing machine qualification is NOT claimed.
        let outcome = unsafe {
            super::mapped_first_admission::run_mapped(
                command,
                config,
                image,
                log.record_sink(),
                self.summary,
                &self.summary_json,
            )
        };
        let flushed = log.with(|writer| writer.flush());
        let failure = log.failure();
        let status = outcome?;
        flushed?;
        anyhow::ensure!(
            failure.is_none(),
            "actual mapped INFO/DETLOG sink failed: {failure:?}"
        );
        anyhow::ensure!(
            detcore::detlog::determinism_loss().is_none(),
            "actual mapped DETLOG reports determinism loss"
        );
        Ok((status, None, None))
    }

    fn run_in_container(
        &self,
        global: &GlobalOpts,
        capture_output: bool,
        identity_sources: Option<&IdentityGuard>,
    ) -> Result<(ExitStatus, Option<Output>, Option<detcore::TerminalOutcome>), Error> {
        let _guard = global.init_tracing_for_backend(self.runtime_backend());
        self.log_auto_resolution();
        hermit::proc_mount::warn_if_readonly_proc();
        // Inside the container: its mounts, the overlays among them, are in
        // place.
        for line in exclusive_rewrite_applied_lines(&self.exclusive_rewrite_overlays) {
            crate::app::tracing::diagnostic_eprintln!("{line}");
        }

        let backend = self.runtime_backend();
        let command = self.guest_command()?;
        // The command has no `pre_exec` callback, so it installs no seccomp
        // filter.
        let config = complete_container_config(
            self.effective_det_config(),
            identity_sources,
            self.launch_workdir(),
        )?;

        let timeout = self.run_timeout();
        let summary_json = self.summary_json.clone();
        // A terminal outcome the backend unwound (a deadlock under Hermit's
        // model, a virtual timeout) is a finished run that returns
        // `hermit::SchedulerEndedRun`. A plain run reports it by its class
        // line; a caller that captures the output (analyze) keeps the output,
        // whose status is the outcome's, with the outcome that says the guest
        // did not exit with it.
        if capture_output {
            hermit::SchedulerEndedRun::output_of(hermit::run_with_output_backend_timeout(
                command,
                config,
                self.summary,
                &summary_json,
                backend,
                timeout,
            ))
            .map(|(out, outcome)| (out.status, Some(out), outcome))
        } else {
            hermit::run_with_backend_timeout(
                command,
                config,
                self.summary,
                &summary_json,
                backend,
                timeout,
            )
            .map(|status| (status, None, None))
        }
    }

    fn run_verify_in_container(
        &self,
        log_file: &mut Option<fs::File>,
        forwarded_detlogs: &mut ForwardedDetlogPlan,
        global: &GlobalOpts,
        identity_sources: Option<&IdentityGuard>,
        native: Option<NativeContainerRun>,
        #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))] mapped: Option<
            &MappedVerificationRun,
        >,
    ) -> Result<(Output, u64, RunLatches, Option<detcore::TerminalOutcome>), Error> {
        #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
        if let Some(mapped) = mapped {
            anyhow::ensure!(
                identity_sources.is_some(),
                "mapped verification requires actual container identity"
            );
            hermit::proc_mount::warn_if_readonly_proc();
            let (log, _guard) = self.verification_run_log(log_file, global)?;
            let (command, config) = self.mapped_command_config(identity_sources)?;
            let result = super::mapped_first_admission::capture_verification_in_container(
                super::mapped_first_admission::VerificationCapture {
                    command,
                    config,
                    retained: &mapped.image,
                    records: log.record_sink(),
                    print_summary: self.summary,
                    summary_json: &self.summary_json,
                    policy: mapped.policy,
                    forwarded: forwarded_detlogs,
                },
            );
            return finish_mapped_verification_log(result, log).map(
                |(output, skid_overshoots, latches)| (output, skid_overshoots, latches, None),
            );
        }
        let mut forwarded_detlogs = forwarded_detlogs.open_in_container()?;
        hermit::proc_mount::warn_if_readonly_proc();
        for line in exclusive_rewrite_applied_lines(&self.exclusive_rewrite_overlays) {
            crate::app::tracing::diagnostic_eprintln!("{line}");
        }

        let (log, _guard) = self.verification_run_log(log_file, global)?;

        let command = self.guest_command()?;

        // The command has no `pre_exec` callback.
        let config = complete_container_config(
            self.effective_det_config(),
            identity_sources,
            self.launch_workdir(),
        )?;

        #[cfg(feature = "native-exec")]
        if let Some(native) = native {
            let (sender, receiver) = forwarded_detlogs.take().ok_or_else(|| {
                Error::msg("native exec requires the actual verification DETLOG source")
            })?;
            detcore::detlog::set_forwarded_source(receiver, log.record_sink());
            let policy = hermit::in_guest_detlog_forward_policy_for_native();
            let mut config = hermit::prepare_in_guest_loader_route_config(config)?;
            config.in_guest_detlog_forward_policy = Some(policy.encode());
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let result = runtime.block_on(super::native_exec_run::run(
                super::native_exec_run::PhysicalRun {
                    command,
                    config,
                    root: native.root,
                    runtime_files: native.runtime_files,
                    artifacts: native.artifacts,
                    endpoints: native.endpoints,
                    detlog_sender: sender,
                    print_summary: self.summary,
                    summary_json: self.summary_json.clone(),
                },
            ));
            detcore::detlog::clear_forwarded_source();
            let output = result?;
            let _ = log.clone().flush();
            let latches = RunLatches {
                determinism_loss: detcore::detlog::determinism_loss(),
                log_sink_failure: log.failure(),
            };
            return Ok((output, 0, latches, None));
        }
        #[cfg(not(feature = "native-exec"))]
        let _ = native;
        let forwarding = forwarded_detlogs.take().map(|(sender, receiver)| {
            hermit::forward_in_guest_detlogs_to(sender);
            detcore::detlog::set_forwarded_source(receiver, log.record_sink());
        });
        let result = hermit::run_with_output_backend_timeout_and_skid_overshoots(
            command,
            config,
            self.summary,
            &self.summary_json,
            self.runtime_backend(),
            None,
        );
        if forwarding.is_some() {
            // Records sent after the guest's last request, then close the socket.
            detcore::detlog::clear_forwarded_source();
        }
        // A run that ended with a terminal outcome carries its output in
        // `hermit::SchedulerEndedRun`; one that also overshot its skid stays that
        // refusal.
        let (output, skid_overshoots, terminal_outcome) =
            match hermit::SchedulerEndedRun::split(result)? {
                Ok((output, skid_overshoots)) => (output, skid_overshoots, None),
                Err(ended) => {
                    let outcome = ended.outcome;
                    (ended.into_output()?, 0, Some(outcome))
                }
            };
        // A flush error counts too.
        let _ = log.clone().flush();
        let latches = RunLatches {
            determinism_loss: detcore::detlog::determinism_loss(),
            log_sink_failure: log.failure(),
        };
        Ok((output, skid_overshoots, latches, terminal_outcome))
    }
}

#[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
fn finish_mapped_verification_image<T>(
    outcome: Result<T, Error>,
    close: std::io::Result<()>,
) -> Result<T, Error> {
    match (outcome, close) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(close)) => Err(close.into()),
        (Err(error), Err(close)) => Err(retain_verification_error(
            error,
            "closing private verifier image",
            close.into(),
        )),
    }
}

#[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
fn finish_mapped_verification_log(
    outcome: Result<Output, Error>,
    mut log: SharedLog,
) -> Result<(Output, u64, RunLatches), Error> {
    // Flush the same writer even after an execution refusal. A later log error
    // cannot replace the original typed execution error or disappear silently.
    let _ = log.flush();
    let latches = RunLatches {
        determinism_loss: detcore::detlog::determinism_loss(),
        log_sink_failure: log.failure(),
    };
    match outcome {
        Ok(output) => Ok((output, 0, latches)),
        Err(error) => match latches.log_sink_failure {
            Some(failure) => Err(retain_verification_error(
                error,
                "flushing private verifier log",
                Error::msg(failure),
            )),
            None => Err(error),
        },
    }
}

/// The error that ends a verification whose first run's status `verify_allow`
/// rejects, after publishing its report to `verify_json`, or `None` when the
/// status is allowed. A rejected run that overshot its PMU skid is that
/// infrastructure error; otherwise the report records the rejected
/// disposition, so it is not mistaken for a run that never started. Shared by
/// `run --verify`, which then does not start a second run, and `hermit
/// log-diff --verify-json`, which then does not compare.
/// The error `run --verify` and `log-diff` end with when [`reject_first_run`]
/// refused run 1. A scheduler terminal outcome is not a guest result, so it
/// gets no exemption from `--verify-allow`: the default `success` refuses it
/// as it refuses a guest that failed, and the report is not `verified`. That
/// holds for a deadlock under Hermit's model and, by the same rule, for a
/// virtual timeout, a bound Hermit imposed on a guest that had not finished.
/// It still ends with the outcome's own class line and status rather than a
/// CLI error. A skid overshoot keeps its refusal.
pub(super) fn rejected_first_run(
    error: Error,
    terminal_outcome: Option<detcore::TerminalOutcome>,
) -> Error {
    match terminal_outcome {
        Some(outcome)
            if error.downcast_ref::<SkidOvershootError>().is_none()
                && detcore::ReplayDepartureError::in_error(&error).is_none() =>
        {
            error.context(SchedulerOutcome(outcome))
        }
        _ => error,
    }
}

pub(super) fn reject_first_run(
    verify_json: Option<&Path>,
    verify_allow: VerifyAllow,
    out1: &Output,
    skid_overshoots_run1: u64,
    summary1: Option<&RunSummary>,
) -> Option<Error> {
    if verify_allow.satisfies(out1.status) {
        return None;
    }
    if skid_overshoots_run1 > 0 {
        if let Some(path) = verify_json
            && let Err(error) = write_skid_overshoot_without_comparison_json(
                path,
                skid_overshoots_run1,
                verification_runtime_from_summaries(summary1, None),
                Some(out1.status),
            )
        {
            return Some(error);
        }
        return Some(Error::new(SkidOvershootError::new(skid_overshoots_run1)));
    }
    let status = describe_exit_status(out1.status);
    eprintln!(
        "First run errored during --verify, not continuing to a second.\nExit status: {status}\nStdout:\n{}\nStderr:\n{}",
        String::from_utf8_lossy(&out1.stdout),
        String::from_utf8_lossy(&out1.stderr),
    );
    // ⚠️ RECORD THE DISPOSITION HERE, WHERE IT IS KNOWN. The pre-stamped
    // `no_result` record was previously left untouched on this path -- so the
    // artifact reported `guest_exit_code: null` for a guest whose exit code we
    // had. That made this refusal byte-identical to a container that never
    // ran the guest, and 14 rotating e2e failures were undiagnosable as a
    // direct result. A best-effort write: an unwritable artifact must not
    // convert a rejected first run into a different error.
    if let Some(path) = verify_json {
        let report =
            first_run_rejected_report(out1, verification_runtime_from_summaries(summary1, None));
        if let Err(error) = write_report_json(path, &report) {
            eprintln!(
                "WARNING: could not record the rejected first run in {}: {}",
                path.display(),
                error
            );
        }
    }
    Some(Error::msg(format!("First run during --verify {status}")))
}

/// How a `--verify-run` execution that returned `error` ended, classified as
/// `run --verify` classifies the same error of one of its runs.
pub(super) fn verify_run_outcome_of(error: &Error) -> verify_run::VerifyRunOutcome {
    if let Some(overshoot) = error.downcast_ref::<SkidOvershootError>() {
        return verify_run::VerifyRunOutcome::SkidOvershoot {
            count: overshoot.count(),
        };
    }
    if let Some(disposition) = super::verify::container_failure_of(error) {
        return verify_run::VerifyRunOutcome::ContainerFailed { disposition };
    }
    if let Some(failure) = super::verify::run_failure_of(error) {
        // A determinism loss names its run (`RunLatches::refuse_comparison`);
        // the comparison names it again by its place in the pair.
        let detail = match error.downcast_ref::<DeterminismLossRecorded>() {
            Some(loss) => format!("determinism loss recorded: {}", loss.reason),
            None => format!("{error:#}"),
        };
        return verify_run::VerifyRunOutcome::RunFailed { failure, detail };
    }
    verify_run::VerifyRunOutcome::Error {
        detail: format!("{error:#}"),
    }
}

/// One completed execution of a two-run verification: what
/// [`conclude_verification`] compares.
pub(super) struct CompletedVerificationRun<'a> {
    pub(super) output: &'a Output,
    /// The run's verification log, which the comparison consumes.
    pub(super) log: tempfile::TempPath,
    pub(super) skid_overshoots: u64,
    pub(super) summary: Option<RunSummary>,
    /// The host files the run's guest opened (`hermit::host_input_change`).
    pub(super) host_inputs: Option<std::io::Result<HostInputs>>,
}

/// How a two-run verification is concluded once both runs completed.
pub(super) struct VerificationConclusion<'a> {
    pub(super) verify_json: Option<&'a Path>,
    pub(super) comparison_options: ComparisonOptions,
    /// Completed backend facts supplement the common comparison.
    pub(super) evidence: Option<super::verify::NativeEvidenceComparison>,
    /// Static identity text, admitted only with completed native evidence.
    pub(super) native_identity_diagnostic: Option<&'static str>,
    /// The built-in backend, when no executable-selected adapter is present.
    pub(super) backend: Backend,
    pub(super) application_backend: Option<&'a str>,
    /// Whether both runs used `--passthru-opt`.
    pub(super) passthru_opt: bool,
    /// Whether both runs had `--rewrite-exclusives`, when known: what a
    /// divergence that store-exclusive failures explain suggests
    /// ([`store_exclusive_retries`]).
    pub(super) rewrite_exclusives: Option<bool>,
}

/// Compare two completed verification runs, publish the report to the
/// conclusion's `verify_json`, and announce the outcome. This is the one
/// conclusion of a verification whose two runs completed: `run --verify`
/// reaches it after its second run, and `hermit log-diff --verify-json` after
/// reading two `--verify-run` directories. A skid overshoot in either run is
/// published as the infrastructure error it is and returned as one.
pub(super) fn conclude_verification(
    conclusion: VerificationConclusion<'_>,
    run1: CompletedVerificationRun<'_>,
    run2: CompletedVerificationRun<'_>,
) -> Result<VerificationOutcome, Error> {
    let VerificationConclusion {
        verify_json,
        mut comparison_options,
        evidence,
        native_identity_diagnostic,
        backend,
        application_backend,
        passthru_opt,
        rewrite_exclusives,
    } = conclusion;
    let out1 = run1.output;
    let out2 = run2.output;
    let log1_path = run1.log;
    let log2_path = run2.log;
    let summary1 = run1.summary;
    let summary2 = run2.summary;
    // Say what was actually established. Buffer hashing is ON BY DEFAULT, so
    // this qualification is now reachable only when the caller has asked for
    // the weaker comparison with `--no-detlog-io-buffers`. With it, the
    // compared records carry no syscall output-buffer CONTENT -- Reverie
    // types many output buffers as bare pointers, so the record shows the
    // address and not the bytes -- and two runs whose buffers differ while
    // their return values agree compare equal. Measured on a netlink
    // `recvmsg` that returns a stable `Ok(1468)` while four payload bytes
    // vary, back when hashing was opt-in: this path reported "Determinism
    // verified" on a run that the same command with hashing enabled
    // reports as diverged. Claiming
    // determinism there was the defect, so the sentence now names its limit.
    let success_message = if !comparison_options.compare_io_buffers {
        // The "Determinism verified" marker is RETAINED verbatim and the
        // qualification appended after it. That is not politeness: ~95
        // Rust integration-test files under hermit-cli/tests assert on
        // that exact substring, so replacing the sentence would be a
        // project-wide contract change rather than a wording fix. Appending
        // keeps every consumer working while the reader still learns the
        // limit.
        "Success: deterministic. Determinism verified. NOTE: syscall \
         output-buffer CONTENT was not compared because \
         --no-detlog-io-buffers was given, so a divergence confined to a \
         buffer whose length is stable would not have been seen; drop that \
         flag to include it."
    } else {
        "Success: deterministic. Determinism verified."
    };
    let failure_message = "Failure: nondeterministic.";
    let skid_overshoots = run1.skid_overshoots.saturating_add(run2.skid_overshoots);
    if skid_overshoots > 0
        && let Some(path) = verify_json
    {
        write_skid_overshoot_without_comparison_json(
            path,
            skid_overshoots,
            verification_runtime_from_summaries(summary1.as_ref(), summary2.as_ref()),
            Some(out1.status),
        )?;
    }
    // A skid overshoot makes this verification an infrastructure error
    // below, whatever the comparison finds, so a log match is not the
    // verdict. `--keep-logs` then retains both logs, as every other error
    // path does, instead of the single golden log kept after a match.
    comparison_options.match_overridden |= skid_overshoots > 0;
    if let Some(comparison) = &evidence {
        comparison.configure(&mut comparison_options);
    }
    // The comparison consumes both logs, so a host file change is placed
    // in its run's log now, and judged against the divergence below.
    let host_input_change = if skid_overshoots == 0 {
        run1.host_inputs
            .zip(run2.host_inputs)
            .and_then(|(first, second)| {
                place_host_input_change(first, second, [&log1_path, &log2_path])
            })
    } else {
        None
    };
    let mut outcome = compare_two_runs(
        ComparedRun {
            output: out1,
            log: log1_path,
            // Accurate on this path: `verify` calls `run_verify` twice, and
            // each call builds its own container and guest command and
            // reaches a separate tracer spawn, so these really are two
            // fresh executions of the guest.
            label: "run 1",
        },
        ComparedRun {
            output: out2,
            log: log2_path,
            label: "run 2",
        },
        comparison_options,
    )?;
    let native_messages = evidence.map(|comparison| comparison.apply(&mut outcome));
    outcome.runtime = verification_runtime_from_summaries(summary1.as_ref(), summary2.as_ref());
    // A divergence that failed store-exclusives explain is named as such
    // (aarch64): reporting only, the verdict stays what the comparison found.
    outcome.store_exclusive_retries = store_exclusive_retries(
        outcome.verdict,
        summary1.as_ref(),
        summary2.as_ref(),
        rewrite_exclusives,
    );

    // A divergence that a host file changing during one run explains is
    // named as such: only a divergence is examined, and only a change
    // found before the divergence, by a comparison that compared
    // everything, explains it. On a backend that can run guest code
    // before Hermit observes it, an identical log prefix does not show
    // that the guest did not make the change itself, so the change is
    // reported without being named as the cause.
    let host_input_change = host_input_change
        .filter(|_| outcome.verdict == Verdict::Diverged)
        .filter(|(_, positions)| {
            explains_divergence(
                outcome.comparison.is_bitwise_parity(),
                *positions,
                outcome.first_divergent_record_positions,
            )
        })
        .map(|(difference, _)| difference.into_infrastructure_error())
        .filter(|change| {
            let unobserved = application_backend
                .map(|_| "execution before an external adapter observes the guest is not qualified")
                .or_else(|| unobserved_guest_execution(backend, passthru_opt));
            if let Some(reason) = unobserved {
                eprintln!("HERMIT_HOST_INPUT_CHANGE_UNATTRIBUTED {change}; not named as the cause: {reason}");
            }
            unobserved.is_none()
        });

    // Emit the machine-readable verdict (if requested) before collapsing the
    // outcome to the historical exit-code convention. The verdict is recorded
    // whether or not the runs matched, and independent of the guest's own
    // exit status.
    if let Some(path) = verify_json {
        if skid_overshoots > 0 {
            write_skid_overshoot_verification_json(path, &outcome, skid_overshoots)?;
        } else if let Some(change) = &host_input_change {
            write_host_input_change_verification_json(path, &outcome, change.clone())?;
        } else {
            write_verification_json(path, &outcome)?;
        }
    }
    if skid_overshoots > 0 {
        return Err(Error::new(SkidOvershootError::new(skid_overshoots)));
    }
    if let Some(messages) = native_messages {
        for message in messages {
            eprintln!(":: {message}");
        }

        if let Some(diagnostic) = native_identity_diagnostic {
            eprintln!(":: {diagnostic}");
        }
    }
    announce_verification_outcome(&outcome, SecondRun::Rerun, success_message, failure_message);
    if let Some(change) = &host_input_change {
        // The divergence stays a failure; this names its cause.
        eprintln!("HERMIT_HOST_INPUT_CHANGED {change}");
    }
    if host_input_change.is_some() {
        // The published verdict is that infrastructure error, not a
        // divergence, so no cause of a divergence is named.
        outcome.store_exclusive_retries = None;
    }
    if let Some(retries) = &outcome.store_exclusive_retries {
        // Likewise: a possible cause named, the verdict unchanged.
        eprintln!(
            "HERMIT_STORE_EXCLUSIVE_RETRIES {}",
            store_exclusive_retries_line(retries)
        );
    }
    Ok(outcome)
}

/// Represents a tmpfs location. There are different ways to construct `/tmp` for
/// the container and this encapsulates all of them.
enum Tmpfs {
    /// Use an existing path as `/tmp`.
    Path(PathBuf),

    /// Use a new temporary directory as `/tmp`.
    Temp(tempfile::TempDir),
}

impl Tmpfs {
    /// Returns the path to `/tmp`.
    pub fn path(&self) -> &Path {
        match self {
            Self::Path(path) => path,
            Self::Temp(temp) => temp.path(),
        }
    }
}

/// Complete `config` inside the container whose identity mounts are
/// `identity_sources`, or, with none (`--no-namespace`), in this process's own
/// namespace, on every backend. Either way the config gets a container mount
/// table, so Detcore renders the guest's mount files. The guest inherits this thread's
/// seccomp filters, as Hermit installs none besides the backend's own.
pub(super) fn complete_container_config(
    mut config: DetConfig,
    identity_sources: Option<&IdentityGuard>,
    launch_workdir: Option<&Path>,
) -> Result<DetConfig, Error> {
    config.inherited_seccomp = detcore::InheritedSeccomp::measure();
    // Each run of `--verify` is its own container, whose namespace numbers
    // its mounts afresh, so each builds its own table, from the same plan and
    // the same host facts. The table, the identity order and the namespace
    // they name come from one read (`IdentityGuard::container_mount_view`),
    // the snapshot a backend that checks its provenance keeps.
    let view = identity_sources
        .map(IdentityGuard::container_mount_view)
        .transpose()?;
    config.mount_id_assignment_order.clear();
    config.mountinfo_mount_ids_captured = true;
    // The placeholder directory the container's setup mounted, if any, and
    // whether the guest runs in a container with a procfs of its own; neither
    // without a container (`--no-namespace`), where Detcore serves no `/proc`
    // directory.
    config.directory_placeholder = identity_sources
        .and_then(IdentityGuard::directory_placeholder)
        .map(Path::to_path_buf);
    config.container_procfs = identity_sources.is_some();
    config.host_input_snapshots = identity_sources
        .map(IdentityGuard::host_input_snapshots)
        .unwrap_or_default();
    match view {
        Some(view) => {
            config.mountinfo_root_rewrites = view.root_rewrites;
            config.mountinfo_mount_ids = view.order;
            config.mountinfo_mount_namespace = Some(view.namespace);
            config.container_mount_table = view.table;
        }
        // No container (`--no-namespace`): the guest runs in this process's
        // own namespace, where Hermit installed no mount, so its table is
        // that namespace's launch mounts (the guest's working directory's and
        // /tmp's) and base mountpoints, with the order from the same read.
        // Nothing here changes this process's namespace.
        None => {
            let namespace = hermit::current_mount_namespace()?;
            // `--workdir`, or the working directory the guest inherits ("."),
            // resolved as the guest's chdir resolves it: an absolute one
            // without Hermit's own working directory, and neither by asking
            // that directory for its name (it may have been removed).
            let launch_cwd = launch_workdir.unwrap_or(Path::new("."));
            let (table, order) = hermit::unplanned_container_mount_table(launch_cwd)?;
            config.mountinfo_root_rewrites = Vec::new();
            config.mountinfo_mount_ids = order;
            config.mountinfo_mount_namespace = Some(namespace);
            config.container_mount_table = Some(table);
        }
    }
    Ok(config)
}

/// Names recording `id` as the newest in `data_dir` (see
/// [`hermit::HermitData::note_latest`]). Best effort: the recording stays
/// reachable by its ID.
fn note_latest(data_dir: &Path, id: hermit::Id, asked: bool) {
    if let Err(error) = hermit::HermitData::with_dir(data_dir).note_latest(&id, asked) {
        crate::app::tracing::diagnostic_eprintln!(
            "hermit: could not name recording {id} as the newest in {}: {error:#}",
            data_dir.display()
        );
    }
}

/// Writes `DATA/<id>/manifest.json` for the network recording whose
/// manifest is `sibling`, naming its trace by absolute path, and names it
/// the newest recording.
fn store_network_manifest(
    data_dir: &Path,
    sibling: &Path,
    mut document: crate::app::manifest::Manifest,
) -> Result<(), Error> {
    let trace = document
        .artifacts
        .as_mut()
        .and_then(|artifacts| artifacts.network_trace.as_mut())
        .ok_or_else(|| anyhow::anyhow!("the network manifest names no trace"))?;
    let file = std::path::absolute(sibling.with_file_name(&trace.path))?;
    trace.path = file
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("network trace path {} is not UTF-8", file.display()))?
        .to_owned();
    let id: hermit::Id = document
        .id
        .parse()
        .with_context(|| format!("the manifest's id {:?} is not a recording ID", document.id))?;
    crate::app::manifest::write(
        &data_dir
            .join(id.to_string())
            .join(crate::app::manifest::MANIFEST_NAME),
        &document,
    )?;
    note_latest(data_dir, id, document.kind.moves_last());
    crate::app::manifest::prune(data_dir, crate::app::manifest::KEEP_MANIFEST_ONLY);
    Ok(())
}

// The four physical MM controls use the same namespace/root/identity launcher
// as the real flag; no direct bare-host mapped_run substitute or invented PID.
#[cfg(all(test, feature = "mm-cancellation-test-support"))]
pub(crate) fn physical_mm_cancellation_control(
    point: detcore::initial_mm_probe::Point,
) -> anyhow::Result<()> {
    let mut options = RunOpts::try_parse_from([
        "run",
        "--strict",
        "--in-guest-exec-stop",
        "--no-rcb-time",
        "--max-timeslice=disabled",
        "--epoch=2026-01-01T00:00:00Z",
        "--seed=0",
        "--sched-seed=0",
        "--rng-seed=0",
        "--",
        "/bin/true",
    ])?;
    options.set_backend(Some(Backend::InGuestTrap));
    options.validate_args()?;
    anyhow::ensure!(
        !options.no_namespace,
        "MM physical control requires real PID namespace"
    );
    let image = std::sync::Arc::new(super::mapped_first_admission::RetainedImage::open()?);
    let held_image = image.clone();
    let work_options = options.clone();
    let result = options.launch_in_container(
        held_image,
        "same retained runtime image for physical MM cancellation",
        None,
        move |image, identity, native| {
            anyhow::ensure!(
                native.is_none(),
                "MM control cannot consume native enrollment"
            );
            anyhow::ensure!(
                identity.is_some(),
                "MM control requires actual container identity"
            );
            let command = work_options.guest_command()?;
            let config = complete_container_config(
                work_options.effective_det_config(),
                identity,
                work_options.launch_workdir(),
            )?;
            let mut config = hermit::prepare_in_guest_loader_route_config(config)?;
            config.in_guest_detlog_forward_policy =
                Some(detcore::detlog::ForwardPolicy::all().encode());
            super::mapped_first_admission::cancel_mm_in_container(point, command, config, image)
        },
    );
    // Close the SAME outer image even if the real container/control returned
    // Err; never replace a physical failure with an image verification success.
    let close = image.verify();
    match (result, close) {
        (Ok(((), _)), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(close)) => Err(close.into()),
        (Err(error), Err(close)) => Err(anyhow::anyhow!(
            "physical MM={error:#}; image close={close:#}"
        )),
    }
}

/// Logs a successful x86-64-v3 admission.
#[cfg(target_arch = "x86_64")]
fn log_v3_admission(probe: &hermit::xstate_probe::ProbeReport) {
    tracing::info!(
        "--cpu-profile x86-64-v3 admitted: XINUSE probe on CPU {}, XCR0 {:#x}, {} cases, \
         {} certified pairs",
        probe.cpu,
        probe.xcr0,
        probe.cases,
        probe.certified_pairs
    );
}

/// No x86-64-v3 admission succeeds off x86 (`cpu_profile_admission::Qualification`).
#[cfg(not(target_arch = "x86_64"))]
fn log_v3_admission(probe: &std::convert::Infallible) {
    match *probe {}
}

#[cfg(test)]
mod tests {
    #[test]
    fn native_verification_run_path_binds_terminal_verdict_to_branch_stats() {
        let common = include_str!("run.rs")
            .split_once("    pub(super) fn verify_with_evidence<")
            .unwrap()
            .1
            .split_once("    /// The `--bind`s")
            .unwrap()
            .0;
        let at = |needle| {
            common
                .find(needle)
                .unwrap_or_else(|| panic!("missing common owner boundary: {needle}"))
        };
        let run1 = at("&run1_options,");
        let signal1 = at("signal_reports.prepare(\"run 1\"");
        let latch1 = at("latches1.refuse_comparison");
        let rejected = at("if let Some(error) = reject_first_run(");
        let stats1 = at("finish(pending1, &out1, VerificationRun::Run1)");
        let run2 = at("&run2_options,");
        let signal2 = at("signal_reports.prepare(\"run 2\"");
        let latch2 = at("latches2.refuse_comparison");
        let stats2 = at("finish(pending2, &out2, VerificationRun::Run2)");
        let comparison = at("super::verify::NativeEvidenceComparison::new(");
        let conclude = at("let outcome = conclude_verification(");
        let exit = at("return outcome.into_exit_status()");
        assert!(
            run1 < signal1
                && signal1 < latch1
                && latch1 < rejected
                && rejected < stats1
                && stats1 < run2
        );
        assert!(
            run2 < signal2
                && signal2 < latch2
                && latch2 < stats2
                && stats2 < comparison
                && comparison < conclude
                && conclude < exit
        );
        let conclusion = include_str!("run.rs")
            .split_once("pub(super) fn conclude_verification(")
            .unwrap()
            .1;
        let boundary = |needle| conclusion.find(needle).unwrap();
        let retention = boundary("comparison.configure(&mut comparison_options)");
        let compare = boundary("let mut outcome = compare_two_runs(");
        let apply = boundary("comparison.apply(&mut outcome)");
        let publish = boundary("write_verification_json(path, &outcome)");
        let announce = boundary("if let Some(messages) = native_messages");
        assert!(retention < compare && compare < apply && apply < publish && publish < announce);
        let offline = include_str!("verify_run.rs")
            .split_once("fn compare_with_application(")
            .unwrap()
            .1
            .split_once("fn compare_recording_and_replay(")
            .unwrap()
            .0;
        assert!(
            offline.find("let logs = [link(").unwrap()
                < offline.find("native_evidence(&run1, &run2)").unwrap()
        );
        assert!(
            offline.find("native_evidence(&run1, &run2)").unwrap()
                < offline.find("conclude_verification(").unwrap()
        );
        assert!(offline.contains("fs::hard_link(&source, &target)"));
        assert!(exit < at("signal_reports.verified()"));
        let rejection = &common[rejected..stats1];
        assert!(rejection.contains("drop(signal_reports)"));
        let reject = include_str!("run.rs")
            .split_once("pub(super) fn reject_first_run(")
            .unwrap()
            .1
            .split_once("pub(super) fn verify_run_outcome_of(")
            .unwrap()
            .0;
        assert!(reject.contains("first_run_rejected_report("));
        assert!(reject.contains("write_report_json(path, &report)"));
        for (begin, end) in [(stats1, run2), (stats2, comparison)] {
            let refusal = &common[begin..end];
            assert!(refusal.contains("publish_run_failure("));
            assert!(refusal.contains("retain_logs_after_verification_error("));
            assert!(refusal.contains("return Err(error)"));
        }
    }

    #[test]
    fn common_verifier_checks_first_run_disposition_before_native_finish() {
        for (case, status, loss) in [
            ("latch", 0, Some("injected run-1 loss")),
            ("rejected", 23, None),
            ("stats", 0, None),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let report_path = directory.path().join("verdict.json");
            let logs = directory.path().join("logs");
            let mut options = RunOpts::parse_from([
                "run",
                "--verify",
                "--verify-allow=success",
                "--keep-logs",
                "--verify-log-dir",
                logs.to_str().unwrap(),
                "--verify-json",
                report_path.to_str().unwrap(),
                "--",
                "/bin/true",
            ]);
            options
                .select_application_backend("native-policy-control")
                .unwrap();
            write_pending_verification_json(&report_path).unwrap();
            let finish_calls = std::cell::RefCell::new(Vec::new());
            let mut calls = Vec::new();
            let mut log_path = None;
            let error = options
                .verify_with_evidence(
                    VerificationEvidence {
                        requirement:
                            super::super::backend_hook::EvidenceRequirement::NativeCounters,
                        records_run_metadata: false,
                        transport: Some(
                            super::super::backend_hook::VerificationLogTransport::NativeEvidence,
                        ),
                        native_identity_diagnostic: None,
                    },
                    |_, mut log, path, forwarded, run| {
                        calls.push(run);
                        assert_eq!(run, VerificationRun::Run1, "{case}: run 2 must not start");
                        assert!(forwarded.is_none());
                        log.write_all(b"retained first-run evidence\n")?;
                        log_path = Some(path.to_path_buf());
                        Ok(super::super::backend_hook::PhysicalRun {
                            output: reverie::process::Output {
                                status: ExitStatus::Exited(status),
                                stdout: Vec::new(),
                                stderr: Vec::new(),
                            },
                            skid_overshoots: 0,
                            latches: super::super::run::RunLatches {
                                determinism_loss: loss.map(str::to_owned),
                                log_sink_failure: None,
                            },
                            terminal_outcome: None,
                            pending: (),
                        })
                    },
                    |(), _, run| {
                        finish_calls.borrow_mut().push(run);
                        // The extension's actual missing sink/decoder/finish
                        // control owns this typed marker and cause. Here the
                        // real common driver must reach it only after its
                        // latch and first-run-disposition checks.
                        Err::<(), _>(
                            Error::new(hermit::BackendRunFailure)
                                .context("typed whole-process statistics were missing"),
                        )
                    },
                    |_: &(), _: &()| unreachable!("these cases refuse before pair comparison"),
                )
                .unwrap_err();
            assert_eq!(calls, [VerificationRun::Run1]);
            assert_eq!(
                *finish_calls.borrow(),
                if case == "stats" {
                    vec![VerificationRun::Run1]
                } else {
                    Vec::new()
                },
                "{case}: native completion must follow both first-run refusal phases"
            );
            assert_eq!(
                fs::read(log_path.unwrap()).unwrap(),
                b"retained first-run evidence\n"
            );
            let report: serde_json::Value =
                serde_json::from_slice(&fs::read(report_path).unwrap()).unwrap();
            assert_eq!(report["verdict"], "no_result");
            assert_eq!(report["verified"], false);
            let reason = &report["no_result_reason"];
            let detail = format!("{error:#}");
            match case {
                "latch" => {
                    assert_eq!(reason["failure"], "determinism_loss");
                    assert_eq!(reason["run"], "run1");
                    assert!(detail.contains("injected run-1 loss"), "{detail}");
                    assert!(!detail.contains("statistics were missing"), "{detail}");
                }
                "rejected" => {
                    assert_eq!(reason["kind"], "first_run_rejected");
                    assert_eq!(report["guest_exit_code"], 23);
                    assert!(detail.contains("First run during --verify"), "{detail}");
                    assert!(!detail.contains("statistics were missing"), "{detail}");
                }
                "stats" => {
                    assert_eq!(reason["kind"], "run_failed");
                    assert_eq!(reason["failure"], "backend_failure");
                    assert_eq!(reason["run"], "run1");
                    assert!(detail.contains("statistics were missing"), "{detail}");
                }
                _ => unreachable!(),
            }
        }
    }
    #[test]
    fn common_verifier_retains_run1_when_second_native_completion_fails() {
        let directory = tempfile::tempdir().unwrap();
        let report_path = directory.path().join("verdict.json");
        let logs = directory.path().join("logs");
        let mut options = RunOpts::parse_from([
            "run",
            "--verify",
            "--verify-allow=both",
            "--keep-logs",
            "--verify-log-dir",
            logs.to_str().unwrap(),
            "--verify-json",
            report_path.to_str().unwrap(),
            "--",
            "/bin/true",
        ]);
        options
            .select_application_backend("native-policy-control")
            .unwrap();
        write_pending_verification_json(&report_path).unwrap();
        let finishes = std::cell::RefCell::new(Vec::new());
        let mut runs = Vec::new();
        let error = options
            .verify_with_evidence(
                VerificationEvidence {
                    requirement: super::super::backend_hook::EvidenceRequirement::NativeCounters,
                    records_run_metadata: false,
                    transport: Some(
                        super::super::backend_hook::VerificationLogTransport::NativeEvidence,
                    ),
                    native_identity_diagnostic: None,
                },
                |_, mut log, path, forwarded, run| {
                    assert!(forwarded.is_none());
                    let bytes = format!("completed {run:?}: guest exit 23\n");
                    log.write_all(bytes.as_bytes())?;
                    runs.push((run, path.to_path_buf(), bytes));
                    Ok(super::super::backend_hook::PhysicalRun {
                        output: reverie::process::Output {
                            status: ExitStatus::Exited(23),
                            stdout: Vec::new(),
                            stderr: Vec::new(),
                        },
                        skid_overshoots: 0,
                        latches: super::super::run::RunLatches::default(),
                        terminal_outcome: None,
                        pending: run,
                    })
                },
                |pending, output, run| {
                    assert_eq!(pending, run);
                    assert_eq!(output.status, ExitStatus::Exited(23));
                    finishes.borrow_mut().push(run);
                    if run == VerificationRun::Run1 {
                        // The paired W control below derives these exact facts
                        // from the original encoded record through its REAL
                        // sink, decoder and BackendAdapter::finish. This core
                        // fixture tests the common driver's deferred ownership.
                        return Ok(super::super::backend_hook::NativeFacts {
                            counted_branches: 1,
                            intercepted_syscalls: 1,
                            rewritten_syscalls: 1,
                            stdin_reads: 0,
                            memory_hash_fold: 0,
                            initialization_records: 1,
                            statistics_diagnostic: "native first-run control".to_owned(),
                        });
                    }
                    Err(Error::new(hermit::BackendRunFailure)
                        .context("typed whole-process statistics were missing"))
                },
                |_, _| unreachable!("missing run2 facts must refuse before comparison"),
            )
            .unwrap_err();
        assert_eq!(
            *finishes.borrow(),
            [VerificationRun::Run1, VerificationRun::Run2],
            "both completions must use their original deferred run identity"
        );
        assert_eq!(
            runs.iter().map(|(run, _, _)| *run).collect::<Vec<_>>(),
            [VerificationRun::Run1, VerificationRun::Run2]
        );
        for (_, path, bytes) in runs {
            assert_eq!(fs::read_to_string(path).unwrap(), bytes);
        }
        assert!(format!("{error:#}").contains("statistics were missing"));
        let report: serde_json::Value =
            serde_json::from_slice(&fs::read(report_path).unwrap()).unwrap();
        assert_eq!(report["verdict"], "no_result");
        assert_eq!(report["no_result_reason"]["kind"], "run_failed");
        assert_eq!(report["no_result_reason"]["failure"], "backend_failure");
        assert_eq!(report["no_result_reason"]["run"], "run2");
        // The existing failure schema does not invent a guest status or
        // comparison from incomplete facts. Both real per-run logs survive.
        assert!(report["guest_exit_code"].is_null());
        assert!(report["comparison"].is_null());
    }

    use clap::CommandFactory;

    use super::*;
    use crate::app::container_clone::CheckedClone;

    /// The outer package gate owns source/qualification binding. This selected
    /// test requires its actual installation and the regular-file stdin-cat
    /// fixture; neither missing operand is a reason to skip a real execution.
    #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
    #[test]
    #[ignore = "requires the fresh qualified private runtime and piped_stdin_cat fixture; run the exact package gate"]
    fn mapped_verification_retains_actual_package_after_parent_rename() {
        use std::os::unix::fs::MetadataExt;
        use std::process::Stdio;
        use std::sync::Arc;

        use super::super::mapped_first_admission::RetainedImage;

        const NAME: &str =
            "app::run::tests::mapped_verification_retains_actual_package_after_parent_rename";
        const CHILD: &str = "HERMIT_RETAINED_PACKAGE_TEST_CHILD";
        const INSTALL: &str = "HERMIT_RETAINED_PACKAGE_TEST_INSTALL";
        const PROGRAM: &str = "HERMIT_RETAINED_PACKAGE_TEST_PROGRAM";
        const INPUT: &[u8] = b"first\0line\nlast-without-newline\xff";
        const MEMBERS: [&str; 2] = [
            "detcore_private_runtime.elf",
            "detcore_private_runtime.package.json",
        ];

        let required_path = |name: &str| {
            let path = std::env::var_os(name)
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| panic!("{name} must name an actual package-gate input"));
            fs::canonicalize(path).unwrap_or_else(|error| panic!("{name}: {error}"))
        };
        let program = required_path(PROGRAM);
        assert!(
            program.is_file(),
            "{PROGRAM} must name the stdin-cat fixture"
        );

        if std::env::var(CHILD).ok().as_deref() != Some(NAME) {
            let source = required_path(INSTALL);
            let directory = tempfile::tempdir().unwrap();
            let install = directory.path().join("selected-install");
            fs::create_dir_all(install.join("rsrcs")).unwrap();
            for member in MEMBERS {
                let input = source.join("rsrcs").join(member);
                assert!(
                    input.is_file(),
                    "required package member: {}",
                    input.display()
                );
                fs::copy(&input, install.join("rsrcs").join(member)).unwrap();
            }
            // Child-local discovery and a test-owned copy keep the rename from
            // changing either another test's environment or the producer files.
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    NAME,
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(CHILD, NAME)
                .env(PROGRAM, &program)
                .env(hermit_resources::INSTALL_DIR_ENV, &install)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let written = child.stdin.take().unwrap().write_all(INPUT);
            if written.is_err() {
                let _ = child.kill();
            }
            // wait_with_output drains both streams and reaps the isolated
            // child; the package gate owns its wall/CPU/memory/pid bounds.
            let output = child.wait_with_output().unwrap();
            assert!(
                written.is_ok(),
                "stdin write failed: {written:?}; {output:?}"
            );
            assert!(output.status.success(), "{output:?}");
            assert!(
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .any(|line| line.starts_with(
                        "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured;"
                    )),
                "the exact ignored child test must actually execute: {output:?}"
            );
            assert_eq!(
                output
                    .stdout
                    .windows(INPUT.len())
                    .filter(|bytes| *bytes == INPUT)
                    .count(),
                1,
                "the real verifier must emit the complete guest stdout once: {output:?}"
            );
            // Retain the actual comparison in the outer gate's captured log.
            let report = fs::read_to_string(directory.path().join("verification.json")).unwrap();
            eprintln!("retained package verification report: {report}");
            return;
        }

        let install = required_path(hermit_resources::INSTALL_DIR_ENV);
        let directory = install.parent().unwrap();
        let report = directory.join("verification.json");
        let held = directory.join("retained-install");
        let original = RetainedImage::open().expect("open the actual provided runtime package");
        let (expected, files) = original.export_pair_inputs().unwrap();
        let image = Arc::new(
            RetainedImage::adopt_pair_inputs(expected.clone(), files)
                .expect("adopt the original runtime and descriptor OFDs once"),
        );
        drop(original);
        let retained_fds = image.pair_fds().map(|fd| fd.as_raw_fd());

        // Exercise the real capture entry with the actual retained package.
        // Requested-PID refusal must precede both the expired qualification
        // deadline and any rewind of the shared verification stdin snapshot.
        // This negative control creates no guest and never enters the image.
        {
            use crate::app::mapped_first_admission::CapturePolicy;
            use crate::app::mapped_first_admission::VerificationCapture;
            use crate::app::mapped_first_admission::capture_verification_in_container;

            let mut command = Command::new("/not-an-executable/mapped-command-control");
            command.pid(reverie::process::Pid::from_raw(123));
            let mut forwarded = ForwardedDetlogPlan::Socket { opened: false };
            let error = capture_verification_in_container(VerificationCapture {
                command,
                config: detcore::Config::default(),
                retained: &image,
                records: Box::new(|_| panic!("refused command must emit no guest records")),
                print_summary: false,
                summary_json: &None,
                policy: CapturePolicy::Qualification {
                    deadline: tokio::time::Instant::now() - std::time::Duration::from_secs(1),
                },
                forwarded: &mut forwarded,
            })
            .unwrap_err();
            let error = error.downcast_ref::<std::io::Error>().unwrap();
            assert_eq!(error.kind(), std::io::ErrorKind::Other);
            assert_eq!(
                error.to_string(),
                "mapped-command-requested-pid-unsupported"
            );
            assert!(matches!(
                forwarded,
                ForwardedDetlogPlan::Socket { opened: false }
            ));
            image.verify().unwrap();
        }

        let mut options = run_opts_for(&[
            "hermit",
            "--backend=in-guest-trap",
            "--log=info",
            "run",
            "--in-guest-exec-stop",
            "--strict",
            "--verify",
            "--verify-strict",
            detcore_model::config::CANONICAL_TEST_EPOCH_ARG,
            "--seed=0",
            "--sched-seed=0",
            "--rng-seed=0",
            "--max-timeslice=disabled",
            "--no-rcb-time",
            "--base-env=empty",
            "--verify-json",
            report.to_str().unwrap(),
            "--",
            program.to_str().unwrap(),
        ]);
        let global = GlobalOpts::parse_from(["hermit", "--backend=in-guest-trap", "--log=info"]);
        // Use real admission and argument validation while initial discovery
        // still exists, just as the production pre-retention path does.
        options.validate_args().unwrap();
        options.refuse_strict_with_host_drawn_inputs().unwrap();
        options.validate_program().unwrap();
        options.validate_mount_sources().unwrap();
        validate_log_level(&global).unwrap();
        hermit::validate_tracing_environment(hermit::NamespaceUse::Creates).unwrap();
        hermit::reserve_output_stdin_snapshot(super::super::startup_stdin().unwrap()).unwrap();
        write_pending_verification_json(&report).unwrap();
        let options = options.with_verification_ld_so_cache().unwrap();
        assert!(options.mapped_shared_image.is_none());

        let inode_state = |path: &Path| {
            let m = fs::metadata(path).unwrap();
            [
                m.dev(),
                m.ino(),
                m.len(),
                u64::from(m.mode()),
                m.mtime() as u64,
                m.mtime_nsec() as u64,
                m.ctime() as u64,
                m.ctime_nsec() as u64,
            ]
        };
        let before = MEMBERS.map(|name| inode_state(&install.join("rsrcs").join(name)));
        assert_eq!(before[0], expected.image.state);
        assert_eq!(before[1], expected.descriptor.state);
        // Moving the ancestor removes both discovery paths without changing
        // either retained file's ctime. File rename/unlink would test mutation.
        fs::rename(&install, &held).unwrap();
        for (index, name) in MEMBERS.iter().enumerate() {
            assert!(!install.join("rsrcs").join(name).exists());
            assert_eq!(inode_state(&held.join("rsrcs").join(name)), before[index]);
        }
        assert!(!super::super::mapped_first_admission::packaged_image_declared());
        image.verify().unwrap();

        // This is the production pair owner, including both physical runs,
        // full capture, all admission checks and the unchanged comparator.
        let status = options
            .verify_with_retained_adapters(
                &global,
                None,
                &super::super::core_adapter::CoreApplicationAdapter,
                (),
                Some(image.clone()),
            )
            .unwrap();
        assert_eq!(status, ExitStatus::Exited(0));

        let verdict: serde_json::Value =
            serde_json::from_slice(&fs::read(&report).unwrap()).unwrap();
        assert_eq!(verdict["verdict"], "matched", "{verdict}");
        assert_eq!(verdict["verified"], true, "{verdict}");
        assert_eq!(verdict["bitwise_parity"], true, "{verdict}");
        assert_eq!(
            verdict["comparison"]["strictness"], "canonical",
            "{verdict}"
        );
        assert_eq!(
            verdict["comparison"]["compare_io_buffers"], true,
            "{verdict}"
        );
        let compared = &verdict["compared_log_messages"];
        assert!(
            compared["left"].as_u64().is_some_and(|count| count > 0)
                && compared["left"] == compared["right"],
            "both actual runs must compare nonempty INFO streams: {verdict}"
        );
        for side in ["left", "right"] {
            let actual = &verdict["compared_outputs"][side];
            assert_eq!(actual["exit_code"], 0, "{side}: {verdict}");
            assert!(actual["signal"].is_null(), "{side}: {verdict}");
            assert_eq!(actual["stdout_bytes"], INPUT.len(), "{side}: {verdict}");
            assert_eq!(
                actual["stdout_sha256"],
                detcore::Digest::new(INPUT).to_string(),
                "{side}: {verdict}"
            );
            assert_eq!(actual["stderr_bytes"], 0, "{side}: {verdict}");
            assert_eq!(
                actual["stderr_sha256"],
                detcore::Digest::new(b"").to_string(),
                "{side}: {verdict}"
            );
        }
        image.verify().unwrap();
        for (index, name) in MEMBERS.iter().enumerate() {
            assert!(!install.join("rsrcs").join(name).exists());
            assert_eq!(inode_state(&held.join("rsrcs").join(name)), before[index]);
        }
        assert_eq!(
            Arc::strong_count(&image),
            1,
            "both run owners must have retired"
        );
        drop(image);
        for fd in retained_fds {
            // SAFETY: F_GETFD only queries whether this descriptor is open.
            assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EBADF)
            );
        }
    }

    /// Exercise the actual saved writer and its per-run descriptor metadata.
    /// The physical observations below are synthetic, not a guest qualification.
    #[test]
    fn saved_capture_keeps_private_metadata_transport_and_record_last_commit() {
        use super::super::backend_hook::PhysicalRun;
        use super::super::backend_hook::PreparedPhysical;
        use super::super::backend_hook::VerificationCapture;
        use super::super::backend_hook::VerificationLogTransport;

        for transport in [
            VerificationLogTransport::CoordinatorTracing,
            VerificationLogTransport::InGuestSocket,
        ] {
            let parent = tempfile::tempdir().unwrap();
            let directory = parent.path().join("saved");
            let mut options = run_opts_for(&["hermit", "run", "--", "/bin/true"]);
            options.select_application_backend("saved-control").unwrap();
            let physical =
                PreparedPhysical::new(&options, hermit::backend_capabilities(Backend::Ptrace));
            assert!(!physical.config().record_host_inputs);
            assert!(physical.config().host_input_log.is_none());
            let summary = b"{\"saved_owner_control\":true}\n";
            let host_inputs = b"{\"records\":0}\n";
            let finished = std::cell::Cell::new(false);
            let result = options.verify_run_with_execution(
                &directory,
                Some(transport),
                false,
                |working, log, path, forwarded| {
                    assert!(!directory.join(verify_run::RECORD).exists());
                    assert_eq!(forwarded.requires_socket(),
                        transport == VerificationLogTransport::InGuestSocket);
                    let summary_path = working.summary_json.as_ref().unwrap();
                    let host_inputs_path = working.host_input_log.as_ref().unwrap();
                    assert_ne!(summary_path, &directory.join(verify_run::SUMMARY));
                    assert_ne!(host_inputs_path, &directory.join(verify_run::HOST_INPUTS));
                    fs::write(summary_path, summary)?;
                    fs::write(host_inputs_path, host_inputs)?;
                    let mut capture = VerificationCapture {
                        log,
                        path,
                        run: VerificationRun::Run1,
                        options: working,
                        physical: Some(&physical),
                        requested_log: None,
                        log_budget: None,
                        log_transport: transport,
                        forwarded_detlogs: forwarded,
                    };
                    let actual = capture.config();
                    assert!(actual.record_host_inputs);
                    assert_eq!(actual.host_input_log.as_ref(), Some(host_inputs_path));
                    assert_eq!(actual.backend, physical.config().backend);
                    let line = if transport == VerificationLogTransport::InGuestSocket {
                        format!("{NORMALIZED_FORWARDED_DETLOG_TIMESTAMP} INFO detcore: DETLOG [syscall] saved control\n")
                    } else {
                        "1970-01-01T00:00:00.000000Z INFO detcore: saved control\n".to_owned()
                    };
                    std::io::Write::write_all(&mut capture.log, line.as_bytes())?;
                    Ok(PhysicalRun {
                        output: Output {
                            status: ExitStatus::Exited(0),
                            stdout: b"saved stdout".to_vec(),
                            stderr: b"saved stderr".to_vec(),
                        },
                        skid_overshoots: 0,
                        latches: RunLatches::default(),
                        terminal_outcome: None,
                        pending: (),
                    })
                },
                |(), _| {
                    assert!(!directory.join(verify_run::RECORD).exists());
                    finished.set(true);
                    Ok(None)
                },
            );
            assert_eq!(result.unwrap(), ExitStatus::Exited(0));
            assert!(finished.get());
            let record = verify_run::VerifyRunRecord::read(&directory).unwrap();
            assert_eq!(record.backend, "saved-control");
            assert_eq!(record.role, verify_run::RunRole::Run);
            assert!(record.dbt.is_none());
            assert_eq!(
                record.outcome,
                verify_run::VerifyRunOutcome::Completed {
                    status: verify_run::RecordedStatus::Exited { code: 0 },
                    skid_overshoots: 0,
                    forwarded_syscalls: (transport == VerificationLogTransport::InGuestSocket)
                        .then_some(1),
                    guest_deadlock: false,
                    virtual_timeout_ns: None,
                }
            );
            assert_eq!(
                fs::read(directory.join(verify_run::STDOUT)).unwrap(),
                b"saved stdout"
            );
            assert_eq!(
                fs::read(directory.join(verify_run::STDERR)).unwrap(),
                b"saved stderr"
            );
            assert_eq!(
                fs::read(directory.join(verify_run::SUMMARY)).unwrap(),
                summary
            );
            assert_eq!(
                fs::read(directory.join(verify_run::HOST_INPUTS)).unwrap(),
                host_inputs
            );
            assert!(!directory.join(verify_run::DBT_STATS).exists());
        }
    }

    #[test]
    fn saved_capture_preserves_latch_finish_and_payload_failure_order() {
        use hermit::canonical_verdict::RunFailure;

        use super::super::backend_hook::PhysicalRun;
        use super::super::backend_hook::SavedNativeEvidence;
        use super::super::backend_hook::VerificationLogTransport;

        for failure in [None, Some("latch"), Some("finish"), Some("payload")] {
            let parent = tempfile::tempdir().unwrap();
            let directory = parent.path().join("saved");
            let mut options = run_opts_for(&["hermit", "run", "--", "/bin/true"]);
            options.select_application_backend("saved-control").unwrap();
            let finished = std::cell::Cell::new(false);
            let raw = b"synthetic writer payload; not native-runtime qualification";
            let result = options.verify_run_with_execution(
                &directory,
                Some(VerificationLogTransport::NativeEvidence),
                true,
                |working, mut log, _, forwarded| {
                    assert!(working.summary_json.is_none());
                    assert!(working.host_input_log.is_none());
                    assert!(!forwarded.requires_socket());
                    assert!(!directory.join(verify_run::RECORD).exists());
                    std::io::Write::write_all(&mut log, b"synthetic saved log\n")?;
                    if failure == Some("payload") {
                        fs::write(directory.join(verify_run::STDOUT), b"existing payload")?;
                    }
                    Ok(PhysicalRun {
                        output: Output {
                            status: ExitStatus::Exited(0),
                            stdout: b"out".to_vec(),
                            stderr: b"err".to_vec(),
                        },
                        skid_overshoots: 0,
                        latches: RunLatches {
                            determinism_loss: (failure == Some("latch"))
                                .then(|| "saved control loss".to_owned()),
                            log_sink_failure: None,
                        },
                        terminal_outcome: None,
                        pending: (),
                    })
                },
                |(), _| {
                    assert!(!directory.join(verify_run::RECORD).exists());
                    finished.set(true);
                    if failure == Some("finish") {
                        return Err(Error::new(hermit::BackendRunFailure)
                            .context("saved control deferred failure"));
                    }
                    Ok(Some(SavedNativeEvidence::new(raw.to_vec(), 3)))
                },
            );
            assert_eq!(result.is_ok(), failure.is_none());
            assert_eq!(finished.get(), failure != Some("latch"));
            if failure == Some("payload") {
                assert!(
                    !directory.join(verify_run::RECORD).exists(),
                    "a failed create-new payload write must never publish the record"
                );
                assert_eq!(
                    fs::read(directory.join(verify_run::STDOUT)).unwrap(),
                    b"existing payload"
                );
                assert!(!directory.join(verify_run::DBT_STATS).exists());
                continue;
            }
            let record = verify_run::VerifyRunRecord::read(&directory).unwrap();
            assert_eq!(record.backend, "saved-control");
            assert_eq!(
                record.comparison.strictness,
                LogCompareStrictness::Canonical
            );
            match failure {
                None => {
                    assert!(matches!(
                        record.outcome,
                        verify_run::VerifyRunOutcome::Completed { .. }
                    ));
                    assert_eq!(record.dbt.unwrap().initialization_records, 3);
                    assert_eq!(
                        fs::read(directory.join(verify_run::DBT_STATS)).unwrap(),
                        raw
                    );
                    assert_eq!(
                        fs::read(directory.join(verify_run::STDOUT)).unwrap(),
                        b"out"
                    );
                }
                Some("latch") => {
                    assert!(matches!(
                        record.outcome,
                        verify_run::VerifyRunOutcome::RunFailed {
                            failure: RunFailure::DeterminismLoss,
                            ..
                        }
                    ));
                    assert!(record.dbt.is_none());
                    assert!(!directory.join(verify_run::DBT_STATS).exists());
                }
                Some("finish") => {
                    assert!(matches!(
                        record.outcome,
                        verify_run::VerifyRunOutcome::RunFailed {
                            failure: RunFailure::BackendFailure,
                            ..
                        }
                    ));
                    assert!(record.dbt.is_none());
                    assert!(!directory.join(verify_run::DBT_STATS).exists());
                }
                _ => unreachable!(),
            }
            assert!(!directory.join(verify_run::SUMMARY).exists());
            assert!(!directory.join(verify_run::HOST_INPUTS).exists());
        }
    }

    /// Exercise the actual common socket/capture owner without starting a guest
    /// or supplying a comparator verdict. A plain container child isolates its
    /// real tracing subscriber and forwarded-source lifetime from the test host.
    fn socket_capture_control(
        file: &tempfile::NamedTempFile,
        budget: Option<super::super::tracing::LogBudget>,
        records: Vec<Vec<u8>>,
        fail: bool,
    ) -> Result<super::super::backend_hook::PhysicalRun<()>, Error> {
        use super::super::backend_hook::CapturedOutput;
        use super::super::backend_hook::PreparedPhysical;
        use super::super::backend_hook::VerificationCapture;
        use super::super::backend_hook::VerificationLogTransport;

        let options = run_opts_for(&[
            "hermit",
            "run",
            "--no-namespace",
            "--timeout=5",
            "--",
            "/bin/true",
        ]);
        let capabilities = hermit::backend_capabilities(Backend::Ptrace);
        let physical = PreparedPhysical::new(&options, capabilities);
        let capture = VerificationCapture {
            log: file.reopen()?,
            path: file.path(),
            run: VerificationRun::Run1,
            options: &options,
            physical: Some(&physical),
            requested_log: Some(tracing::metadata::LevelFilter::INFO),
            log_budget: budget,
            log_transport: VerificationLogTransport::InGuestSocket,
            forwarded_detlogs: ForwardedDetlogPlan::Socket { opened: false },
        };
        let (captured, ()) = capture.run_in_container(
            (),
            "actual common-capture socket control; no guest",
            move |_, request| {
                assert_eq!(request.config.backend, capabilities);
                let sender = hermit::inheritable_in_guest_detlog_socket()?
                    .expect("the declared transport opened its actual source");
                for (index, record) in records.iter().enumerate() {
                    let message = detcore::detlog::tagged_forwarded_message(
                        unsafe { libc::syscall(libc::SYS_gettid) as i32 },
                        (index + 1) as u64,
                        record,
                    );
                    let sent = unsafe {
                        libc::send(
                            sender.as_raw_fd(),
                            message.as_ptr().cast(),
                            message.len(),
                            libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                        )
                    };
                    assert_eq!(sent, message.len() as isize, "send complete actual record");
                }
                // Just as the physical adapter must do after real teardown,
                // retire the inherited source before the common ordered drain.
                drop(sender);
                anyhow::ensure!(!fail, "injected physical callback failure");
                Ok(CapturedOutput {
                    output: Output {
                        status: ExitStatus::Exited(0),
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    },
                    skid_overshoots: 0,
                    pending: (),
                })
            },
        )?;
        Ok(captured)
    }

    #[test]
    fn common_capture_drains_actual_socket_before_success_or_error_returns() {
        if crate::app::test_isolation::rerun_alone() {
            return;
        }
        // The malformed source runs before the injected physical failure: a
        // failed container callback leaves its owner retained for the rest of
        // the process (`owned_container::run` refuses every later container
        // here, by design), so that case must be this process's last.
        let invalid = tempfile::NamedTempFile::new().unwrap();
        let captured =
            socket_capture_control(&invalid, None, vec![b"HINFO".to_vec()], false).unwrap();
        let error = captured
            .latches
            .refuse_comparison("actual malformed source")
            .unwrap_err();
        assert!(format!("{error:#}").contains("determinism loss"));
        assert!(
            fs::read_to_string(invalid.path())
                .unwrap()
                .contains(FORWARDING_REFUSED_MARKER)
        );
        for fail in [false, true] {
            let file = tempfile::NamedTempFile::new().unwrap();
            let first = b"INFO detcore: DETLOG [syscall] capture first\n";
            let second = b"INFO detcore: DETLOG [syscall] capture second\n";
            let result =
                socket_capture_control(&file, None, vec![first.to_vec(), second.to_vec()], fail);
            if fail {
                assert!(
                    format!("{:#}", result.err().expect("physical error must propagate"))
                        .contains("injected physical callback failure")
                );
            } else {
                let captured = result.unwrap();
                assert_eq!(captured.output.status, ExitStatus::Exited(0));
                captured
                    .latches
                    .refuse_comparison("capture control")
                    .unwrap();
            }
            let text = fs::read_to_string(file.path()).unwrap();
            let first_at = text
                .find("capture first")
                .expect("first real source record drained");
            let second_at = text
                .find("capture second")
                .expect("second real source record drained");
            assert!(first_at < second_at, "{text}");
            assert!(!text.contains(FORWARDING_REFUSED_MARKER), "{text}");
        }
    }

    #[test]
    fn common_capture_charges_one_invocation_budget_across_actual_runs() {
        if crate::app::test_isolation::rerun_alone() {
            return;
        }
        let first = tempfile::NamedTempFile::new().unwrap();
        let second = tempfile::NamedTempFile::new().unwrap();
        let record = b"INFO detcore: DETLOG [syscall] shared capture budget\n";
        let line_bytes = NORMALIZED_FORWARDED_DETLOG_TIMESTAMP.len() + 1 + record.len();
        let budget = super::super::tracing::LogBudget::new((2 * line_bytes - 1) as u64).unwrap();
        let captured =
            socket_capture_control(&first, Some(budget.clone()), vec![record.to_vec()], false)
                .unwrap();
        captured.latches.refuse_comparison("first capture").unwrap();
        assert_eq!(fs::metadata(first.path()).unwrap().len(), line_bytes as u64);
        let error = socket_capture_control(&second, Some(budget), vec![record.to_vec()], false)
            .err()
            .expect("the second actual record must cross the shared invocation cap");
        assert!(
            error.downcast_ref::<LogCapExceeded>().is_some(),
            "{error:#}"
        );
        assert_eq!(fs::metadata(first.path()).unwrap().len(), line_bytes as u64);
    }

    fn forwarding_plan_options(forwarded: bool) -> RunOpts {
        let backend = if forwarded { "in-guest-trap" } else { "ptrace" };
        run_opts_for(&["hermit", "--backend", backend, "run", "--", "/bin/true"])
    }

    fn assert_channel_record(sender: &OwnedFd, receiver: &OwnedFd, record: &[u8]) {
        let sent = unsafe {
            libc::send(
                sender.as_raw_fd(),
                record.as_ptr().cast(),
                record.len(),
                libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            )
        };
        assert_eq!(sent, record.len() as isize, "send exact channel record");
        let mut bytes = [0u8; 128];
        let received = unsafe {
            libc::recv(
                receiver.as_raw_fd(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
                libc::MSG_DONTWAIT,
            )
        };
        assert_eq!(received, record.len() as isize, "receive complete record");
        assert_eq!(&bytes[..record.len()], record);
    }

    fn assert_channel_empty_with_writer(receiver: &OwnedFd) {
        let mut poll = libc::pollfd {
            fd: receiver.as_raw_fd(),
            events: libc::POLLIN | libc::POLLHUP,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut poll, 1, 0) }, 0);
        assert_eq!(poll.revents, 0, "a live writer must prevent HUP");
        let mut byte = 0u8;
        let received = unsafe {
            libc::recv(
                receiver.as_raw_fd(),
                (&raw mut byte).cast(),
                1,
                libc::MSG_DONTWAIT,
            )
        };
        assert_eq!(received, -1, "an empty live source is not EOF");
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EAGAIN)
        );
    }

    fn assert_channel_eof_after_last_writer(receiver: &OwnedFd) {
        let mut poll = libc::pollfd {
            fd: receiver.as_raw_fd(),
            events: libc::POLLIN | libc::POLLHUP,
            revents: 0,
        };
        assert_eq!(
            unsafe { libc::poll(&mut poll, 1, 250) },
            1,
            "last writer retirement must become observable while ancestor waits"
        );
        assert_ne!(
            poll.revents & libc::POLLHUP,
            0,
            "last writer must yield HUP"
        );
        assert_eq!(poll.revents & (libc::POLLERR | libc::POLLNVAL), 0);
        let mut byte = 0u8;
        assert_eq!(
            unsafe {
                libc::recv(
                    receiver.as_raw_fd(),
                    (&raw mut byte).cast(),
                    1,
                    libc::MSG_DONTWAIT,
                )
            },
            0,
            "last writer must yield actual recv EOF"
        );
    }

    #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
    #[test]
    fn mapped_verification_requires_unconsumed_real_forwarded_plan() {
        assert!(
            ForwardedDetlogPlan::None
                .open_mapped_in_container()
                .is_err()
        );
        let mut plan = ForwardedDetlogPlan::Socket { opened: false };
        let (sender, receiver) = plan.open_mapped_in_container().unwrap();
        assert_channel_record(&sender, &receiver, b"actual mapped verification channel");
        assert!(plan.open_mapped_in_container().is_err());
        drop(sender);
        assert_channel_eof_after_last_writer(&receiver);
    }

    #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
    struct VerificationFlushFailure;
    #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
    impl Write for VerificationFlushFailure {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "late verifier flush",
            ))
        }
    }

    #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
    #[test]
    fn mapped_verification_late_flush_refuses_output_comparison() {
        let output = Output {
            status: ExitStatus::Exited(23),
            stdout: b"actual output".to_vec(),
            stderr: vec![],
        };
        let (output, skid, latches) =
            finish_mapped_verification_log(Ok(output), SharedLog::new(VerificationFlushFailure))
                .unwrap();
        assert_eq!(output.status, ExitStatus::Exited(23));
        assert_eq!(output.stdout, b"actual output");
        assert_eq!(skid, 0);
        assert!(
            latches
                .log_sink_failure
                .as_deref()
                .unwrap()
                .contains("late verifier flush")
        );
        assert!(latches.refuse_comparison("private unit").is_err());
    }

    #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
    #[test]
    fn mapped_verification_execution_error_survives_late_flush_failure() {
        let error = finish_mapped_verification_log(
            Err(
                std::io::Error::new(std::io::ErrorKind::Interrupted, "original setup error").into(),
            ),
            SharedLog::new(VerificationFlushFailure),
        )
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::Interrupted
        );
        assert!(format!("{error:#}").contains("late verifier flush"));
        assert!(format!("{error:#}").contains("original setup error"));
    }

    #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
    #[test]
    fn mapped_verification_image_close_preserves_original_error() {
        let error = finish_mapped_verification_image::<()>(
            Err(
                std::io::Error::new(std::io::ErrorKind::Interrupted, "original execution error")
                    .into(),
            ),
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "late image verification",
            )),
        )
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::Interrupted
        );
        assert!(format!("{error:#}").contains("late image verification"));
        let error = finish_mapped_verification_image(
            Ok(23),
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "late image verification",
            )),
        )
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn forwarded_detlog_plan_record_reaches_eof_while_parent_waits() {
        if crate::app::test_isolation::rerun_alone() {
            return;
        }
        let options = forwarding_plan_options(true);
        let plan = options.forwarded_detlog_plan().unwrap();
        assert!(plan.requires_socket());
        let ((), _guards) = options
            .launch_in_container(plan, "forwarded DETLOG test plan", None, |plan, _, _| {
                let (sender, receiver) = plan.open_in_container()?.expect("forwarded pair");
                assert_channel_record(
                    &sender,
                    &receiver,
                    &detcore::detlog::tagged_forwarded_message(
                        unsafe { libc::syscall(libc::SYS_gettid) as i32 },
                        1,
                        b"DETLOG retained record\n",
                    ),
                );
                drop(sender);
                assert_channel_eof_after_last_writer(&receiver);
                assert!(
                    plan.open_in_container().is_err(),
                    "one physical stream only"
                );
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn forwarded_detlog_plan_duplicate_writer_blocks_eof() {
        if crate::app::test_isolation::rerun_alone() {
            return;
        }
        use std::os::fd::FromRawFd;

        let options = forwarding_plan_options(true);
        let plan = options.forwarded_detlog_plan().unwrap();
        let ((), _guards) = options
            .launch_in_container(plan, "duplicated forwarding writer", None, |plan, _, _| {
                let (sender, receiver) = plan.open_in_container()?.expect("forwarded pair");
                let raw = unsafe { libc::fcntl(sender.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
                assert!(raw >= 0);
                let duplicate = unsafe { OwnedFd::from_raw_fd(raw) };
                assert_channel_record(
                    &sender,
                    &receiver,
                    &detcore::detlog::tagged_forwarded_message(
                        unsafe { libc::syscall(libc::SYS_gettid) as i32 },
                        1,
                        b"DETLOG before last writer\n",
                    ),
                );
                drop(sender);
                assert_channel_empty_with_writer(&receiver);
                drop(duplicate);
                assert_channel_eof_after_last_writer(&receiver);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn forwarded_detlog_plan_nonforwarded_opens_no_channel() {
        if crate::app::test_isolation::rerun_alone() {
            return;
        }
        let options = forwarding_plan_options(false);
        let plan = options.forwarded_detlog_plan().unwrap();
        assert!(!plan.requires_socket());
        let ((), _guards) = options
            .launch_in_container(plan, "nonforwarding test plan", None, |plan, _, _| {
                assert!(!plan.requires_socket());
                assert!(plan.open_in_container()?.is_none());
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn forwarded_detlog_plan_two_runs_keep_streams_independent() {
        if crate::app::test_isolation::rerun_alone() {
            return;
        }
        let options = forwarding_plan_options(true);
        let plans = (
            options.forwarded_detlog_plan().unwrap(),
            options.forwarded_detlog_plan().unwrap(),
        );
        let ((), _guards) = options
            .launch_in_container(
                plans,
                "independent forwarding plans",
                None,
                |(first, second), _, _| {
                    let (sender1, receiver1) = first.open_in_container()?.expect("first pair");
                    let (sender2, receiver2) = second.open_in_container()?.expect("second pair");
                    let first_identity =
                        detcore::detlog::socket_identity(receiver1.as_raw_fd()).unwrap();
                    let second_identity =
                        detcore::detlog::socket_identity(receiver2.as_raw_fd()).unwrap();
                    assert_ne!(first_identity, second_identity);
                    assert_channel_record(
                        &sender1,
                        &receiver1,
                        &detcore::detlog::tagged_forwarded_message(
                            unsafe { libc::syscall(libc::SYS_gettid) as i32 },
                            1,
                            b"DETLOG physical run one\n",
                        ),
                    );
                    assert_channel_empty_with_writer(&receiver2);
                    assert_channel_record(
                        &sender2,
                        &receiver2,
                        &detcore::detlog::tagged_forwarded_message(
                            unsafe { libc::syscall(libc::SYS_gettid) as i32 },
                            1,
                            b"DETLOG physical run two\n",
                        ),
                    );
                    assert_channel_empty_with_writer(&receiver1);
                    drop(sender1);
                    assert_channel_eof_after_last_writer(&receiver1);
                    assert_channel_empty_with_writer(&receiver2);
                    drop(sender2);
                    assert_channel_eof_after_last_writer(&receiver2);
                    Ok(())
                },
            )
            .unwrap();
    }

    #[test]
    #[cfg(feature = "native-exec")]
    fn native_observer_plan_reaches_eof_while_parent_waits() {
        if crate::app::test_isolation::rerun_alone() {
            return;
        }
        let options = forwarding_plan_options(true);
        let staging = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let endpoints =
            super::super::native_exec_run::NativeEndpoints::bind(staging.path()).unwrap();
        let ((), _guards) = options
            .launch_in_container(
                (staging, endpoints),
                "native observer test plan",
                None,
                |(_, endpoints), _, _| {
                    let (receiver, sender) = endpoints.open_observer_in_container()?;
                    assert_channel_record(&sender, &receiver, b"observer retained record");
                    drop(sender);
                    assert_channel_eof_after_last_writer(&receiver);
                    assert!(
                        endpoints.open_observer_in_container().is_err(),
                        "one observer stream only"
                    );
                    Ok(())
                },
            )
            .unwrap();
    }

    #[test]
    fn host_base_env_keeps_hermits_own_variables_from_the_guest() {
        let mut command = Command::new("/bin/true");
        apply_base_and_explicit_environment(&mut command, &BaseEnv::Host, &[]).unwrap();
        let removed: Vec<_> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name.to_owned())
            .collect();
        for name in [
            super::super::verify::VERIFY_TMPDIR_ENV,
            DAP_LISTEN_INNER_ENV,
        ] {
            assert!(
                removed.iter().any(|removed| removed == name),
                "{name} must not reach a guest with the host environment: removed {removed:?}"
            );
        }
    }

    /// An `--image` guest gets each `--base-env=minimal` variable its image
    /// does not declare, and the image's value for each one it does.
    #[test]
    fn image_env_replaces_only_the_minimal_defaults_it_declares() {
        let mut command = Command::new("/bin/true");
        let image_env = [
            ("PATH".to_owned(), "/image/bin".to_owned()),
            ("USER".to_owned(), "builder".to_owned()),
            ("IMAGE_ONLY".to_owned(), "present".to_owned()),
        ];
        apply_image_base_environment(&mut command, &image_env);
        let envs = command.get_captured_envs();
        let expected = [
            ("HOME", "/root"),
            ("HOSTNAME", "hermetic-container.local"),
            ("IMAGE_ONLY", "present"),
            ("LOGNAME", "root"),
            ("PATH", "/image/bin"),
            ("USER", "builder"),
        ];
        assert_eq!(
            envs.iter()
                .map(|(name, value)| (name.to_str().unwrap(), value.to_str().unwrap()))
                .collect::<Vec<_>>(),
            expected,
        );
    }

    #[test]
    fn a_green_run_removes_only_its_own_fatal_cores() {
        let dir = tempfile::tempdir().unwrap();
        let mine = fatal_core_owner_token();
        let theirs = fatal_core_owner_token();
        // Same controller process ID, as after PID reuse or in another PID
        // namespace sharing the directory, but a different invocation.
        assert_ne!(mine, theirs);
        assert_eq!(mine.split('-').next(), theirs.split('-').next());
        let files = [
            format!("hermit-{mine}-core.3.3.SIGSEGV.zst"),
            format!(".hermit-{mine}-core.4.4.SIGABRT.zst.tmp"),
            format!("hermit-{theirs}-core.3.3.SIGSEGV.zst"),
            format!(".hermit-{theirs}-core.4.4.SIGABRT.zst.tmp"),
            "unrelated".to_string(),
        ];
        for file in &files {
            fs::write(dir.path().join(file), b"x").unwrap();
        }
        assert_eq!(remove_fatal_core_files(dir.path(), &mine), 2);
        let mut left: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        let mut expected = files[2..].to_vec();
        expected.sort();
        assert_eq!(left, expected);
    }

    #[derive(Clone)]
    struct PlainInfoWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl Write for PlainInfoWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn typed_plain_info_sink_preserves_native_message_format_and_legacy_detlog() {
        let native = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let native_writer = native.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || PlainInfoWriter(native_writer.clone()))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "detcore::syscalls::files", "Injecting additional fstat to retrieve file metadata on fd {}.", 3);
        });
        let native = String::from_utf8(native.lock().unwrap().clone()).unwrap();
        // Timestamp/level padding belongs to the existing host format. Compare
        // the whole original target/message portion, with no field rewriting.
        let native_payload = native.split_once("INFO ").unwrap().1;
        let bytes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = SharedLog::new(PlainInfoWriter(bytes.clone()));
        let mut sink = log.record_sink();
        sink(
            &detcore::detlog::encode_plain_info(
                "detcore::syscalls::files",
                "Injecting additional fstat to retrieve file metadata on fd 3.",
            )
            .unwrap(),
        );
        let rdtsc = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let rdtsc_writer = rdtsc.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || PlainInfoWriter(rdtsc_writer.clone()))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "detcore", "[dtid {}] inbound rdtsc, new logical time: {:?}", 3, 17);
        });
        let rdtsc = String::from_utf8(rdtsc.lock().unwrap().clone()).unwrap();
        assert_eq!(
            rdtsc.split_once("INFO ").unwrap().1,
            "detcore: [dtid 3] inbound rdtsc, new logical time: 17\n"
        );
        let rdtsc_typed = detcore::detlog::encode_plain_info(
            "detcore",
            "[dtid 3] inbound rdtsc, new logical time: 17",
        )
        .unwrap();
        let (target, message) = detcore::detlog::decode_plain_info(&rdtsc_typed).unwrap();
        assert_eq!(
            rdtsc.split_once("INFO ").unwrap().1,
            format!("{target}: {message}\n")
        );
        sink(b"INFO detcore: DETLOG legacy unchanged\n");
        sink(&detcore::detlog::encode_plain_info("detcore", "DETLOG ordinary text").unwrap());
        sink(b"INFO detcore: raw plain INFO remains refused\n");
        let output = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert_eq!(
            output,
            format!(
                "{NORMALIZED_FORWARDED_DETLOG_TIMESTAMP} INFO {native_payload}{NORMALIZED_FORWARDED_DETLOG_TIMESTAMP} INFO detcore: DETLOG legacy unchanged\n{NORMALIZED_FORWARDED_DETLOG_TIMESTAMP} INFO detcore: DETLOG ordinary text\n{FORWARDING_REFUSED_MARKER} INFO detcore: raw plain INFO remains refused\n"
            )
        );
        assert!(log.failure().is_none());
    }

    #[test]
    fn malformed_typed_plain_info_sink_latches_refusal_without_source_reentry() {
        const NAME: &str = "app::run::tests::malformed_typed_plain_info_sink_latches_refusal_without_source_reentry";
        const CHILD: &str = "HERMIT_PLAIN_INFO_SINK_TEST";
        if std::env::var(CHILD).ok().as_deref() != Some(NAME) {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME])
                .env(CHILD, NAME)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    let output = child.wait_with_output().unwrap();
                    assert!(
                        status.success(),
                        "{}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                    assert!(
                        String::from_utf8_lossy(&output.stdout)
                            .lines()
                            .any(|line| line == format!("test {NAME} ... ok"))
                    );
                    return;
                }
                if std::time::Instant::now() >= deadline {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    panic!("typed sink control timed out");
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        let bytes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = SharedLog::new(PlainInfoWriter(bytes.clone()));
        let mut sink = log.record_sink();
        let mut bad = detcore::detlog::encode_plain_info("detcore", "text").unwrap();
        bad[9] = 2; // unsupported class, not a textual fallback
        sink(&bad);
        sink(b"HINFO");
        sink(b"HINFO\0\0\0\x01");
        assert!(detcore::detlog::determinism_loss().is_some());
        assert!(
            String::from_utf8(bytes.lock().unwrap().clone())
                .unwrap()
                .starts_with(FORWARDING_REFUSED_MARKER)
        );
        assert!(
            log.failure().is_none(),
            "a semantic refusal is not a fabricated I/O error"
        );
    }

    /// Fails its first write and accepts every later one.
    struct FailsOnce {
        failed: bool,
        written: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    }

    impl Write for FailsOnce {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if !self.failed {
                self.failed = true;
                return Err(std::io::Error::other("injected write failure"));
            }
            self.written.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The design's fault injection: the loss notice's write fails once and
    /// later writes recover, so the log reads complete apart from the notice.
    /// Verification must still refuse, from the latch.
    #[test]
    fn a_failed_log_write_refuses_comparison_after_later_writes_recover() {
        let written = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = SharedLog::new(FailsOnce {
            failed: false,
            written: written.clone(),
        });
        let mut sink = log.record_sink();
        sink(
            format!(
                "{} 0 0 lost under test\n",
                detcore::detlog::FORWARDING_LOSS_NOTICE
            )
            .as_bytes(),
        );
        sink(b"INFO detcore DETLOG [syscall] write(1, 0x1000, 5) = 5\n");
        let mut tracing_writer = log.clone();
        tracing_writer
            .write_all(b"a later tracing event\n")
            .unwrap();
        // The notice is gone from the log, and the later writes are there.
        let text = String::from_utf8(written.lock().unwrap().clone()).unwrap();
        assert!(!text.contains("lost under test"), "{text}");
        assert!(text.contains("DETLOG [syscall]"), "{text}");
        let latches = RunLatches {
            determinism_loss: None,
            log_sink_failure: log.failure(),
        };
        let refusal = latches.refuse_comparison("run 1").unwrap_err().to_string();
        assert!(
            refusal.contains("log sink failure: injected write failure"),
            "{refusal}"
        );
    }

    #[test]
    fn a_recorded_determinism_loss_refuses_comparison() {
        assert!(RunLatches::default().refuse_comparison("run 1").is_ok());
        let latches = RunLatches {
            determinism_loss: Some("in-guest process 7 exited without deregistering".to_string()),
            log_sink_failure: None,
        };
        let refusal = latches.refuse_comparison("run 2").unwrap_err().to_string();
        assert_eq!(
            refusal,
            "run 2: determinism loss recorded: in-guest process 7 exited without deregistering"
        );
    }

    /// https://github.com/rrnewton/hermit/pull/3686, restacked onto a main
    /// whose verification log is one writer shared by the tracing subscriber
    /// and the coordinator's forwarded in-guest records: `--max-log-bytes`
    /// charges the forwarded records too. Records alone, with no tracing
    /// event, cross the cap and end the process with the cap's exit code, and
    /// the log holds only what was written before the crossing.
    #[test]
    fn forwarded_records_alone_cross_the_log_cap() {
        let budget = super::super::tracing::LogBudget::new(100).unwrap();
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut sink =
            verification_log(file.reopen().unwrap(), 1 << 20, Some(budget)).record_sink();
        let record = b"INFO detcore DETLOG [syscall] write(1, 0x1000, 5) = 5\n";
        // SAFETY: the child only calls the sink, which formats one line into a
        // new Vec, takes the uncontended mutex created above and writes to the
        // file, and then _exits.
        let child = unsafe { libc::fork() };
        assert!(
            child >= 0,
            "fork failed: {}",
            std::io::Error::last_os_error()
        );
        if child == 0 {
            // SAFETY: plain descriptor calls. The cap's final message goes to
            // stderr, so point it away from the test runner's.
            unsafe {
                let null = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY);
                libc::dup2(null, 2);
            }
            for _ in 0..10 {
                sink(record);
            }
            // SAFETY: _exit has no preconditions.
            unsafe { libc::_exit(0) }
        }
        let mut status = 0;
        // SAFETY: waitpid writes only `status`.
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert!(libc::WIFEXITED(status), "status {status:#x}");
        assert_eq!(
            libc::WEXITSTATUS(status),
            hermit::HERMIT_LOG_CAP_EXIT,
            "ten forwarded records of {} bytes each, against a cap of 100 bytes, did not end \
             the process with the log cap's exit code",
            record.len() + NORMALIZED_FORWARDED_DETLOG_TIMESTAMP.len() + 1
        );
        let mut expected = format!("{NORMALIZED_FORWARDED_DETLOG_TIMESTAMP} ").into_bytes();
        expected.extend_from_slice(record);
        assert_eq!(
            String::from_utf8_lossy(&fs::read(file.path()).unwrap()),
            String::from_utf8_lossy(&expected),
            "the log holds the one record admitted before the crossing, and not the crossing one"
        );
    }

    #[test]
    fn real_returned_guest_disposition_still_uses_first_run_rejected() {
        if crate::app::test_isolation::rerun_alone() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("verify.json");
        write_pending_verification_json(&path).unwrap();
        let output = run_verification_execution(Some(&path), VerificationRun::Run1, || {
            super::super::container::classify_container_result(Container::new().checked_run(|| {
                Ok::<_, hermit::SerializableError>(Output {
                    status: ExitStatus::Signaled(reverie::process::Signal::SIGALRM, false),
                    stdout: b"returned guest output".to_vec(),
                    stderr: Vec::new(),
                })
            }))
        })
        .unwrap();
        let options = RunOpts::parse_from(["run", "--", "/bin/true"]);
        assert!(!options.verify_allow.satisfies(output.status));
        let report = first_run_rejected_report(&output, None);
        write_report_json(&path, &report).unwrap();
        let parsed = VerificationReport::from_current_json_slice(&fs::read(path).unwrap()).unwrap();
        assert!(matches!(
            parsed.no_result_reason,
            Some(NoResultReason::FirstRunRejected {
                signal: Some(libc::SIGALRM),
                ..
            })
        ));
        assert_eq!(parsed.guest_signal, Some(libc::SIGALRM));
        assert!(parsed.comparison.is_none());
    }

    #[test]
    fn first_run_rejected_report_keeps_available_guest_exit_code() {
        let output = Output {
            status: ExitStatus::Exited(23),
            stdout: b"out".to_vec(),
            stderr: b"error".to_vec(),
        };

        let report = first_run_rejected_report(&output, None);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("verification.json");
        write_report_json(&path, &report).unwrap();
        let persisted = VerificationReport::from_current_json_value(
            serde_json::from_slice(&fs::read(path).unwrap()).unwrap(),
        )
        .unwrap();

        assert_eq!(persisted.guest_exit_code, Some(23));
        assert_eq!(persisted.guest_signal, None);
        assert_eq!(
            persisted.no_result_reason,
            Some(NoResultReason::FirstRunRejected {
                exit_code: Some(23),
                signal: None,
                stdout_bytes: 3,
                stderr_bytes: 5,
            })
        );
    }

    #[test]
    fn run_one_summary_is_empty_again_before_run_two() -> Result<(), Error> {
        let file = tempfile::NamedTempFile::new().unwrap();
        let summary = RunSummary {
            sched_turns: 12,
            virttime_elapsed: 34,
            syscalls: Some(5),
            ..Default::default()
        };
        fs::write(
            file.path(),
            serde_json::to_vec(&summary).expect("summary fixture serializes"),
        )
        .unwrap();

        let captured = take_verify_summary_before_next_run(file.path())?
            .expect("run one summary remains readable");
        assert_eq!(captured.sched_turns, 12);
        assert_eq!(captured.virttime_elapsed, 34);
        assert_eq!(captured.syscalls, Some(5));
        assert_eq!(fs::read(file.path()).unwrap(), b"");

        Ok::<(), Error>(())
    }

    #[test]
    fn ptrace_engagement_record_reads_the_typed_run_summary() {
        let summary_file = tempfile::NamedTempFile::new().unwrap();
        let engagement_file = tempfile::NamedTempFile::new().unwrap();
        let mut options = run_opts_for(&["hermit", "--backend=ptrace", "run", "/bin/true"]);
        options.summary_json = Some(summary_file.path().to_owned());
        options.backend_engagement_json = Some(engagement_file.path().to_owned());

        for scheduler_turns in [12, 13] {
            let summary = RunSummary {
                sched_turns: scheduler_turns,
                ..Default::default()
            };
            fs::write(summary_file.path(), serde_json::to_vec(&summary).unwrap()).unwrap();
            options.write_backend_engagement_after_run().unwrap();
            let report: BackendEngagementReport =
                serde_json::from_slice(&fs::read(engagement_file.path()).unwrap()).unwrap();
            assert_eq!(
                report.engagement,
                BackendEngagement::Ptrace { scheduler_turns }
            );
        }
    }

    #[test]
    fn exit_status_diagnostic_reports_guest_exit_code_and_signal() {
        assert_eq!(
            describe_exit_status(ExitStatus::Exited(23)),
            "exited with code 23"
        );
        assert_eq!(
            describe_exit_status(ExitStatus::Signaled(
                reverie::process::Signal::SIGTERM,
                false,
            )),
            "terminated by signal 15 (SIGTERM)"
        );
        assert_eq!(
            describe_exit_status(ExitStatus::Signaled(
                reverie::process::Signal::SIGSEGV,
                true,
            )),
            "terminated by signal 11 (SIGSEGV) (core dumped)"
        );
    }

    #[test]
    fn verification_report_is_published_before_success_is_announced() {
        // Search only the code before this test module. Searching the whole
        // file let the literals below match themselves, so the ordering check
        // passed whatever the code did.
        let source = include_str!("run.rs")
            .split_once("#[cfg(test)]\nmod tests {")
            .expect("test module")
            .0;
        let verification = source
            .split_once("let mut outcome = compare_two_runs(")
            .expect("verification comparison")
            .1;
        let publish = verification
            .find("write_verification_json(path, &outcome)")
            .expect("verification report publication");
        let announce = verification
            .find("announce_verification_outcome(&outcome")
            .expect("verification announcement");
        assert!(publish < announce);
        // Run mode compares two independent runs, never a replay, so its match
        // line must not use the replay wording for the time note.
        let call = &verification[announce..];
        let call = &call[..call.find(");").expect("end of announcement call")];
        assert!(call.contains("SecondRun::Rerun"), "{call}");
    }

    /// A skid overshoot turns whatever the comparison finds into an
    /// infrastructure error (`SkidOvershootError`), so a log match is not the
    /// verdict, and `--keep-logs` must keep both logs, as every other error
    /// path does, instead of the golden log alone
    /// (https://github.com/rrnewton/hermit/issues/3301). `verify` says so by
    /// setting `match_overridden` on the options it compares with; verify.rs's
    /// `an_overridden_match_keeps_both_logs_in_the_failure_directory` and the
    /// overridden rows of `verification_log_retention_matches_the_cli_contract`
    /// pin what an overridden match retains. A skid overshoot cannot be
    /// provoked on demand, so the statement is pinned in the source.
    #[test]
    fn skid_overshoot_overrides_a_log_match_for_retention() {
        let source = include_str!("run.rs");
        // Search only the production code: this test names the statements it
        // looks for, so the whole file always contains them.
        let production = source
            .split_once("\n#[cfg(test)]\nmod tests {")
            .expect("run.rs has a test module")
            .0;
        let total =
            "let skid_overshoots = run1.skid_overshoots.saturating_add(run2.skid_overshoots);";
        let overridden = "comparison_options.match_overridden |= skid_overshoots > 0;";
        let compared = "let mut outcome = compare_two_runs(";
        for statement in [total, overridden, compared] {
            assert_eq!(
                production.matches(statement).count(),
                1,
                "run.rs must contain `{statement}` exactly once"
            );
        }
        let total_at = production.find(total).unwrap();
        let overridden_at = production.find(overridden).unwrap();
        let compared_at = production.find(compared).unwrap();
        assert!(
            total_at < overridden_at && overridden_at < compared_at,
            "the skid total must mark the match overridden before the runs are compared"
        );
        // The DBT facts may add an override but must preserve a prior one;
        // the facts test exercises both initial values. No other mutation of
        // the comparison options belongs between the skid override and use.
        let between = &production[overridden_at + overridden.len()..compared_at];
        let add_backend_facts = "comparison.configure(&mut comparison_options);";
        assert_eq!(between.matches(add_backend_facts).count(), 1);
        assert!(
            !between
                .replace(add_backend_facts, "")
                .contains("comparison_options"),
            "only the OR-preserving backend facts may update the options before comparison"
        );
        let call = &production[compared_at..];
        let call = &call[..call.find(")?;").expect("the comparison call ends")];
        assert!(
            call.trim_end().ends_with("comparison_options,"),
            "the runs must be compared with the options that carry the override:\n{call}"
        );
    }

    #[test]
    fn verification_log_flags_are_discoverable_and_old_print_spelling_still_parses() {
        let current = RunOpts::try_parse_from([
            "run",
            "--verify",
            "--print-verify-logs",
            "--keep-logs",
            "--verify-log-dir",
            "/tmp/logs",
            "/bin/true",
        ])
        .unwrap();
        assert!(current.print_verify_logs);
        assert!(current.keep_logs);
        assert_eq!(current.verify_log_dir, Some(PathBuf::from("/tmp/logs")));

        let compatible =
            RunOpts::try_parse_from(["run", "--verify", "--verify-logs", "/bin/true"]).unwrap();
        assert!(compatible.print_verify_logs);

        let mut help = Vec::new();
        RunOpts::command().write_long_help(&mut help).unwrap();
        let help = String::from_utf8(help).unwrap();
        assert!(help.contains("--print-verify-logs"));
        assert!(help.contains("--keep-logs"));
        assert!(help.contains("--verify-log-dir"));
        assert!(!help.contains("--verify-logs"));

        assert!(
            RunOpts::try_parse_from([
                "run",
                "--verify",
                "--verify-log-dir",
                "/tmp/logs",
                "/bin/true",
            ])
            .is_err(),
            "a retention directory without --keep-logs must be refused"
        );
    }

    #[test]
    fn user_selected_retention_directory_is_resolved_and_created() {
        let parent = tempfile::tempdir().unwrap();
        let requested = parent.path().join("retained");
        let options = RunOpts::try_parse_from([
            "run",
            "--verify",
            "--keep-logs",
            "--verify-log-dir",
            requested.to_str().unwrap(),
            "/bin/true",
        ])
        .unwrap();
        assert_eq!(
            options.retained_verify_log_dir().unwrap(),
            Some(fs::canonicalize(requested).unwrap())
        );
    }

    #[test]
    fn extracts_forwarded_detlogs_and_preserves_guest_stderr() {
        let coordinator = "2026-08-02T00:00:00.000000Z INFO detcore: coordinator message\n";
        let log = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(log.path(), coordinator).unwrap();
        // SaBRe: records share the guest's stderr and are found by their text, as
        // before; every other line stays.
        let mut stderr = b"guest stderr\n INFO detcore: inbound syscall: getpid() = ?\n\
              INFO detcore: DETLOG scheduler event\n\
              INFO detcore: DETLOG [syscall] finish syscall #1: getpid() = Ok(3)\n"
            .to_vec();
        let syscall_records = extract_forwarded_detlogs(log.path(), &mut stderr, 0, false).unwrap();
        assert_eq!(syscall_records, 1);
        assert_eq!(
            stderr,
            b"guest stderr\n INFO detcore: inbound syscall: getpid() = ?\n"
        );
        assert_eq!(
            std::fs::read_to_string(log.path()).unwrap(),
            format!(
                "{coordinator}\
                 1970-01-01T00:00:00.000000Z INFO detcore: DETLOG scheduler event\n\
                 1970-01-01T00:00:00.000000Z INFO detcore: DETLOG [syscall] finish syscall #1: getpid() = Ok(3)\n"
            ),
        );

        // In-guest LiteInst: the socket carries nothing but records.
        let log = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(log.path(), coordinator).unwrap();
        let mut records = b"INFO detcore::tool_local: DETLOG local state\n\
              INFO detcore: DETLOG [syscall] finish syscall #2: getppid() = Ok(1)\n"
            .to_vec();
        assert_eq!(
            extract_forwarded_detlogs(log.path(), &mut records, 0, true).unwrap(),
            1
        );
        assert!(records.is_empty());

        // A record the Tool could not send, or anything else, refuses the run.
        for (stream, expected) in [
            (
                format!(
                    "INFO detcore: DETLOG [syscall] kept\n{} 16777300 90\n",
                    detcore::detlog::FORWARDING_LOSS_NOTICE
                ),
                "could not forward 1 DETLOG record",
            ),
            (
                "INFO detcore: DETLOG [syscall] kept\nstray\n".to_owned(),
                "bytes outside any record",
            ),
        ] {
            let log = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(log.path(), coordinator).unwrap();
            let mut stream = stream.into_bytes();
            let error = extract_forwarded_detlogs(log.path(), &mut stream, 0, true).unwrap_err();
            assert!(error.to_string().contains(expected), "{error:#}");
        }
    }

    /// Forwarded records are appended within the log's size bound, counting the
    /// bytes already in it, and the log still ends in the truncation marker the
    /// comparators look for, so a run whose records were cut compares as no
    /// result. A log the bound already truncated gets nothing appended.
    #[test]
    fn forwarded_detlogs_respect_the_log_bound_and_keep_its_truncation_marker() {
        let records = |count: usize| {
            let mut bytes = Vec::new();
            for index in 0..count {
                bytes.extend_from_slice(
                    format!("INFO detcore: DETLOG [syscall] finish syscall #{index}\n").as_bytes(),
                );
            }
            bytes
        };
        let coordinator = "2026-08-02T00:00:00.000000Z INFO detcore: coordinator message\n";

        // Records that would cross the bound are cut, and the marker ends the log.
        let log = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(log.path(), coordinator).unwrap();
        let mut stream = records(100);
        let limit = coordinator.len() as u64 + 200;
        assert_eq!(
            extract_forwarded_detlogs(log.path(), &mut stream, limit, true).unwrap(),
            100
        );
        let text = std::fs::read_to_string(log.path()).unwrap();
        assert!(detcore::logdiff::log_was_truncated(&text), "{text}");
        assert!(text.starts_with(coordinator), "{text}");

        // A log the bound already truncated keeps the marker on its last line.
        let truncated = std::fs::read(log.path()).unwrap();
        let mut stream = records(3);
        extract_forwarded_detlogs(log.path(), &mut stream, 0, true).unwrap();
        assert_eq!(std::fs::read(log.path()).unwrap(), truncated);

        // Within the bound nothing is cut.
        let log = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(log.path(), coordinator).unwrap();
        let mut stream = records(2);
        extract_forwarded_detlogs(log.path(), &mut stream, 1 << 20, true).unwrap();
        let text = std::fs::read_to_string(log.path()).unwrap();
        assert!(!detcore::logdiff::log_was_truncated(&text), "{text}");
        assert_eq!(text.lines().count(), 3);
    }

    /// The four branches of [`summary_dir_under`].
    ///
    /// ⚠️ THESE EXIST BECAUSE THE FIRST VERSION OF THIS CHANGE HAD NO TEST THAT
    /// COULD REACH IT. Its tests lived in `hermit-manifest-plan` and asserted
    /// that a file under `ignored/` is not enumerated as source -- true, useful,
    /// and silent about whether anything ever writes one there. A private
    /// `src/bin` item is not importable from another crate, so that surface
    /// could not name this function even in principle. Two readers found it; I
    /// had already found and fixed a narrower blind spot in the same tests and
    /// still missed that the whole set was inert. Hence: same file, same crate,
    /// driving the function itself.
    #[test]
    fn summary_dir_is_the_work_tree_disposable_directory() {
        let tmp = tempfile::tempdir().expect("fixture root");
        let root = tmp.path();
        std::fs::create_dir(root.join(".git")).expect("fixture work tree");
        std::fs::write(root.join(".gitignore"), "/ignored/\n").expect("fixture ignore rule");

        // From the root.
        let chosen = summary_dir_under(root).expect("resolving from the work-tree root");
        assert_eq!(chosen, root.join("ignored"));
        assert!(chosen.is_dir(), "the disposable directory was not created");

        // From a subdirectory: the enumeration is rooted at the work tree, so a
        // summary in a tracked subdirectory would still be counted as source.
        let nested = root.join("hermit-cli").join("src");
        std::fs::create_dir_all(&nested).expect("fixture subdirectory");
        assert_eq!(
            summary_dir_under(&nested).expect("resolving from a subdirectory"),
            root.join("ignored"),
            "a summary resolved relative to the subdirectory rather than the work tree"
        );
    }

    #[test]
    fn summary_dir_outside_a_work_tree_is_unchanged() {
        let tmp = tempfile::tempdir().expect("fixture root");
        let plain = tmp.path().join("not-a-checkout");
        std::fs::create_dir(&plain).expect("fixture directory");
        assert_eq!(
            summary_dir_under(&plain).expect("resolving outside a work tree"),
            plain,
            "the previous behaviour must be kept where nothing enumerates source"
        );
    }

    /// A `.git` in an ancestor that does not ignore `ignored/` must be skipped.
    ///
    /// MEASURED on this host: `/tmp/.git` exists. An earlier version of this
    /// walk accepted any ancestor carrying a `.git`, so a run started in any
    /// temporary directory resolved its work tree to `/tmp` and would have
    /// written the summary to `/tmp/ignored` -- not the project, and not
    /// visible to both run containers, which is the property the whole
    /// arrangement exists to preserve. This test found it; I had not predicted
    /// it.
    #[test]
    fn a_stray_git_that_does_not_ignore_the_directory_is_not_a_work_tree() {
        let tmp = tempfile::tempdir().expect("fixture root");
        let stray = tmp.path();
        std::fs::create_dir(stray.join(".git")).expect("fixture stray git");
        // No .gitignore at all, as in the real /tmp/.git case.
        let nested = stray.join("scratch");
        std::fs::create_dir(&nested).expect("fixture directory");
        assert_eq!(
            summary_dir_under(&nested).expect("resolving under a stray git"),
            nested,
            "a stray .git without an ignore rule was mistaken for the work tree"
        );

        // And one that DOES ignore it is accepted, so the check discriminates
        // rather than refusing everything.
        std::fs::write(stray.join(".gitignore"), "/ignored/\n").expect("fixture ignore rule");
        assert_eq!(
            summary_dir_under(&nested).expect("resolving under a real work tree"),
            stray.join("ignored"),
            "a work tree that does ignore the directory was rejected"
        );
    }

    /// Serialises the two tests that change the process working directory.
    ///
    /// `private_verify_summary` reads the cwd through `private_summary_dir`, so
    /// exercising the WRITER means setting it. That is process-global, so it
    /// cannot run beside another test that also depends on it.
    static CWD_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Both writers must route through the resolver, not merely agree with it.
    ///
    /// ⚠️ THIS IS THE THIRD LEVEL OF THE SAME DEFECT ON THIS CHANGE, and the
    /// reviewer found it by mutation: reverting BOTH writers to
    /// `tempfile_in(current_dir())` left every test green. The resolver tests
    /// prove `summary_dir_under` computes the right directory; they say nothing
    /// about whether anything calls it. A future writer, or a revert of either
    /// existing one, would restore the original defect undetected.
    ///
    /// The earlier two levels were: the first test could not see the real
    /// .gitignore, and then ALL of them sat in a crate that cannot reach a
    /// private `src/bin` item. Each level was found by a different reader.
    ///
    /// So this calls the writers and looks at where the file actually landed.
    /// Under the mutation the parent is the work tree itself rather than its
    /// disposable directory, and both assertions below fail.
    #[test]
    fn both_writers_put_their_summary_in_the_disposable_directory() {
        let _guard = CWD_GUARD
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let tmp = tempfile::tempdir().expect("fixture root");
        let root = tmp.path().canonicalize().expect("canonical fixture root");
        std::fs::create_dir(root.join(".git")).expect("fixture work tree");
        std::fs::write(root.join(".gitignore"), "/ignored/\n").expect("fixture ignore rule");

        let previous = std::env::current_dir().expect("saving the working directory");
        std::env::set_current_dir(&root).expect("entering the fixture work tree");
        let produced = [
            ("verification", private_verify_summary()),
            ("backend engagement", private_backend_engagement_summary()),
        ];
        std::env::set_current_dir(&previous).expect("restoring the working directory");

        let expected = root.join("ignored");
        for (what, result) in produced {
            let file = result.unwrap_or_else(|error| panic!("{what} summary: {error:#}"));
            let parent = file
                .path()
                .parent()
                .expect("a summary has a parent")
                .canonicalize()
                .expect("canonical summary parent");
            assert_eq!(
                parent,
                expected,
                "the {what} summary was written to {} rather than the disposable \
                 directory, so it is counted as prepared source again",
                parent.display()
            );
        }
    }

    /// The control, and the reviewer's own case: a regular FILE named `ignored`.
    ///
    /// `create_dir_all` fails, and the first version answered by returning the
    /// work tree itself -- silently putting the summary back exactly where the
    /// defect was. Refusing is the only safe direction inside a work tree.
    #[test]
    fn summary_dir_refuses_rather_than_falling_back_into_the_work_tree() {
        let tmp = tempfile::tempdir().expect("fixture root");
        let root = tmp.path();
        std::fs::create_dir(root.join(".git")).expect("fixture work tree");
        std::fs::write(root.join(".gitignore"), "/ignored/\n").expect("fixture ignore rule");
        std::fs::write(root.join("ignored"), b"not a directory").expect("fixture file");

        let error = summary_dir_under(root)
            .expect_err("a work tree that cannot hold the disposable directory must refuse");
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("refusing to fall back"),
            "the refusal did not say why it refused: {rendered}"
        );
    }
}

#[cfg(all(
    test,
    any(feature = "native-exec", feature = "native-exec-input-admission")
))]
fn assert_policy_refusal_exit(error: &Error) {
    assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
    assert_eq!(super::failure_exit_code(error), 122, "{error:#}");
    assert_eq!(
        super::classify_failure(error),
        "HERMIT_POLICY_REFUSAL class=policy-refusal",
        "{error:#}"
    );
}

// The first-party split removed dbt/e9patch from the core enum:
// https://github.com/rrnewton/hermit/commit/ee51613d1cd
// Their dedicated frontdoors retain the physical owner controls. This
// private-state selection also checks the real main path without a synthetic adapter.
#[test]
#[cfg(feature = "native-exec")]
fn native_enrollment_external_guard_does_not_create_a_capture() {
    let mut options = RunOpts::parse_from([
        "run",
        "--verify",
        "--native-exec-inputs=/nonexistent-native-declaration",
        "/nonexistent-native-guest",
    ]);
    options
        .select_application_backend("native-enrollment-guard-control")
        .unwrap();
    let global = GlobalOpts::parse_from(["hermit"]);
    let error = options.main(&global).unwrap_err();
    assert_eq!(
        error.to_string(),
        "an external physical adapter cannot consume a native exec enrollment"
    );
    assert_policy_refusal_exit(&error);
    assert!(options.native_exec_capture.is_none());
}

#[test]
#[cfg(all(feature = "native-exec-input-admission", not(feature = "native-exec")))]
fn native_admission_only_refuses_before_program_or_declaration_access() {
    for (backend, expected) in [
        ("ptrace", "native exec currently requires in-guest-trap"),
        ("liteinst", "native exec currently requires in-guest-trap"),
        (
            "in-guest-trap",
            "native exec execution is not compiled into this executable",
        ),
    ] {
        let backend_flag = format!("--backend={backend}");
        let global = GlobalOpts::parse_from(["hermit", &backend_flag]);
        let mut options = RunOpts::parse_from([
            "run",
            "--verify",
            "--native-exec-inputs=/nonexistent-native-declaration",
            "/nonexistent-native-guest",
        ]);
        let error = options.main(&global).unwrap_err();
        assert_eq!(error.to_string(), expected, "{backend}: {error:#}");
        assert_policy_refusal_exit(&error);
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("rebuild hermit-run with --features native-exec"));
        assert!(diagnostic.contains("drop --native-exec-inputs"));
    }

    let global = GlobalOpts::parse_from(["hermit"]);
    let mut options = RunOpts::parse_from([
        "run",
        "--verify",
        "--native-exec-inputs=/nonexistent-native-declaration",
        "/nonexistent-native-guest",
    ]);
    options
        .select_application_backend("native-enrollment-guard-control")
        .unwrap();
    let error = options.main(&global).unwrap_err();
    assert_eq!(
        error.to_string(),
        "an external physical adapter cannot consume a native exec enrollment"
    );
    assert_policy_refusal_exit(&error);
    let diagnostic = format!("{error:#}");
    assert!(diagnostic.contains("rebuild hermit-run with --features native-exec"));
    assert!(diagnostic.contains("drop --native-exec-inputs"));

    // The same words without the marker remain an internal CLI error.
    let untyped = Error::msg(error.to_string());
    assert_eq!(super::failure_exit_code(&untyped), 125);
    assert_eq!(
        super::classify_failure(&untyped),
        "HERMIT_INTERNAL_FAILURE class=cli-error"
    );
}

#[test]
#[cfg(all(feature = "native-exec-input-admission", not(feature = "native-exec")))]
fn native_admission_only_preserves_requests_without_native_enrollment() {
    let options = RunOpts::parse_from(["run", "/nonexistent-ordinary-guest"]);
    for backend in [Backend::Ptrace, Backend::Liteinst, Backend::InGuestTrap] {
        options.refuse_uncompiled_native_exec(backend).unwrap();
    }

    // No enrollment: real main must preserve the existing exec-stop owner
    // refusal on ptrace, before any nonexistent program can be launched.
    let global = GlobalOpts::parse_from(["hermit", "--backend=ptrace"]);
    let mut options =
        RunOpts::parse_from(["run", "--in-guest-exec-stop", "/nonexistent-ordinary-guest"]);
    assert!(!options.program_path().exists());
    let error = options.main(&global).unwrap_err();
    assert_eq!(
        error.to_string(),
        concat!(
            "--in-guest-exec-stop: exec-stop-runtime-not-ready: requires the packaged private runtime, ",
            "in-guest-trap, and supported direct-run or singleton verification options"
        )
    );
    assert_policy_refusal_exit(&error);
}

#[test]
#[cfg(any(feature = "native-exec", feature = "native-exec-input-admission"))]
fn native_enrollment_still_requires_a_verification_mode() {
    use clap::CommandFactory;

    let error = RunOpts::try_parse_from([
        "run",
        "--native-exec-inputs=/nonexistent-native-declaration",
        "/nonexistent-native-guest",
    ])
    .unwrap_err();
    assert_eq!(
        error.kind(),
        clap::error::ErrorKind::MissingRequiredArgument
    );

    let command = RunOpts::command();
    let help = command
        .get_arguments()
        .find(|argument| argument.get_id() == "native_exec_inputs")
        .and_then(|argument| argument.get_help())
        .expect("the admitted flag has help")
        .to_string();
    #[cfg(feature = "native-exec")]
    {
        assert!(help.contains("Freeze explicitly declared dynamic files"));
        assert!(help.contains("Static files, scripts, threads, fork and timers"));
    }
    #[cfg(not(feature = "native-exec"))]
    {
        assert!(help.contains("Recognize native exec enrollment requests"));
        assert!(help.contains("refuse execution in this admission-only build"));
        assert!(help.contains("--features native-exec"));
        assert!(help.contains("drop --native-exec-inputs"));
        assert!(!help.contains("Freeze explicitly declared dynamic files"));
    }
}
