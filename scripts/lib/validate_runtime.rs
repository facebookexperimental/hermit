// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Runtime admission control and process accounting for the validate driver.
//!
//! Everything here answers a question about the *running world* rather than about
//! the plan: is another validate already driving this checkout, is this process a
//! nested payload of an outer run, how many peers were genuinely burning CPU
//! beside us, was a gate's red caused by the host rather than the tree, and did
//! this run spend its wall clock computing or waiting.
//!
//! # Why these live together
//!
//! Each one is a place where the previous implementation keyed off a *proxy* for
//! the fact it claimed, and each fix here replaces that proxy with something
//! observable:
//!
//! | question | old proxy | observable binding used here |
//! | --- | --- | --- |
//! | is a peer validate running? | `ps \| grep validate.sh` matched a process group | a per-run record whose **flock the kernel releases on death**, plus a measured CPU delta |
//! | am I nested? | the `HERMIT_VALIDATE_ACTIVE` env var alone | that pid must also appear in **this process's `/proc` ancestry** |
//! | is this red environmental? | a regex over the whole gate region | the failing node's own `----- detail -----` region, classified into a **named** class |
//! | did the run do work? | wall clock only | wall **and** `getrusage` CPU (self + reaped children) |
//!
//! # The concurrency primitive is `flock`, never a pidfile and never a scan
//!
//! * `flock` is released by the KERNEL when the holder dies, so a crashed or
//!   `SIGKILL`ed run cannot strand a lock. A pidfile makes the dead-owner case
//!   something you must represent and reclaim; `flock` makes it unrepresentable.
//! * A `ps | grep` scan counts PARKED fixtures. Measured on this box on
//!   2026-08-07: **6 live `validate.sh` process groups, all six orphaned
//!   stop-test fixtures** (`ppid=1`, ages 2h20m-4h30m, parked in `sleep 1` at
//!   CPU/wall ~0.00). A scan-based refusal would have refused EVERY validate on
//!   the box - an outage strictly worse than the bug it set out to fix. The same
//!   fixtures are why the shipped ledger carries `concurrent_validates` values up
//!   to 20 (histogram over the last 200 rows: 11x7, 7x11, 7x12, 5x13, ... , 1x20)
//!   that describe nothing anybody ran.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

// ------------------------------------------------------------------ environmental blocks

/// One initial manifest-cell attempt and at most one retry of a product failure.
/// The manifest runner owns retry admission; validate retains the maximum when
/// proving both possible attempts fit inside the enclosing node deadline.
pub const MAX_ATTEMPTS_PER_CELL: usize =
    hermit_manifest_plan::runner::MAX_ATTEMPTS_PER_CELL as usize;

pub use hermit_manifest_plan::environmental_block::EnvBlockClass;
pub use hermit_manifest_plan::environmental_block::EnvBlockObservation;
pub use hermit_manifest_plan::environmental_block::environmental_block_class;
pub use hermit_manifest_plan::environmental_block::environmental_block_observation;

/// Infrastructure diagnostics in one node's own captured detail.
///
/// The owner classified PMU RCB overshoot and the existing environmental denial
/// cases as infrastructure. Full diagnostic shapes narrow the match; captured
/// text is not source authentication. The caller must preserve measured product
/// failures even when the same node also carries one of these diagnostics.
/// Generic container mount EPERM does not identify the historically diagnosed
/// TMPDIR nosuid/nodev cause, so that signature alone remains a product failure.
pub fn understood_infrastructure_class(output: &str) -> Option<&'static str> {
    let lower = output.to_ascii_lowercase();
    // Unlike the retry classifier, this does NOT accept a bare mention of
    // `bpfjailer`: that broad match is an explicit retry-favouring trade and is
    // unsafe for removing a terminal failure from the product bucket.
    if lower.contains("blocked on this server based on a security policy")
        || lower.contains("enforcer: fs, reason:")
        || lower.contains("enforcer: exec, reason:")
        || lower.contains("enforcer: net, reason:")
    {
        return Some("bpfjailer-banner");
    }
    if let Some(class @ ("proxy-egress" | "toolchain-eperm" | "vcs-fs-denial")) =
        environmental_block_class(output)
    {
        return Some(class);
    }
    if lower.contains("prehook: pmu rcb overshoot!")
        && lower.contains("clock_value:")
        && lower.contains("stepped forward")
        && lower.contains("rcbs, but should have trapped at")
    {
        return Some("PMU RCB overshoot");
    }
    None
}

/// Infrastructure signatures emitted by tools below the validation runner.
///
/// They are inspected only inside the exact failed node's captured detail
/// region. The runner records the resulting
/// [`hermit_manifest_plan::runner::FailureClass`] directly; an outer ledger
/// consumer must not reopen the aggregate log and infer the class again.
const INFRASTRUCTURE_FAILURE_SIGNATURES: &[&str] = &[
    "in archive is not an object",
    "archive has no index",
    "malformed archive",
    "file format not recognized",
    "bad file descriptor while reading archive",
    "failed to verify the checksum",
    "is different than the directory",
    "does not match the source",
    "corrupted",
    "undefined reference to `dynamorio::",
    ": failed to run command",
];

const PREREQUISITE_FAILURE_SIGNATURES: &[&str] = &[
    ": command not found",
    "unable to locate package",
    "has no installation candidate",
    "no match for argument",
    "executable file not found in $path",
];

const PREPARE_FAILURE_PREFIXES: &[&str] = &[
    "prepare failed for ",
    "c program compilation failed for ",
    "rust program compilation failed for ",
];

const PREPARE_PROGRESS_PREFIXES: &[&str] = &["built ", "skip ", "prebuilt "];

const PREREQUISITE_PREPARE_SIGNATURES: &[&str] = &[
    "not found",
    "no such file or directory",
    "command not found",
    " on path",
];

/// Classify one failed node's exact captured detail region.
///
/// `None` means the detail carried no recognized non-product evidence. The
/// caller keeps the completed nonzero execution as `product_failure`; absence
/// of a match is never converted into infrastructure or prerequisite success.
/// The optional string is the closed evidence value written beside the class.
pub fn failure_class_from_detail(
    output: &str,
) -> Option<(hermit_manifest_plan::runner::FailureClass, &'static str)> {
    if let EnvBlockObservation::Denied(class) = environmental_block_observation(output) {
        return Some((
            hermit_manifest_plan::runner::FailureClass::UnderstoodInfrastructureFailure,
            class.as_str(),
        ));
    }

    let lower = output.to_ascii_lowercase();
    if let Some(signature) = INFRASTRUCTURE_FAILURE_SIGNATURES
        .iter()
        .copied()
        .find(|signature| lower.contains(signature))
    {
        return Some((
            hermit_manifest_plan::runner::FailureClass::UnderstoodInfrastructureFailure,
            signature,
        ));
    }
    if let Some(signature) = PREREQUISITE_FAILURE_SIGNATURES
        .iter()
        .copied()
        .find(|signature| lower.contains(signature))
    {
        return Some((
            hermit_manifest_plan::runner::FailureClass::UnderstoodPrerequisiteFailure,
            signature,
        ));
    }

    let lines: Vec<&str> = lower.lines().map(str::trim).collect();
    for (index, line) in lines.iter().enumerate() {
        if !PREPARE_FAILURE_PREFIXES
            .iter()
            .any(|prefix| line.starts_with(prefix))
        {
            continue;
        }
        let Some(reason) = lines.get(index + 1) else {
            continue;
        };
        if PREPARE_FAILURE_PREFIXES
            .iter()
            .any(|prefix| reason.starts_with(prefix))
            || PREPARE_PROGRESS_PREFIXES
                .iter()
                .any(|prefix| reason.starts_with(prefix))
        {
            continue;
        }
        if let Some(signature) = PREREQUISITE_PREPARE_SIGNATURES
            .iter()
            .copied()
            .find(|signature| reason.contains(signature))
        {
            return Some((
                hermit_manifest_plan::runner::FailureClass::UnderstoodPrerequisiteFailure,
                signature,
            ));
        }
    }
    None
}

/// What an actual re-execution established about an environmental classification.
///
/// Classification binds a host/sandbox signature to one failed node attempt. It
/// is only a hypothesis about cause until the same node executes again. Keeping
/// the never-executed case explicit prevents a planned or aborted retry from
/// being reported as evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvBlockVerdict {
    /// A later, reported, non-aborted execution passed.
    Confirmed,
    /// A later, reported, non-aborted execution failed.
    Refuted,
    /// No later execution produced a usable completion payload.
    Unconfirmed,
}

impl EnvBlockVerdict {
    /// Settle the hypothesis from the first later execution that actually
    /// completed. `None` means no such execution occurred.
    pub fn settle(rerun_result: Option<bool>) -> Self {
        match rerun_result {
            Some(true) => Self::Confirmed,
            Some(false) => Self::Refuted,
            None => Self::Unconfirmed,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Confirmed => "confirmed",
            Self::Refuted => "refuted",
            Self::Unconfirmed => "unconfirmed",
        }
    }
}

/// How the failed re-execution's environmental signature compares with the
/// signature that caused the retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefutedShape {
    /// The re-execution emitted a new detail region and failed without the
    /// original environmental signature.
    BannerGone,
    /// The re-execution failed with the same environmental signature.
    Persistent,
    /// The re-execution failed with a different environmental signature.
    SignatureChanged,
}

impl RefutedShape {
    pub fn of(original: &str, latest: Option<&str>) -> Self {
        match latest {
            None => Self::BannerGone,
            Some(latest) if latest == original => Self::Persistent,
            Some(_) => Self::SignatureChanged,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::BannerGone => "banner-gone",
            Self::Persistent => "persistent",
            Self::SignatureChanged => "signature-changed",
        }
    }
}

/// Extract one failed DAG node's captured output from the driver's durable log.
///
/// `dagrun` re-emits a failed step's combined stdout+stderr between
/// `[tag] ----- detail -----` and `[tag] ----- end detail -----`, one line per
/// prefixed line (scheduler.rs:844-849). Reading THAT region - rather than a
/// whole-log tail - is what binds the classification to the node that actually
/// failed, so a jail banner printed by an unrelated concurrent node cannot excuse
/// a genuine product red.
///
/// Returns `None` when either delimiter is absent. A region is evidence only
/// after its matching end marker arrives; accepting a partial tee write would
/// turn an in-progress attempt into a classification.
pub fn extract_node_detail(log: &str, tag: &str) -> Option<String> {
    let open = format!("[{tag}] ----- detail -----");
    let close = format!("[{tag}] ----- end detail -----");
    let prefix = format!("[{tag}] ");
    // The LAST region, so a retried node is classified on its newest attempt.
    let start = log.rfind(&open)? + open.len();
    let rest = &log[start..];
    let end = rest.find(&close)?;
    let mut out = String::new();
    for line in rest[..end].lines() {
        out.push_str(line.strip_prefix(&prefix).unwrap_or(line));
        out.push('\n');
    }
    Some(out)
}

// ------------------------------------------------------------------ CPU vs wall

/// Clock ticks per second, for converting `/proc/<pid>/stat` utime+stime.
pub fn clk_tck() -> f64 {
    let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if v > 0 { v as f64 } else { 100.0 }
}

fn rusage_seconds(who: libc::c_int) -> (f64, f64) {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(who, &mut ru) } != 0 {
        return (0.0, 0.0);
    }
    let s = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1_000_000.0;
    (s(ru.ru_utime), s(ru.ru_stime))
}

/// CPU seconds (user, sys) for this process PLUS every child it has reaped.
///
/// This is exactly what bash's `times` builtin reports (validate.sh:1614), and it
/// is why the number must be taken in the top-level process: a subshell - or, here,
/// a worker thread's local view - would see only its own accounting. Every gate
/// runs as a child of this process through `dagrun`, and the runner
/// waits on each one, so `RUSAGE_CHILDREN` accumulates the whole suite.
pub fn process_cpu_seconds() -> (f64, f64) {
    let (su, ss) = rusage_seconds(libc::RUSAGE_SELF);
    let (cu, cs) = rusage_seconds(libc::RUSAGE_CHILDREN);
    (su + cu, ss + cs)
}

/// Aggregate CPU seconds (user+sys) for the process tree rooted at `root`,
/// summed from `/proc/<pid>/stat`.
///
/// Controller-free and host-portable: it does NOT need a delegated cgroup `cpu`
/// controller, which is often absent on the many-core dev hosts. The `comm` field
/// can contain spaces and parentheses, so parsing splits on the LAST `)` and
/// indexes the fixed fields after it - the same rule validate.sh:1404 used.
pub fn tree_cpu_seconds(root: i32) -> f64 {
    let mut ppid: HashMap<i32, i32> = HashMap::new();
    let mut ticks: HashMap<i32, f64> = HashMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return 0.0;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(pid) = name.parse::<i32>() else { continue };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
        let Some(rp) = stat.rfind(')') else { continue };
        let f: Vec<&str> = stat[rp + 2..].split_whitespace().collect();
        if f.len() < 13 {
            continue;
        }
        let Ok(pp) = f[1].parse::<i32>() else { continue };
        let ut: f64 = f[11].parse().unwrap_or(0.0);
        let st: f64 = f[12].parse().unwrap_or(0.0);
        ppid.insert(pid, pp);
        ticks.insert(pid, ut + st);
    }
    // Transitive closure of "is a descendant of root", root included.
    let mut in_tree: std::collections::HashSet<i32> = std::collections::HashSet::new();
    in_tree.insert(root);
    let mut changed = true;
    while changed {
        changed = false;
        for (&p, &pp) in &ppid {
            if !in_tree.contains(&p) && in_tree.contains(&pp) {
                in_tree.insert(p);
                changed = true;
            }
        }
    }
    let total: f64 = ticks.iter().filter(|(p, _)| in_tree.contains(p)).map(|(_, t)| *t).sum();
    total / clk_tck()
}

/// The load-bearing shape hint for a CPU-vs-wall pair.
///
/// CPU (user+sys, whole process tree) against wall is what distinguishes a
/// genuinely busy run from one that is blocked or spinning while merely appearing
/// hung. This is how the 53-minute pre-gate wedge was identified on 2026-08-07:
/// the wall clock alone said "still going", the CPU/wall ratio said "waiting".
///
/// * CPU below 10% of wall over a non-trivial run => waiting/blocked.
/// * ~1.0x on a multi-core host => single-threaded work, or a spin.
///
/// Returns `None` when the run is too short (<30 s) for either shape to mean
/// anything.
pub fn cpu_wall_hint(cpu: f64, wall: f64, host_cpus: usize) -> Option<&'static str> {
    if wall < 30.0 {
        return None;
    }
    if cpu < 0.10 * wall {
        return Some("low CPU vs wall — mostly waiting/blocked, not compute-bound");
    }
    let ratio = if wall > 0.0 { cpu / wall } else { 0.0 };
    if host_cpus > 2 && (0.8..=1.2).contains(&ratio) {
        return Some("~1 core busy — single-threaded or possibly spinning");
    }
    None
}

/// Format the always-printed wall+CPU line (validate.sh's `print_wall_cpu_summary`).
pub fn cpu_wall_line(
    human: fn(f64) -> String,
    wall: f64,
    user: f64,
    sys: f64,
    host_cpus: usize,
) -> String {
    let cpu = user + sys;
    let ratio =
        if wall > 0.0 { format!("{:.1}", cpu / wall) } else { "n/a".to_string() };
    let hint = cpu_wall_hint(cpu, wall, host_cpus)
        .map(|h| format!("  ({h})"))
        .unwrap_or_default();
    format!(
        "wall {} | CPU {} (user {}, sys {}) | CPU/wall {}x across {} cores{}",
        human(wall),
        human(cpu),
        human(user),
        human(sys),
        ratio,
        host_cpus,
        hint
    )
}

// ------------------------------------------------------------------ nesting

/// Environment marker naming the live top-level validate, inherited by every
/// gate this run spawns.
pub const ACTIVE_ENV: &str = "HERMIT_VALIDATE_ACTIVE";

/// What this process is with respect to an outer validate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Nesting {
    /// True only when an outer validate is provably an ANCESTOR of this process.
    pub nested: bool,
    /// The outer pid, when nesting was established.
    pub outer_pid: Option<i32>,
    /// Set when the marker was present but did NOT survive the ancestry check, so
    /// the reason a stale marker was ignored is stated rather than silent.
    pub stale_marker: Option<i32>,
}

/// Walk `/proc/<pid>/status` PPid links from `start` to pid 1, looking for `want`.
fn is_ancestor(want: i32, start: i32) -> bool {
    let mut cur = start;
    // Bounded: the ancestry chain is short, and the bound makes a corrupted
    // /proc unable to hang the driver.
    for _ in 0..256 {
        if cur == want {
            return true;
        }
        if cur <= 1 {
            return false;
        }
        let Ok(status) = std::fs::read_to_string(format!("/proc/{cur}/status")) else {
            return false;
        };
        let Some(line) = status.lines().find(|l| l.starts_with("PPid:")) else {
            return false;
        };
        let Ok(pp) = line[5..].trim().parse::<i32>() else { return false };
        cur = pp;
    }
    false
}

/// Read a process's kernel identity `(state, start_ticks)` from `/proc`.
pub fn process_identity(pid: i32) -> Option<(char, u64)> {
    if pid <= 1 {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    let fields: Vec<&str> = stat.get(close + 1..)?.split_whitespace().collect();
    let state = fields.first()?.chars().next()?;
    // `fields[0]` is stat field 3 (state), so field 22 is index 19.
    let start_ticks = fields.get(19)?.parse::<u64>().ok()?;
    Some((state, start_ticks))
}

/// Bind an ancestor PID to its start ticks so PID reuse cannot satisfy proof.
pub fn identity_in_ancestry(want: i32, want_start_ticks: u64) -> bool {
    let mut cur = std::process::id() as i32;
    for _ in 0..256 {
        if cur == want {
            return process_identity(cur).is_some_and(|(state, ticks)| {
                !matches!(state, 'Z' | 'T' | 't') && ticks == want_start_ticks
            });
        }
        if cur <= 1 {
            return false;
        }
        let Ok(status) = std::fs::read_to_string(format!("/proc/{cur}/status")) else {
            return false;
        };
        let Some(line) = status.lines().find(|line| line.starts_with("PPid:")) else {
            return false;
        };
        let Ok(parent) = line[5..].trim().parse::<i32>() else { return false };
        cur = parent;
    }
    false
}

/// Decide whether this invocation is a NESTED payload of an outer validate.
///
/// Focused internal fixtures can deliberately re-enter the driver. What must
/// never happen is a full driver inside a full driver: that pays the entire
/// preamble twice, appends a SECOND ledger row, and can publish a SECOND receipt
/// for one logical run. Portable strict compatibility is not such a payload: its
/// corpus rows are expanded directly into the outer graph.
///
/// **The env var alone is a proxy.** A marker can outlive its writer - exported
/// into an operator's shell, or inherited by a detached unit - and a stale marker
/// would make a legitimate TOP-LEVEL full run exit 2 forever: an outage, not a
/// guard. So nesting is asserted only when the named pid is observably an
/// ancestor of this process in `/proc`. A marker that fails that check is
/// reported as stale and ignored.
pub fn detect_nesting() -> Nesting {
    let raw = std::env::var(ACTIVE_ENV).unwrap_or_default();
    let Ok(outer) = raw.trim().parse::<i32>() else {
        return Nesting { nested: false, outer_pid: None, stale_marker: None };
    };
    if outer <= 0 {
        return Nesting { nested: false, outer_pid: None, stale_marker: None };
    }
    let me = std::process::id() as i32;
    if is_ancestor(outer, me) {
        Nesting { nested: true, outer_pid: Some(outer), stale_marker: None }
    } else {
        Nesting { nested: false, outer_pid: None, stale_marker: Some(outer) }
    }
}

/// Claim the marker for children. Called AFTER [`detect_nesting`], so diagnostics
/// name the run we are nested inside rather than ourselves.
pub fn claim_active_marker() {
    std::env::set_var(ACTIVE_ENV, std::process::id().to_string());
}

// ------------------------------------------------------------------ per-checkout invocation lock

/// A held, kernel-backed exclusive lock on this checkout's validate slot.
pub struct InvocationLock {
    _file: File,
    guard: File,
    holder: PathBuf,
    record: InvocationHolderRecord,
}

impl Drop for InvocationLock {
    fn drop(&mut self) {
        // Serialize the record removal and lock handoff. A new holder cannot
        // acquire the invocation lock and expose a predecessor record to a
        // contender while this guard is held.
        if flock_exclusive(self.guard.as_raw_fd()).is_ok() {
            let _ = std::fs::remove_file(&self.holder);
            flock_unlock(self._file.as_raw_fd());
            flock_unlock(self.guard.as_raw_fd());
        }
    }
}

/// Outcome of trying to claim the per-checkout validate slot.
pub enum LockOutcome {
    /// Claimed. Hold the value for the lifetime of the run.
    Acquired(InvocationLock),
    /// Another validate holds it. `detail` belongs in the common summary;
    /// `epilogue` is rendered after that summary so an action stays last.
    Busy { detail: Vec<String>, epilogue: Vec<String> },
    /// The metadata guard itself could not be established. Proceeding would
    /// disable the primary exclusion guarantee, so the caller must fail closed.
    SafetyRefusal(String),
    /// The lock could not be created at all (for example, unwritable `target/`).
    /// The caller must refuse; proceeding would run against shared output with
    /// no exclusion.
    Unavailable(String),
}

fn flock_nb(fd: i32) -> bool {
    unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

fn flock_nb_result(fd: i32) -> io::Result<bool> {
    if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::WouldBlock {
        Ok(false)
    } else {
        Err(error)
    }
}

fn flock_exclusive(fd: i32) -> io::Result<()> {
    loop {
        if unsafe { libc::flock(fd, libc::LOCK_EX) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn flock_unlock(fd: i32) {
    let _ = unsafe { libc::flock(fd, libc::LOCK_UN) };
}

/// Claim the exclusive per-checkout validate slot, or produce a typed refusal.
///
/// SCOPE IS PER-CHECKOUT, deliberately. Box-wide exclusivity belongs to `ci-hub
/// validate-lock`; duplicating it here would give the fleet two independent
/// admission controllers that can disagree. Two validates in ONE checkout are
/// unambiguously wrong: both drive one `target/` tree and one ledger.
///
/// The refusal is IMMEDIATE (never a wait) and names the holder - but only after
/// a LIVENESS CHECK, so a record left by an earlier run can never be presented as
/// a live process. The lock is `flock`, so the "holder died with the lock held"
/// case does not exist to be reclaimed.
/// The descriptive record beside the per-checkout invocation lock. ONE
/// definition, so the writer, the refusal reader, and the log-path update
/// cannot drift onto different files.
fn invocation_holder_path(root: &Path) -> PathBuf {
    root.join("target/validation").join("validate-invocation.holder")
}

fn invocation_holder_guard_path(root: &Path) -> PathBuf {
    root.join("target/validation").join("validate-invocation-holder.lock")
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct InvocationHolderRecord {
    pid: i32,
    started_at: String,
    commit: String,
    profile: String,
    checkout: PathBuf,
    log: Option<PathBuf>,
}

fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        encoded.push(DIGITS[(byte >> 4) as usize] as char);
        encoded.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn hex_decode(encoded: &str) -> Option<Vec<u8>> {
    if !encoded.len().is_multiple_of(2) {
        return None;
    }
    encoded
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let hi = (pair[0] as char).to_digit(16)?;
            let lo = (pair[1] as char).to_digit(16)?;
            Some(((hi << 4) | lo) as u8)
        })
        .collect()
}

fn encode_text(value: &str) -> String {
    hex_encode(value.as_bytes())
}

fn decode_text(value: &str) -> Option<String> {
    let decoded = String::from_utf8(hex_decode(value)?).ok()?;
    (!decoded.chars().any(char::is_control)).then_some(decoded)
}

fn encode_path(path: &Path) -> String {
    hex_encode(path.as_os_str().as_bytes())
}

fn decode_path(value: &str) -> Option<PathBuf> {
    let bytes = hex_decode(value)?;
    if bytes.contains(&0) {
        return None;
    }
    Some(PathBuf::from(OsString::from_vec(bytes)))
}

impl InvocationHolderRecord {
    fn serialize(&self) -> String {
        let mut record = format!(
            "version=1\npid={}\nstarted_at_hex={}\ncommit_hex={}\nprofile_hex={}\ncheckout_hex={}\n",
            self.pid,
            encode_text(&self.started_at),
            encode_text(&self.commit),
            encode_text(&self.profile),
            encode_path(&self.checkout),
        );
        if let Some(log) = &self.log {
            record.push_str(&format!("log_hex={}\n", encode_path(log)));
        }
        record
    }

    fn parse(record: &str) -> Option<Self> {
        if !record.ends_with('\n') {
            return None;
        }
        let mut fields = BTreeMap::new();
        for line in record.lines() {
            let (key, value) = line.split_once('=')?;
            if value.is_empty() || fields.insert(key, value).is_some() {
                return None;
            }
        }
        let allowed = [
            "version",
            "pid",
            "started_at_hex",
            "commit_hex",
            "profile_hex",
            "checkout_hex",
            "log_hex",
        ];
        if fields.keys().any(|key| !allowed.contains(key))
            || fields.get("version") != Some(&"1")
        {
            return None;
        }
        let pid = fields.get("pid")?.parse().ok()?;
        if pid <= 0 {
            return None;
        }
        Some(Self {
            pid,
            started_at: decode_text(fields.get("started_at_hex")?)?,
            commit: decode_text(fields.get("commit_hex")?)?,
            profile: decode_text(fields.get("profile_hex")?)?,
            checkout: decode_path(fields.get("checkout_hex")?)?,
            log: match fields.get("log_hex") {
                Some(value) => Some(decode_path(value)?),
                None => None,
            },
        })
    }
}

static HOLDER_WRITE_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

fn write_invocation_holder(path: &Path, record: &InvocationHolderRecord) -> io::Result<()> {
    let sequence = HOLDER_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = path.with_extension(format!("holder.{}.{}.tmp", std::process::id(), sequence));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp)?;
        file.write_all(record.serialize().as_bytes())?;
        file.flush()?;
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// Quote an arbitrary Unix path as one Bash ANSI-C argv word.
///
/// Unlike display quoting, this preserves non-UTF-8 bytes and renders embedded
/// newlines as `\n`, keeping the advertised command on one physical line.
fn shell_quote_path(path: &Path) -> String {
    let mut quoted = String::from("$'");
    for &byte in path.as_os_str().as_bytes() {
        match byte {
            b'\\' => quoted.push_str("\\\\"),
            b'\'' => quoted.push_str("\\'"),
            b'\n' => quoted.push_str("\\n"),
            b'\r' => quoted.push_str("\\r"),
            b'\t' => quoted.push_str("\\t"),
            0x20..=0x7e => quoted.push(byte as char),
            _ => quoted.push_str(&format!("\\x{byte:02x}")),
        }
    }
    quoted.push('\'');
    quoted
}

/// Record where this run's durable log lives, so a later validate that is
/// REFUSED can print a command to tail it.
///
/// The lock is claimed BEFORE the durable log exists, so the holder record is
/// necessarily written without one and atomically replaced once the path is
/// known. Only the process holding the lock may call this. Readers therefore
/// see either the complete startup record or the complete record with a log,
/// never a partially rewritten command payload.
pub fn record_invocation_log_path(lock: &mut InvocationLock, log: &Path) {
    if flock_exclusive(lock.guard.as_raw_fd()).is_ok() {
        lock.record.log = Some(log.to_path_buf());
        let _ = write_invocation_holder(&lock.holder, &lock.record);
        flock_unlock(lock.guard.as_raw_fd());
    }
}

pub fn acquire_invocation_lock(root: &Path, profile: &str, commit: &str) -> LockOutcome {
    acquire_invocation_lock_with_hook(root, profile, commit, || {})
}

fn acquire_invocation_lock_with_hook(
    root: &Path,
    profile: &str,
    commit: &str,
    after_lock_acquired: impl FnOnce(),
) -> LockOutcome {
    let dir = root.join("target/validation");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return LockOutcome::Unavailable(format!("cannot create {}: {e}", dir.display()));
    }
    let lock_path = dir.join("validate-invocation.lock");
    let holder = invocation_holder_path(root);
    let guard_path = invocation_holder_guard_path(root);
    let guard = match std::fs::OpenOptions::new().create(true).append(true).open(&guard_path) {
        Ok(file) => file,
        Err(error) => {
            return LockOutcome::SafetyRefusal(format!(
                "cannot open holder metadata guard {}: {error}; refusing rather than running \
                 without primary invocation exclusion",
                guard_path.display(),
            ))
        }
    };
    match flock_nb_result(guard.as_raw_fd()) {
        Ok(true) => {}
        Ok(false) => return LockOutcome::Busy {
            detail: vec![
                "another validate is already running or changing ownership in THIS checkout"
                    .into(),
                format!("checkout: {root:?}"),
                "holder metadata is transitioning; no holder identity or watch command is safe \
                 to publish yet"
                    .into(),
                "this is an immediate refusal, not a wait; retry after the transition completes"
                    .into(),
            ],
            epilogue: Vec::new(),
        },
        Err(error) => {
            return LockOutcome::SafetyRefusal(format!(
                "cannot lock holder metadata guard {}: {error}; refusing rather than running \
                 without primary invocation exclusion",
                guard_path.display(),
            ))
        }
    }
    let file = match std::fs::OpenOptions::new().create(true).append(true).open(&lock_path) {
        Ok(f) => f,
        Err(e) => {
            return LockOutcome::Unavailable(format!("cannot open {}: {e}", lock_path.display()))
        }
    };
    if !flock_nb(file.as_raw_fd()) {
        let mut msg = vec![
            "another validate is already running in THIS checkout".to_string(),
            format!("checkout: {root:?}"),
        ];
        let record = std::fs::read_to_string(&holder)
            .ok()
            .and_then(|record| InvocationHolderRecord::parse(&record));
        // Only a LIVE holder has a log worth watching; a stale record's log
        // describes a run that already ended. Held back and pushed LAST so the
        // refusal ends with something directly pastable.
        let mut watch: Vec<String> = Vec::new();
        match record {
            Some(record) if unsafe { libc::kill(record.pid, 0) } == 0 => {
                let pid = record.pid;
                msg.push(format!("holder (pid {pid} is LIVE):"));
                msg.push(format!("  started_at={}", record.started_at));
                msg.push(format!("  commit={}", record.commit));
                msg.push(format!("  profile={}", record.profile));
                msg.push(format!("  checkout={:?}", record.checkout));
                match record.log {
                    // `-F` not `-f`: the holder may rotate or recreate the file,
                    // and it may finish between this refusal and the paste.
                    Some(path) => {
                        msg.push(format!("  log={path:?}"));
                        watch.push("watch the holder's live log with:".into());
                        watch.push(format!("  tail -F -- {}", shell_quote_path(&path)));
                    }
                    // Say so rather than printing a guessed path: the lock is
                    // claimed before the durable log is opened, so a holder that
                    // is still starting up genuinely has no log yet.
                    None => watch.push(
                        "the holder has not opened its durable log yet, so there is nothing to \
                         tail; re-run this command in a moment to get the path"
                            .into(),
                    ),
                }
            }
            Some(record) => {
                let pid = record.pid;
                msg.push(format!(
                    "holder: the lock IS held, but the recorded pid {pid} is NOT alive, so this \
                     record is STALE and does not describe the current holder"
                ));
            }
            None => msg.push(
                "holder: (lock held, but the holder record was unreadable or incomplete; no \
                 watch command was emitted)"
                    .into(),
            ),
        }
        msg.push(
            "this is a refusal, not a wait: two validates in one checkout share target/ and the \
             ledger, and would corrupt each other's results"
                .into(),
        );
        msg.push("wait for the holder to finish, or run in a different checkout".into());
        return LockOutcome::Busy { detail: msg, epilogue: watch };
    }
    after_lock_acquired();
    // The holder guard spans invocation-lock acquisition and record
    // publication. A contender cannot read a predecessor record during this
    // handoff: it waits for the guard, then observes this holder's complete
    // atomically published record.
    let _ = std::fs::remove_file(&holder);
    let record = InvocationHolderRecord {
        pid: std::process::id() as i32,
        started_at: crate::utc_now(),
        commit: commit.to_owned(),
        profile: profile.to_owned(),
        checkout: root.to_path_buf(),
        log: None,
    };
    let _ = write_invocation_holder(&holder, &record);
    flock_unlock(guard.as_raw_fd());
    LockOutcome::Acquired(InvocationLock { _file: file, guard, holder, record })
}

// ------------------------------------------------------------------ box-wide live-run registry

/// One live-run record: an flock the kernel drops when this process dies.
pub struct RunRecord {
    _file: File,
    path: PathBuf,
}

impl Drop for RunRecord {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Where live-run records go. The parent workspace when one is resolvable (every
/// slot on this box resolves to the same parent, which is what makes the count
/// box-wide), else a per-uid directory.
pub fn registry_dir(parent: Option<&Path>) -> PathBuf {
    match parent {
        Some(p) => p.join("ignored").join("validate").join("runs"),
        None => {
            let uid = unsafe { libc::getuid() };
            PathBuf::from(format!("/tmp/hermit-validate-runs-{uid}"))
        }
    }
}

/// Publish this run as a live top-level validate.
///
/// Only a TOP-LEVEL, non-stop-test driver registers, which is what keeps parked
/// fixtures and nested payloads out of every peer count by construction rather
/// than by filtering.
fn register_run_with_hooks(
    dir: &Path,
    profile: &str,
    checkout: &Path,
    after_open: impl FnOnce(&Path),
    write_record: impl FnOnce(&mut File, &[u8]) -> io::Result<()>,
) -> Result<RunRecord, String> {
    std::fs::create_dir_all(dir).map_err(|error| {
        format!(
            "cannot create live-run registry directory {}: {error}",
            dir.display()
        )
    })?;
    let pid = std::process::id();
    let path = dir.join(format!("{pid}.run"));
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        // Do not truncate until this process owns the record's flock. A
        // contender must not erase the live holder's published identity.
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|error| format!("cannot open live-run record {}: {error}", path.display()))?;
    after_open(&path);
    match flock_nb_result(file.as_raw_fd()) {
        Ok(true) => {}
        Ok(false) => {
            return Err(format!(
                "cannot lock live-run record {}: another process holds it",
                path.display()
            ));
        }
        Err(error) => {
            return Err(format!(
                "cannot lock live-run record {}: {error}",
                path.display()
            ));
        }
    }
    let opened = file
        .metadata()
        .map_err(|error| format!("cannot inspect locked live-run record {}: {error}", path.display()))?;
    let published = std::fs::metadata(&path).map_err(|error| {
        format!(
            "locked live-run record {} is no longer published: {error}; refusing rather than \
             running with an undiscoverable registration",
            path.display()
        )
    })?;
    if opened.dev() != published.dev() || opened.ino() != published.ino() {
        return Err(format!(
            "locked live-run record {} no longer names the opened inode; refusing rather than \
             running with an undiscoverable registration",
            path.display()
        ));
    }
    let contents = format!("pid={pid}\nprofile={profile}\ncheckout={}\n", checkout.display());
    if let Err(error) = file.set_len(0).and_then(|()| write_record(&mut file, contents.as_bytes())) {
        // Remove the unpublished record while its flock is still held. Leaving a
        // partial record behind would make a later census manufacture a peer.
        let _ = std::fs::remove_file(&path);
        return Err(format!("cannot write live-run record {}: {error}", path.display()));
    }
    Ok(RunRecord { _file: file, path })
}

pub fn register_run(dir: &Path, profile: &str, checkout: &Path) -> Result<RunRecord, String> {
    register_run_with_hooks(dir, profile, checkout, |_| {}, |file, contents| {
        file.write_all(contents)
    })
}

/// A peer top-level validate observed by the monitor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeerCensus {
    /// Records whose owner is provably alive (their flock is still held).
    pub live: usize,
    /// Of those, the ones whose process tree BURNED CPU between two samples.
    pub cpu_active: usize,
    /// Records whose owner was dead, so the kernel had already released the lock.
    pub stale_reaped: usize,
}

/// Read the pid recorded in a `<pid>.run` file name.
fn record_pid(path: &Path) -> Option<i32> {
    path.file_stem()?.to_str()?.parse().ok()
}

/// Census the registry once. `previous` carries each peer's last CPU sample so a
/// peer can be judged ACTIVE only on an observed CPU delta.
///
/// Liveness is proven by trying the peer's own flock non-blockingly: success
/// means the KERNEL already released it, i.e. the owner is dead, and the record
/// is reaped. Failure (`EWOULDBLOCK`) means a live process still holds it. There
/// is no dead-owner state to represent.
pub fn census_peers(
    dir: &Path,
    self_pid: i32,
    previous: &mut BTreeMap<i32, f64>,
) -> PeerCensus {
    let mut c = PeerCensus::default();
    let Ok(entries) = std::fs::read_dir(dir) else { return c };
    let mut seen: Vec<i32> = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|s| s.to_str()) != Some("run") {
            continue;
        }
        let Some(pid) = record_pid(&path) else { continue };
        if pid == self_pid {
            continue;
        }
        let Ok(f) = File::open(&path) else { continue };
        if flock_nb(f.as_raw_fd()) {
            // Acquired => nobody holds it => the owner is gone.
            let _ = std::fs::remove_file(&path);
            c.stale_reaped += 1;
            previous.remove(&pid);
            continue;
        }
        c.live += 1;
        seen.push(pid);
        let now = tree_cpu_seconds(pid);
        // A CPU delta of a full tick is the smallest thing /proc can even show;
        // require more than that so scheduler noise is not "busy".
        if let Some(prev) = previous.insert(pid, now) {
            if now - prev > 0.05 {
                c.cpu_active += 1;
            }
        }
    }
    previous.retain(|p, _| seen.contains(p));
    c
}

/// A running peak of CPU-ACTIVE peer validates, sampled for the whole run.
///
/// A point-in-time count at start or finish misses a validate that starts and
/// ends in the middle, which is why this is a monitor and not a probe.
#[derive(Clone)]
pub struct ConcurrencyMonitor {
    peak_active: Arc<AtomicUsize>,
    peak_live: Arc<AtomicUsize>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl ConcurrencyMonitor {
    /// Start sampling `dir` every `period`. The thread is detached and exits when
    /// [`ConcurrencyMonitor::finish`] flips its stop flag.
    pub fn start(dir: PathBuf, period: std::time::Duration) -> ConcurrencyMonitor {
        let m = ConcurrencyMonitor {
            peak_active: Arc::new(AtomicUsize::new(0)),
            peak_live: Arc::new(AtomicUsize::new(0)),
            stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let (pa, pl, stop) = (m.peak_active.clone(), m.peak_live.clone(), m.stop.clone());
        let self_pid = std::process::id() as i32;
        std::thread::spawn(move || {
            let mut prev: BTreeMap<i32, f64> = BTreeMap::new();
            while !stop.load(Ordering::Relaxed) {
                let c = census_peers(&dir, self_pid, &mut prev);
                pa.fetch_max(c.cpu_active, Ordering::Relaxed);
                pl.fetch_max(c.live, Ordering::Relaxed);
                std::thread::sleep(period);
            }
        });
        m
    }

    /// Stop sampling and report `(peak_cpu_active, peak_live)`.
    pub fn finish(&self) -> (usize, usize) {
        self.stop.store(true, Ordering::Relaxed);
        (self.peak_active.load(Ordering::Relaxed), self.peak_live.load(Ordering::Relaxed))
    }
}

// ------------------------------------------------------------------ stop-test seam

/// Environment switch for the stop-path fixture (`scripts/test_validate_stop_paths.py`).
pub const STOP_TEST_ENV: &str = "HERMIT_VALIDATE_STOP_TEST_MODE";

/// Is this invocation the stop-path fixture?
pub fn stop_test_requested() -> bool {
    std::env::var(STOP_TEST_ENV).map(|v| v == "1").unwrap_or(false)
}

fn env_is(name: &str, want: &str) -> bool {
    std::env::var(name).map(|v| v == want).unwrap_or(false)
}

fn env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// How long an orphaned stop-test fixture may live before self-terminating.
///
/// THE LEAK THIS CLOSES. The fixture's whole job is to park until its parent test
/// signals it, and the test spawns it with `start_new_session=True` - so if the
/// test process dies first (an assertion before the signal, a `wait` timeout, or
/// the agent being recycled), nothing ever signals the fixture and nothing in its
/// new session can. Measured on this box 2026-08-07: **6 orphaned `validate.sh
/// full` process groups, all `ppid=1`, ages 2h20m to 4h30m, each parked in `sleep
/// 1` at CPU/wall ~0.00.** Two independent exits now make that unrepresentable:
/// orphan detection (`getppid() == 1`) fires within a poll, and this deadline is
/// the backstop for the case where the fixture is reparented to something other
/// than init.
pub const STOP_TEST_MAX_SECONDS_DEFAULT: f64 = 300.0;

/// Why the stop-test fixture stopped parking.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StopTestExit {
    /// `VALIDATE_STOP_TEST_EXIT_EARLY=1`: an ordinary incomplete exit, NOT a stop.
    EarlyExit,
    /// A stop signal arrived (the case the fixture exists to exercise).
    Signalled,
    /// The parent test died without signalling: this fixture is an orphan.
    Orphaned,
    /// The lifetime deadline expired.
    Deadline,
}

/// Park until a stop signal arrives, this process is orphaned, or the deadline
/// expires. `interrupted` reports the signal name once a handler has recorded one.
pub fn stop_test_park(interrupted: fn() -> Option<&'static str>) -> StopTestExit {
    if env_is("VALIDATE_STOP_TEST_EXIT_EARLY", "1") {
        return StopTestExit::EarlyExit;
    }
    let max = env_f64("VALIDATE_STOP_TEST_MAX_SECONDS", STOP_TEST_MAX_SECONDS_DEFAULT);
    let start = std::time::Instant::now();
    loop {
        if interrupted().is_some() {
            return StopTestExit::Signalled;
        }
        if unsafe { libc::getppid() } == 1 {
            return StopTestExit::Orphaned;
        }
        if start.elapsed().as_secs_f64() >= max {
            return StopTestExit::Deadline;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Announce readiness exactly the way the Python test waits for it.
pub fn stop_test_announce() {
    if let Ok(p) = std::env::var("VALIDATE_STOP_TEST_PID_FILE") {
        if !p.is_empty() {
            let _ = std::fs::write(&p, format!("{}\n", std::process::id()));
        }
    }
    println!("VALIDATE_STOP_TEST_READY pid={}", std::process::id());
    let _ = std::io::stdout().flush();
}

/// The cleanup-race hook: signal readiness, then linger inside the critical
/// section while the test hammers the process with `SIGTERM`.
pub fn stop_test_cleanup_hook() {
    let Ok(p) = std::env::var("VALIDATE_STOP_TEST_CLEANUP_READY_FILE") else { return };
    if p.is_empty() {
        return;
    }
    let _ = std::fs::write(&p, format!("{}\n", std::process::id()));
    let delay = env_f64("VALIDATE_STOP_TEST_CLEANUP_DELAY_SECONDS", 0.5);
    std::thread::sleep(std::time::Duration::from_secs_f64(delay));
}

/// Make the evidence-commit window signal-atomic.
///
/// Cleanup is where the single ledger append happens. A second stop signal must
/// not abort it between teardown and that append, or a run would leave no record
/// of having run at all. `SIG_IGN` for the whole window is what
/// `trap '' INT TERM HUP` bought the bash (validate.sh:1817).
pub fn enter_cleanup_critical_section() {
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
}

// ------------------------------------------------------------------ self-test

/// Inert two-sided brackets for everything in this module.
///
/// Nothing here runs a gate, publishes a label, writes the real ledger, or claims
/// a lock any other process could be waiting on: the lock and registry brackets
/// operate inside a private temporary directory, and the concurrency bracket
/// registers a FAKE peer record held by a short-lived child of this process, which
/// cannot authorize anything.
pub fn self_test() -> Result<String, String> {
    if MAX_ATTEMPTS_PER_CELL != 2 {
        return Err(format!(
            "manifest retry policy: expected one initial cell attempt plus exactly one retry, \
             got MAX_ATTEMPTS_PER_CELL={MAX_ATTEMPTS_PER_CELL}"
        ));
    }

    // ---- THREE STATES, because two cannot carry three facts ----
    //
    // ⚠️ THE MIDDLE AND LAST CASES ARE THE POINT. Both map to `None` through the
    // legacy `environmental_block_class`, and they are opposite facts: "we read
    // the evidence and there was no denial" versus "there was no evidence". The
    // first makes a product failure real; the second makes it unproven. A
    // unification that merges two classifiers is exactly where that distinction
    // gets absorbed, so it is asserted here rather than left to callers.
    for (want, label, text) in [
        (
            EnvBlockObservation::Denied(EnvBlockClass::BpfjailerBanner),
            "a denial that FIRED",
            "An action was blocked on this server based on a security policy!",
        ),
        (
            EnvBlockObservation::NoDenial,
            "evidence present, NO denial in it",
            "assertion failed: left == right\n  left: 4\n right: 5",
        ),
        (
            EnvBlockObservation::NothingObserved,
            "NOTHING to evaluate — not a statement that no denial occurred",
            "   \n\t\n",
        ),
    ] {
        let got = environmental_block_observation(text);
        if got != want {
            return Err(format!(
                "three-state: {label} must observe {want:?}, got {got:?}"
            ));
        }
    }
    // And the collapse is one-way ONLY: both non-denials read as `None` through
    // the legacy view, which is why anything needing to tell them apart must not
    // use it.
    if environmental_block_class("   ").is_some() || environmental_block_class("plain failure").is_some() {
        return Err("three-state: the legacy view must report neither non-denial as a class".into());
    }

    // The current writer records the owner's four failure classes directly.
    // These brackets cover both non-product classes and prove that weak prose
    // remains product evidence unless it occurs in the harness's preparation
    // failure shape.
    use hermit_manifest_plan::runner::FailureClass;
    for (text, want_class, want_detail) in [
        (
            "error: failed to verify the checksum for `crate v1.0.0`",
            FailureClass::UnderstoodInfrastructureFailure,
            "failed to verify the checksum",
        ),
        (
            "tool: command not found",
            FailureClass::UnderstoodPrerequisiteFailure,
            ": command not found",
        ),
        (
            "prepare failed for language-runtimes/lua-random.sh\nno Lua interpreter on PATH",
            FailureClass::UnderstoodPrerequisiteFailure,
            " on path",
        ),
    ] {
        if failure_class_from_detail(text) != Some((want_class, want_detail)) {
            return Err(format!(
                "failure class: expected {want_class:?}/{want_detail:?} for {text:?}, got {:?}",
                failure_class_from_detail(text)
            ));
        }
    }
    for text in [
        "assertion failed: expected key not found",
        "error[E0432]: unresolved import `detcore::sched`",
        "",
    ] {
        if failure_class_from_detail(text).is_some() {
            return Err(format!(
                "failure class: product or absent evidence was reclassified: {text:?}"
            ));
        }
    }

    // ---- understood infrastructure: exact measured signatures only ----
    for (want, text) in [
        (
            "bpfjailer-banner",
            "An action was blocked on this server based on a security policy!\n\
             Enforcer: FS, Reason: FILE_OPEN",
        ),
        (
            "PMU RCB overshoot",
            "2026-08-27 ERROR detcore: prehook: PMU RCB overshoot! Clock_value: 16249. \
             Stepped forward 139 RCBs, but should have trapped at 100",
        ),
    ] {
        if understood_infrastructure_class(text) != Some(want) {
            return Err(format!(
                "understood infrastructure: measured signature did not classify as {want}: {text:?}"
            ));
        }
    }
    for text in [
        "HERMIT_INTERNAL_FAILURE class=cli-error\nError: Sandbox container failed to spawn\n\
         Caused by: mount failed: -1",
        "[e2e.metadata] Bunnylol `scuba bpfjailer_enforce` for more details",
        "error: failed to run custom build command for `reverie-dbi v0.1.0`",
        "guest wrote: prehook: PMU RCB overshoot!",
        "guest wrote: mount failed: -1",
        "Error: Sandbox container failed to spawn\nCaused by: No such file or directory",
    ] {
        if understood_infrastructure_class(text).is_some() {
            return Err(format!(
                "understood infrastructure: an incomplete or guest-only phrase was excused: {text:?}"
            ));
        }
    }

    // ---- environmental classification: qualifying cases must be ACCEPTED ----
    let accept: &[(&str, &str)] = &[
        (
            "bpfjailer-banner",
            "test tests::x ... An action was blocked on this server based on a security policy!\n\
             Enforcer: FS, Reason: FILE_OPEN\nFAILED",
        ),
        (
            "bpfjailer-banner",
            "[e2e.metadata] Bunnylol `scuba bpfjailer_enforce` for more details",
        ),
        (
            "toolchain-eperm",
            "/usr/include/signal.h:311:11: fatal error: /usr/lib/gcc/x86_64-redhat-linux/11/include/stddef.h: Operation not permitted",
        ),
        (
            "toolchain-eperm",
            "error: could not write output to /w/target/debug/deps/x.rcgu.o: Operation not permitted",
        ),
        (
            "toolchain-eperm",
            "Fatal error: can't create CMakeFiles/ebl_pic.dir/x.c.o: Operation not permitted",
        ),
        (
            "toolchain-eperm",
            "cc: fatal error: cannot execute 'as': execvp: Operation not permitted",
        ),
        (
            "third-party-build",
            "error: failed to run custom build command for `reverie-dbi v0.1.0`",
        ),
        // NEW class 1: the proxy/DNS failure measured in
        // /tmp/hermit-validate.WUrHlJ.log, which the old regex did NOT catch.
        (
            "proxy-egress",
            "Lookup error: git ls-remote https://github.com/rrnewton/reverie.git refs/heads/main \
             failed: fatal: unable to access 'https://github.com/rrnewton/reverie.git/': Could not \
             resolve proxy: fwdproxy",
        ),
        (
            "proxy-egress",
            "fatal: unable to access 'https://github.com/rr-debugger/rr/': CONNECT tunnel failed, \
             response 403",
        ),
        // NEW class 2: a banner-less git FS denial in a /tmp fixture repository.
        (
            "vcs-fs-denial",
            "fatal: could not create leading directories of \
             /tmp/check-reverie-pin-stale-lock-1/.git/config: Operation not permitted",
        ),
        (
            "vcs-fs-denial",
            "error: chmod on /tmp/check-reverie-pin-x/.git/config.lock failed: Permission denied",
        ),
        // NEW class 3: three phrasings of one host condition, all MEASURED on a
        // jailed dev host on 2026-08-17 and all previously recorded as product
        // reds. Paths below are shortened; only the phrasing is under test.
        //
        // (a) cargo's build-script spawn denial. The tool phrase and the denial
        //     are on DIFFERENT lines, two `Caused by:` apart; this is the case
        //     that forced the block scan. Verbatim from build.runtime_release.
        (
            "toolchain-eperm",
            "error: failed to run custom build command for `libm v0.2.16`\n\n\
             Caused by:\n  \
             could not execute process /w/target/release/build/libm-a0/build-script-build \
             (never executed)\n\n\
             Caused by:\n  \
             Permission denied (os error 13)",
        ),
        // (b) rustc's incremental copy step. One line, but "unable to copy" was
        //     not an anchor while its sibling "could not write output to" was.
        (
            "toolchain-eperm",
            "error: unable to copy /w/target/debug/incremental/x-1/s-a-working/y.o to \
             /w/target/debug/deps/x-2.y.rcgu.o: Operation not permitted (os error 1)",
        ),
        // (c) a link denial from `cp -a`. Carried the jail banner when measured,
        //     so Form 1 caught it; this bracket is the banner-less form, which
        //     "can't create" above does not spell the same way.
        (
            "toolchain-eperm",
            "cp: cannot create symbolic link '/tmp/tmp.zIdYFwxfiQ/detcore/.ignore': \
             Operation not permitted",
        ),
        // (e) coreutils rm refusing to unlink a fixture scratch file.
        (
            "toolchain-eperm",
            "rm: cannot remove '/tmp/tmp.smlR46sit2': Operation not permitted",
        ),
        // (d) clippy-driver failing to write a .rmeta, measured in lint.clippy.
        (
            "toolchain-eperm",
            "error: failed to write /w/target/debug/deps/librustbin_futex_and_print-06f5.rmeta: \
             Operation not permitted (os error 1)",
        ),
    ];
    let mut accepted = 0usize;
    for (want, text) in accept {
        match environmental_block_class(text) {
            Some(got) if got == *want => accepted += 1,
            other => {
                return Err(format!(
                    "environmental: {text:?} must classify as {want}, got {other:?}"
                ))
            }
        }
    }
    // ---- and violating cases must be REFUSED (else the retry loop would eat
    // every genuine product red and report it as host flake) ----
    let refuse: &[&str] = &[
        // Real guest EPERM output. These are the exact shapes validate.sh's
        // comment names as must-not-match.
        "2026-08-03T14:12:23Z INFO detcore::syscalls::memory: DETLOG [dtid 2800245] madvise advice \
         100 rejected with -1 EPERM (Operation not permitted)",
        "kcmp-eperm: kcmp returned EPERM (Operation not permitted) as expected",
        "context: Mount { .. }: Operation not permitted",
        // Real product failures.
        "test result: FAILED. 9 passed; 1 failed; 0 ignored",
        "thread 'tests::scheduler_is_deterministic' panicked at detcore/src/scheduler.rs:100:5:\n\
         assertion `left == right` failed",
        "error[E0308]: mismatched types",
        // A test that merely PRINTS the words must not be excused.
        "test permission_denied_is_reported ... ok",
        "guest wrote: permission denied",
        "guest wrote: cannot create temp file for here-document: Operation not permitted",
        "",
        // The block scan must not turn a GENUINE compile failure into a retry.
        // Verbatim cc output from breaking tests/backend-parity/fixtures/
        // mkdir_rmdir.c on purpose, indented source excerpt and all: no denial
        // anywhere, so no block can qualify.
        "/w/tests/backend-parity/fixtures/mkdir_rmdir.c:102:1: error: unknown type name 'this'\n  \
         102 | this is not valid C and must fail to compile;\n      \
         | ^~~~\n\
         collect2: error: ld returned 1 exit status",
        // A block must not ABSORB an unrelated denial. The compile error and the
        // stray denial are separate unindented lines, so they are separate
        // blocks: the error block has no denial and the denial block has no tool
        // phrase. This is the property that keeps the scoping rule honest.
        "error[E0308]: mismatched types\n   \
         expected `u32`, found `i64`\n\
         Permission denied (os error 13)",
        // Same shape with the denial in the INDENTED continuation of a product
        // failure: still refused, because no tool phrase joins it.
        "error: test harness reported a failure\n  \
         guest wrote: permission denied",
        // Guest prose that happens to contain "failed to write" beside a real
        // EPERM. This is why that one anchor carries the `error: ` prefix.
        "guest wrote: failed to write /tmp/scratch: Operation not permitted",
        // Likewise for the exact prefixes: quoting the complete diagnostic
        // inside guest/product prose must not earn an environmental retry.
        "guest wrote: cannot remove the lock file: Operation not permitted",
        "guest wrote: error: failed to write /tmp/scratch: Operation not permitted",
        "guest wrote: rm: cannot remove /tmp/scratch: Permission denied",
        // Generic tool-like prose and a denial on different lines in one
        // indented product block must not qualify for the Cargo-only widening.
        "test failed: could not open expected output\n  guest errno: Permission denied",
    ];
    let mut refused = 0usize;
    for text in refuse {
        if let Some(class) = environmental_block_class(text) {
            return Err(format!(
                "environmental: {text:?} is a PRODUCT failure but classified as {class}"
            ));
        }
        refused += 1;
    }
    // ---- node-detail extraction, both directions ----
    let log = "[build.workspace] ✗ FAIL   Workspace build (12s, exit 101)\n\
               [build.workspace] ----- detail -----\n\
               [build.workspace] error: could not write output to /w/x.o: Operation not permitted\n\
               [build.workspace] ----- end detail -----\n\
               [lint.clippy] ✓ PASS   Clippy (3s)\n";
    let detail = extract_node_detail(log, "build.workspace")
        .ok_or("extract: the failed node's detail region must be found")?;
    if !detail.contains("could not write output to") || detail.contains("[build.workspace]") {
        return Err(format!("extract: prefix must be stripped, got {detail:?}"));
    }
    if environmental_block_class(&detail) != Some("toolchain-eperm") {
        return Err("extract+classify: the extracted region must classify environmental".into());
    }
    if extract_node_detail(log, "lint.clippy").is_some() {
        return Err("extract: a node with no detail region must yield None".into());
    }
    let partial = "[build.workspace] ----- detail -----\n\
                   [build.workspace] Enforcer: FS, Reason: FILE_OPEN\n";
    if extract_node_detail(partial, "build.workspace").is_some() {
        return Err("extract: an unterminated detail region must remain unknown".into());
    }

    // ---- environmental retry verdict: all three states and refuted shapes ----
    // A coincident banner and real assertion still classifies on the first
    // attempt. Only the later execution's typed result may settle that
    // hypothesis; classification itself can never turn a red into a pass.
    let coincident = "[test.detcore] ----- detail -----\n\
                      [test.detcore] Enforcer: FS, Reason: FILE_OPEN\n\
                      [test.detcore] assertion `left == right` failed\n\
                      [test.detcore] test result: FAILED. 412 passed; 1 failed\n\
                      [test.detcore] ----- end detail -----\n";
    let coincident_detail = extract_node_detail(coincident, "test.detcore")
        .ok_or("retry verdict: coincident detail region was not found")?;
    if environmental_block_class(&coincident_detail) != Some("bpfjailer-banner") {
        return Err(
            "retry verdict: coincident banner + real failure must remain a classified hypothesis"
                .into(),
        );
    }
    for (rerun, want) in [
        (Some(true), EnvBlockVerdict::Confirmed),
        (Some(false), EnvBlockVerdict::Refuted),
        (None, EnvBlockVerdict::Unconfirmed),
    ] {
        if EnvBlockVerdict::settle(rerun) != want {
            return Err(format!(
                "retry verdict: rerun result {rerun:?} did not settle as {want:?}"
            ));
        }
    }
    for (latest, want) in [
        (None, RefutedShape::BannerGone),
        (Some("bpfjailer-banner"), RefutedShape::Persistent),
        (Some("proxy-egress"), RefutedShape::SignatureChanged),
    ] {
        if RefutedShape::of("bpfjailer-banner", latest) != want {
            return Err(format!(
                "retry verdict: latest class {latest:?} did not attribute refutation as {want:?}"
            ));
        }
    }

    // ---- CPU-vs-wall hints, both directions ----
    if cpu_wall_hint(5.0, 600.0, 316) != Some("low CPU vs wall — mostly waiting/blocked, not compute-bound") {
        return Err("cpu/wall: 5s CPU over 600s wall must read as blocked".into());
    }
    if cpu_wall_hint(600.0, 600.0, 316) != Some("~1 core busy — single-threaded or possibly spinning") {
        return Err("cpu/wall: 1.0x on 316 cores must read as ~1 core busy".into());
    }
    if cpu_wall_hint(4000.0, 600.0, 316).is_some() {
        return Err("cpu/wall: a genuinely parallel run must get NO hint".into());
    }
    if cpu_wall_hint(0.0, 10.0, 316).is_some() {
        return Err("cpu/wall: a <30s run is too short for either shape to mean anything".into());
    }
    let line = cpu_wall_line(|s| format!("{}s", s.round() as i64), 600.0, 30.0, 10.0, 316);
    if !line.contains("CPU/wall 0.1x across 316 cores") || !line.contains("mostly waiting") {
        return Err(format!("cpu/wall: line must carry ratio AND hint, got {line:?}"));
    }

    // ---- process CPU accounting must be live, not a stub ----
    let (u, s) = process_cpu_seconds();
    if u + s <= 0.0 {
        return Err("cpu: getrusage reported zero CPU for a process that has run".into());
    }
    let own = tree_cpu_seconds(std::process::id() as i32);
    if own <= 0.0 {
        return Err("cpu: /proc tree accounting reported zero for our own live tree".into());
    }

    // ---- nesting: ancestry is what binds it, not the env var ----
    let saved = std::env::var(ACTIVE_ENV).ok();
    std::env::set_var(ACTIVE_ENV, "1");
    // pid 1 IS an ancestor of everything, so this is the qualifying positive.
    let positive = detect_nesting();
    if !positive.nested || positive.outer_pid != Some(1) {
        return Err(format!("nesting: pid 1 must be seen as an ancestor, got {positive:?}"));
    }
    // A pid that is NOT in our ancestry is a STALE marker, not nesting. (2^22 is
    // above the default pid_max, so it cannot name a live process.)
    std::env::set_var(ACTIVE_ENV, "4194303");
    let negative = detect_nesting();
    if negative.nested || negative.stale_marker != Some(4194303) {
        return Err(format!("nesting: a non-ancestor marker must be STALE, got {negative:?}"));
    }
    std::env::set_var(ACTIVE_ENV, "not-a-pid");
    if detect_nesting().nested {
        return Err("nesting: a malformed marker must not assert nesting".into());
    }
    std::env::remove_var(ACTIVE_ENV);
    if detect_nesting().nested {
        return Err("nesting: an absent marker must not assert nesting".into());
    }
    match saved {
        Some(v) => std::env::set_var(ACTIVE_ENV, v),
        None => std::env::remove_var(ACTIVE_ENV),
    }

    // ---- the invocation lock, BOTH directions, in a private sandbox ----
    //
    // Both directions matter equally: a guard that refuses the sequential case
    // too is a worse outage than the concurrency it prevents.
    let sandbox = std::env::temp_dir().join(format!("validate-lock-selftest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sandbox);
    std::fs::create_dir_all(&sandbox).map_err(|e| format!("lock bracket: {e}"))?;
    let mut lock_accept = 0usize;
    let mut lock_refuse = 0usize;
    let mut lock_safety_refuse = 0usize;
    {
        let mut first = match acquire_invocation_lock(&sandbox, "self-test", "0000000") {
            LockOutcome::Acquired(l) => {
                lock_accept += 1;
                l
            }
            LockOutcome::Busy { detail, epilogue } => {
                return Err(format!(
                    "lock: a free slot must be granted: detail={detail:?} epilogue={epilogue:?}"
                ))
            }
            LockOutcome::SafetyRefusal(e) => {
                return Err(format!("lock: sandbox safety guard refused: {e}"))
            }
            LockOutcome::Unavailable(e) => return Err(format!("lock: sandbox unusable: {e}")),
        };
        // NEGATIVE: a concurrent claim, from a real second fd, must be REFUSED and
        // must name the live holder.
        match acquire_invocation_lock(&sandbox, "self-test", "0000000") {
            LockOutcome::Busy { detail, epilogue } => {
                lock_refuse += 1;
                let joined = detail
                    .iter()
                    .chain(&epilogue)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n");
                if !joined.contains("is LIVE") || !joined.contains(&std::process::id().to_string()) {
                    return Err(format!("lock: refusal must name the LIVE holder pid: {joined}"));
                }
                // Before the holder records a log there is nothing to tail, and
                // the refusal must SAY so rather than print a guessed path.
                if !joined.contains("has not opened its durable log yet") {
                    return Err(format!(
                        "lock: with no recorded log the refusal must say so, not guess: {joined}"
                    ));
                }
                if joined.contains("tail -F") {
                    return Err(format!(
                        "lock: refusal offered a tail command with no recorded log: {joined}"
                    ));
                }
            }
            _ => return Err("lock: a second concurrent claim MUST be refused".into()),
        }
        // With a log recorded, the refusal must END with a command that a Bash
        // user can paste without the path being split, expanded, or executed.
        // Exercise each dangerous class independently so removing any one escape
        // has a causal failing case.
        for name in [
            "holder run.log",
            "holder'quote.log",
            "holder;$(touch HOLDER_QUOTE_INJECTION)&|*.log",
            "holder\nnewline.log",
        ] {
            let fake_log = sandbox.join(name);
            std::fs::write(&fake_log, "holder log\n")
                .map_err(|e| format!("lock bracket: {e}"))?;
            record_invocation_log_path(&mut first, &fake_log);
            match acquire_invocation_lock(&sandbox, "self-test", "0000000") {
                LockOutcome::Busy { detail: _, epilogue } => {
                    lock_refuse += 1;
                    let quoted = shell_quote_path(&fake_log);
                    let want = format!("  tail -F -- {quoted}");
                    if epilogue.last().map(String::as_str) != Some(want.as_str()) {
                        return Err(format!(
                            "lock: refusal must END with `{want}`, got: {:?}",
                            epilogue.last()
                        ));
                    }
                    let probe = std::process::Command::new("bash")
                        .current_dir(&sandbox)
                        .arg("-c")
                        .arg(format!("set -- {quoted}; printf %s \"$1\""))
                        .output()
                        .map_err(|e| format!("lock quote probe: {e}"))?;
                    if !probe.status.success()
                        || probe.stdout.as_slice() != fake_log.as_os_str().as_bytes()
                    {
                        return Err(format!(
                            "lock: tail path did not survive shell parsing: {:?}",
                            fake_log
                        ));
                    }
                    if sandbox.join("HOLDER_QUOTE_INJECTION").exists() {
                        return Err("lock: shell metacharacters executed from the tail hint".into());
                    }
                }
                _ => return Err("lock: a second concurrent claim MUST be refused".into()),
            }
        }

        // A reader racing an incomplete or malformed record must fail closed:
        // it may report that the lock is held, but it must not publish a command
        // assembled from untrusted fragments.
        let holder_path = invocation_holder_path(&sandbox);
        let nul_path_record = InvocationHolderRecord {
            pid: std::process::id() as i32,
            started_at: "now".into(),
            commit: "0000000".into(),
            profile: "self-test".into(),
            checkout: sandbox.clone(),
            log: Some(PathBuf::from(OsString::from_vec(
                b"/tmp/before\0after".to_vec(),
            ))),
        }
        .serialize();
        let complete_without_final_newline = {
            let mut record = InvocationHolderRecord {
                pid: std::process::id() as i32,
                started_at: "now".into(),
                commit: "0000000".into(),
                profile: "self-test".into(),
                checkout: sandbox.clone(),
                log: Some(sandbox.join("complete-but-truncated.log")),
            }
            .serialize();
            if record.pop() != Some('\n') {
                return Err("lock: serialized holder record lost its final newline".into());
            }
            record
        };
        for malformed in [
            format!("version=1\npid={}\nstarted_at_hex=00", std::process::id()),
            format!(
                "pid={}\nlog=/tmp/holder; touch HOLDER_RECORD_INJECTION\n",
                std::process::id()
            ),
            nul_path_record,
            complete_without_final_newline,
        ] {
            std::fs::write(&holder_path, malformed)
                .map_err(|e| format!("lock malformed-record bracket: {e}"))?;
            match acquire_invocation_lock(&sandbox, "self-test", "0000000") {
                LockOutcome::Busy { detail, epilogue } => {
                    lock_refuse += 1;
                    let joined = detail
                        .iter()
                        .chain(&epilogue)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !joined.contains("unreadable or incomplete")
                        || joined.contains("tail -F")
                        || !epilogue.is_empty()
                    {
                        return Err(format!(
                            "lock: malformed holder record must fail closed without a command: {joined}"
                        ));
                    }
                }
                _ => return Err("lock: a second concurrent claim MUST be refused".into()),
            }
        }
        drop(first);
    }

    // HANDOFF: leave a predecessor record behind as a crashed process can, then
    // pause the next holder after it owns the invocation lock but before it
    // publishes its record. A third independently-opened fd must refuse
    // immediately without attributing the predecessor's record to the new lock
    // owner; after publication, the next refusal must name the new holder.
    let predecessor = InvocationHolderRecord {
        pid: std::process::id() as i32,
        started_at: "predecessor".into(),
        commit: "predecessor-commit".into(),
        profile: "self-test".into(),
        checkout: sandbox.clone(),
        log: Some(sandbox.join("predecessor.log")),
    };
    write_invocation_holder(&invocation_holder_path(&sandbox), &predecessor)
        .map_err(|e| format!("lock handoff predecessor: {e}"))?;
    let (new_locked_tx, new_locked_rx) = std::sync::mpsc::sync_channel(0);
    let (publish_tx, publish_rx) = std::sync::mpsc::sync_channel(0);
    let new_root = sandbox.clone();
    let new_holder = std::thread::spawn(move || {
        acquire_invocation_lock_with_hook(&new_root, "self-test", "new-holder-commit", || {
            new_locked_tx.send(()).expect("handoff receiver exists");
            publish_rx.recv().expect("handoff publisher exists");
        })
    });
    new_locked_rx
        .recv_timeout(Duration::from_secs(2))
        .map_err(|e| format!("lock handoff did not reach publication seam: {e}"))?;
    let (contender_started_tx, contender_started_rx) = std::sync::mpsc::sync_channel(0);
    let (contender_tx, contender_rx) = std::sync::mpsc::sync_channel(0);
    let contender_root = sandbox.clone();
    let contender = std::thread::spawn(move || {
        contender_started_tx.send(()).expect("handoff receiver exists");
        let outcome = acquire_invocation_lock(&contender_root, "self-test", "contender-commit");
        contender_tx.send(outcome).expect("handoff receiver exists");
    });
    contender_started_rx
        .recv_timeout(Duration::from_secs(2))
        .map_err(|e| format!("lock handoff contender did not start: {e}"))?;
    let transition_outcome = match contender_rx.recv_timeout(Duration::from_millis(500)) {
        Ok(outcome) => outcome,
        Err(error) => {
            let _ = publish_tx.send(());
            if let Ok(LockOutcome::Acquired(lock)) = new_holder.join() {
                drop(lock);
            }
            let _ = contender_rx.recv_timeout(Duration::from_secs(2));
            let _ = contender.join();
            return Err(format!(
                "lock handoff: contender did not refuse immediately during publication: {error}"
            ));
        }
    };
    contender.join().map_err(|_| "lock handoff contender panicked")?;
    match transition_outcome {
        LockOutcome::Busy { detail, epilogue } => {
            lock_refuse += 1;
            let joined = detail.iter().chain(&epilogue).cloned().collect::<Vec<_>>().join("\n");
            if !joined.contains("metadata is transitioning")
                || joined.contains("predecessor-commit")
                || joined.contains("tail -F")
                || !epilogue.is_empty()
            {
                return Err(format!(
                    "lock handoff: transition refusal exposed unsafe holder data: {joined}"
                ));
            }
        }
        LockOutcome::Acquired(_) => {
            return Err("lock handoff: contender acquired during publication".into())
        }
        LockOutcome::SafetyRefusal(error) => {
            return Err(format!("lock handoff: transition safety-refused unexpectedly: {error}"))
        }
        LockOutcome::Unavailable(error) => {
            return Err(format!("lock handoff: transition became unguarded: {error}"))
        }
    }
    publish_tx.send(()).map_err(|e| format!("lock handoff publish release: {e}"))?;
    let new_lock = match new_holder.join().map_err(|_| "lock handoff new holder panicked")? {
        LockOutcome::Acquired(lock) => lock,
        LockOutcome::Busy { detail, epilogue } => {
            return Err(format!(
                "lock handoff: new holder was refused: detail={detail:?} epilogue={epilogue:?}"
            ))
        }
        LockOutcome::SafetyRefusal(error) => {
            return Err(format!("lock handoff: new holder safety-refused: {error}"))
        }
        LockOutcome::Unavailable(error) => {
            return Err(format!("lock handoff: new holder unavailable: {error}"))
        }
    };
    match acquire_invocation_lock(&sandbox, "self-test", "post-publication-contender") {
        LockOutcome::Busy { detail, epilogue } => {
            lock_refuse += 1;
            let joined = detail
                .iter()
                .chain(&epilogue)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");
            if !joined.contains("new-holder-commit") || joined.contains("predecessor-commit") {
                return Err(format!(
                    "lock handoff: contender did not observe the new holder record: {joined}"
                ));
            }
        }
        LockOutcome::Acquired(_) => {
            return Err("lock handoff: contender acquired while new holder was live".into())
        }
        LockOutcome::SafetyRefusal(error) => {
            return Err(format!("lock handoff: contender safety-refused: {error}"))
        }
        LockOutcome::Unavailable(error) => {
            return Err(format!("lock handoff: contender unavailable: {error}"))
        }
    }
    drop(new_lock);

    // A broken metadata guard must not fall through the caller's explicitly
    // unguarded compatibility path. Make the guard path unopenable while the
    // primary lock is free and require the distinct fail-closed outcome.
    let guard_path = invocation_holder_guard_path(&sandbox);
    std::fs::remove_file(&guard_path).map_err(|e| format!("lock guard failure cleanup: {e}"))?;
    std::fs::create_dir(&guard_path).map_err(|e| format!("lock guard failure setup: {e}"))?;
    match acquire_invocation_lock(&sandbox, "self-test", "guard-failure") {
        LockOutcome::SafetyRefusal(error) if error.contains("cannot open holder metadata guard") => {
            lock_safety_refuse += 1;
        }
        LockOutcome::SafetyRefusal(error) => {
            return Err(format!("lock guard failure named the wrong cause: {error}"))
        }
        LockOutcome::Unavailable(error) => {
            return Err(format!("lock guard failure incorrectly permitted unguarded execution: {error}"))
        }
        LockOutcome::Busy { detail, epilogue } => {
            return Err(format!(
                "lock guard failure was misreported as contention: detail={detail:?} epilogue={epilogue:?}"
            ))
        }
        LockOutcome::Acquired(_) => {
            return Err("lock guard failure incorrectly acquired the invocation lock".into())
        }
    }
    std::fs::remove_dir(&guard_path).map_err(|e| format!("lock guard failure teardown: {e}"))?;

    // POSITIVE, and the one that matters most: after the holder releases, the
    // NEXT sequential run must succeed.
    match acquire_invocation_lock(&sandbox, "self-test", "0000000") {
        LockOutcome::Acquired(l) => {
            lock_accept += 1;
            drop(l);
        }
        LockOutcome::Busy { detail, epilogue } => {
            return Err(format!(
                "lock: a SEQUENTIAL re-claim must succeed, got refusal: detail={detail:?} epilogue={epilogue:?}"
            ))
        }
        LockOutcome::SafetyRefusal(e) => {
            return Err(format!("lock: sequential safety guard refused: {e}"))
        }
        LockOutcome::Unavailable(e) => return Err(format!("lock: sandbox unusable: {e}")),
    }

    // ---- registry census: live vs stale vs CPU-active ----
    let reg = sandbox.join("runs");
    std::fs::create_dir_all(&reg).map_err(|e| format!("registry bracket: {e}"))?;
    let mut prev: BTreeMap<i32, f64> = BTreeMap::new();
    // A STALE record: a plausible file whose owner never held the lock. The
    // census must reap it rather than counting a peer that does not exist -
    // this is the exact fiction that put `concurrent_validates: 20` in the ledger.
    std::fs::write(reg.join("4194302.run"), "pid=4194302\n").map_err(|e| format!("{e}"))?;
    let c = census_peers(&reg, std::process::id() as i32, &mut prev);
    if c.live != 0 || c.stale_reaped != 1 || reg.join("4194302.run").exists() {
        return Err(format!("registry: a dead owner's record must be reaped, got {c:?}"));
    }
    // A LIVE record: registered by this process, then observed from a census that
    // does NOT exclude us, so the liveness path is exercised for real.
    let held = register_run(&reg, "self-test", &sandbox)
        .map_err(|error| format!("registry: registering a free slot must succeed: {error}"))?;
    let lock_error = match register_run(&reg, "self-test", &sandbox) {
        Ok(_) => return Err("registry: a second registration for the held record succeeded".into()),
        Err(error) => error,
    };
    if !lock_error.contains("cannot lock live-run record") {
        return Err(format!(
            "registry: a held record named the wrong registration failure: {lock_error}"
        ));
    }

    // Deterministically force the create-before-flock interleaving: census sees
    // the just-created path as unlocked and reaps it before registration claims
    // the flock. Registration must then refuse the unlinked descriptor rather
    // than return success with a record no peer can discover.
    let race_reg = sandbox.join("create-before-flock-runs");
    let mut race_census = None;
    let race_error = match register_run_with_hooks(
        &race_reg,
        "self-test",
        &sandbox,
        |_path| {
            let mut race_prev = BTreeMap::new();
            race_census = Some(census_peers(&race_reg, -1, &mut race_prev));
        },
        |file, contents| file.write_all(contents),
    ) {
        Ok(_) => {
            return Err(
                "registry: create-before-flock race returned an undiscoverable registration".into(),
            );
        }
        Err(error) => error,
    };
    if race_census != Some(PeerCensus { live: 0, cpu_active: 0, stale_reaped: 1 })
        || !race_error.contains("no longer published")
    {
        return Err(format!(
            "registry: create-before-flock race was not detected: census={race_census:?} \
             error={race_error}"
        ));
    }
    let c = census_peers(&reg, -1, &mut prev);
    if c.live != 1 || c.stale_reaped != 0 {
        return Err(format!("registry: a live holder must be counted live, got {c:?}"));
    }
    // First sighting has no previous sample, so it can never be "active" yet:
    // activity requires an OBSERVED CPU delta, which is what stops a parked
    // fixture from counting like a 22-core validate.
    if c.cpu_active != 0 {
        return Err(format!("registry: a first sighting cannot be CPU-active, got {c:?}"));
    }
    // Now burn measurable CPU and re-census: the same peer must flip to active.
    let mut spin = 0u64;
    let t0 = std::time::Instant::now();
    while t0.elapsed().as_millis() < 150 {
        spin = spin.wrapping_add(1);
    }
    std::hint::black_box(spin);
    let c2 = census_peers(&reg, -1, &mut prev);
    if c2.live != 1 || c2.cpu_active != 1 {
        return Err(format!("registry: a CPU-burning peer must read active, got {c2:?}"));
    }
    drop(held);
    // And once it is gone the count returns to zero: the guard is not sticky.
    let c3 = census_peers(&reg, -1, &mut prev);
    if c3.live != 0 {
        return Err(format!("registry: a finished peer must stop counting, got {c3:?}"));
    }

    // A write failure is not a missing peer or a zero-peer observation. Inject
    // one at the write boundary and require the registration error to survive to
    // the caller rather than being converted to absence.
    let write_failure_reg = sandbox.join("write-failure-runs");
    let write_error = match register_run_with_hooks(
        &write_failure_reg,
        "self-test",
        &sandbox,
        |_| {},
        |_file, _contents| Err(io::Error::new(io::ErrorKind::WriteZero, "planted write refusal")),
    ) {
        Ok(_) => return Err("registry: a failed record write registered successfully".into()),
        Err(error) => error,
    };
    if !write_error.contains("cannot write live-run record") {
        return Err(format!(
            "registry: a failed record write named the wrong registration stage: {write_error}"
        ));
    }
    if write_failure_reg.join(format!("{}.run", std::process::id())).exists() {
        return Err("registry: a failed record write left a partial record behind".into());
    }
    let _ = std::fs::remove_dir_all(&sandbox);

    Ok(format!(
        "runtime: one retry per cell; environmental classifier bracketed {accepted} accept / {refused} refuse \
         (incl. the proxy/VCS classes and the 5 measured build-tool phrasings, one of them \
         spanning a cargo Caused-by block), node-detail extraction 1 hit / 2 miss (incl. partial), \
         retry verdict 1 confirmed / 1 refuted / 1 unconfirmed with all 3 refuted shapes, \
         CPU-vs-wall hints \
         2 fire / 2 silent, nesting 1 ancestor-accept / 3 refuse, invocation lock \
         {lock_accept} accept (incl. the sequential re-claim) / {lock_refuse} concurrent-refuse / \
         {lock_safety_refuse} safety-refuse, \
         registry registration 1 success / 1 lock-refuse / 1 write-refuse / 1 \
         create-before-flock race-refuse, census 1 live / 1 stale-reaped / 1 cpu-active"
    ))
}
