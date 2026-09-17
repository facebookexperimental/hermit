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
use std::fs::OpenOptions;
use std::hash::Hash;
use std::hash::Hasher;
use std::io::Read;
use std::io::Write;
use std::num::NonZeroU64;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::LazyLock;
use std::time::Duration;

use clap::Parser;
use colored::Colorize;
use detcore_model::backend_engagement::BackendEngagement;
use detcore_model::backend_engagement::BackendEngagementReport;
use detcore_model::happens_before::HappensBeforeProgram;
use detcore_model::happens_before::Strength;
use detcore_model::summary::RunSummary;
use hermit::Backend;
use hermit::Context;
use hermit::DetConfig;
use hermit::Error;
use hermit::Shebang;
use hermit::SkidOvershootError;
use hermit::happens_before::DebugInfoResolver;
use hermit::happens_before::describe_anchor;
use hermit::happens_before::load_program;
use hermit::happens_before::resolve_program;
use hermit::run_evidence::GuestRunDeterminism;
use reverie::Errno;
use reverie::process::Bind;
use reverie::process::Command;
use reverie::process::Container;
use reverie::process::ExitStatus;
use reverie::process::Mount;
use reverie::process::MountFlags;
use reverie::process::Namespace;
use reverie::process::Output;

use super::container::IdentityGuard;
use super::container::PolicyRefusal;
use super::container::apply_affinity;
use super::container::default_container;
use super::container::identity_hardening_mounts;
use super::container::image_container;
use super::container::with_container;
use super::global_opts::GlobalOpts;
use super::guest_capture::GuestRunCapturePaths;
use super::guest_capture::GuestRunCaptureSession;
use super::record_envelope::RecordEnvelope;
use super::tracing::BoundedWriter;
use super::tracing::init_sync_file_tracing;
use super::tracing::log_max_bytes;
use super::verify::ComparedRun;
use super::verify::ComparisonOptions;
use super::verify::LogCompareStrictness;
use super::verify::NoResultReason;
use super::verify::VerificationReport;
use super::verify::VerificationRuntime;
use super::verify::announce_verification_outcome;
use super::verify::compare_two_runs;
use super::verify::default_failed_verify_log_retention;
use super::verify::retain_verification_logs;
use super::verify::temp_log_files_in;
use super::verify::validate_log_level;
use super::verify::verification_log_level;
use super::verify::verification_runtime_from_summaries;
use super::verify::write_pending_verification_json;
use super::verify::write_report_json;
use super::verify::write_skid_overshoot_verification_json;
use super::verify::write_skid_overshoot_without_comparison_json;
use super::verify::write_verification_json;

const TMP_DIR: &str = "/tmp";
const FAIL_CLOSED_ENV: &str = "HERMIT_FAIL_CLOSED";
const NORMALIZED_SABRE_DETLOG_TIMESTAMP: &str = "1970-01-01T00:00:00.000000Z";

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
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
            eprintln!("WARNING: verification runtime statistics unavailable: {error:#}");
            None
        }
    }
}

fn first_run_rejected_report(
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
///     git ls-files --others --exclude-standard -z
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
    // location is still correct and still visible to both run containers.
    Ok(start.to_path_buf())
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

fn extract_sabre_detlogs(path: &Path, stderr: &mut Vec<u8>) -> Result<usize, Error> {
    let mut log = OpenOptions::new().append(true).open(path)?;
    let mut guest_stderr = Vec::with_capacity(stderr.len());
    let mut syscall_records = 0;
    for line in stderr.split_inclusive(|byte| *byte == b'\n') {
        let start = line
            .iter()
            .position(|byte| !byte.is_ascii_whitespace())
            .unwrap_or(line.len());
        let payload = line[start..].strip_suffix(b"\n").unwrap_or(&line[start..]);
        let payload = payload.strip_suffix(b"\r").unwrap_or(payload);
        if payload.starts_with(b"INFO detcore") && contains_bytes(payload, b" DETLOG ") {
            log.write_all(NORMALIZED_SABRE_DETLOG_TIMESTAMP.as_bytes())?;
            log.write_all(b" ")?;
            log.write_all(payload)?;
            log.write_all(b"\n")?;
            syscall_records += usize::from(contains_bytes(payload, b"DETLOG [syscall]"));
        } else {
            guest_stderr.extend_from_slice(line);
        }
    }
    *stderr = guest_stderr;
    Ok(syscall_records)
}
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

    /// Path to a happens-before specification (JSON; see RFC #1146) that places
    /// deterministic ordering edges between anchored events. Anchor code
    /// locations (`func`, `file:line`) are resolved against the guest program's
    /// debug info. Scheduler enforcement is not yet wired; combine with
    /// `--hb-list-events` to preview how the spec resolves against the binary.
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

    #[clap(flatten)]
    pub(crate) det_opts: DetOptions,

    /// Require Hermit's deterministic defaults and reject incompatible opt-outs. Unsupported
    /// syscalls already fail closed in ordinary runs.
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

    /// Override the processor-specific PMU skid margin, in retired conditional branches. Larger
    /// values schedule the overflow interrupt earlier and can increase single-stepping overhead.
    // TODO-HUMAN-REVIEW(PR-991): Review the ptrace PMU skid-margin CLI override.
    #[clap(long, value_name = "RCBS")]
    skid_margin: Option<u64>,

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

    /// PROTOTYPE: run the guest against the root filesystem of a pinned OCI image
    /// instead of the host filesystem. The reference is materialized (pulled and
    /// unpacked, rootless) via `buildah` and the guest is `chroot`ed into a
    /// read-only cached root with an isolated writable `/tmp`, so its FILE INPUTS
    /// come deterministically from the image. Pin by digest for
    /// reproducibility, e.g.
    /// `--image docker.io/library/busybox@sha256:...`. The guest program path
    /// must resolve inside the image: give an absolute path (e.g. `/bin/sh`) or
    /// a relative path containing a `/` (e.g. `./bin/sh`), which is resolved
    /// against the image working directory; a bare command name (PATH search
    /// inside the image) is not yet supported. Requires namespaces (incompatible
    /// with `--no-namespace`). The prototype currently supports only the ptrace
    /// backend and does not yet compose with custom `--mount`/`--bind` options.
    #[clap(long, value_name = "OCI-REFERENCE")]
    image: Option<String>,

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
    /// of substantially larger logs and stricter comparison. Implies the strict,
    /// bitwise comparison of --verify-strict, and additionally raises the diff
    /// verbosity (larger logs, more syscall history).
    #[clap(long, requires = "verify")]
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
    #[clap(long, requires = "verify")]
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

    /// Retain both captured verification logs after either a match or a
    /// divergence. Logs are written under --verify-log-dir when provided;
    /// otherwise under $XDG_STATE_HOME/hermit/verify-logs (normally
    /// ~/.local/state/hermit/verify-logs). The final paths are printed.
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

    /// Print a summary of the process tree's execution to stderr before exiting.
    #[clap(long, short = 'u')]
    pub(crate) summary: bool,

    /// Print a machine readable version of --summary to a file.
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

    /// Retain typed evidence for this one ordinary run in a newly created
    /// directory. NEW_PATH must not exist. Hermit writes a private synchronous
    /// INFO log without changing the guest's standard descriptors, validates it
    /// under BitwiseInfoV1, publishes the log with a SHA-256 digest, and publishes
    /// manifest.json last. A missing, malformed, truncated, or empty log is a
    /// no-result, never successful evidence. This is currently supported by
    /// ptrace, LiteInst, and KVM. DBT is refused because its authenticated evidence
    /// transport currently implies isolated process-group policy that can change
    /// guest setsid/setpgid results. SaBRe is refused because plugin DETLOG is
    /// emitted only on shared stderr. KVM's manifest records its exit-code-only
    /// disposition limitation.
    #[clap(
        long,
        conflicts_with_all = ["verify", "namespace_only"],
        value_name = "NEW_PATH"
    )]
    run_evidence_dir: Option<PathBuf>,

    /// Harness-only terminal sidecar for one ordinary run's exact guest
    /// stdout/stderr. All three capture paths must be new, distinct children of
    /// one non-symlink directory shared with --run-evidence-dir. Supported only
    /// by ptrace, LiteInst, and KVM. KVM copies its existing virtual-console
    /// output into the held files because its guest has no host file descriptors.
    #[clap(
        long,
        value_name = "NEW_PATH",
        requires_all = ["guest_stdout", "guest_stderr", "run_evidence_dir"],
        conflicts_with_all = ["verify", "namespace_only"]
    )]
    run_result_json: Option<PathBuf>,

    /// Harness-only no-clobber regular file inherited as guest stdout.
    /// Requires --run-result-json, --guest-stderr, and --run-evidence-dir.
    #[clap(
        long,
        value_name = "NEW_PATH",
        requires_all = ["run_result_json", "guest_stderr", "run_evidence_dir"],
        conflicts_with_all = ["verify", "namespace_only"]
    )]
    guest_stdout: Option<PathBuf>,

    /// Harness-only no-clobber regular file inherited as guest stderr.
    /// Requires --run-result-json, --guest-stdout, and --run-evidence-dir.
    #[clap(
        long,
        value_name = "NEW_PATH",
        requires_all = ["run_result_json", "guest_stdout", "run_evidence_dir"],
        conflicts_with_all = ["verify", "namespace_only"]
    )]
    guest_stderr: Option<PathBuf>,

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

    /// Complete root-image preparation result, retained so later consumers do
    /// not have to recover any count from the presentation banner.
    #[clap(skip)]
    e9patch_engagement: Option<BackendEngagement>,
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
            command.env_clear();
            command.env("HOSTNAME", "hermetic-container.local");
            command.env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            );
            command.env("HOME", "/root");
        }
        BaseEnv::Host => {}
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

/// Displays as a string which needs only to be prepended with "hermit " to be a runnable command.
impl fmt::Display for RunOpts {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut dop = self.det_opts.det_config.clone();
        // Fail-closed is the `run` default, so serialize only the compatibility
        // opt-out. `DetConfig` is also used outside `run` and retains a neutral
        // false default, so its Display would otherwise add the explicit opt-in.
        dop.panic_on_unsupported_syscalls = false;

        if let Some(backend) = self.backend {
            write!(f, " --backend={}", backend.as_str())?;
        }
        if let Some(skid_margin) = self.skid_margin {
            write!(f, " --skid-margin={skid_margin}")?;
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
        if let Some(p) = &self.backend_engagement_json {
            let s = p.to_str().expect("valid unicode path");
            write!(f, " --backend-engagement-json={}", shell_words::quote(s))?;
        }
        if let Some(p) = &self.run_evidence_dir {
            let s = p.to_str().expect("valid unicode path");
            write!(f, " --run-evidence-dir={}", shell_words::quote(s))?;
        }
        for (name, path) in [
            ("run-result-json", self.run_result_json.as_ref()),
            ("guest-stdout", self.guest_stdout.as_ref()),
            ("guest-stderr", self.guest_stderr.as_ref()),
        ] {
            if let Some(path) = path {
                write!(
                    f,
                    " --{name}={}",
                    shell_words::quote(&path.to_string_lossy())
                )?;
            }
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
/// both produce a 9,233-byte console. Combined with `--strict` it is worse than
/// useless — strict mode rejects the now-unvirtualized `gettimeofday` and Hermit
/// exits 1 before the guest starts. Recommending it sent readers down a dead end
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
        ("dbt", Backend::Dbt),
        ("liteinst", Backend::Liteinst),
        ("sabre", Backend::Sabre),
        ("kvm", Backend::Kvm),
        ("e9patch", Backend::E9patch),
    ] {
        let mut ro = RunOpts::parse_from(["fakehermit", "--backend", value, "fakeprog"]);
        ro.validate_args_with_perf_support(true).unwrap();
        assert_eq!(ro.backend, Some(expected));
        assert_eq!(ro.selected_backend(), expected);
        let normalized = format!(" --backend={value} -- fakeprog");
        assert_eq!(format!("{}", ro), normalized);
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
    for (value, backend) in [
        ("ptrace", Backend::Ptrace),
        ("dbt", Backend::Dbt),
        ("liteinst", Backend::Liteinst),
        ("sabre", Backend::Sabre),
        ("kvm", Backend::Kvm),
        ("e9patch", Backend::E9patch),
    ] {
        let plain = RunOpts::parse_from(["fakehermit", "--backend", value, "--verify", "fakeprog"]);
        assert_eq!(plain.selected_backend(), backend);
        assert_eq!(
            plain.verification_strictness(),
            LogCompareStrictness::Stripped,
            "plain --verify on {value} must stay on the lossy comparator, so it \
             cannot claim canonical bitwise parity"
        );

        for flag in ["--verify-strict", "--verify-verbose"] {
            let canonical = RunOpts::parse_from([
                "fakehermit",
                "--backend",
                value,
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
    for backend in ["ptrace", "dbt", "liteinst", "sabre", "kvm", "e9patch"] {
        let run = RunOpts::parse_from(["fakehermit", "--backend", backend, "--verify", "fakeprog"]);
        assert!(
            run.verification_comparison_options().compare_logs,
            "--verify on {backend} must compare both retained logs"
        );
    }
}

#[test]
fn every_backend_keeps_io_buffer_checking_on_by_default() {
    for backend in ["ptrace", "dbt", "liteinst", "sabre", "kvm", "e9patch"] {
        let run = RunOpts::parse_from([
            "fakehermit",
            "--backend",
            backend,
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
        if run.selected_backend() != Backend::Dbt {
            // DBT has its own production comparator in `backends.rs`; the
            // no-JSON content-mutation integration test exercises that path.
            assert_eq!(
                run.verification_comparison_options().compare_io_buffers,
                prepared.detlog_io_buffers,
                "{backend} reported an io-buffer comparison state different from the \
                 configuration that backend actually receives"
            );
        }

        let opted_out = RunOpts::parse_from([
            "fakehermit",
            "--backend",
            backend,
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
        if opted_out.selected_backend() != Backend::Dbt {
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
    let ro = RunOpts::parse_from(["fakehermit", "--backend", "ptrace", "/bin/../bin/echo"]);
    ro.validate_program().unwrap();
}

/// The guest environment must be identical across backends. Sanitizer leak
/// detection is disabled in `guest_command()` (the single backend-independent
/// place) rather than only in the ptrace tracer, so the out-of-process KVM
/// backend presents the guest the same environment as ptrace/e9patch/sabre.
/// Regression test for the KVM-vs-ptrace env divergence, where the guest saw
/// two fewer variables under KVM.
#[test]
fn guest_env_disables_sanitizer_leak_detection_on_every_backend() {
    let asan = OsStr::new("ASAN_OPTIONS");
    let lsan = OsStr::new("LSAN_OPTIONS");
    let expected = OsStr::new("detect_leaks=0").to_os_string();

    // The two variables are present with identical values regardless of the
    // selected backend, so no backend-specific spawn hook is required for parity.
    for backend in ["ptrace", "kvm", "sabre", "dbt"] {
        let ro = RunOpts::parse_from(["fakehermit", "--backend", backend, "/bin/echo", "hi"]);
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
        4,
        "unexpected namespace-only environment: {envs:?}"
    );
    assert_eq!(
        envs.get(OsStr::new("HOME")).map(|value| value.as_os_str()),
        Some(OsStr::new("/root"))
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
fn dbt_rejects_mount_and_bind_but_accepts_workdir() {
    let mut with_mount = RunOpts::parse_from([
        "fakehermit",
        "--backend",
        "dbt",
        "--mount=type=tmpfs,target=/test",
        "/bin/true",
    ]);
    let error = with_mount
        .validate_args_with_perf_support(true)
        .unwrap_err()
        .to_string();
    assert!(error.contains("dbt backend cannot apply --mount"));

    let mut with_bind = RunOpts::parse_from([
        "fakehermit",
        "--backend",
        "dbt",
        "--bind=/tmp:/test",
        "/bin/true",
    ]);
    let error = with_bind
        .validate_args_with_perf_support(true)
        .unwrap_err()
        .to_string();
    assert!(error.contains("dbt backend cannot apply --mount"));

    let mut with_workdir = RunOpts::parse_from([
        "fakehermit",
        "--backend",
        "dbt",
        "--workdir",
        "/test",
        "/bin/true",
    ]);
    with_workdir.validate_args_with_perf_support(true).unwrap();
    assert_eq!(
        with_workdir.guest_command().unwrap().get_current_dir(),
        Some(Path::new("/test"))
    );
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
fn unsupported_syscalls_fail_closed_by_default_with_explicit_opt_out() {
    let mut normal = RunOpts::parse_from(["fakehermit", "fakeprog"]);
    normal.validate_args_with_perf_support(true).unwrap();
    assert!(normal.det_opts.det_config.panic_on_unsupported_syscalls);
    assert!(!normal.det_opts.det_config.passthru_opt);
    assert_eq!(format!("{}", normal), " -- fakeprog");

    let mut strict = RunOpts::parse_from(["fakehermit", "--strict", "fakeprog"]);
    strict.validate_args_with_perf_support(true).unwrap();

    assert!(strict.det_opts.det_config.sequentialize_threads);
    assert!(strict.det_opts.det_config.deterministic_io);
    assert!(!strict.det_opts.det_config.passthru_opt);
    assert!(strict.det_opts.det_config.panic_on_unsupported_syscalls);
    assert_eq!(format!("{}", strict), " -- fakeprog");

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
        " --allow-unsupported-syscalls -- fakeprog"
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
        " --allow-unsupported-syscalls --passthru-opt -- fakeprog"
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
fn skid_margin_override_parses_and_round_trips() {
    let mut opts = RunOpts::parse_from(["fakehermit", "--skid-margin=500", "fakeprog"]);
    opts.validate_args_with_perf_support(true).unwrap();

    assert_eq!(opts.skid_margin, Some(500));
    assert_eq!(format!("{opts}"), " --skid-margin=500 -- fakeprog");
}

#[test]
fn skid_margin_override_rejects_non_ptrace_backed_backends() {
    for backend in ["dbt", "kvm", "sabre"] {
        let mut opts = RunOpts::parse_from([
            "fakehermit",
            &format!("--backend={backend}"),
            "--skid-margin=500",
            "fakeprog",
        ]);
        let error = opts.validate_args_with_perf_support(true).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("requires a ptrace-backed backend"),
            "unexpected {backend} error: {error}"
        );
    }
}

#[test]
fn skid_margin_override_is_available_to_liteinst_host_hybrid() {
    let mut opts = RunOpts::parse_from([
        "fakehermit",
        "--backend=liteinst",
        "--skid-margin=500",
        "fakeprog",
    ]);
    opts.validate_args_with_perf_support(true).unwrap();
    assert_eq!(opts.skid_margin, Some(500));
    assert_eq!(
        format!("{opts}"),
        " --backend=liteinst --skid-margin=500 -- fakeprog"
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

#[test]
fn strict_rejects_every_route_to_host_networking() {
    // Explicit, and the two routes that reach host networking as a side effect
    // of a flag that is not about networking. All three were accepted silently
    // before: measured from inside the guest, each showed interfaces `eth0 lo`
    // where `--strict` alone shows `lo`.
    for (args, expected_cause) in [
        (vec!["--strict", "--network=host"], "--network=host"),
        (vec!["--strict", "--no-namespace"], "--no-namespace"),
        (vec!["--strict", "--gdbserver"], "--gdbserver"),
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

#[test]
fn non_strict_runs_still_allow_host_networking() {
    // The refusal is about `--strict` claiming something it did not enforce,
    // not about forbidding host networking outright.
    let mut opts = RunOpts::parse_from(["fakehermit", "--network=host", "fakeprog"]);
    opts.validate_args_with_perf_support(true)
        .expect("host networking without --strict must remain allowed");
    assert_eq!(opts.network, NetworkingMode::Host);
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
fn dbt_backend_disables_uts_assumption() {
    // The DBT backend runs the guest under DynamoRIO without Reverie's
    // `Container`, so it never enters a UTS namespace or applies the
    // deterministic hostname. `has_uts_namespace` must therefore be false even
    // with namespaces otherwise enabled, so Detcore's `handle_uname` rewrites
    // the nodename to `hermetic-container.local` instead of leaking the host
    // FQDN. Regression guard for DBT uname parity with the ptrace backend.
    let mut opts = RunOpts::parse_from(["fakehermit", "--backend=dbt", "fakeprog"]);
    opts.validate_args_with_perf_support(true).unwrap();
    assert_eq!(opts.selected_backend(), Backend::Dbt);
    assert!(!opts.no_namespace);
    assert!(!opts.det_opts.det_config.has_uts_namespace);
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
    let mut backend = RunOpts::parse_from([
        "fakehermit",
        "--image=busybox@sha256:deadbeef",
        "--backend=dbt",
        "/bin/sh",
    ]);
    let message = backend
        .validate_args_with_perf_support(true)
        .unwrap_err()
        .to_string();
    assert!(
        message.contains("only the ptrace backend"),
        "message: {message}"
    );

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
        "--preemption-timeout",
        "--target-timeslice",
        "syscall boundaries",
        "--backend <BACKEND>",
        "Select the process instrumentation backend",
        "ptrace",
        "dbt",
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
    Shebang::interpreter_from_buf(&bytes[..count])
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
            .map(|root| crate::image::resolve_in_rootfs(root, &interpreter))
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
///   * a bare command name (no `/`) would require a PATH search inside the
///     image, which the prototype does not implement, so it is rejected.
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

/// Create two logging destinations and two global configs. Returns non-zero exit
/// status if there was a difference in any component of the output.
impl RunOpts {
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

    pub(crate) fn run_evidence_request(
        &self,
        global_backend: Option<Backend>,
    ) -> Option<(&Path, Backend)> {
        self.run_evidence_dir.as_deref().map(|directory| {
            let backend = self.backend.or(global_backend).unwrap_or_default();
            (directory, backend)
        })
    }

    fn guest_run_capture_paths(&self) -> Result<Option<GuestRunCapturePaths>, Error> {
        match (
            self.run_result_json.as_ref(),
            self.guest_stdout.as_ref(),
            self.guest_stderr.as_ref(),
        ) {
            (None, None, None) => Ok(None),
            (Some(result), Some(stdout), Some(stderr)) => Ok(Some(GuestRunCapturePaths::new(
                result.clone(),
                stdout.clone(),
                stderr.clone(),
            ))),
            _ => Err(Error::msg(
                "--run-result-json, --guest-stdout, and --guest-stderr must be supplied together",
            )),
        }
    }

    fn check_capture_companions(&self, paths: &GuestRunCapturePaths) -> Result<(), Error> {
        paths.require_distinct_from(
            self.backend_engagement_json.as_deref(),
            "--backend-engagement-json",
        )?;
        paths.check_host_summary(self.summary_json.as_deref())
    }

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

    /// Which comparator a `--verify` run uses, as a function of the request
    /// ALONE.
    ///
    /// DELIBERATELY BACKEND-INDEPENDENT, AND THAT IS THE PROPERTY UNDER GUARD.
    /// This used to read `let kvm_output_only = self.selected_backend() ==
    /// Backend::Kvm`, so plain KVM verification bypassed internal-log comparison
    /// entirely and `--verify-strict` could not reach the canonical comparator on
    /// that backend at all. The special case was not narrowed, it was removed:
    /// every backend now retains both logs and picks its comparator here from
    /// `verify_verbose`/`verify_strict` only.
    ///
    /// Removing a branch leaves nothing behind to notice its return, so
    /// `comparator_choice_does_not_depend_on_the_backend` pins the absence
    /// across every `Backend` variant. Re-introducing a backend term here is the
    /// regression it exists to catch.
    ///
    /// `--verify-verbose` historically implied a bitwise compare (it flipped
    /// `strip_lines` off and `FullTrace` on); that is preserved, and
    /// `--verify-strict` selects the same comparison quietly.
    fn verification_strictness(&self) -> LogCompareStrictness {
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
        let config =
            hermit::prepare_backend_config(self.effective_det_config(), self.runtime_backend());
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
            failed_log_retention: (!self.keep_logs).then(default_failed_verify_log_retention),
            record_envelope: RecordEnvelope::all_records_v1(),
            // Read from the LIVE config, not a constant: `--no-virtualize-time`
            // makes this a genuine runtime choice on the run path, so a hard-coded
            // `true` would publish a time policy the run did not use. The
            // record/replay path is a fixed decision instead and names it once, as
            // `RECORD_REPLAY_VIRTUALIZES_TIME`.
            virtualize_time: config.virtualize_time,
        }
    }

    fn verify_liteinst_activation(&self) -> Result<(), Error> {
        let executable = std::env::current_exe().context("locate Hermit LiteInst probe")?;
        let mut command = Command::new(executable);
        command
            .env_clear()
            .env(super::LITEINST_ACTIVATION_PROBE_ENV, "1");
        let output = hermit::run_with_output_backend(
            command,
            self.effective_det_config(),
            false,
            &None,
            Backend::Liteinst,
        )?;
        let expected = b"hermit-liteinst-activation calls=32 traps=1 hooks=31\n";
        if output.status != ExitStatus::Exited(0) || output.stdout != expected {
            anyhow::bail!(
                "LiteInst activation probe failed closed: status={:?}, stdout={:?}, stderr={:?}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
        Ok(())
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

    pub fn main(&mut self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        // Set up an early tracing option before we're ready to set the global default:

        // The backend may be given in the preferred global position
        // (`hermit --backend X run ...`) or, for backwards compatibility, after the
        // subcommand (`hermit run --backend X ...`). An explicit subcommand-level
        // value wins; otherwise fall back to the global one.
        self.backend = self.backend.or(global.backend);
        let guest_capture_paths = self.guest_run_capture_paths()?;
        if let Some(paths) = &guest_capture_paths {
            // Evidence is intentionally claimed by main before this preflight.
            // Refuse capture collisions before clearing engagement or creating
            // streams; summary also requires a check in its writer namespace.
            self.check_capture_companions(paths)?;
        }
        if let Some(path) = &self.backend_engagement_json {
            clear_machine_record(path, "backend engagement")?;
        }
        if self.verify {
            validate_log_level(global)?;
        }
        let dbt_verification_stdin = if self.verify && self.selected_backend() == Backend::Dbt {
            // DBT owns its two-run adapter and replays this descriptor there.
            // The common output-capturing backends reserve the same input in
            // `hermit` because their execution path lives in the library.
            super::startup_stdin()?
        } else if self.verify {
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

        // TODO(T124429978): temporarily disabling this because it inexplicably clobbers our
        // subsequent tracing_subscriber::fmt::init() call.
        // tracing::subscriber::with_default(super::tracing::stderr_subscriber(global.log), || {
        self.validate_args()?;
        if self.allow_unsupported_syscalls {
            eprintln!(
                "WARNING: --allow-unsupported-syscalls permits unmodeled syscalls to reach the \
                 host; a successful exit does not establish complete deterministic execution."
            );
        }
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
        } else if backend != Backend::Kvm {
            // E9patch's own availability check covers both its ptrace runtime and
            // the `e9patch` cargo feature (it reports "not included in this build"
            // when the feature is disabled), so it no longer needs a special case.
            backend.ensure_available()?;
        }
        self.install_pmu_config()?;
        // The KVM backend reaches real reverie-kvm code from its dispatch path
        // and reports an accurate, program-specific error there, so it is not
        // pre-empted by the generic availability probe above. E9patch is a CLI
        // preprocessor and probes its ptrace runtime and tool separately.
        self.validate_mount_sources()?;
        self.validate_program()?;
        // ⚠️ HERE, NOT IN `run()`. The DBT arm below RETURNS `run_dbt(..)` and
        // never reaches `RunOpts::run`, so a check placed there covers every
        // backend except the one measured furthest from working. Same shape as
        // the `--namespace-only` second launch path documented in `run()`: the
        // first placement worked and was not yet complete. Measured: with the
        // check in `run()`, `--backend dbt --timeout 3` still accepted the flag
        // and ran unbounded.
        self.ensure_timeout_supported()?;
        if self.hb_list_events {
            return self.list_happens_before_events();
        }
        if self.happens_before.is_some() {
            // Resolve the spec against the guest binary's debug info now and cache
            // it so every subsequent `effective_det_config()` (including both
            // `--verify` runs) hands the scheduler the identical resolved program.
            self.resolved_happens_before = Some(self.load_and_resolve_happens_before()?);
        }
        if backend == Backend::E9patch {
            self.prepare_e9patch_program()?;
        }
        let mut guest_capture = guest_capture_paths
            .map(|paths| {
                let evidence = self.run_evidence_dir.as_deref().ok_or_else(|| {
                    Error::msg("harness guest capture requires --run-evidence-dir")
                })?;
                GuestRunCaptureSession::create(&paths, evidence)
            })
            .transpose()?;
        let private_engagement_summary = if self.backend_engagement_json.is_some()
            && backend == Backend::Ptrace
            && self.summary_json.is_none()
        {
            let file = private_backend_engagement_summary()?;
            self.summary_json = Some(file.path().to_owned());
            Some(file)
        } else {
            None
        };
        // });

        // DBT uses its dedicated CLI launch adapter. SaBRe, LiteInst, KVM,
        // e9patch, and ptrace use the common container and run/verify machinery.
        match backend {
            Backend::Ptrace
            | Backend::Liteinst
            | Backend::Sabre
            | Backend::Kvm
            | Backend::E9patch => {}
            Backend::Dbt => {
                let environment = self.guest_command()?.get_captured_envs();
                // Keep the dedicated DynamoRIO launcher, but give it the same
                // backend capability configuration as the public library path.
                let config = hermit::prepare_backend_config(self.effective_det_config(), backend);
                let retained_log_dir = self.retained_verify_log_dir()?;
                return super::backends::run_dbt(
                    &self.program,
                    &self.args,
                    self.verify,
                    self.verify_verbose,
                    self.verify_allow,
                    self.print_verify_logs,
                    self.keep_logs,
                    retained_log_dir.as_deref(),
                    self.verify_json.as_deref(),
                    self.summary,
                    self.backend_engagement_json.as_deref(),
                    global.log,
                    global.log_file.as_deref(),
                    &config,
                    environment,
                    self.workdir.as_deref().map(Path::new),
                    dbt_verification_stdin,
                );
            }
        }

        if backend == Backend::Liteinst {
            self.verify_liteinst_activation()?;
            eprintln!(
                "hermit: [liteinst host hybrid] activation verified (traps=1, hooks=31); Detcore Tool active in ptrace host"
            );
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
            let (status, _) = self.run_with_guest_capture(global, false, guest_capture.as_ref())?;
            if let Some(capture) = &mut guest_capture {
                let config = hermit::prepare_backend_config(
                    self.effective_det_config(),
                    self.runtime_backend(),
                );
                capture.finish(
                    self.selected_backend(),
                    status,
                    GuestRunDeterminism {
                        detlog_io_buffers: config.detlog_io_buffers,
                        virtualize_time: config.virtualize_time,
                    },
                )?;
            }
            self.write_backend_engagement_after_run(guest_capture.as_ref())?;
            drop(private_engagement_summary);
            Ok(status)
        }
    }

    /// Some arguments imply others. This is the place where that validation occurs.
    /// Also this performs side effects like accessing system randomness to implement --seed-from=SystemArgs
    pub fn validate_args(&mut self) -> Result<(), Error> {
        let perf_supported = match self.selected_backend() {
            Backend::Ptrace | Backend::Liteinst | Backend::E9patch => {
                reverie_ptrace::is_perf_supported()
            }
            Backend::Dbt | Backend::Sabre | Backend::Kvm => true,
        };
        self.validate_args_with_perf_support(perf_supported)
    }

    fn validate_args_with_perf_support(&mut self, perf_supported: bool) -> Result<(), Error> {
        let backend = self.selected_backend();
        if self.run_evidence_dir.is_some() {
            let limitation = match backend {
                Backend::Ptrace | Backend::Liteinst | Backend::Kvm => None,
                Backend::Dbt => Some(
                    "authenticated DBT evidence currently implies an isolated process group, \
                     which can change guest setsid/setpgid results",
                ),
                Backend::Sabre => Some(
                    "the SaBRe plugin currently emits DETLOG only on shared stderr, which is not \
                     an authoritative private evidence channel",
                ),
                Backend::E9patch => Some(
                    "e9patch preprocessing has not been qualified for this evidence contract; \
                     request the ptrace backend directly",
                ),
            };
            if let Some(limitation) = limitation {
                return Err(Error::new(PolicyRefusal).context(format!(
                    "--run-evidence-dir is unavailable for backend `{}`: {limitation}",
                    backend.as_str()
                )));
            }
        }
        if self.skid_margin.is_some()
            && (self.namespace_only
                || !matches!(
                    backend,
                    Backend::Ptrace | Backend::Liteinst | Backend::E9patch
                ))
        {
            anyhow::bail!(
                "--skid-margin configures the Reverie ptrace PMU timer and requires a ptrace-backed backend"
            );
        }
        if self.image.is_some() && self.no_namespace {
            anyhow::bail!(
                "--image chroots the guest into a materialized OCI rootfs, which requires mount \
                 and PID namespaces; it is incompatible with --no-namespace"
            );
        }
        if self.image.is_some() && backend != Backend::Ptrace {
            anyhow::bail!(
                "--image currently supports only the ptrace backend; backend `{}` has separate \
                 launcher/runtime-file requirements that have not been qualified inside the OCI rootfs",
                backend.as_str()
            );
        }
        if backend == Backend::Dbt && (!self.mount.is_empty() || !self.bind.is_empty()) {
            anyhow::bail!(
                "the dbt backend cannot apply --mount or --bind because its DynamoRIO adapter \
                 does not enter the guest mount namespace"
            );
        }
        if self.backend_engagement_json.is_some()
            && !matches!(backend, Backend::Ptrace | Backend::E9patch | Backend::Dbt)
        {
            anyhow::bail!(
                "--backend-engagement-json is not available for backend `{}`; SaBRe publishes \
                 HERMIT_SABRE_PATH_EVIDENCE, and liteinst/KVM expose no engagement value",
                backend.as_str()
            );
        }
        if self.image.is_some() && self.namespace_only {
            anyhow::bail!("--image cannot be combined with --namespace-only");
        }
        if self.image.is_some() && (!self.mount.is_empty() || !self.bind.is_empty()) {
            anyhow::bail!(
                "--image does not yet compose with custom --mount/--bind targets inside the OCI rootfs"
            );
        }

        let config = &mut self.det_opts.det_config;

        // Only the ptrace-family backends launch the guest through Reverie's
        // `Container` (see `container::default_container`), which unshares
        // `CLONE_NEWUTS` and applies the deterministic hostname
        // `hermetic-container.local`. The DBT backend returns early from
        // dispatch and runs the guest under DynamoRIO with no UTS namespace, so
        // its `uname()` nodename would otherwise leak the real host FQDN.
        // Reflect that reality here so Detcore's `handle_uname` deterministic
        // nodename/domainname rewrite fires for DBT (it is gated on
        // `!has_uts_namespace`). Guest `sethostname`/`setdomainname` are refused
        // with a deterministic `EPERM` on every backend, so this never masks a
        // hostname the guest legitimately set.
        let backend_applies_uts_hostname = !matches!(backend, Backend::Dbt);
        config.has_uts_namespace = !self.no_namespace && backend_applies_uts_hostname;

        if self.no_namespace {
            self.network = NetworkingMode::Host;
            self.tmp = Some(PathBuf::from(TMP_DIR));
        }

        if self.analyze_networking {
            config.warn_non_zero_binds = true;
        }

        config.sequentialize_threads = self.strict || !self.no_sequentialize_threads;
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
        if config.max_timeslice.is_some() && !perf_supported {
            // TODO(T124429978): this could change back to tracing::warn! when the bug is fixed:
            let mut stderr = detcore::util::RetryingStderr;
            let _ = writeln!(
                stderr,
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

        // `--strict` calls itself fail-closed strict deterministic mode, so it
        // must refuse the settings hermit itself documents as
        // determinism-compromising rather than accept them silently. Before
        // this, `--strict --network=host` exited 0 with no warning, even though
        // `--network`'s own help says `host` "compromises isolation and
        // deterministic reproducibility".
        //
        // Checked HERE, after every override above, because two of the three
        // routes to host networking are side effects of flags that are not
        // about networking at all: `--no-namespace` sets it above, and
        // `--gdbserver` sets it just above (that one at least prints a
        // warning; the others were silent). Measured from inside the guest,
        // `--strict` alone sees interfaces `lo`, while `--strict --network=host`,
        // `--strict --no-namespace` and `--strict --strace-only` all see
        // `eth0 lo`.
        //
        // This forbids rather than warns because the directive is that a hole
        // must be opened deliberately: drop `--strict`, or ask for the hole
        // explicitly without also claiming strictness.
        if self.strict && self.network == NetworkingMode::Host {
            let cause = if self.no_namespace {
                "--no-namespace, which forces host networking"
            } else if self.det_opts.det_config.gdbserver {
                "--gdbserver, which forces host networking"
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

        // Advise when running a VMM (e.g. QEMU) under host-time virtualization,
        // whose emulated guest clock calibration this corrupts (issue #6).
        // Checked last so it reflects any overrides above that disable virtual
        // time (e.g. --strace-only).
        let virtualize_time = self.det_opts.det_config.virtualize_time;
        if let Some(warning) =
            vmm_time_virtualization_warning(&self.program, &self.args, virtualize_time)
        {
            // TODO(T124429978): this could change back to tracing::warn! when the bug is fixed:
            eprintln!("{warning}");
        }

        Ok(())
    }

    fn install_pmu_config(&self) -> Result<(), Error> {
        let Some(skid_margin) = self.skid_margin else {
            return Ok(());
        };
        let config = reverie_ptrace::PmuConfig::new().with_skid_margin_override(skid_margin);
        reverie_ptrace::set_pmu_config(config).map_err(|_| {
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
        let mut program = load_program(spec_path)?;

        let (_, host) = self.resolve_guest_and_host_program()?;
        match DebugInfoResolver::open(&host) {
            Ok(resolver) if !resolver.is_empty() => {
                let unresolved = resolve_program(&mut program, &resolver);
                if !unresolved.is_empty() {
                    eprintln!(
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
                    eprintln!(
                        "hermit: {} has no usable symbol/debug info; {} code-location anchor(s) \
                         will never fire: {}",
                        host.display(),
                        unresolved.len(),
                        unresolved.join(", ")
                    );
                }
            }
            Err(err) => {
                eprintln!(
                    "hermit: could not read debug info from {}: {:#}; code-location anchors will \
                     never fire",
                    host.display(),
                    err
                );
            }
        }
        Ok(program)
    }

    fn list_happens_before_events(&self) -> Result<ExitStatus, Error> {
        let spec_path = self
            .happens_before
            .as_ref()
            .expect("--hb-list-events requires --happens-before");
        let mut program = load_program(spec_path)?;

        let (_, host) = self.resolve_guest_and_host_program()?;
        let resolver = match DebugInfoResolver::open(&host) {
            Ok(r) if !r.is_empty() => Some(r),
            Ok(_) => {
                eprintln!(
                    "hermit: {} has no usable symbol/debug info; code-location anchors will not \
                     resolve",
                    host.display()
                );
                None
            }
            Err(err) => {
                eprintln!(
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
            eprintln!(
                "hermit: {} anchor(s) with unresolved code locations: {}",
                unresolved.len(),
                unresolved.join(", ")
            );
        }
        Ok(ExitStatus::SUCCESS)
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
            let rootfs = crate::image::materialize_rootfs(image)?;
            let command = self.guest_command()?;
            let requested = Path::new(command.get_program());
            // `guest_command()` sets the image working directory (WorkingDir, or
            // `/`) as the guest cwd; resolve a relative program path against it.
            let guest_cwd = command
                .get_current_dir()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("/"));
            let guest_abs = resolve_image_guest_program(requested, &guest_cwd)?;
            // Resolve chroot-aware: images (nixos/nix especially) expose
            // `/bin/sh` as a symlink to an absolute `/nix/store/...` target that
            // only resolves under the image root, so a naive host `stat` would
            // follow it against the host `/` and fail.
            let in_rootfs = crate::image::resolve_in_rootfs(&rootfs, &guest_abs);
            return validate_executable(&in_rootfs, &guest_abs, Some(&rootfs));
        }

        if self.selected_backend() == Backend::E9patch {
            let (_, host) = self.resolve_guest_and_host_program()?;
            return validate_executable(&host, &self.program, None);
        }

        let command = self.guest_command()?;
        let requested = Path::new(command.get_program());
        if requested.is_absolute() {
            if let GuestPathMapping::Mapped(host) = self.mapped_host_program(requested) {
                return validate_executable(&host, requested, None);
            }
            if requested.starts_with(TMP_DIR) && self.tmp.is_none() && requested.exists() {
                anyhow::bail!(
                    "Program {} is under host /tmp, but Hermit replaces guest /tmp with an \
                     isolated directory. Pass --tmp=/tmp to expose host /tmp or bind the program \
                     into guest /tmp.",
                    requested.display()
                );
            }
            return validate_executable(requested, requested, None);
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
        validate_executable(&resolved, requested, None)
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
            self.e9patch_engagement = Some(BackendEngagement::E9patch {
                candidate_sites: 0,
                mapped_sites: 0,
                b0_sites: 0,
            });
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
        self.e9patch_engagement = Some(BackendEngagement::E9patch {
            candidate_sites: u64::try_from(prepared.candidate_sites)
                .map_err(|_| Error::msg("e9patch candidate-site count does not fit u64"))?,
            mapped_sites: u64::try_from(prepared.patched_sites)
                .map_err(|_| Error::msg("e9patch mapped-site count does not fit u64"))?,
            b0_sites: u64::try_from(prepared.b0_sites)
                .map_err(|_| Error::msg("e9patch B0-site count does not fit u64"))?,
        });
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
             instruction_map_cache={:?}; rewrite_cache={}; artifact_sha256={}; \
             preprocess_us={}",
            prepared.candidate_sites,
            prepared.patched_sites,
            prepared.b0_sites,
            prepared.instruction_map_cache_status,
            rewrite_cache,
            prepared.artifact_sha256.as_deref().unwrap_or("none"),
            prepared.preprocess_micros,
        );
        // Opt-in (HERMIT_E9PATCH_STATS) patch-shape stats. These describe the
        // ahead-of-time rewrite of the single root guest image; the selected
        // `e9patch` spelling runs on the ptrace runtime, so this measures the
        // preprocessing shape, not any runtime instrumentation cost.
        if let Some(shape) = &prepared.patch_shape {
            eprintln!(
                ":: e9patch patch-shape stats (selected=e9patch, runtime=ptrace, \
                 scope=root-image): {shape}"
            );
        }
        Ok(())
    }

    fn write_backend_engagement_after_run(
        &self,
        guest_capture: Option<&GuestRunCaptureSession>,
    ) -> Result<(), Error> {
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
            Backend::E9patch => self.e9patch_engagement.clone().ok_or_else(|| {
                Error::msg("e9patch backend engagement was not recorded during preparation")
            })?,
            Backend::Dbt => unreachable!("the DBT adapter writes its own engagement record"),
            Backend::Liteinst | Backend::Sabre | Backend::Kvm => {
                return Err(Error::msg(format!(
                    "backend `{}` does not expose an engagement value",
                    self.selected_backend().as_str()
                )));
            }
        };
        if let Some(capture) = guest_capture {
            capture.require_distinct_from(path, "--backend-engagement-json")?;
        }
        write_backend_engagement(path, engagement)
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
        self.run_with_guest_capture(global, capture_output, None)
    }

    fn run_with_guest_capture(
        &self,
        global: &GlobalOpts,
        capture_output: bool,
        guest_capture: Option<&GuestRunCaptureSession>,
    ) -> Result<(ExitStatus, Option<Output>), Error> {
        // main has reserved startup stdin before this can allocate a descriptor.
        // Keep the unlinked output open through the real container fork; its
        // controller-local proc-fd spelling is constructed only inside it.
        let summary_output = if guest_capture.is_some() && self.summary_json.is_some() {
            Some(super::staged_summary::private_output(
                &private_summary_dir()?,
            )?)
        } else {
            None
        };
        if self.no_namespace {
            let mut process = Container::new();
            apply_affinity(&mut process, self.pin_threads);
            return with_container(&mut process, || {
                self.run_in_container(
                    global,
                    capture_output,
                    guest_capture,
                    summary_output.as_ref(),
                    None,
                )
            });
        }

        let tmpfs = self.tmpfs()?;

        let (mut container, identity_sources) = self.container(tmpfs.path())?;

        with_container(&mut container, || {
            self.run_in_container(
                global,
                capture_output,
                guest_capture,
                summary_output.as_ref(),
                Some(&identity_sources),
            )
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

        let mut command = self.namespace_only_guest_command()?;
        // `--namespace-only` does NOT go through `with_container`: it unshares
        // `Namespace::PID` and execs the guest directly, so the guest process
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
        // SAFETY: the closure calls only `prctl(PR_SET_PDEATHSIG)`, which is
        // async-signal-safe, touches no caller memory, and allocates nothing --
        // the requirements for a `pre_exec` callback between fork and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                    return Err(Errno::last());
                }
                Ok(())
            });
        }
        command
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
        // Stamp an explicit no-result BEFORE any fallible work. Several exits
        // below (a run that fails to start, a rejected first-run status, a SaBRe
        // capture with zero DETLOG) return early without ever reaching
        // `write_verification_json`; without this, reusing a --verify-json path
        // would leave the PREVIOUS invocation's record -- possibly a green -- to
        // be read as this invocation's result.
        if let Some(path) = &self.verify_json {
            write_pending_verification_json(path)?;
        }
        let retained_log_dir = self.retained_verify_log_dir()?;
        let (log1, log2) = temp_log_files_in("run1", "run2", retained_log_dir.as_deref())
            .context("Failed to create verification log files")?;

        let (log1_file, log1_path) = log1.into_parts();
        let (log2_file, log2_path) = log2.into_parts();

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
        let summary1_file = private_verify_summary()?;
        let summary2_path = self.summary_json.as_deref().unwrap_or(summary1_file.path());
        let mut run1_options = self.clone();
        run1_options.summary_json = Some(summary1_file.path().to_owned());
        let mut run2_options = self.clone();
        run2_options.summary_json = Some(summary2_path.to_owned());

        // Captured BEFORE run 1 so the same values can be put back before run 2.
        // See the restore call below for the measurement this exists for.
        let fd_flags_before_run1 = standard_fd_status_flags();

        eprintln!(":: {}", "Run1...".yellow().bold());

        let (mut out1, skid_overshoots_run1) = match run1_options.run_verify(log1_file, global) {
            Ok(result) => result,
            Err(error) => {
                if let Some(overshoot) = error.downcast_ref::<SkidOvershootError>()
                    && let Some(path) = &self.verify_json
                {
                    let summary1 = read_verify_summary(summary1_file.path());
                    if let Err(report_error) = write_skid_overshoot_without_comparison_json(
                        path,
                        overshoot.count(),
                        verification_runtime_from_summaries(summary1.as_ref(), None),
                        // `run_verify` returned only an Error, so this site has
                        // no guest ExitStatus to record.
                        None,
                    ) {
                        eprintln!(
                            "WARNING: could not record the skid overshoot in {}: {}",
                            path.display(),
                            report_error
                        );
                    }
                }
                if self.keep_logs {
                    retain_verification_logs([("run 1", log1_path)])?;
                }
                return Err(error);
            }
        };
        let sabre_syscalls1 = match (self.selected_backend() == Backend::Sabre)
            .then(|| extract_sabre_detlogs(&log1_path, &mut out1.stderr))
            .transpose()
        {
            Ok(count) => count,
            Err(error) => {
                if self.keep_logs {
                    retain_verification_logs([("run 1", log1_path)])?;
                }
                return Err(error);
            }
        };

        // With --verify the first run's `--log` output was diverted to a
        // temporary file for later comparison rather than shown to the user.
        // When --print-verify-logs is set, echo that first run's log to stderr so the
        // user still sees `--log` output, matching a normal (non-verify) run.
        // The log file is fully flushed here because run_verify runs each
        // execution in a child process that has already exited.
        if self.print_verify_logs {
            match fs::read(&log1_path) {
                Ok(bytes) => std::io::stderr().write_all(&bytes)?,
                Err(err) => eprintln!(
                    "WARNING: --print-verify-logs could not read first-run log {}: {}",
                    log1_path.display(),
                    err
                ),
            }
        }

        if !self.verify_allow.satisfies(out1.status) {
            if skid_overshoots_run1 > 0 {
                if let Some(path) = &self.verify_json {
                    let summary1 = read_verify_summary(summary1_file.path());
                    write_skid_overshoot_without_comparison_json(
                        path,
                        skid_overshoots_run1,
                        verification_runtime_from_summaries(summary1.as_ref(), None),
                        Some(out1.status),
                    )?;
                }
                if self.keep_logs {
                    retain_verification_logs([("run 1", log1_path)])?;
                }
                return Err(Error::new(SkidOvershootError::new(skid_overshoots_run1)));
            }
            let status = describe_exit_status(out1.status);
            eprintln!(
                "First run errored during --verify, not continuing to a second.\nExit status: {status}\nStdout:\n{}\nStderr:\n{}",
                String::from_utf8_lossy(&out1.stdout),
                String::from_utf8_lossy(&out1.stderr),
            );
            // ⚠️ RECORD THE DISPOSITION HERE, WHERE IT IS KNOWN. `out1.status`
            // is in hand, yet the pre-stamped `no_result` record was previously
            // left untouched on this path -- so the artifact reported
            // `guest_exit_code: null` for a guest whose exit code we had. That
            // made this refusal byte-identical to a container that never ran the
            // guest, and 14 rotating e2e failures were undiagnosable as a direct
            // result. A best-effort write: an unwritable artifact must not
            // convert a rejected first run into a different error.
            if let Some(path) = &self.verify_json {
                let summary1 = read_verify_summary(summary1_file.path());
                let report = first_run_rejected_report(
                    &out1,
                    verification_runtime_from_summaries(summary1.as_ref(), None),
                );
                if let Err(error) = write_report_json(path, &report) {
                    eprintln!(
                        "WARNING: could not record the rejected first run in {}: {}",
                        path.display(),
                        error
                    );
                }
            }
            if self.keep_logs {
                retain_verification_logs([("run 1", log1_path)])?;
            }
            return Err(Error::msg(format!("First run during --verify {status}")));
        }

        let summary1 = take_verify_summary_before_next_run(summary1_file.path())?;
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
        // MEASURED 2026-08-26 on `run --backend kvm --strict --verify --
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

        eprintln!(":: {}", "Run2...".yellow().bold());
        let (mut out2, skid_overshoots_run2) = match run2_options.run_verify(log2_file, global) {
            Ok(result) => result,
            Err(error) => {
                if let Some(overshoot) = error.downcast_ref::<SkidOvershootError>()
                    && let Some(path) = &self.verify_json
                {
                    let summary2 = read_verify_summary(summary2_path);
                    let count = skid_overshoots_run1.saturating_add(overshoot.count());
                    if let Err(report_error) = write_skid_overshoot_without_comparison_json(
                        path,
                        count,
                        verification_runtime_from_summaries(summary1.as_ref(), summary2.as_ref()),
                        Some(out1.status),
                    ) {
                        eprintln!(
                            "WARNING: could not record the skid overshoot in {}: {}",
                            path.display(),
                            report_error
                        );
                    }
                }
                if self.keep_logs {
                    retain_verification_logs([("run 1", log1_path), ("run 2", log2_path)])?;
                }
                return Err(error);
            }
        };
        if let Some(sabre_syscalls1) = sabre_syscalls1 {
            let sabre_syscalls2 = match extract_sabre_detlogs(&log2_path, &mut out2.stderr) {
                Ok(count) => count,
                Err(error) => {
                    if self.keep_logs {
                        retain_verification_logs([("run 1", log1_path), ("run 2", log2_path)])?;
                    }
                    return Err(error);
                }
            };
            if sabre_syscalls1 == 0 || sabre_syscalls2 == 0 {
                if self.keep_logs {
                    retain_verification_logs([("run 1", log1_path), ("run 2", log2_path)])?;
                }
                return Err(Error::msg(format!(
                    "SaBRe verification captured no syscall DETLOG records: run1={sabre_syscalls1}, run2={sabre_syscalls2}"
                )));
            }
            eprintln!(
                ":: SaBRe syscall DETLOG records included: run1={sabre_syscalls1}, run2={sabre_syscalls2}"
            );
        }

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
        let comparison_options = self.verification_comparison_options();
        let success_message = if !comparison_options.compare_io_buffers {
            // The "Determinism verified" marker is RETAINED verbatim and the
            // qualification appended after it. That is not politeness: ~110
            // files in this repository assert on that exact substring -- Rust
            // integration tests, tests/e2e/lib/**/*.sh, and the backend-parity
            // Python harnesses -- so replacing the sentence would be a
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
        let summary2 = read_verify_summary(summary2_path);
        let skid_overshoots = skid_overshoots_run1.saturating_add(skid_overshoots_run2);
        if skid_overshoots > 0
            && let Some(path) = &self.verify_json
        {
            write_skid_overshoot_without_comparison_json(
                path,
                skid_overshoots,
                verification_runtime_from_summaries(summary1.as_ref(), summary2.as_ref()),
                Some(out1.status),
            )?;
        }
        let mut outcome = compare_two_runs(
            ComparedRun {
                output: &out1,
                log: log1_path,
                // Accurate on this path: `verify` calls `run_verify` twice, and
                // each call builds its own container and guest command and
                // reaches a separate tracer spawn, so these really are two
                // fresh executions of the guest.
                label: "run 1",
            },
            ComparedRun {
                output: &out2,
                log: log2_path,
                label: "run 2",
            },
            comparison_options,
        )?;
        outcome.runtime = verification_runtime_from_summaries(summary1.as_ref(), summary2.as_ref());

        // Emit the machine-readable verdict (if requested) before collapsing the
        // outcome to the historical exit-code convention. The verdict is recorded
        // whether or not the runs matched, and independent of the guest's own
        // exit status.
        if let Some(path) = &self.verify_json {
            if skid_overshoots > 0 {
                write_skid_overshoot_verification_json(path, &outcome, skid_overshoots)?;
            } else {
                write_verification_json(path, &outcome)?;
            }
        }
        if skid_overshoots > 0 {
            return Err(Error::new(SkidOvershootError::new(skid_overshoots)));
        }
        announce_verification_outcome(&outcome, success_message, failure_message);

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
            if out1.stdout == out2.stdout && out1.stderr == out2.stderr {
                std::io::stdout().write_all(&out1.stdout)?;
                std::io::stderr().write_all(&out1.stderr)?;
            }
            return outcome.into_exit_status();
        }
        let status = outcome.guest_status;

        let backend_banner = match self.selected_backend() {
            Backend::Kvm => Some("KVM (reverie-kvm KvmGuest<Detcore>)"),
            Backend::Liteinst => {
                Some("LiteInst host hybrid (reverie-liteinst patch runtime + ptrace Detcore Tool)")
            }
            Backend::Ptrace | Backend::Dbt | Backend::Sabre | Backend::E9patch => None,
        };
        if let Some(backend_banner) = backend_banner {
            eprintln!(":: Backend: {backend_banner}");
        }
        std::io::stdout().write_all(&out1.stdout)?;
        std::io::stderr().write_all(&out1.stderr)?;
        Ok(status)
    }

    /// Returns the mounts to be used with the container.
    fn mounts(&self, tmpfs: &Path) -> Result<PreparedMounts, Error> {
        let (mut identity_mounts, identity_sources) = identity_hardening_mounts()?;
        let mut identity_sources = identity_sources;
        let mut explicit_tmp_mount = false;
        let mut user_mounts = Vec::with_capacity(self.mount.len() + self.bind.len());

        for mount in &self.mount {
            let user_target = mount.get_target();
            identity_sources.discard_mounts_shadowed_by(&mut identity_mounts, user_target);
            if let Ok(path) = mount.get_target().strip_prefix(TMP_DIR) {
                explicit_tmp_mount |= path.as_os_str().is_empty();
                // If the target is in /tmp, change it so it goes to our
                // temporary /tmp instead.
                user_mounts.push(mount.clone().target(tmpfs.join(path)).touch_target());
            } else {
                user_mounts.push(mount.clone());
            }
        }

        for bind in &self.bind {
            let mount = Mount::from(bind.clone()).rshared();

            // Bind mounts currently only make sense for things in `/tmp` since
            // that is the only directory we overlay.
            if let Ok(relative_path) = mount.get_target().strip_prefix(TMP_DIR) {
                // Discard provenance only for a bind we actually retain. An
                // ignored outside-/tmp bind must not suppress the real
                // identity-hardening mount at that target.
                explicit_tmp_mount |= relative_path.as_os_str().is_empty();
                identity_sources
                    .discard_mounts_shadowed_by(&mut identity_mounts, mount.get_target());
                let target = tmpfs.join(relative_path);
                user_mounts.push(mount.target(target).touch_target());
            } else {
                eprintln!(
                    "WARNING: --bind target {} is outside guest /tmp, so this option has no \
                     effect; files outside /tmp are already visible unless another mount hides them",
                    bind.target.to_string_lossy()
                );
            }
        }

        if self.tmp.is_none() {
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

        let mut mounts = identity_mounts;
        mounts.extend(user_mounts);

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
        // PROTOTYPE: when an OCI image is requested, the guest runs against the
        // image's materialized rootfs (deterministic file inputs) rather than
        // the host filesystem. This replaces the default namespace+mounts setup
        // with a chroot into the pinned image root.
        if let Some(image) = &self.image {
            let rootfs = crate::image::materialize_rootfs(image)?;
            let (mut container, identity_sources) =
                image_container(&rootfs, tmpfs, self.pin_threads, self.tmp.is_none())?;
            match &self.network {
                NetworkingMode::Local => {
                    container.local_networking_only();
                }
                NetworkingMode::Host if self.analyze_networking => {
                    container.local_networking_only();
                }
                NetworkingMode::Host => {}
            }
            return Ok((container, identity_sources));
        }

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

    pub fn run_verify(
        &self,
        log_file: fs::File,
        global: &GlobalOpts,
    ) -> Result<(Output, u64), Error> {
        if self.no_namespace {
            // Verify initializes a process-global tracing subscriber for each run. Keep a plain
            // child-process boundary between runs, but do not configure any namespaces or mounts.
            let mut process = Container::new();
            apply_affinity(&mut process, self.pin_threads);
            let mut log_file = Some(log_file);
            return with_container(&mut process, || {
                self.run_verify_in_container(&mut log_file, global, None)
            });
        }

        let tmpfs = self.tmpfs()?;

        let (mut container, identity_sources) = self.container(tmpfs.path())?;

        let mut log_file = Some(log_file);
        with_container(&mut container, || {
            self.run_verify_in_container(&mut log_file, global, Some(&identity_sources))
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
        self.build_guest_command(true)
    }

    fn namespace_only_guest_command(&self) -> Result<Command, Error> {
        // Namespace-only bypasses instrumentation, so preserve the caller's
        // sanitizer environment instead of installing the leak-disable overrides.
        self.build_guest_command(false)
    }

    fn build_guest_command(
        &self,
        disable_sanitizer_leak_detection_for_guest: bool,
    ) -> Result<Command, Error> {
        let program = self.e9patch_program.as_ref().unwrap_or(&self.program);
        let mut command = Command::new(program);
        command.args(&self.args);
        if self.e9patch_program.is_some() {
            command.arg0(&self.program);
        }
        // PROTOTYPE (--image): the guest lives inside the OCI rootfs, so the
        // guest working directory must be one that exists under the image root.
        // Default it to the image's declared `WorkingDir`, else `/` (a bare
        // chroot leaves cwd pointing at the — now unreachable — host cwd, which
        // makes `getcwd` fail inside the guest). An explicit `--workdir` wins.
        if let Some(current_dir) = &self.workdir {
            command.current_dir(current_dir);
        } else if let Some(image) = &self.image {
            let cfg = crate::image::read_image_config(image)?;
            command.current_dir(cfg.workdir.as_deref().unwrap_or("/"));
        }

        // PROTOTYPE (--image): an OCI rootfs is a self-contained filesystem, so
        // leaking the host environment (a host `PATH` full of paths absent from
        // the image) both breaks usability and undermines determinism. Instead
        // apply the image's *own* declared `Env` — pinned by the image digest,
        // hence deterministic — over an otherwise empty base, then merge user
        // `--env` on top. If the image declares no `PATH`/`HOME` we fall back to
        // the same hermetic defaults `--base-env=minimal` uses.
        if let Some(image) = &self.image {
            command.env_clear();
            let cfg = crate::image::read_image_config(image)?;
            let declares_path = cfg.env.iter().any(|(k, _)| k == "PATH");
            let declares_home = cfg.env.iter().any(|(k, _)| k == "HOME");
            command.env("HOSTNAME", "hermetic-container.local");
            if !declares_path {
                command.env(
                    "PATH",
                    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                );
            }
            if !declares_home {
                command.env("HOME", "/root");
            }
            for (key, value) in &cfg.env {
                command.env(key, value);
            }
            self.merge_from_env_settings(&mut command)?;
            return Ok(command);
        }

        apply_base_and_explicit_environment(&mut command, &self.base_env, &self.env)?;
        if disable_sanitizer_leak_detection_for_guest {
            disable_sanitizer_leak_detection(&mut command);
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
        config
    }

    /// The `--timeout` bound, if the caller asked for one.
    ///
    /// Mirrors `StartOpts::record_timeout` so the two spellings of a hermit
    /// deadline are read the same way.
    fn run_timeout(&self) -> Option<Duration> {
        self.timeout
            .map(|seconds| Duration::from_secs(seconds.get()))
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
    fn ensure_timeout_supported(&self) -> Result<(), Error> {
        if self.timeout.is_none() {
            return Ok(());
        }
        let backend = self.runtime_backend();
        if matches!(backend, Backend::Ptrace | Backend::Liteinst) {
            return Ok(());
        }
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

    fn run_in_container(
        &self,
        global: &GlobalOpts,
        capture_output: bool,
        guest_capture: Option<&GuestRunCaptureSession>,
        summary_output: Option<&File>,
        identity_sources: Option<&IdentityGuard>,
    ) -> Result<(ExitStatus, Option<Output>), Error> {
        let _guard = global.init_tracing();

        if capture_output && guest_capture.is_some() {
            anyhow::bail!("internal output capture cannot be combined with harness guest capture");
        }
        if let (Some(capture), Some(summary)) = (guest_capture, self.summary_json.as_deref()) {
            // Container mounts and chroot are complete here. Relative summary
            // paths use this process's cwd, not the guest Command's --workdir.
            capture.require_distinct_from(summary, "--summary-json")?;
        }
        let backend = self.runtime_backend();
        let mut command = self.guest_command()?;
        if let Some(capture) = guest_capture.filter(|_| backend != Backend::Kvm) {
            let stderr_fd = capture.stderr_fd_for_guest();
            command.stdout(capture.stdout_for_guest()?);
            // The pinned reverie-process `spawn_with` currently constructs
            // child stderr from its stdout configuration. Restore the distinct
            // harness-owned stderr descriptor after Reverie's stdio setup and
            // before exec. The held descriptor survives fork despite CLOEXEC;
            // dup2 both selects fd 2 and clears CLOEXEC on the new descriptor.
            //
            // SAFETY: this callback runs after fork. It captures one integer and
            // calls only async-signal-safe dup2, without allocation or locks.
            unsafe {
                command.pre_exec(move || {
                    if libc::dup2(stderr_fd, libc::STDERR_FILENO) == -1 {
                        return Err(Errno::last());
                    }
                    Ok(())
                });
            }
        }

        let mut config = self.effective_det_config();
        config.mountinfo_root_rewrites = identity_sources
            .map(IdentityGuard::mountinfo_root_rewrites)
            .transpose()?
            .unwrap_or_default();
        let mountinfo_order = identity_sources
            .map(IdentityGuard::mountinfo_identity_order)
            .transpose()?
            .unwrap_or_default();
        config.mountinfo_mount_ids = mountinfo_order;
        config.mountinfo_mount_ids_captured = identity_sources.is_some();
        config.fdinfo_unlisted_mount_ids.clear();
        self.save_config_to_disk()?;

        let timeout = self.run_timeout();
        super::staged_summary::with_published_summary(
            summary_output,
            self.summary_json.as_deref(),
            guest_capture,
            |summary_json| {
                if capture_output || (guest_capture.is_some() && backend == Backend::Kvm) {
                    let out = hermit::run_with_output_backend_timeout(
                        command,
                        config,
                        self.summary,
                        summary_json,
                        backend,
                        timeout,
                    )?;
                    if let Some(capture) = guest_capture {
                        capture.write_kvm_virtual_console(&out.stdout, &out.stderr)?;
                        Ok((out.status, None))
                    } else {
                        Ok((out.status, Some(out)))
                    }
                } else {
                    let status = hermit::run_with_backend_timeout(
                        command,
                        config,
                        self.summary,
                        summary_json,
                        backend,
                        timeout,
                    )?;
                    Ok((status, None))
                }
            },
        )
    }

    fn run_verify_in_container(
        &self,
        log_file: &mut Option<fs::File>,
        global: &GlobalOpts,
        identity_sources: Option<&IdentityGuard>,
    ) -> Result<(Output, u64), Error> {
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
        let _guard = init_sync_file_tracing(Some(level), BoundedWriter::new(log_file, limit));

        let command = self.guest_command()?;

        let mut config = self.effective_det_config();
        config.mountinfo_root_rewrites = identity_sources
            .map(IdentityGuard::mountinfo_root_rewrites)
            .transpose()?
            .unwrap_or_default();
        let mountinfo_order = identity_sources
            .map(IdentityGuard::mountinfo_identity_order)
            .transpose()?
            .unwrap_or_default();
        config.mountinfo_mount_ids = mountinfo_order;
        config.mountinfo_mount_ids_captured = identity_sources.is_some();
        config.fdinfo_unlisted_mount_ids.clear();
        self.save_config_to_disk()?;

        hermit::run_with_output_backend_timeout_and_skid_overshoots(
            command,
            config,
            self.summary,
            &self.summary_json,
            self.runtime_backend(),
            None,
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

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

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
        let mut options = RunOpts::parse_from(["hermit", "--backend=ptrace", "/bin/true"]);
        options.summary_json = Some(summary_file.path().to_owned());
        options.backend_engagement_json = Some(engagement_file.path().to_owned());

        for scheduler_turns in [12, 13] {
            let summary = RunSummary {
                sched_turns: scheduler_turns,
                ..Default::default()
            };
            fs::write(summary_file.path(), serde_json::to_vec(&summary).unwrap()).unwrap();
            options.write_backend_engagement_after_run(None).unwrap();
            let report: BackendEngagementReport =
                serde_json::from_slice(&fs::read(engagement_file.path()).unwrap()).unwrap();
            assert_eq!(
                report.engagement,
                BackendEngagement::Ptrace { scheduler_turns }
            );
        }
    }

    #[test]
    fn e9patch_engagement_record_follows_the_preparation_value() {
        let engagement_file = tempfile::NamedTempFile::new().unwrap();
        let mut options = RunOpts::parse_from(["hermit", "--backend=e9patch", "/bin/true"]);
        options.backend_engagement_json = Some(engagement_file.path().to_owned());

        for (candidate_sites, mapped_sites, b0_sites) in [(0, 0, 0), (2, 2, 0)] {
            options.e9patch_engagement = Some(BackendEngagement::E9patch {
                candidate_sites,
                mapped_sites,
                b0_sites,
            });
            options.write_backend_engagement_after_run(None).unwrap();
            let report: BackendEngagementReport =
                serde_json::from_slice(&fs::read(engagement_file.path()).unwrap()).unwrap();
            assert_eq!(
                report.engagement,
                BackendEngagement::E9patch {
                    candidate_sites,
                    mapped_sites,
                    b0_sites,
                }
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
        let source = include_str!("run.rs");
        let verification = source
            .split_once("let outcome = compare_two_runs(")
            .expect("verification comparison")
            .1;
        let publish = verification
            .find("write_verification_json(path, &outcome)")
            .expect("verification report publication");
        let announce = verification
            .find("announce_verification_outcome(&outcome")
            .expect("verification announcement");
        assert!(publish < announce);
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
    fn run_evidence_backend_scope_matches_authoritative_private_sinks() {
        for backend in [Backend::Ptrace, Backend::Liteinst, Backend::Kvm] {
            let mut options = RunOpts::parse_from([
                "run",
                &format!("--backend={}", backend.as_str()),
                "--run-evidence-dir=/unused/new-path",
                "/bin/true",
            ]);
            options
                .validate_args_with_perf_support(true)
                .unwrap_or_else(|error| panic!("{backend:?} should be supported: {error:#}"));
        }

        for (backend, limitation) in [
            (Backend::Dbt, "isolated process group"),
            (Backend::Sabre, "shared stderr"),
            (Backend::E9patch, "not been qualified"),
        ] {
            let mut options = RunOpts::parse_from([
                "run",
                &format!("--backend={}", backend.as_str()),
                "--run-evidence-dir=/unused/new-path",
                "/bin/true",
            ]);
            let error = options.validate_args_with_perf_support(true).unwrap_err();
            assert!(error.downcast_ref::<PolicyRefusal>().is_some());
            assert!(error.to_string().contains(limitation), "{error:#}");
        }
    }

    #[test]
    fn run_evidence_is_only_an_ordinary_instrumented_run_option() {
        for incompatible in ["--verify", "--namespace-only"] {
            assert!(
                RunOpts::try_parse_from([
                    "run",
                    "--run-evidence-dir=/unused/new-path",
                    incompatible,
                    "/bin/true",
                ])
                .is_err(),
                "{incompatible} must remain incompatible with one-run evidence"
            );
        }
    }

    #[test]
    fn harness_guest_capture_requires_all_paths_and_supported_ordinary_backend() {
        let flags = [
            "--run-evidence-dir=/unused/evidence",
            "--run-result-json=/unused/result.json",
            "--guest-stdout=/unused/stdout",
            "--guest-stderr=/unused/stderr",
        ];
        let ordinary = RunOpts::parse_from(["run", "/bin/true"]);
        assert!(ordinary.guest_run_capture_paths().unwrap().is_none());
        for omitted in 0..flags.len() {
            let mut args = vec!["run"];
            args.extend(
                flags
                    .iter()
                    .enumerate()
                    .filter_map(|(index, flag)| (index != omitted).then_some(*flag)),
            );
            args.push("/bin/true");
            assert!(
                RunOpts::try_parse_from(args).is_err(),
                "accepted capture without {}",
                flags[omitted]
            );
        }
        for incompatible in ["--verify", "--namespace-only"] {
            let mut args = vec!["run", incompatible];
            args.extend(flags);
            args.push("/bin/true");
            assert!(RunOpts::try_parse_from(args).is_err());
        }
        for backend in [
            Backend::Ptrace,
            Backend::Liteinst,
            Backend::Kvm,
            Backend::Dbt,
            Backend::Sabre,
            Backend::E9patch,
        ] {
            let backend_arg = format!("--backend={}", backend.as_str());
            let mut args = vec!["run", backend_arg.as_str()];
            args.extend(flags);
            args.push("/bin/true");
            let mut options = RunOpts::try_parse_from(args).unwrap();
            let paths = options.guest_run_capture_paths().unwrap().unwrap();
            assert_eq!(paths.result, PathBuf::from("/unused/result.json"));
            assert_eq!(paths.stdout, PathBuf::from("/unused/stdout"));
            assert_eq!(paths.stderr, PathBuf::from("/unused/stderr"));
            let result = options.validate_args_with_perf_support(true);
            if matches!(backend, Backend::Ptrace | Backend::Liteinst | Backend::Kvm) {
                result.unwrap();
            } else {
                assert!(
                    result
                        .unwrap_err()
                        .downcast_ref::<PolicyRefusal>()
                        .is_some()
                );
            }
        }

        let paths = GuestRunCapturePaths::new(
            PathBuf::from("/result"),
            PathBuf::from("/stdout"),
            PathBuf::from("/stderr"),
        );
        assert!(
            paths
                .require_distinct_from(Some(Path::new("/stdout")), "--summary-json")
                .unwrap_err()
                .to_string()
                .contains("must not reuse the --summary-json path")
        );
    }

    #[test]
    fn capture_preflight_preserves_summary_and_engagement_collisions() {
        use std::os::unix::fs::MetadataExt as _;
        use std::os::unix::fs::symlink;

        let global = GlobalOpts::parse_from(["hermit"]);
        for option in ["--summary-json", "--backend-engagement-json"] {
            for capture_name in ["result.json", "stdout", "stderr"] {
                for alias in [false, true] {
                    let directory = tempfile::tempdir().unwrap();
                    let root = directory.path();
                    let target = root.join(capture_name);
                    fs::write(&target, b"preserve captured artifact").unwrap();
                    let before = fs::metadata(&target).unwrap();
                    let companion = if alias {
                        let alias = root.join("alias");
                        symlink(&target, &alias).unwrap();
                        alias
                    } else {
                        target.clone()
                    };
                    let engagement = root.join("engagement");
                    fs::write(&engagement, b"preserve engagement").unwrap();
                    let evidence = root.join("evidence");
                    fs::create_dir(&evidence).unwrap();
                    fs::write(evidence.join("already-claimed"), b"preserve evidence").unwrap();
                    let mut options =
                        RunOpts::parse_from(["run", "/nonexistent-capture-preflight-guest"]);
                    assert!(!options.program.exists());
                    options.run_evidence_dir = Some(evidence.clone());
                    options.run_result_json = Some(root.join("result.json"));
                    options.guest_stdout = Some(root.join("stdout"));
                    options.guest_stderr = Some(root.join("stderr"));
                    options.backend_engagement_json = Some(engagement.clone());
                    if option == "--summary-json" {
                        options.summary_json = Some(companion);
                    } else {
                        options.backend_engagement_json = Some(companion);
                    }
                    let error = options.main(&global).unwrap_err();
                    assert!(
                        error
                            .to_string()
                            .contains(&format!("must not reuse the {option} path")),
                        "{error:#}"
                    );
                    let after = fs::metadata(&target).unwrap();
                    assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
                    assert_eq!(fs::read(&target).unwrap(), b"preserve captured artifact");
                    assert_eq!(fs::read(&engagement).unwrap(), b"preserve engagement");
                    assert_eq!(
                        fs::read(evidence.join("already-claimed")).unwrap(),
                        b"preserve evidence"
                    );
                    for absent in ["result.json", "stdout", "stderr"]
                        .into_iter()
                        .filter(|name| *name != capture_name)
                    {
                        assert!(!root.join(absent).exists());
                    }
                }
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let evidence = root.join("evidence");
        fs::create_dir(&evidence).unwrap();
        let paths = GuestRunCapturePaths::new(
            root.join("result"),
            root.join("stdout"),
            root.join("stderr"),
        );
        let mut options = RunOpts::parse_from(["run", "/nonexistent-capture-preflight-guest"]);
        options.summary_json = Some(root.join("summary"));
        options.backend_engagement_json = Some(root.join("engagement"));
        options.check_capture_companions(&paths).unwrap();
        let mut capture = GuestRunCaptureSession::create(&paths, &evidence).unwrap();
        capture
            .finish(
                Backend::Ptrace,
                ExitStatus::Exited(0),
                GuestRunDeterminism {
                    detlog_io_buffers: true,
                    virtualize_time: true,
                },
            )
            .unwrap();
        fs::write(
            options.summary_json.as_ref().unwrap(),
            serde_json::to_vec(&RunSummary {
                sched_turns: 12,
                ..RunSummary::default()
            })
            .unwrap(),
        )
        .unwrap();
        fs::hard_link(
            &paths.result,
            options.backend_engagement_json.as_ref().unwrap(),
        )
        .unwrap();
        let result = fs::read(&paths.result).unwrap();
        assert!(
            options
                .write_backend_engagement_after_run(Some(&capture))
                .unwrap_err()
                .to_string()
                .contains("must not reuse the --backend-engagement-json path")
        );
        assert_eq!(fs::read(&paths.result).unwrap(), result);
        fs::remove_file(options.backend_engagement_json.as_ref().unwrap()).unwrap();
        options
            .write_backend_engagement_after_run(Some(&capture))
            .unwrap();
        assert_eq!(fs::read(&paths.result).unwrap(), result);
        assert!(
            !fs::read(options.backend_engagement_json.as_ref().unwrap())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn extracts_sabre_detlogs_and_preserves_guest_stderr() {
        let log = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            log.path(),
            "2026-08-02T00:00:00.000000Z INFO detcore: coordinator message\n",
        )
        .unwrap();
        let mut stderr = b"guest stderr\n INFO detcore: inbound syscall: getpid() = ?\n\
              INFO detcore: DETLOG scheduler event\n\
              INFO detcore: DETLOG [syscall] finish syscall #1: getpid() = Ok(3)\n"
            .to_vec();
        let syscall_records = extract_sabre_detlogs(log.path(), &mut stderr).unwrap();

        assert_eq!(syscall_records, 1);
        assert_eq!(
            stderr,
            b"guest stderr\n INFO detcore: inbound syscall: getpid() = ?\n"
        );
        assert_eq!(
            std::fs::read_to_string(log.path()).unwrap(),
            "2026-08-02T00:00:00.000000Z INFO detcore: coordinator message\n\
             1970-01-01T00:00:00.000000Z INFO detcore: DETLOG scheduler event\n\
             1970-01-01T00:00:00.000000Z INFO detcore: DETLOG [syscall] finish syscall #1: getpid() = Ok(3)\n",
        );
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
