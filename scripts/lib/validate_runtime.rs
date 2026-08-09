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
use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;

// ------------------------------------------------------------------ environmental blocks

/// Retry budget for an environmentally-blocked gate (`VALIDATE_ENV_BLOCK_RETRIES`,
/// validate.sh:903). Two retries means up to three attempts.
pub const ENV_BLOCK_RETRIES_DEFAULT: usize = 2;

/// Resolve the environmental-block retry budget from the environment.
pub fn env_block_max_retries() -> usize {
    match std::env::var("VALIDATE_ENV_BLOCK_RETRIES") {
        Ok(v) if !v.is_empty() => v.parse().unwrap_or(ENV_BLOCK_RETRIES_DEFAULT),
        _ => ENV_BLOCK_RETRIES_DEFAULT,
    }
}

/// Phrases that mean "the kernel/sandbox said no", in lowercase.
const DENIALS: &[&str] = &["operation not permitted", "permission denied", "(os error 1)"];

fn has_denial(line: &str) -> bool {
    DENIALS.iter().any(|d| line.contains(d))
}

fn has_any(line: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| line.contains(n))
}

/// Classify a failed gate's output as an ENVIRONMENTAL block, naming the class.
///
/// Returns `None` for a genuine product/test failure. Misclassifying a real
/// failure as environmental is as harmful as the reverse, so every form-2
/// (banner-less) anchor is pinned to build-tool / VCS phrasing: ordinary GUEST
/// output that legitimately produces `EPERM` - `DETLOG ... madvise ... EPERM
/// (Operation not permitted)`, the `kcmp-eperm` fixture, a `context: Mount`
/// EPERM - must never trip it. The self-test brackets both directions with
/// counts.
///
/// The classes, and what each is evidenced by:
///
/// * `bpfjailer-banner` - the canonical jail banner. The agent sandbox is a
///   BPFJailer jail inherited by every descendant (`validate -> cargo ->
///   rustc/cmake/cc1/ld`), and its FS/EXEC/NET enforcers transiently deny an
///   `open`/`execve` for reasons unrelated to the code under test.
/// * `toolchain-eperm` - a raw `EPERM`/`EACCES` leaked to a build tool with NO
///   banner: `cc1 fatal error: .../stddef.h: Operation not permitted`, a CMake or
///   linker denial on a system path, `rustc error: could not write output to ...`.
///   Those files are world-readable `root:root -rw-r--r--` on this host, so a
///   *compiler* reporting it cannot open a header for a permission reason is
///   never legitimate product behaviour.
/// * `third-party-build` - the vendored DynamoRIO/elfutils build under
///   `reverie-dbi`. At `nproc=316` an unbounded dependency scan drives elfutils
///   into a concurrency-exposed `SIGABRT`; that is a HOST build flake, not a
///   Hermit defect (Hermit source is not what failed to compile).
/// * `proxy-egress` - **NEW.** Egress through `fwdproxy` failed, so a networked
///   gate could not reach GitHub. Verbatim from
///   `/tmp/hermit-validate.WUrHlJ.log` (2026-08-07 09:37): `Lookup error: git
///   ls-remote https://github.com/rrnewton/reverie.git refs/heads/main failed:
///   fatal: unable to access '...': Could not resolve proxy: fwdproxy`, which the
///   old regex did NOT match - that run recorded a PRODUCT red for the Reverie
///   pin gate (the log contains no `ENVIRONMENTAL block` line) when nothing about
///   the tree was wrong. `CONNECT tunnel failed, response 403` is the same class:
///   the proxy is a per-destination allowlist, so a 403 is an egress verdict, not
///   a Hermit result.
/// * `vcs-fs-denial` - **NEW, defence in depth.** A banner-less git FS denial,
///   e.g. a jail denying `git init`/`git config` inside
///   `check-reverie-pin.rs`'s `/tmp` fixture repository. NOTE, honestly: the one
///   occurrence found on disk (`/tmp/hermit-validate.H61gJP.log:1400-1419`,
///   `Enforcer: FS, Reason: FILE_OPEN` while running `git -C
///   /tmp/check-reverie-pin-stale-lock-... config user.name`) DID carry the
///   jail banner and WAS already classified and retried successfully
///   (`:1499 ENVIRONMENTAL block on attempt 1/3`). So this anchor is not fixing a
///   measured miss; it covers the same denial arriving without the banner, which
///   is how the FS enforcer surfaces when it denies a child that captures its own
///   stderr.
pub fn environmental_block_class(output: &str) -> Option<&'static str> {
    let lower = output.to_ascii_lowercase();
    // Form 1: the canonical jail banner, anywhere in the region.
    if lower.contains("blocked on this server based on a security policy")
        || lower.contains("bpfjailer")
        || lower.contains("enforcer: fs, reason:")
        || lower.contains("enforcer: exec, reason:")
        || lower.contains("enforcer: net, reason:")
    {
        return Some("bpfjailer-banner");
    }
    // Form 4 (checked before the per-line scan because it is a whole-region
    // signature): the vendored third-party build script.
    if (lower.contains("failed to run custom build command for") && lower.contains("reverie-dbi"))
        || (lower.contains("panicked at") && lower.contains("reverie-dbi/build.rs"))
    {
        return Some("third-party-build");
    }
    let mut vcs_hit = false;
    for line in lower.lines() {
        // Form 3 (NEW): egress through the forward proxy failed.
        if has_any(
            line,
            &[
                "could not resolve proxy",
                "could not resolve host",
                "connect tunnel failed, response 403",
                "proxy connect aborted",
                "failed to connect to fwdproxy",
            ],
        ) {
            return Some("proxy-egress");
        }
        if !has_denial(line) {
            continue;
        }
        // Form 2: a banner-less denial reported by a build tool. Anchored on
        // compiler / build-system / linker phrasing.
        if (line.contains("fatal error: ") && line.matches(':').count() >= 2)
            || line.contains("cmake error")
            || has_any(
                line,
                &[
                    "cannot open",
                    "error opening",
                    "failed to open",
                    "could not open",
                    "opening dependency file",
                    "could not write output to",
                    "couldn't create the temp file",
                    "can't create",
                    "cannot execute",
                    "could not create temporary file",
                    "failed to build archive at",
                ],
            )
        {
            return Some("toolchain-eperm");
        }
        // Form 5 (NEW): a banner-less git FS denial. Requires BOTH a git-fatal
        // shape and a denial on the same line, so a guest test that merely prints
        // "permission denied" cannot trip it.
        if (line.starts_with("fatal:") || line.contains(" fatal: ") || line.contains(".git/"))
            && has_any(
                line,
                &[
                    "cannot mkdir",
                    "could not create work tree dir",
                    "could not create leading directories",
                    "unable to create",
                    "unable to write",
                    "unable to access",
                    "could not lock",
                    "chmod on",
                    ".git/",
                ],
            )
        {
            vcs_hit = true;
        }
    }
    if vcs_hit {
        return Some("vcs-fs-denial");
    }
    None
}

/// Extract one failed DAG node's captured output from the driver's durable log.
///
/// `safe-ci-dag-runner` re-emits a failed step's combined stdout+stderr between
/// `[tag] ----- detail -----` and `[tag] ----- end detail -----`, one line per
/// prefixed line (scheduler.rs:844-849). Reading THAT region - rather than a
/// whole-log tail - is what binds the classification to the node that actually
/// failed, so a jail banner printed by an unrelated concurrent node cannot excuse
/// a genuine product red.
///
/// Returns `None` when the region is absent (log not flushed, or the node's
/// failure predates detail emission).
pub fn extract_node_detail(log: &str, tag: &str) -> Option<String> {
    let open = format!("[{tag}] ----- detail -----");
    let close = format!("[{tag}] ----- end detail -----");
    let prefix = format!("[{tag}] ");
    // The LAST region, so a retried node is classified on its newest attempt.
    let start = log.rfind(&open)? + open.len();
    let rest = &log[start..];
    let end = rest.find(&close).unwrap_or(rest.len());
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
/// runs as a child of this process through `safe-ci-dag-runner`, and the runner
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

/// Decide whether this invocation is a NESTED payload of an outer validate.
///
/// `ci/dag/portable.json`'s `test.strict_compat` node runs
/// `./validate.sh --portable-strict-compat-only`, so re-entry is a designed path,
/// not an accident. What must never happen is a full driver inside a full driver:
/// that pays the entire preamble twice, appends a SECOND ledger row, and can
/// publish a SECOND receipt for one logical run.
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
    holder: PathBuf,
}

impl Drop for InvocationLock {
    fn drop(&mut self) {
        // Remove the descriptive record; the LOCK itself is released by the
        // kernel when the fd closes, which is the whole point of using flock.
        let _ = std::fs::remove_file(&self.holder);
    }
}

/// Outcome of trying to claim the per-checkout validate slot.
pub enum LockOutcome {
    /// Claimed. Hold the value for the lifetime of the run.
    Acquired(InvocationLock),
    /// Another validate holds it. The lines are the typed refusal message.
    Busy(Vec<String>),
    /// The lock could not be created at all (unwritable `target/`); the caller
    /// proceeds, because refusing every run over a lock-file hiccup would be a
    /// worse outage than the concurrency it guards.
    Unavailable(String),
}

fn flock_nb(fd: i32) -> bool {
    unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) == 0 }
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
pub fn acquire_invocation_lock(root: &Path, profile: &str, commit: &str) -> LockOutcome {
    let dir = root.join("target/validation");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return LockOutcome::Unavailable(format!("cannot create {}: {e}", dir.display()));
    }
    let lock_path = dir.join("validate-invocation.lock");
    let holder = dir.join("validate-invocation.holder");
    let file = match std::fs::OpenOptions::new().create(true).append(true).open(&lock_path) {
        Ok(f) => f,
        Err(e) => {
            return LockOutcome::Unavailable(format!("cannot open {}: {e}", lock_path.display()))
        }
    };
    if !flock_nb(file.as_raw_fd()) {
        let mut msg = vec![
            "another validate is already running in THIS checkout".to_string(),
            format!("checkout: {}", root.display()),
        ];
        let record = std::fs::read_to_string(&holder).unwrap_or_default();
        let holder_pid = record
            .lines()
            .find_map(|l| l.strip_prefix("pid="))
            .and_then(|v| v.trim().parse::<i32>().ok());
        match holder_pid {
            Some(pid) if unsafe { libc::kill(pid, 0) } == 0 => {
                msg.push(format!("holder (pid {pid} is LIVE):"));
                msg.extend(record.lines().map(|l| format!("  {l}")));
            }
            Some(pid) => {
                msg.push(format!(
                    "holder: the lock IS held, but the recorded pid {pid} is NOT alive, so this \
                     record is STALE and does not describe the current holder:"
                ));
                msg.extend(record.lines().map(|l| format!("  {l}")));
            }
            None => msg.push("holder: (lock held, but no holder record was readable)".into()),
        }
        msg.push(
            "this is a refusal, not a wait: two validates in one checkout share target/ and the \
             ledger, and would corrupt each other's results"
                .into(),
        );
        msg.push("wait for the holder to finish, or run in a different checkout".into());
        return LockOutcome::Busy(msg);
    }
    let record = format!(
        "pid={}\nstarted_at={}\ncommit={commit}\nprofile={profile}\ncheckout={}\n",
        std::process::id(),
        crate::utc_now(),
        root.display()
    );
    let _ = std::fs::write(&holder, record);
    LockOutcome::Acquired(InvocationLock { _file: file, holder })
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
pub fn register_run(dir: &Path, profile: &str, checkout: &Path) -> Option<RunRecord> {
    std::fs::create_dir_all(dir).ok()?;
    let pid = std::process::id();
    let path = dir.join(format!("{pid}.run"));
    let file = std::fs::OpenOptions::new().create(true).append(true).open(&path).ok()?;
    if !flock_nb(file.as_raw_fd()) {
        return None;
    }
    let _ = std::fs::write(
        &path,
        format!("pid={pid}\nprofile={profile}\ncheckout={}\n", checkout.display()),
    );
    Some(RunRecord { _file: file, path })
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
        "",
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
    {
        let first = match acquire_invocation_lock(&sandbox, "self-test", "0000000") {
            LockOutcome::Acquired(l) => {
                lock_accept += 1;
                l
            }
            LockOutcome::Busy(m) => return Err(format!("lock: a free slot must be granted: {m:?}")),
            LockOutcome::Unavailable(e) => return Err(format!("lock: sandbox unusable: {e}")),
        };
        // NEGATIVE: a concurrent claim, from a real second fd, must be REFUSED and
        // must name the live holder.
        match acquire_invocation_lock(&sandbox, "self-test", "0000000") {
            LockOutcome::Busy(msg) => {
                lock_refuse += 1;
                let joined = msg.join("\n");
                if !joined.contains("is LIVE") || !joined.contains(&std::process::id().to_string()) {
                    return Err(format!("lock: refusal must name the LIVE holder pid: {joined}"));
                }
            }
            _ => return Err("lock: a second concurrent claim MUST be refused".into()),
        }
        drop(first);
    }
    // POSITIVE, and the one that matters most: after the holder releases, the
    // NEXT sequential run must succeed.
    match acquire_invocation_lock(&sandbox, "self-test", "0000000") {
        LockOutcome::Acquired(l) => {
            lock_accept += 1;
            drop(l);
        }
        LockOutcome::Busy(m) => {
            return Err(format!("lock: a SEQUENTIAL re-claim must succeed, got refusal: {m:?}"))
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
        .ok_or("registry: registering a free slot must succeed")?;
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
    let _ = std::fs::remove_dir_all(&sandbox);

    Ok(format!(
        "runtime: environmental classifier bracketed {accepted} accept / {refused} refuse \
         (incl. the 2 NEW classes), node-detail extraction 1 hit / 1 miss, CPU-vs-wall hints \
         2 fire / 2 silent, nesting 1 ancestor-accept / 3 refuse, invocation lock \
         {lock_accept} accept (incl. the sequential re-claim) / {lock_refuse} concurrent-refuse, \
         registry census 1 live / 1 stale-reaped / 1 cpu-active"
    ))
}
