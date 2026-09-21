#!/usr/bin/env -S rust-script --force
//! ```cargo
//! [dependencies]
//! detcore = { package = "hermit-detcore", path = "../detcore" }
//! ```
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Run one guest under two backends and report the first canonical INFO
//! divergence.
//!
//! This is a diagnostic comparison of one run from each execution path. It does
//! not establish L2, full parity, or repeat determinism: those require
//! `hermit run --strict --verify --verify-strict --verify-json ...` for each
//! in-scope backend. In particular, matching guest output is not canonical log
//! parity.
//!
//! **The first-divergence report is the point.** A boolean ("these backends
//! differ") is not actionable. "They agree for 157 records and then ptrace logs
//! `openat(...) = Ok(3)` where DBT logs `openat(...) = Ok(4)`" is a bug report.
//! The shared `detcore::logdiff` implementation prints the divergent pair with
//! context from both sides.
//!
//! ## Two things that make this harder than `hermit log-diff`
//!
//! `hermit log-diff` compares two logs. It does not produce them, and producing
//! them across backends is where the sharp edges are:
//!
//! 1. **Evidence must come from an authoritative sink.** `ptrace` honours the
//!    host-opened `--log-file`. DBT's ordinary single-run adapter deliberately
//!    refuses that option and shares stderr with the guest. SaBRe forwards its
//!    real Detcore records to raw stderr while the host `--log-file` contains
//!    only an incomplete controller stream. This tool therefore refuses both
//!    paths rather than accepting guest-forgeable or incomplete records. It
//!    never chooses a stderr stream merely because it contains more
//!    record-looking lines.
//! 2. **Normalization is where fake parity gets manufactured.** Comparison uses
//!    the fixed `BitwiseInfoV1` policy from `detcore::logdiff`: remove only the
//!    real wall-clock prefix, canonicalize explicitly marked host addresses by
//!    first appearance, and compare every other selected byte exactly. Legacy
//!    lossy normalization flags are recognized but refused before either run.
//!
//! ## Usage
//!
//! ```text
//! ./scripts/cross-backend-detlog-diff.rs --backends ptrace,ptrace -- /bin/true
//! ./scripts/cross-backend-detlog-diff.rs --backends ptrace,liteinst --detlog-heap -- ./guest arg
//! ./scripts/cross-backend-detlog-diff.rs --context 3 --backends ptrace,kvm -- ./guest
//! ./scripts/cross-backend-detlog-diff.rs --self-test
//! ./scripts/cross-backend-detlog-diff.rs --list-normalizations
//! ```
//!
//! Exit codes: `0` streams agree · `1` they diverge · `2` the harness could not
//! produce a comparable pair (a run failed, evidence was incomplete/untrusted,
//! or a backend emitted no authoritative trace).
//! Divergence is exit 1, not an error: finding one is a successful measurement.

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::env;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use detcore::detlog::DetLogEvent;
use detcore::detlog::record_suffix;
use detcore::logdiff::BitwiseInfoV1Diagnostics;
use detcore::logdiff::ComparisonSideLabels;
use detcore::logdiff::LogDiffSummary;
use detcore::logdiff::try_compare_bitwise_info_v1_bytes_with_records_and_diagnostics;
use detcore::logdiff::write_bitwise_info_v1_bytes;

/// A historical command-line normalization name and whether the fixed
/// `BitwiseInfoV1` policy applies it. Names that can erase deterministic
/// content remain parse-compatible but are refused before capture.
struct Normalization {
    name: &'static str,
    default_on: bool,
    what: &'static str,
    /// Set when enabling this can mask a real deterministic difference. Printed loudly.
    masks: Option<&'static str>,
}

const NORMALIZATIONS: &[Normalization] = &[
    Normalization {
        name: "wall-clock",
        default_on: true,
        what: "the leading real-time `2026-01-01T00:00:00.000000Z` prefix",
        masks: None,
    },
    Normalization {
        name: "host-addresses",
        default_on: true,
        what: "explicit <hostaddr 0x...> markers -> first-appearance ordinals, preserving identity and aliasing; bare hex remains exact",
        masks: None,
    },
    Normalization {
        name: "virtual-time",
        default_on: false,
        what: "virtual timestamps like 1_767_225_600.007_940_575s -> <VTIME>",
        masks: Some(
            "virtual time is a deterministic function of work done; a difference here is a REAL \
             divergence in syscall/RCB counts, not noise",
        ),
    },
    Normalization {
        name: "thread-identity",
        default_on: false,
        what: "dtid/dettid values -> first-appearance ordinals",
        masks: Some(
            "thread identities are deterministic evidence; ordinalizing them can hide a real \
             backend disagreement",
        ),
    },
];

struct Config {
    hermit: PathBuf,
    backends: Vec<String>,
    guest: Vec<String>,
    detlog_stack: bool,
    detlog_heap: bool,
    context: usize,
    normalize: Vec<String>,
    keep: Option<PathBuf>,
}

fn backend_description(backend: &str) -> Result<&'static str, String> {
    match backend {
        "ptrace" => Ok("ptrace backend"),
        "dbt" => Ok("DynamoRIO DBT backend"),
        "liteinst" => Ok("ptrace-hosted LiteInst backend"),
        "sabre" => Ok("SaBRe backend"),
        "kvm" => Ok("KVM backend"),
        "e9patch" => Ok("e9patch preprocessing with the ptrace backend"),
        _ => Err(format!(
            "unknown backend {backend:?}; expected ptrace, dbt, liteinst, sabre, kvm, or e9patch"
        )),
    }
}

fn has_authoritative_complete_single_run_log_file(backend: &str) -> bool {
    // Live hermit-cli policy: DBT's ordinary single-run adapter refuses
    // --log-file. SaBRe accepts the controller's --log-file, but the injected
    // Detcore plugin forwards its actual records to raw stderr instead. Neither
    // path exposes a complete isolated/authenticated sink to this diagnostic.
    !matches!(backend, "dbt" | "sabre")
}

fn usage() -> String {
    let mut s = String::from(
        "Usage: scripts/cross-backend-detlog-diff.rs [OPTIONS] -- <guest> [guest-args...]\n\n\
         Diagnostic only: this compares one selected DETLOG stream per execution path.\n\
         It does not establish L2, full parity, or repeat determinism.\n\n\
         Options:\n\
         \x20 --backends A,B          two execution paths (required)\n\
         \x20 --hermit PATH           hermit binary (default: target/debug/hermit)\n\
         \x20 --detlog-stack          pass --detlog-stack to both runs\n\
         \x20 --detlog-heap           pass --detlog-heap to both runs\n\
         \x20 --context N             completed syscalls before divergence (default: 5)\n\
         \x20 --normalize LIST        legacy names; lossy requests refuse before capture\n\
         \x20 --keep DIR              keep the raw captured streams in DIR\n\
         \x20 --self-test             run inert comparison and selection checks\n\
         \x20 --list-normalizations   describe every normalization and exit\n\n\
         Backends: ptrace, dbt, liteinst, sabre, kvm. `e9patch` means\n\
         preprocessing followed by the ptrace runtime; it is not a backend.\n\
         DBT currently refuses because its one-run stderr is guest-controllable.\n\
         SaBRe also refuses: its Detcore records use raw guest-controllable\n\
         stderr, while the host --log-file is incomplete. Neither path exposes\n\
         an isolated authenticated complete sink.\n\n\
         Exit: 0 agree, 1 diverge, 2 could not compare.\n",
    );
    s.push_str("\nNormalizations:\n");
    for n in NORMALIZATIONS {
        s.push_str(&format!(
            "  {:<16} {} {}\n",
            n.name,
            if n.default_on { "[on] " } else { "[off]" },
            n.what
        ));
    }
    s
}

fn parse_args() -> Result<Config, String> {
    let mut cfg = Config {
        hermit: PathBuf::from("target/debug/hermit"),
        backends: Vec::new(),
        guest: Vec::new(),
        detlog_stack: false,
        detlog_heap: false,
        context: 5,
        normalize: NORMALIZATIONS
            .iter()
            .filter(|n| n.default_on)
            .map(|n| n.name.to_string())
            .collect(),
        keep: None,
    };
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Err(usage()),
            "--list-normalizations" => {
                let mut out = String::new();
                for n in NORMALIZATIONS {
                    out.push_str(&format!(
                        "{}\n  default: {}\n  rewrites: {}\n",
                        n.name,
                        if n.default_on { "ON" } else { "off" },
                        n.what
                    ));
                    if let Some(m) = n.masks {
                        out.push_str(&format!("  CAUTION: {}\n", m));
                    }
                    out.push('\n');
                }
                return Err(out);
            }
            "--backends" => {
                let v = args.next().ok_or("--backends needs a value")?;
                cfg.backends = v.split(',').map(|s| s.trim().to_string()).collect();
                if cfg.backends.len() != 2 {
                    return Err("--backends takes exactly two, e.g. ptrace,dbt".into());
                }
                for backend in &cfg.backends {
                    backend_description(backend)?;
                }
            }
            "--hermit" => cfg.hermit = PathBuf::from(args.next().ok_or("--hermit needs a value")?),
            "--detlog-stack" => cfg.detlog_stack = true,
            "--detlog-heap" => cfg.detlog_heap = true,
            "--context" => {
                cfg.context = args
                    .next()
                    .ok_or("--context needs a value")?
                    .parse()
                    .map_err(|_| "--context takes a number")?;
            }
            "--keep" => cfg.keep = Some(PathBuf::from(args.next().ok_or("--keep needs a value")?)),
            "--normalize" => {
                let v = args.next().ok_or("--normalize needs a value")?;
                for name in v.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    if !NORMALIZATIONS.iter().any(|n| n.name == name) {
                        return Err(format!(
                            "unknown normalization {name:?}; see --list-normalizations"
                        ));
                    }
                    if !cfg.normalize.iter().any(|n| n == name) {
                        cfg.normalize.push(name.to_string());
                    }
                }
            }
            "--self-test" => {
                return Err("--self-test must be the only argument".into());
            }
            "--" => {
                cfg.guest = args.collect();
                break;
            }
            other => return Err(format!("unexpected argument {other:?}\n\n{}", usage())),
        }
    }
    if cfg.backends.is_empty() {
        return Err(format!(
            "--backends is required; name two execution paths explicitly\n\n{}",
            usage()
        ));
    }
    if cfg.guest.is_empty() {
        return Err(format!("no guest command given\n\n{}", usage()));
    }
    Ok(cfg)
}

fn select_authoritative_stream<'a>(
    side: &str,
    backend: &str,
    from_file: &'a str,
    stderr: &[u8],
) -> Result<(&'static str, &'a str, usize), String> {
    if backend == "dbt" {
        return Err(format!(
            "{side} DynamoRIO DBT ordinary single-run evidence is available only on stderr, which is shared with the guest and can be forged; no isolated typed/authenticated one-run sink is exposed, so this diagnostic refuses DBT (use strict canonical verify instead)"
        ));
    }
    if backend == "sabre" {
        return Err(format!(
            "{side} SaBRe Detcore records are forwarded to raw stderr, which is shared with the guest and can be forged, while the host --log-file contains an incomplete controller stream; no isolated typed/authenticated complete sink is exposed, so this diagnostic refuses SaBRe (use strict canonical verify instead)"
        ));
    }
    if from_file.is_empty() {
        return Err(format!(
            "{side} {} produced no authoritative --log-file evidence; refusing {} guest-controllable stderr byte(s)",
            backend_description(backend).unwrap_or("unknown execution path"),
            stderr.len()
        ));
    }
    Ok(("log-file", from_file, stderr.len()))
}

/// One backend's authoritative captured log and its provenance.
struct Capture {
    backend: String,
    /// "log-file" or "stderr" — which stream carried the trace.
    source: &'static str,
    /// Bytes seen on the other stream, so a silent split can be reported.
    other_stream_bytes: usize,
    exit_code: Option<i32>,
    authoritative: Vec<u8>,
    selected_records: usize,
}

fn capture(cfg: &Config, backend: &str, side: &str, tmpdir: &Path) -> Result<Capture, String> {
    let log_file = tmpdir.join(format!("{side}-{backend}.log-file"));
    let mut cmd = Command::new(&cfg.hermit);
    cmd.arg("--log").arg("info");
    if has_authoritative_complete_single_run_log_file(backend) {
        cmd.arg("--log-file").arg(&log_file);
    }
    cmd.arg("run")
        .arg(format!("--backend={backend}"))
        .arg("--strict");
    if cfg.detlog_stack {
        cmd.arg("--detlog-stack");
    }
    if cfg.detlog_heap {
        cmd.arg("--detlog-heap");
    }
    cmd.arg("--").args(&cfg.guest);

    let out = cmd
        .output()
        .map_err(|e| format!("could not run {} for {backend}: {e}", cfg.hermit.display()))?;
    finish_capture(cfg, backend, side, &log_file, out)
}

// Keep the process output separate from the authoritative log. This also lets
// the native self-test exercise the real validation and retention path.
fn finish_capture(
    cfg: &Config,
    backend: &str,
    side: &str,
    log_file: &Path,
    out: std::process::Output,
) -> Result<Capture, String> {
    let stderr = out.stderr;
    let from_file = match fs::read(log_file) {
        Ok(bytes) => String::from_utf8(bytes).map_err(|error| {
            format!(
                "{} emitted a non-UTF-8 --log-file, so its DETLOG cannot be compared: {error}",
                backend_description(backend).unwrap_or("unknown execution path")
            )
        })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(format!(
                "cannot read {} --log-file {}: {error}",
                backend_description(backend).unwrap_or("unknown execution path"),
                log_file.display()
            ));
        }
    };

    if !out.status.success() {
        let stderr_text = String::from_utf8_lossy(&stderr);
        let last_stderr = stderr_text.lines().last().unwrap_or("<empty stderr>");
        return Err(format!(
            "{} run failed with {}; refusing to compare its {} captured log bytes as successful evidence (last stderr line: {last_stderr})",
            backend_description(backend).unwrap_or("unknown execution path"),
            out.status,
            stderr.len() + from_file.len()
        ));
    }

    // `--log-file` is opened by Hermit in the host namespace and is the only
    // authoritative single-run sink available here. Stderr is shared with the
    // guest: record-looking lines there are diagnostics, never evidence.
    let (source, authoritative, other_bytes) =
        select_authoritative_stream(side, backend, &from_file, &stderr)?;
    let mut canonical_records = Vec::new();
    let evidence_label = format!(
        "{side} {}",
        backend_description(backend).unwrap_or("unknown execution path")
    );
    let selected_records = write_bitwise_info_v1_bytes(
        authoritative.as_bytes(),
        &evidence_label,
        &mut canonical_records,
    )
    .map_err(|error| {
        format!("cannot read canonical INFO evidence for {evidence_label}: {error}")
    })?;
    if selected_records == 0 {
        return Err(format!(
            "{} authoritative {source} contained no comparable INFO records",
            backend_description(backend).unwrap_or("unknown execution path")
        ));
    }

    if let Some(dir) = &cfg.keep {
        fs::create_dir_all(dir).map_err(|error| {
            format!("cannot create --keep directory {}: {error}", dir.display())
        })?;
        let retained = [
            (
                dir.join(format!("{side}-{backend}.stderr")),
                stderr.as_slice(),
            ),
            (
                dir.join(format!("{side}-{backend}.log-file")),
                from_file.as_bytes(),
            ),
            (
                dir.join(format!("{side}-{backend}.records")),
                canonical_records.as_slice(),
            ),
        ];
        if let Some((path, _)) = retained.iter().find(|(path, _)| path.exists()) {
            return Err(format!(
                "refusing to overwrite retained evidence {}",
                path.display()
            ));
        }
        for (path, bytes) in retained {
            fs::write(&path, bytes).map_err(|error| {
                format!(
                    "cannot write retained evidence for {backend} to {}: {error}",
                    path.display()
                )
            })?;
        }
    }

    Ok(Capture {
        backend: backend.to_string(),
        source,
        other_stream_bytes: other_bytes,
        exit_code: out.status.code(),
        authoritative: authoritative.as_bytes().to_vec(),
        selected_records,
    })
}

fn print_manifest(cfg: &Config, a: &Capture, b: &Capture) {
    println!("== provenance ==");
    for c in [a, b] {
        println!(
            "  {:<8} ({}) trace from {:<8} ({} INFO records, {} bytes on the other stream), guest exit {}",
            c.backend,
            backend_description(&c.backend).unwrap_or("unknown execution path"),
            c.source,
            c.selected_records,
            c.other_stream_bytes,
            c.exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into()),
        );
    }
    println!(
        "  assurance: diagnostic comparison of one run per execution path; not L2, full parity, or repeat determinism"
    );
    if a.source != b.source {
        println!(
            "  NOTE: the two backends reported through DIFFERENT streams ({} vs {}). That is a \
             known per-backend logging difference, not a guest divergence.",
            a.source, b.source
        );
    }
    println!("== normalizations applied ==");
    if cfg.normalize.is_empty() {
        println!("  (none - records compared verbatim)");
    }
    for name in &cfg.normalize {
        let n = NORMALIZATIONS
            .iter()
            .find(|n| n.name == name.as_str())
            .unwrap();
        println!("  {:<16} {}", n.name, n.what);
        if let Some(m) = n.masks {
            println!("      !! CAUTION: {}", m);
        }
    }
    let off: Vec<&str> = NORMALIZATIONS
        .iter()
        .filter(|n| !cfg.normalize.iter().any(|e| e == n.name))
        .map(|n| n.name)
        .collect();
    if !off.is_empty() {
        println!("  not applied: {}", off.join(", "));
    }
}

fn lossy_normalizations(enabled: &[String]) -> Vec<&'static str> {
    NORMALIZATIONS
        .iter()
        .filter(|normalization| {
            normalization.masks.is_some() && enabled.iter().any(|name| name == normalization.name)
        })
        .map(|normalization| normalization.name)
        .collect()
}

fn validate_fixed_comparison(cfg: &Config) -> Result<(), String> {
    let lossy = lossy_normalizations(&cfg.normalize);
    if !lossy.is_empty() {
        return Err(format!(
            "lossy normalization(s) {} were requested; BitwiseInfoV1 compares virtual time and thread identity exactly",
            lossy.join(", ")
        ));
    }
    Ok(())
}

struct CanonicalComparison {
    summary: LogDiffSummary,
    total_left: usize,
    total_right: usize,
    diagnostic: Vec<u8>,
}

fn compare_captures(
    cfg: &Config,
    left: &Capture,
    right: &Capture,
) -> Result<CanonicalComparison, String> {
    let mut diagnostic = Vec::new();
    let (summary, total_left, total_right) =
        try_compare_bitwise_info_v1_bytes_with_records_and_diagnostics(
            &left.authoritative,
            &right.authoritative,
            ComparisonSideLabels::new(&left.backend, &right.backend),
            BitwiseInfoV1Diagnostics {
                difference_limit: 1,
                syscall_history: u64::try_from(cfg.context).unwrap_or(u64::MAX),
                no_color: true,
                print_logs: false,
            },
            &mut diagnostic,
        )
        .map_err(|error| format!("BitwiseInfoV1 comparison failed: {error}"))?;
    Ok(CanonicalComparison {
        summary,
        total_left,
        total_right,
        diagnostic,
    })
}

fn comparison_exit_code(summary: &LogDiffSummary) -> i32 {
    if summary.refusal_reason.is_some() || summary.compared_left == 0 || summary.compared_right == 0
    {
        2
    } else if summary.diff_found {
        1
    } else if summary.matched_with_evidence() {
        0
    } else {
        2
    }
}

fn report_comparison(left: &Capture, right: &Capture, comparison: &CanonicalComparison) -> i32 {
    let summary = &comparison.summary;
    println!(
        "  selected INFO records: {} {} | {} {} (all parsed records: {} | {})",
        left.backend,
        summary.compared_left,
        right.backend,
        summary.compared_right,
        comparison.total_left,
        comparison.total_right,
    );
    match comparison_exit_code(summary) {
        0 => {
            println!(
                "\nDIAGNOSTIC MATCH: {} selected INFO records compared.",
                summary.compared_left
            );
            0
        }
        1 => {
            match summary.first_divergent_record {
                Some(record) => println!("\nFIRST DIVERGENCE at 1-based log record {record}."),
                None => println!("\nFIRST DIVERGENCE: the selected INFO streams differ."),
            }
            println!(
                "  Only the FIRST divergence is meaningful; everything after it is downstream."
            );
            1
        }
        _ => {
            if summary.refusal_reason.is_some() {
                eprintln!("\nREFUSAL: a captured log is truncated; no result was produced.");
            } else {
                eprintln!(
                    "\nREFUSAL: the comparison selected {} | {} INFO records; both sides must contain evidence.",
                    summary.compared_left, summary.compared_right
                );
            }
            2
        }
    }
}

fn main() {
    rust_script_prelude::init();
    let argv = env::args().collect::<Vec<_>>();
    if argv.get(1).map(String::as_str) == Some("--self-test") {
        if argv.len() != 2 {
            eprintln!("cross-backend-detlog-diff: --self-test must be the only argument");
            std::process::exit(2);
        }
        run_self_test();
        return;
    }
    let cfg = match parse_args() {
        Ok(c) => c,
        Err(message) => {
            // --help / --list-normalizations exit 0; a real parse error exits 2.
            let is_help = message.starts_with("Usage:") || message.contains("\n  default: ");
            if is_help {
                print!("{message}");
                std::process::exit(0);
            }
            eprintln!("cross-backend-detlog-diff: {message}");
            std::process::exit(2);
        }
    };

    if let Err(error) = validate_fixed_comparison(&cfg) {
        eprintln!("cross-backend-detlog-diff: {error}");
        std::process::exit(2);
    }

    if !cfg.hermit.exists() {
        eprintln!(
            "cross-backend-detlog-diff: hermit binary not found at {} (pass --hermit PATH)",
            cfg.hermit.display()
        );
        std::process::exit(2);
    }

    // The retained copies are written by this host-side harness and therefore
    // survive under /tmp. Warn once, outside the per-backend capture loop, so
    // users do not infer that a guest-written --log-file is safe there too.
    if let Some(dir) = &cfg.keep {
        if dir.starts_with("/tmp") {
            eprintln!(
                "cross-backend-detlog-diff: warning: --keep {} is under /tmp, which hermit \
                 overmounts for the guest; these host-written copies survive, but do not use \
                 this location for guest-written evidence.",
                dir.display()
            );
        }
    }

    // NOT env::temp_dir(). Hermit overmounts the guest's /tmp, so a --log-file
    // written under /tmp lands inside the container and silently never appears
    // on the host: the run succeeds, the file is absent, and the harness sees
    // zero records. That failure looks exactly like "this backend emits no
    // trace", which is why it is worth spelling out here.
    let scratch_root = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .ok_or_else(|| "neither XDG_CACHE_HOME nor HOME is set".to_string())
        .unwrap_or_else(|error| {
            eprintln!(
                "cross-backend-detlog-diff: {error}; cannot choose a host-visible scratch directory"
            );
            std::process::exit(2);
        })
        .join("hermit-xbackend-detlog");
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let tmpdir = scratch_root.join(format!("run-{}-{nonce}", std::process::id()));
    if let Err(e) = fs::create_dir_all(&tmpdir) {
        eprintln!(
            "cross-backend-detlog-diff: cannot create scratch dir {}: {e}",
            tmpdir.display()
        );
        std::process::exit(2);
    }

    let mut captures = Vec::new();
    for (index, backend) in cfg.backends.iter().enumerate() {
        let side = if index == 0 { "left" } else { "right" };
        match capture(&cfg, backend, side, &tmpdir) {
            Ok(c) => captures.push(c),
            Err(e) => {
                if let Err(cleanup) = fs::remove_dir_all(&tmpdir) {
                    eprintln!(
                        "cross-backend-detlog-diff: warning: could not remove scratch dir {}: {cleanup}",
                        tmpdir.display()
                    );
                }
                eprintln!("cross-backend-detlog-diff: {e}");
                std::process::exit(2);
            }
        }
    }
    let (a, b) = (&captures[0], &captures[1]);

    println!(
        "cross-backend canonical INFO diff: {} vs {}  guest: {}",
        a.backend,
        b.backend,
        cfg.guest.join(" ")
    );
    print_manifest(&cfg, a, b);

    let comparison = match compare_captures(&cfg, a, b) {
        Ok(comparison) => comparison,
        Err(error) => {
            eprintln!("\ncross-backend-detlog-diff: {error}; no result was produced");
            if let Err(cleanup) = fs::remove_dir_all(&tmpdir) {
                eprintln!(
                    "cross-backend-detlog-diff: warning: could not remove scratch dir {}: {cleanup}",
                    tmpdir.display()
                );
            }
            std::process::exit(2);
        }
    };
    {
        let mut stdout = std::io::stdout().lock();
        if let Err(error) = stdout.write_all(&comparison.diagnostic) {
            eprintln!("cross-backend-detlog-diff: cannot write comparison report: {error}");
            if let Err(cleanup) = fs::remove_dir_all(&tmpdir) {
                eprintln!(
                    "cross-backend-detlog-diff: warning: could not remove scratch dir {}: {cleanup}",
                    tmpdir.display()
                );
            }
            std::process::exit(2);
        }
    }

    let code = report_comparison(a, b, &comparison);
    if let Err(error) = fs::remove_dir_all(&tmpdir) {
        eprintln!(
            "cross-backend-detlog-diff: warning: could not remove scratch dir {}: {error}",
            tmpdir.display()
        );
    }
    std::process::exit(code);
}

fn run_capture_self_test(
    authoritative: &str,
    different: &str,
    expected_records: &[u8],
    check: &mut impl FnMut(bool, &str),
) {
    use std::os::unix::process::ExitStatusExt;

    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let scratch = env::temp_dir().join(format!(
        "hermit-cross-backend-detlog-self-test-{}-{nonce}",
        std::process::id()
    ));
    if let Err(error) = fs::create_dir(&scratch) {
        check(
            false,
            &format!("cannot create capture self-test directory: {error}"),
        );
        return;
    }
    let result = (|| -> Result<(), String> {
        let log_file = scratch.join("input.log");
        let keep = scratch.join("keep");
        let mut cfg = Config {
            hermit: scratch.join("unused-hermit"),
            backends: vec!["ptrace".into(), "ptrace".into()],
            guest: Vec::new(),
            detlog_stack: false,
            detlog_heap: false,
            context: 1,
            normalize: vec!["wall-clock".into(), "host-addresses".into()],
            keep: Some(keep.clone()),
        };
        // Invalid UTF-8 and a longer forged INFO stream are diagnostic bytes.
        let mut stderr = b"binary stderr: \0\x80\xff\n".to_vec();
        stderr.extend_from_slice(different.as_bytes());
        stderr.extend_from_slice(different.as_bytes());
        let output = |raw_status| std::process::Output {
            status: std::process::ExitStatus::from_raw(raw_status),
            stdout: b"same guest output\n".to_vec(),
            stderr: stderr.clone(),
        };
        fs::write(&log_file, authoritative.as_bytes()).map_err(|error| error.to_string())?;
        let left = finish_capture(&cfg, "ptrace", "left", &log_file, output(0))?;
        check(
            left.authoritative.as_slice() == authoritative.as_bytes()
                && stderr.len() > authoritative.len()
                && left.source == "log-file"
                && left.selected_records == 1
                && left.other_stream_bytes == stderr.len()
                && left.exit_code == Some(0),
            "binary stderr changed the authoritative capture or its provenance",
        );
        let retained = [
            (keep.join("left-ptrace.stderr"), stderr.as_slice()),
            (keep.join("left-ptrace.log-file"), authoritative.as_bytes()),
            (keep.join("left-ptrace.records"), expected_records),
        ];
        for (path, expected) in &retained {
            let bytes = fs::read(path).map_err(|error| error.to_string())?;
            check(
                bytes.as_slice() == *expected,
                &format!("retained capture bytes changed at {}", path.display()),
            );
        }
        check(
            finish_capture(&cfg, "ptrace", "left", &log_file, output(0))
                .is_err_and(|error| error.contains("refusing to overwrite retained evidence")),
            "an existing retained capture was overwritten",
        );
        for (path, expected) in &retained {
            let bytes = fs::read(path).map_err(|error| error.to_string())?;
            check(
                bytes.as_slice() == *expected,
                &format!("retained bytes changed after refusal at {}", path.display()),
            );
        }

        cfg.keep = None;
        let right = finish_capture(&cfg, "ptrace", "right", &log_file, output(0))?;
        let comparison = compare_captures(&cfg, &left, &right)?;
        check(
            comparison.summary.matched_with_evidence()
                && comparison.summary.compared_left == 1
                && comparison.summary.compared_right == 1
                && comparison_exit_code(&comparison.summary) == 0,
            "valid authoritative INFO did not compare beside binary stderr",
        );
        fs::write(&log_file, different.as_bytes()).map_err(|error| error.to_string())?;
        let right = finish_capture(&cfg, "ptrace", "right", &log_file, output(0))?;
        let comparison = compare_captures(&cfg, &left, &right)?;
        check(
            comparison.summary.diff_found
                && comparison.summary.first_divergent_record == Some(1)
                && comparison_exit_code(&comparison.summary) == 1,
            "identical guest output hid a canonical INFO difference",
        );

        fs::write(&log_file, authoritative.as_bytes()).map_err(|error| error.to_string())?;
        for raw_status in [37 << 8, 15] {
            check(
                finish_capture(&cfg, "ptrace", "left", &log_file, output(raw_status)).is_err_and(
                    |error| {
                        error.contains("run failed with") && error.contains("refusing to compare")
                    },
                ),
                "binary stderr hid a failed or signalled run",
            );
        }
        for backend in ["dbt", "sabre"] {
            check(
                finish_capture(&cfg, backend, "left", &log_file, output(0))
                    .is_err_and(|error| error.contains("no isolated typed/authenticated")),
                "binary stderr enabled a backend without an authoritative complete sink",
            );
        }
        check(
            finish_capture(
                &cfg,
                "ptrace",
                "left",
                &scratch.join("missing.log"),
                output(0),
            )
            .is_err_and(|error| error.contains("no authoritative --log-file evidence")),
            "forged binary stderr replaced a missing authoritative log",
        );
        check(
            finish_capture(&cfg, "ptrace", "left", &scratch, output(0))
                .is_err_and(|error| error.contains("cannot read ptrace backend --log-file")),
            "an unreadable authoritative log was replaced by stderr",
        );

        let mut invalid_utf8 = authoritative.as_bytes().to_vec();
        invalid_utf8.push(0x80);
        let truncated = format!("{authoritative}{}\n", detcore::logdiff::TRUNCATION_MARKER);
        let malformed =
            b"2026-08-06T13:38:01.654561Z INFO detcore: DETLOG broken DETLOG_RECORD={not-json}\n";
        let no_info = b"2026-08-06T13:38:01.654561Z WARN guest_observer: not selected\n";
        for (name, bytes, expected_error) in [
            (
                "empty",
                b"".as_slice(),
                "no authoritative --log-file evidence",
            ),
            (
                "invalid-utf8",
                invalid_utf8.as_slice(),
                "non-UTF-8 --log-file",
            ),
            (
                "truncated",
                truncated.as_bytes(),
                "was truncated at the configured size bound",
            ),
            (
                "malformed",
                malformed.as_slice(),
                "invalid structured DETLOG result",
            ),
            (
                "no-info",
                no_info.as_slice(),
                "contained no comparable INFO records",
            ),
        ] {
            fs::write(&log_file, bytes).map_err(|error| error.to_string())?;
            let refused_keep = scratch.join(format!("refused-{name}"));
            cfg.keep = Some(refused_keep.clone());
            check(
                finish_capture(&cfg, "ptrace", "left", &log_file, output(0))
                    .is_err_and(|error| error.contains(expected_error)),
                &format!("{name} authoritative evidence was not refused beside binary stderr"),
            );
            check(
                !refused_keep.exists(),
                &format!("{name} authoritative evidence was retained despite refusal"),
            );
        }
        Ok(())
    })();
    if let Err(error) = result {
        check(false, &format!("capture self-test failed: {error}"));
    }
    if let Err(error) = fs::remove_dir_all(&scratch) {
        check(
            false,
            &format!("cannot remove capture self-test directory: {error}"),
        );
    }
}

fn run_self_test() {
    let mut failures = Vec::new();
    let mut checks = 0usize;
    let mut check = |condition: bool, message: &str| {
        checks += 1;
        if !condition {
            failures.push(message.to_string());
        }
    };

    for backend in ["ptrace", "dbt", "liteinst", "sabre", "kvm", "e9patch"] {
        check(
            backend_description(backend).is_ok(),
            &format!("known execution path {backend} was rejected"),
        );
    }
    check(
        backend_description("dbi").is_err(),
        "obsolete dbi spelling was accepted",
    );
    check(
        !has_authoritative_complete_single_run_log_file("dbt")
            && !has_authoritative_complete_single_run_log_file("sabre")
            && has_authoritative_complete_single_run_log_file("ptrace"),
        "DBT/SaBRe single-run log sink policy was not represented",
    );
    check(
        backend_description("unknown").is_err(),
        "unknown backend was accepted",
    );
    check(
        backend_description("e9patch")
            .unwrap()
            .contains("preprocessing with the ptrace backend"),
        "e9patch was presented as an independent backend",
    );

    let structured_record = |second: usize, body: &str| {
        format!(
            "2026-08-06T13:38:{second:02}.654561Z INFO detcore: {body}{}\n",
            record_suffix(DetLogEvent::Other)
        )
    };
    let left = structured_record(1, "DETLOG address=<hostaddr 0x111111>");
    let right = structured_record(1, "DETLOG address=<hostaddr 0xaaaaaa>");
    let mut diagnostic = Vec::new();
    let (matched, total_left, total_right) =
        try_compare_bitwise_info_v1_bytes_with_records_and_diagnostics(
            left.as_bytes(),
            right.as_bytes(),
            ComparisonSideLabels::new("left", "right"),
            BitwiseInfoV1Diagnostics {
                difference_limit: 1,
                syscall_history: 1,
                no_color: true,
                print_logs: false,
            },
            &mut diagnostic,
        )
        .expect("shared canonical comparison should parse current records");
    check(
        matched.matched_with_evidence()
            && matched.compared_left == 1
            && matched.compared_right == 1
            && total_left == 1
            && total_right == 1,
        "shared canonical comparison did not match ordinal-equivalent addresses",
    );
    check(
        comparison_exit_code(&matched) == 0,
        "shared canonical match did not produce exit 0",
    );

    let different = structured_record(1, "DETLOG value=2");
    let (diverged, _, _) = try_compare_bitwise_info_v1_bytes_with_records_and_diagnostics(
        left.as_bytes(),
        different.as_bytes(),
        ComparisonSideLabels::new("left", "right"),
        BitwiseInfoV1Diagnostics {
            difference_limit: 1,
            syscall_history: 1,
            no_color: true,
            print_logs: false,
        },
        &mut Vec::new(),
    )
    .expect("shared canonical comparison should report a divergence");
    check(
        diverged.diff_found && diverged.first_divergent_record == Some(1),
        "shared comparator did not report the first divergent record",
    );
    check(
        comparison_exit_code(&diverged) == 1,
        "shared canonical divergence did not produce exit 1",
    );

    let truncated = format!("{left}{}\n", detcore::logdiff::TRUNCATION_MARKER);
    let (refused, _, _) = try_compare_bitwise_info_v1_bytes_with_records_and_diagnostics(
        truncated.as_bytes(),
        right.as_bytes(),
        ComparisonSideLabels::new("left", "right"),
        BitwiseInfoV1Diagnostics::default(),
        &mut Vec::new(),
    )
    .expect("truncation is a typed refusal, not a parser error");
    check(
        refused.refusal_reason.is_some() && comparison_exit_code(&refused) == 2,
        "shared comparator did not refuse a truncated input",
    );

    let mut rendered = Vec::new();
    let rendered_count = write_bitwise_info_v1_bytes(left.as_bytes(), "left", &mut rendered)
        .expect("shared canonical renderer should accept current records");
    let rendered = String::from_utf8(rendered).expect("canonical INFO is UTF-8");
    check(
        rendered_count == 1 && rendered.contains("<addr1>") && !rendered.contains("0x111111"),
        "shared canonical renderer did not ordinalize the marked host address",
    );

    run_capture_self_test(&left, &different, rendered.as_bytes(), &mut check);

    let authoritative = "INFO detcore: DETLOG authoritative";
    let forged_longer = "INFO detcore: DETLOG forged-1\nINFO detcore: DETLOG forged-2";
    check(
        matches!(
            select_authoritative_stream("left", "ptrace", authoritative, forged_longer.as_bytes()),
            Ok(("log-file", selected, _)) if selected == authoritative
        ),
        "longer forged stderr displaced the authoritative log file",
    );
    check(
        matches!(
            select_authoritative_stream(
                "left",
                "ptrace",
                "INFO detcore: DETLOG file",
                b"INFO detcore: DETLOG conflicting-stderr",
            ),
            Ok(("log-file", "INFO detcore: DETLOG file", _))
        ),
        "equal-length conflicting stderr made authoritative selection ambiguous",
    );
    check(
        select_authoritative_stream("left", "ptrace", "", forged_longer.as_bytes()).is_err(),
        "guest-controllable stderr was accepted without an authoritative log file",
    );
    check(
        select_authoritative_stream("left", "dbt", "", forged_longer.as_bytes()).is_err(),
        "DBT guest-controllable stderr was accepted as evidence",
    );
    check(
        select_authoritative_stream("left", "sabre", authoritative, forged_longer.as_bytes())
            .is_err_and(|error| {
                error.contains("SaBRe Detcore records are forwarded to raw stderr")
                    && error.contains("host --log-file contains an incomplete controller stream")
                    && error.contains("no isolated typed/authenticated complete sink")
            }),
        "SaBRe guest-controllable stderr or incomplete host log file was accepted as evidence",
    );
    check(
        select_authoritative_stream("left", "ptrace", "", b"").is_err(),
        "empty authoritative evidence did not refuse",
    );

    let compare = |left: &str, right: &str| {
        try_compare_bitwise_info_v1_bytes_with_records_and_diagnostics(
            left.as_bytes(),
            right.as_bytes(),
            ComparisonSideLabels::new("left", "right"),
            BitwiseInfoV1Diagnostics::default(),
            &mut Vec::new(),
        )
        .expect("shared canonical comparison should accept current records")
        .0
    };
    let bare_left = structured_record(2, "DETLOG result=0xabcdef01");
    let bare_right = structured_record(2, "DETLOG result=0xabcdef02");
    check(
        compare(&bare_left, &bare_right).diff_found,
        "meaningful bare hexadecimal difference was erased",
    );
    let alias_left = structured_record(3, "DETLOG pair=<hostaddr 0x111111>,<hostaddr 0x111111>");
    let alias_right = structured_record(3, "DETLOG pair=<hostaddr 0xaaaaaa>,<hostaddr 0xbbbbbb>");
    check(
        compare(&alias_left, &alias_right).diff_found,
        "host-address canonicalization erased an aliasing difference",
    );

    let fixed = Config {
        hermit: PathBuf::from("target/debug/hermit"),
        backends: vec!["ptrace".into(), "ptrace".into()],
        guest: vec!["/bin/true".into()],
        detlog_stack: false,
        detlog_heap: false,
        context: 5,
        normalize: vec!["wall-clock".into(), "host-addresses".into()],
        keep: None,
    };
    check(
        validate_fixed_comparison(&fixed).is_ok(),
        "fixed canonical normalizations were refused",
    );
    let lossy = Config {
        normalize: vec!["wall-clock".into(), "virtual-time".into()],
        ..fixed
    };
    check(
        validate_fixed_comparison(&lossy).is_err(),
        "lossy virtual-time normalization was not refused before capture",
    );

    if failures.is_empty() {
        println!("cross-backend-detlog-diff self-test: PASS ({checks} checks)");
    } else {
        for failure in &failures {
            eprintln!("cross-backend-detlog-diff self-test: FAIL: {failure}");
        }
        std::process::exit(2);
    }
}
