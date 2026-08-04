#!/usr/bin/env rust-script
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! validate.rs — PHASE 1 thin, typed wrapper that drives the CI validation
//! lanes by calling `safe-ci-dag-runner` **as a library** (an in-process typed
//! call, NOT a subprocess).
//!
//! # What this is (and is not) yet
//!
//! The long-term goal is to DELETE the 4000-line `validate.sh` bash orchestrator
//! and replace it with a thin Rust entrypoint over the DAG runner. That full
//! migration — porting the ~60 non-DAG gates and repointing `make validate` — is
//! PHASE 2. This file is PHASE 1: an ADDITIVE new entrypoint that runs the
//! DAG-lane profiles (`ci/dag/<profile>.json`) through the typed library and
//! establishes the ledger / boxing / typed-classification foundation. It does
//! not delete `validate.sh` and does not yet repoint `make validate`.
//!
//! # Why a library call, not a subprocess
//!
//! Calling `run_dag_boxed_ordered` directly gives us TYPED results
//! (`RunResult`/`StepOutcome`) instead of scraped text. Every decision this
//! wrapper makes — process exit code, per-node cost table, ledger `result`, and
//! failure classification — is derived STRUCTURALLY from typed fields
//! (`RunResult.ok`, `StepOutcome.ok`/`returncode`/`reason`/`aborted`). We never
//! grep stdout to decide anything. `StepOutcome.reason` is produced by the
//! library's own `step_failure_reason`, which already classifies
//! oom/timeout/cpu_timeout/signal, so classification is not re-implemented here.
//!
//! # Boxing is the primary purpose — fail closed by default
//!
//! cgroup-v2 two-level boxing is the reason the DAG runner exists. This wrapper
//! reproduces the library's own `resolve_cgroups` policy exactly: by DEFAULT it
//! re-execs into a transient `systemd --user` scope (the "systemd --user scope
//! producer path") and, if boxing still cannot be established, exits 3. Passing
//! `--allow-cgroup-failure` downgrades to an UNBOXED run with a loud warning.
//! This is not a bypass; it is the same fail-closed contract the CLI enforces.
//!
//! # Ledger schema-transition design constraint — VERSION-AWARE ACCEPTANCE
//!
//! The ledger PRODUCER travels with the branch: an in-flight PR carries its own
//! (possibly older) copy of this file, so a PR emits records in ITS producer's
//! schema, not whatever `main` currently writes. As of this writing 57 of 74
//! open PRs predate `bfb0a9ef` and therefore emit an OLDER schema. A consumer
//! that hard-rejects an older-but-valid version breaks every one of them at once
//! — which is exactly the live incident this design must prevent: a consumer
//! tightened AHEAD of its producers and began rejecting 254 of 255 ledger rows
//! fleet-wide, forcing a hermit-validate pause. Tightening a reader before the
//! producers emit the newly-required shape is the same failure mode as deleting
//! a producer before its replacement covers every gate.
//!
//! The durable cure is VERSION-AWARE ACCEPTANCE (chosen over a time-boxed grace
//! period or a forced fleet-wide rebase, because only version-awareness survives
//! a THIRD tightening). Its contract, which any future bump MUST preserve:
//!
//!   1. THE WRITER STAMPS A SCHEMA VERSION and ALWAYS emits the required
//!      selection-accounting fields (`schema_version` + `executed_tests` +
//!      `filtered_tests` + `profile`) with REAL values on every run. A record is
//!      never emitted with these fields omitted or zero-filled.
//!   2. THE READER ACCEPTS OLDER VALID VERSIONS instead of hard-rejecting them:
//!      it dispatches on `schema_version`, reads every field via a
//!      get-with-default, and treats an older-but-valid record as valid.
//!   3. DEFINED DEFAULT/DERIVATION FOR EACH NEW REQUIRED FIELD. Any field a new
//!      schema treats as required must have a well-defined value for records an
//!      OLDER producer wrote without it (a static default or a derivation from
//!      fields that already exist). A bump that cannot supply such a default
//!      would retroactively invalidate green receipts from open PRs and is
//!      therefore disallowed.
//!
//! Concretely: this producer writes `schema_version: 3` and ALWAYS emits the
//! three selection-accounting fields `profile`, `executed_tests`, and
//! `filtered_tests` (plus `commit`/`commit_anchored`/`tree_dirty` for commit
//! anchoring). Because the qualification travels WITH the value (all three
//! written at the single ledger-write point below), a downstream reader can never
//! pair a bare `pass` with inferred coverage.
//!
//! ## What `executed_tests` / `filtered_tests` MEAN for a DAG-lane run
//!
//! The unit of execution in a DAG lane is the NODE (gate) — each node runs one
//! command (a build, a `cargo test` target, a harness). The typed `RunResult`
//! exposes NODE outcomes and resource metrics, not individual cargo-test-case
//! counts (the runner surfaces only the last output line as `summary`, not a
//! parsed per-test count). So, consistent with how `validate.sh` itself counts
//! gates rather than test cases, this producer binds:
//!   * `executed_tests`  = number of gates that actually RAN (`outcomes.len()`),
//!   * `filtered_tests`  = number of gates SKIPPED because a dependency failed
//!                         (`skipped.len()`; a full green run has zero).
//! These are genuine counts from typed fields, never fabricated or zero-filled.
//! A run that executed zero gates, or that skipped gates (incomplete coverage),
//! yields a record the consumer correctly declines to treat as a full green.
//!
//! # Usage
//!
//! ```text
//! ./scripts/validate.rs <profile> [-j N] [-v] [--allow-cgroup-failure]
//!                       [--perf-dir DIR] [-k|--keep-going] [--dag-file PATH]
//! ```
//!
//! `<profile>` selects `ci/dag/<profile>.json` (portable | privileged, or any
//! other `ci/dag/*.json` present). `--dag-file PATH` (or the `RUN_DAG_FILE_OVERRIDE`
//! env, mirroring `ci/run-dag.sh`) runs an exact DAG file instead, keeping the
//! profile label for the ledger.
//!
//! ```cargo
//! [dependencies]
//! safe-ci-dag-runner = { path = "../agent-utils/rs/safe-ci-dag-runner" }
//! serde_json = "1"
//! ```

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;
use std::sync::Arc;

use safe_ci_dag_runner::cgroup::install_scope_teardown;
use safe_ci_dag_runner::cgroup::is_in_scope;
use safe_ci_dag_runner::cgroup::reexec_in_scope;
use safe_ci_dag_runner::cgroup::CgroupManager;
use safe_ci_dag_runner::cgroup::Cgroups;
use safe_ci_dag_runner::io::dag_from_json;
use safe_ci_dag_runner::model::RunResult;
use safe_ci_dag_runner::model::StepOutcome;
use safe_ci_dag_runner::perflog::append_step_profiles;
use safe_ci_dag_runner::scheduler::run_dag_boxed_ordered;
use safe_ci_dag_runner::scheduler::BoxedCgroups;

/// Ledger schema this producer emits. See the schema-transition constraint in
/// the module doc comment before changing this.
const LEDGER_SCHEMA_VERSION: i64 = 3;

/// Producer identity recorded in each ledger row, so a backward-tolerant reader
/// can tell a validate.rs receipt from a validate.sh one without inference.
const LEDGER_PRODUCER: &str = "validate.rs";

/// Env var that names an explicit ledger file (highest precedence). Matches the
/// override `validate.sh` honors so both producers can share one ledger.
const LEDGER_ENV: &str = "HERMIT_VALIDATE_LEDGER";

/// Env var naming the dev-hermit parent workspace (second precedence).
const PARENT_ENV: &str = "DEV_HERMIT_PARENT";

/// Checkout-local default ledger file (third precedence). This is the landmine
/// fix: a STANDALONE checkout with neither env set previously produced no
/// receipt at all; now it always writes here so a green claim has evidence.
const LOCAL_LEDGER_BASENAME: &str = ".hermit-validate-ledger.jsonl";

/// Env override for an exact DAG file, mirroring `ci/run-dag.sh`.
const DAG_FILE_OVERRIDE_ENV: &str = "RUN_DAG_FILE_OVERRIDE";

/// Profile-store dir env, mirroring the runner's own default resolution.
const PROFILE_DIR_ENV: &str = "SAFE_CI_DAG_RUNNER_PROFILE_DIR";

// --------------------------------------------------------------------------- args

struct Args {
    profile: String,
    dag_file: Option<String>,
    jobs: Option<i64>,
    verbosity: i64,
    keep_going: bool,
    allow_cgroup_failure: bool,
    perf_dir: Option<String>,
}

fn usage() -> &'static str {
    "usage: validate.rs <profile> [options]\n\
     \n\
     PHASE 1 typed wrapper: run a CI validation lane as a safe-ci-dag-runner DAG,\n\
     in-process (library call, not a subprocess), boxed by default.\n\
     \n\
     <profile>                selects ci/dag/<profile>.json (e.g. portable, privileged)\n\
     -j N                     scheduler width (default: host_cpus/8, floor 2, cap 16)\n\
     -v                       increase verbosity (repeatable)\n\
     -k, --keep-going         do not eager-exit on the first failure\n\
     --allow-cgroup-failure   downgrade to an UNBOXED run instead of failing closed\n\
     --perf-dir DIR           forward per-step profile rows to DIR\n\
     --dag-file PATH          run this exact DAG file (keeps <profile> as the label);\n\
     \x20                        also settable via RUN_DAG_FILE_OVERRIDE\n\
     -h, --help               print this help and exit"
}

/// Parse argv. Returns `Err(code)` for a usage error (2) or a handled `--help` (0).
fn parse_args() -> Result<Args, u8> {
    let mut profile: Option<String> = None;
    let mut dag_file: Option<String> = std::env::var(DAG_FILE_OVERRIDE_ENV).ok().filter(|s| !s.is_empty());
    let mut jobs: Option<i64> = None;
    let mut verbosity: i64 = 0;
    let mut keep_going = false;
    let mut allow_cgroup_failure = false;
    let mut perf_dir: Option<String> = None;

    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let a = &argv[i];
        match a.as_str() {
            "-h" | "--help" => {
                println!("{}", usage());
                return Err(0);
            }
            "-v" => verbosity += 1,
            "-k" | "--keep-going" => keep_going = true,
            "--allow-cgroup-failure" => allow_cgroup_failure = true,
            "-j" => {
                i += 1;
                let v = argv.get(i).ok_or_else(|| {
                    eprintln!("validate.rs: -j requires an argument");
                    2u8
                })?;
                jobs = Some(v.parse::<i64>().map_err(|_| {
                    eprintln!("validate.rs: -j argument must be an integer, got {v:?}");
                    2u8
                })?);
            }
            "--perf-dir" => {
                i += 1;
                perf_dir = Some(
                    argv.get(i)
                        .ok_or_else(|| {
                            eprintln!("validate.rs: --perf-dir requires an argument");
                            2u8
                        })?
                        .clone(),
                );
            }
            "--dag-file" => {
                i += 1;
                dag_file = Some(
                    argv.get(i)
                        .ok_or_else(|| {
                            eprintln!("validate.rs: --dag-file requires an argument");
                            2u8
                        })?
                        .clone(),
                );
            }
            other if other.starts_with('-') => {
                eprintln!("validate.rs: unknown option {other:?}");
                eprintln!("{}", usage());
                return Err(2);
            }
            other => {
                if profile.is_some() {
                    eprintln!("validate.rs: unexpected extra positional argument {other:?}");
                    return Err(2);
                }
                profile = Some(other.to_string());
            }
        }
        i += 1;
    }

    let profile = profile.ok_or_else(|| {
        eprintln!("validate.rs: missing required <profile> argument");
        eprintln!("{}", usage());
        2u8
    })?;

    Ok(Args {
        profile,
        dag_file,
        jobs,
        verbosity,
        keep_going,
        allow_cgroup_failure,
        perf_dir,
    })
}

// --------------------------------------------------------------------------- jobs default

/// SINGLE SOURCE OF TRUTH for the default scheduler width. Host-adaptive:
/// `host_cpus/8`, floored at 2, capped at 16 — the exact rule validate.sh uses
/// (validate.sh:485-487). Called from exactly one site (main), only when `-j`
/// was not supplied.
fn default_jobs() -> i64 {
    let host_cpus = std::thread::available_parallelism()
        .map(|n| n.get() as i64)
        .unwrap_or(1);
    (host_cpus / 8).clamp(2, 16)
}

// --------------------------------------------------------------------------- boxing

/// Establish the two-level cgroup-v2 boxing that is the runner's PRIMARY purpose,
/// mirroring the library's private `cli::resolve_cgroups`. Returns the manager to
/// use (`None` = intentional UNBOXED run) or `Err(exit_code)` the caller returns.
/// On the default path this re-execs into a transient `systemd --user` scope and
/// never returns on success.
fn resolve_cgroups(allow_failure: bool) -> Result<BoxedCgroups, u8> {
    if is_in_scope() {
        let mgr = Cgroups::new();
        if mgr.enabled() {
            install_scope_teardown();
            eprintln!(
                "validate.rs: cgroup boxing ACTIVE (two-level cgroup-v2 scope; per-step \
                 memory/CPU caps + setsid-proof teardown)."
            );
            return Ok(Some(Arc::new(mgr) as Arc<dyn CgroupManager>));
        }
        if allow_failure {
            eprintln!(
                "validate.rs: warning: inside a scope but per-step cgroup setup failed; \
                 running best-effort UNBOXED (--allow-cgroup-failure)."
            );
            return Ok(None);
        }
        eprintln!(
            "validate.rs: ERROR: inside a managed scope but per-step cgroups could not be \
             set up; re-run with --allow-cgroup-failure to run UNBOXED."
        );
        return Err(3);
    }
    if allow_failure {
        eprintln!(
            "validate.rs: warning: cgroup boxing not established (--allow-cgroup-failure); \
             running UNBOXED (process-group teardown only, no per-step memory/CPU caps)."
        );
        return Ok(None);
    }
    // Default: boxing is required -> re-exec into a transient systemd --user scope.
    // On success this never returns (exec replaces the process); a return means
    // boxing is unavailable.
    let reexeced_or_skipped = reexec_in_scope(None, None);
    let detail = if reexeced_or_skipped {
        "boxing was skipped (e.g. CI without a systemd --user scope)"
    } else {
        "cgroup-v2 + a working systemd --user scope are unavailable"
    };
    eprintln!(
        "validate.rs: ERROR: cgroup boxing could not be established: {detail}. Cgroup \
         resource boxing is this tool's primary purpose; re-run with --allow-cgroup-failure \
         to run UNBOXED."
    );
    Err(3)
}

// --------------------------------------------------------------------------- git / ledger

fn git_sha() -> String {
    match Command::new("git").args(["rev-parse", "HEAD"]).output() {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() {
                "unknown".to_string()
            } else {
                s
            }
        }
        _ => "unknown".to_string(),
    }
}

/// True when the working tree differs from HEAD in ANY way (porcelain non-empty).
/// Drives commit anchoring: a record is only faithfully attributable to a SHA
/// when the tree exactly matches that HEAD.
fn tree_dirty() -> bool {
    match Command::new("git").args(["status", "--porcelain"]).output() {
        Ok(o) if o.status.success() => !String::from_utf8_lossy(&o.stdout).trim().is_empty(),
        // Outside a git repo or on error: not dirty, just "not anchored".
        _ => false,
    }
}

/// Repo root via `git rev-parse --show-toplevel`, so profile/DAG paths resolve
/// no matter the caller's cwd. Falls back to the current dir.
fn repo_root() -> PathBuf {
    match Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
    {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !s.is_empty() {
                return PathBuf::from(s);
            }
        }
        _ => {}
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Resolve the ledger file with a defined precedence that is NEVER empty (the
/// standalone-checkout landmine fix):
///   1. `$HERMIT_VALIDATE_LEDGER` — explicit file.
///   2. `$DEV_HERMIT_PARENT/ignored/validate-run-ledger.jsonl` — parent workspace.
///   3. `<repo_root>/.hermit-validate-ledger.jsonl` — checkout-local default.
fn ledger_path(root: &Path) -> PathBuf {
    if let Ok(explicit) = std::env::var(LEDGER_ENV) {
        if !explicit.is_empty() {
            return PathBuf::from(explicit);
        }
    }
    if let Ok(parent) = std::env::var(PARENT_ENV) {
        if !parent.is_empty() {
            return PathBuf::from(parent)
                .join("ignored")
                .join("validate-run-ledger.jsonl");
        }
    }
    root.join(LOCAL_LEDGER_BASENAME)
}

fn utc_now() -> String {
    match Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => "unknown".to_string(),
    }
}

/// Write one JSONL ledger record. Every qualification (profile/executed_tests/
/// filtered_tests, commit anchoring, per-gate reason) is written HERE, at the
/// single ledger-write point, so no downstream reader can pair a bare `pass`
/// with inferred coverage.
#[allow(clippy::too_many_arguments)]
fn write_ledger_record(
    ledger: &Path,
    started_at: &str,
    finished_at: &str,
    profile: &str,
    result: &RunResult,
    exit_code: u8,
    commit: &str,
    tree_is_dirty: bool,
    selection_mode: &str,
) {
    // DAG-lane semantics: the gate (node) is the unit of execution. See the
    // module doc comment. executed_tests = gates that ran; filtered_tests = gates
    // skipped because a dependency failed. Both are genuine typed counts.
    let executed_tests = result.outcomes.len();
    let filtered_tests = result.skipped.len();
    // Genuine, non-aborted failures — the honest failure count.
    let failures = result
        .outcomes
        .iter()
        .filter(|o| !o.ok && !o.aborted)
        .count();
    let commit_anchored = commit != "unknown" && !tree_is_dirty;
    let overall = if result.ok && failures == 0 {
        "pass"
    } else {
        "fail"
    };

    let host = Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    let gates: Vec<serde_json::Value> = result
        .outcomes
        .iter()
        .map(|o| {
            serde_json::json!({
                "name": o.tag,
                "result": if o.ok { "pass" } else { "fail" },
                "returncode": o.returncode,
                "reason": o.reason,
                "aborted": o.aborted,
                "real_seconds": o.duration_s,
            })
        })
        .collect();

    let record = serde_json::json!({
        "schema_version": LEDGER_SCHEMA_VERSION,
        "producer": LEDGER_PRODUCER,
        "started_at": started_at,
        "finished_at": finished_at,
        "host": host,
        // Selection accounting — the three fields that must travel WITH the value.
        // ALWAYS emitted with real values so a version-aware consumer never has to
        // reject this record for a missing/zero-filled count (see doc comment).
        "profile": profile,
        "executed_tests": executed_tests,
        "filtered_tests": filtered_tests,
        "selection_mode": selection_mode,
        // Commit anchoring.
        "commit": commit,
        "commit_anchored": commit_anchored,
        "tree_dirty": tree_is_dirty,
        // Verdict.
        "result": overall,
        "exit_code": exit_code,
        "checks": executed_tests,
        "failures": failures,
        "real_seconds": result.wall_s,
        "gates": gates,
    });

    if let Some(dir) = ledger.parent() {
        if !dir.as_os_str().is_empty() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                eprintln!(
                    "validate.rs: warning: could not create ledger dir {}: {e}",
                    dir.display()
                );
                return;
            }
        }
    }

    use std::io::Write;
    let line = format!("{}\n", serde_json::to_string(&record).unwrap());
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(ledger)
    {
        Ok(mut f) => {
            if let Err(e) = f.write_all(line.as_bytes()) {
                eprintln!(
                    "validate.rs: warning: could not append ledger {}: {e}",
                    ledger.display()
                );
            } else {
                eprintln!("validate.rs: ledger record appended to {}", ledger.display());
            }
        }
        Err(e) => eprintln!(
            "validate.rs: warning: could not open ledger {}: {e}",
            ledger.display()
        ),
    }
}

// --------------------------------------------------------------------------- reporting

/// The headline feature: a readable per-node cost table built entirely from typed
/// `StepOutcome` fields (never scraped text).
fn print_cost_table(outcomes: &[StepOutcome], skipped: &[String]) {
    println!("\n=== per-node cost (safe-ci-dag-runner) ===");
    println!("{:<40} {:>10}  {:<8} {}", "node", "seconds", "status", "reason/returncode");
    println!("{}", "-".repeat(80));
    let mut total = 0.0_f64;
    for o in outcomes {
        total += o.duration_s;
        let status = if o.ok {
            "ok"
        } else if o.aborted {
            "ABORTED"
        } else {
            "FAIL"
        };
        // Prefer the library-derived reason; fall back to the typed returncode.
        let detail = if !o.reason.is_empty() {
            o.reason.clone()
        } else if let Some(rc) = o.returncode {
            if rc < 0 {
                format!("signal {}", -rc)
            } else {
                format!("rc {rc}")
            }
        } else {
            String::new()
        };
        println!("{:<40} {:>10.2}  {:<8} {}", o.tag, o.duration_s, status, detail);
    }
    println!("{}", "-".repeat(80));
    println!("{:<40} {:>10.2}  (sum of node wall)", "TOTAL", total);
    if !skipped.is_empty() {
        println!(
            "\nskipped (dependency failed, never ran): {}",
            skipped.join(", ")
        );
    }
}

// --------------------------------------------------------------------------- main

fn main() -> ExitCode {
    // FIRST thing, before any output: tolerate a downstream reader closing the
    // pipe early (the typed cure for the SIGPIPE-text-grep landmine).
    rust_script_prelude::init();

    let args = match parse_args() {
        Ok(a) => a,
        Err(code) => return ExitCode::from(code),
    };

    let root = repo_root();

    // Resolve the DAG file: explicit --dag-file / RUN_DAG_FILE_OVERRIDE wins,
    // else ci/dag/<profile>.json.
    let dag_path: PathBuf = match &args.dag_file {
        Some(p) => PathBuf::from(p),
        None => root.join("ci").join("dag").join(format!("{}.json", args.profile)),
    };
    if !dag_path.is_file() {
        eprintln!(
            "validate.rs: no such DAG file: {} (profile {:?})",
            dag_path.display(),
            args.profile
        );
        return ExitCode::from(2);
    }
    let dag_text = match std::fs::read_to_string(&dag_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("validate.rs: cannot read {}: {e}", dag_path.display());
            return ExitCode::from(2);
        }
    };
    let cfg = match dag_from_json(&dag_text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("validate.rs: invalid DAG {}: {e}", dag_path.display());
            return ExitCode::from(2);
        }
    };

    // Fail-closed boxing. On the default path this re-execs into a transient
    // systemd --user scope and never returns on success; the code below runs in
    // the boxed re-exec.
    let cgroups: BoxedCgroups = match resolve_cgroups(args.allow_cgroup_failure) {
        Ok(c) => c,
        Err(code) => return ExitCode::from(code),
    };

    let jobs = args.jobs.unwrap_or_else(default_jobs);
    let started_at = utc_now();
    eprintln!(
        "validate.rs: running profile {:?} (DAG {}) at -j {jobs}",
        args.profile,
        dag_path.display()
    );

    let result = run_dag_boxed_ordered(
        &cfg,
        jobs,
        args.keep_going,
        args.verbosity,
        cgroups,
        None,
        None,
    );

    let finished_at = utc_now();

    // Structural verdict — never text-grep. `RunResult.ok` is the library's own
    // "no genuine, non-aborted failure occurred" verdict.
    let failures = result
        .outcomes
        .iter()
        .filter(|o| !o.ok && !o.aborted)
        .count();
    let exit_code: u8 = if result.ok && failures == 0 { 0 } else { 1 };

    print_cost_table(&result.outcomes, &result.skipped);

    // Forward per-step profile rows only when a profile dir is configured
    // (--perf-dir or the env), mirroring the runner's own opt-in.
    let profile_dir = args
        .perf_dir
        .clone()
        .or_else(|| std::env::var(PROFILE_DIR_ENV).ok().filter(|s| !s.is_empty()));
    if let Some(dir) = profile_dir {
        let sha = git_sha();
        append_step_profiles(
            Path::new(&dir),
            &result.step_profile_rows,
            &sha,
            jobs,
            None,
            "unverified",
            LEDGER_PRODUCER,
        );
        eprintln!("validate.rs: forwarded {} step profile row(s) to {dir}", result.step_profile_rows.len());
    }

    // Ledger — always writes (checkout-local default when no env override), at
    // the single write point that carries every qualification with the value.
    let commit = git_sha();
    let dirty = tree_dirty();
    let selection_mode = if args.dag_file.is_some() { "override" } else { "full" };
    write_ledger_record(
        &ledger_path(&root),
        &started_at,
        &finished_at,
        &args.profile,
        &result,
        exit_code,
        &commit,
        dirty,
        selection_mode,
    );

    let verdict = if exit_code == 0 { "PASS" } else { "FAIL" };
    eprintln!(
        "validate.rs: {verdict} - {} executed, {failures} failed, {} skipped in {:.1}s",
        result.outcomes.len(),
        result.skipped.len(),
        result.wall_s
    );

    ExitCode::from(exit_code)
}
