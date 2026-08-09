// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Compatibility-corpus tables for the validate driver.
//!
//! # Where these rows came from (provenance matters here)
//!
//! `ci/compat/corpus-<mode>.json` was NOT hand-transcribed from `validate.sh`.
//! Hand-copying ~750 probe invocations across four modes is exactly the kind of
//! silent-drift transcription a reviewer cannot audit. Instead the tables were
//! lifted MECHANICALLY: `strict_compatibility_probe` was temporarily intercepted
//! so that, instead of executing, it recorded its exact `(label, argv)` — after
//! every `COMPATIBILITY_MODE` gate in `run_compatibility_corpus` had already been
//! evaluated by the real bash. The dump therefore reflects the code path, not a
//! reading of it.
//!
//! The extraction is corroborated by the counts the bash itself declares:
//! `strict` dumped **191** rows against `STRICT_COMPAT_TOTAL=191`, and `sabre`
//! dumped **212** against `SABRE_COMPAT_TOTAL=212` — exact, independent matches.
//! `rr` dumps 174 admitted rows which the driver then filters to the 139
//! `RR_COMPAT_PASSING_LABELS` (the bash filters at the same point, inside
//! `rr_compatibility_probe`), and `e9patch` dumps 172 admitted rows.
//!
//! # Why a data file rather than Rust literals
//!
//! The corpus is DATA, and it changes on a different clock than the driver: rows
//! are added when a program becomes compatible. Keeping it as JSON means a corpus
//! change is a reviewable data diff, and it keeps the driver's control flow
//! readable instead of burying it under 750 argv literals.

use std::collections::BTreeMap;
use std::path::Path;

/// One corpus probe: a stable label and the exact guest argv to run under Hermit.
#[derive(Clone, Debug)]
pub struct CorpusRow {
    pub label: String,
    pub argv: Vec<String>,
}

/// Runtime values substituted into the `{{PLACEHOLDER}}` tokens the extractor left
/// behind. Each corresponds to a per-run path the bash computed at startup, so the
/// checked-in table stays host- and run-independent.
pub struct CorpusPaths<'a> {
    pub root_dir: &'a str,
    pub real_compat_fixtures: &'a str,
    pub validation_tmp_dir: &'a str,
    pub shell_build_dir: &'a str,
}

impl CorpusPaths<'_> {
    fn expand(&self, arg: &str) -> String {
        arg.replace("{{ROOT_DIR}}", self.root_dir)
            .replace("{{REAL_COMPAT_FIXTURES}}", self.real_compat_fixtures)
            .replace("{{VALIDATION_TMP_DIR}}", self.validation_tmp_dir)
            .replace("{{SHELL_BUILD_DIR}}", self.shell_build_dir)
    }
}

/// Load `ci/compat/corpus-<mode>.json` and expand its placeholders.
///
/// Fails LOUDLY rather than returning an empty corpus: a silently-empty corpus
/// would turn a compatibility gate into a no-op that still reports "pass", which
/// is the zero-executed-tests defect class in a new costume.
pub fn load(root: &Path, mode: &str, paths: &CorpusPaths) -> Result<Vec<CorpusRow>, String> {
    let file = root.join("ci").join("compat").join(format!("corpus-{mode}.json"));
    let text = std::fs::read_to_string(&file)
        .map_err(|e| format!("cannot read compatibility corpus {}: {e}", file.display()))?;
    let doc: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("invalid JSON in {}: {e}", file.display()))?;
    let rows = doc
        .get("rows")
        .and_then(|r| r.as_array())
        .ok_or_else(|| format!("{} has no `rows` array", file.display()))?;
    let mut out = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let label = row
            .get("label")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("{} row {i}: missing string `label`", file.display()))?;
        let argv_raw = row
            .get("argv")
            .and_then(|v| v.as_array())
            .ok_or_else(|| format!("{} row {i}: missing array `argv`", file.display()))?;
        let mut argv = Vec::with_capacity(argv_raw.len());
        for a in argv_raw {
            let s = a.as_str().ok_or_else(|| {
                format!("{} row {i} ({label}): non-string argv element", file.display())
            })?;
            argv.push(paths.expand(s));
        }
        if argv.is_empty() {
            return Err(format!("{} row {i} ({label}): empty argv", file.display()));
        }
        out.push(CorpusRow { label: label.to_string(), argv });
    }
    if out.is_empty() {
        return Err(format!("{} contained zero rows", file.display()));
    }
    Ok(out)
}

// ------------------------------------------------------------------ ratchets

/// `STRICT_COMPAT_TOTAL` (validate.sh:1108). The corpus contains semantic
/// workloads only; banner-only probes were removed when the E2E harness landed.
pub const STRICT_COMPAT_TOTAL: usize = 191;

/// `RR_COMPAT_EXPECTED` (validate.sh:1117). The exact set measured to pass
/// record/replay. Raising this without a fresh sweep produces a phantom ratchet.
pub const RR_COMPAT_EXPECTED: usize = 139;

/// `SABRE_COMPAT_EXPECTED` (validate.sh:1121) — the blocking floor.
pub const SABRE_COMPAT_EXPECTED: usize = 207;

/// `SABRE_COMPAT_TOTAL` (validate.sh:1124) — the measured corpus size.
pub const SABRE_COMPAT_TOTAL: usize = 212;

/// `E9PATCH_COMPAT_TOTAL` (validate.sh:1125).
pub const E9PATCH_COMPAT_TOTAL: usize = 155;

/// `COMPAT_SUMMARY_KNOWN_FAILURES` (validate.sh:1135). Tracked gaps excluded from
/// the executable corpus but retained in the canonical denominator and table.
/// Under `strict` these are NONBLOCKING: the row keeps running so the gap stays
/// visible, mirroring the gcc vfork precedent.
pub fn known_failclosed() -> BTreeMap<&'static str, &'static str> {
    BTreeMap::from([
        ("curl-localhost", "fail-closed --strict rejects the unsupported shutdown syscall on some hosts"),
        ("lsof", "fail-closed --strict rejects the unsupported close_range syscall"),
        ("make", "fail-closed --strict rejects the unsupported setresuid syscall"),
        ("wget-localhost", "fail-closed --strict rejects the unsupported shutdown syscall on some hosts"),
    ])
}

/// `PORTABLE_STRICT_DIAGNOSTIC_FAILURES` (validate.sh:1147). Bounded diagnostics
/// on the GitHub-managed portable runner: a failure here is nonblocking and the
/// probe is given a shortened 20s budget.
pub fn portable_diagnostic() -> BTreeMap<&'static str, &'static str> {
    BTreeMap::from([
        ("top", "live process-table reads differ on the GitHub-managed portable runner"),
        ("zstd", "timed out on the GitHub-managed portable no-PMU runner"),
        ("zstd-roundtrip", "timed out on the GitHub-managed portable no-PMU runner"),
    ])
}

/// `PORTABLE_STRICT_SUPER_ONLY` (validate.sh:1152). Heavy runtime/compiler
/// workloads deferred out of the portable profile into the scheduled super suite;
/// they stay in the table as `N/A` rather than costing 20s per row on every PR.
pub fn portable_super_only() -> BTreeMap<&'static str, &'static str> {
    BTreeMap::from([
        ("rustc", "full compile-link-run workload"),
        ("javac", "JVM startup and compile-run workload"),
        ("java", "threaded JVM filesystem and digest workload"),
        ("node", "Node.js runtime startup workload"),
    ])
}

/// `RR_COMPAT_KNOWN_FAILURES` (validate.sh:1181). Strict-corpus programs measured
/// to FAIL record/replay, hence excluded from the R/R passing ratchet.
pub fn rr_known_failures() -> BTreeMap<&'static str, &'static str> {
    BTreeMap::from([
        ("g++", "replay diverges (thread 13, ~event 132): C++ front-end header/.gch path resolution (readlink vs newfstatat) desyncs the event stream"),
        ("ar", "replay diverges (thread 11, ~event 3): archive workload teardown (execveat rm -rf) reorders against the recorded stream"),
        ("strip", "replay diverges at replayer/mod.rs:776 after a clean record"),
        ("gprof", "replay diverges at replayer/mod.rs:776 after a clean record"),
        ("gcov", "replay diverges at replayer/mod.rs:776 after a clean record"),
    ])
}

/// `RR_COMPAT_PASSING_LABELS` (validate.sh:1191) — exactly the rows measured to
/// pass record/replay. Size is asserted against [`RR_COMPAT_EXPECTED`] at startup,
/// reproducing the bash's own parse-time guard (validate.sh:1219).
pub const RR_PASSING_LABELS: &[&str] = &[
    "echo", "seq", "cat", "wc", "head", "base64", "id",
    "lua", "perl", "awk", "bc", "sqlite3", "bash",
    "gcc", "make", "bzip2", "gzip", "xz", "zstd",
    "openssl", "sort", "uniq", "tr", "cut", "tee",
    "paste", "comm", "join", "find", "stat", "file",
    "basename", "dirname", "env", "printenv", "uname",
    "factor", "expr", "dd", "df", "du", "hostname",
    "whoami", "groups", "tty", "nproc", "arch", "realpath",
    "readlink", "sha256sum", "sha1sum", "md5sum", "wc-lines",
    "nl", "expand", "unexpand", "test", "bracket", "printf",
    "sleep", "stdbuf", "nohup", "nice", "ionice", "taskset",
    "chrt", "flock", "logger", "getopt", "column", "hexdump",
    "xxd", "strings", "od", "sum", "cksum", "b2sum",
    "tsort", "ptx", "pinky", "logname", "users", "uptime",
    "grep", "egrep", "fgrep", "sed", "date", "cal", "yes",
    "tac", "rev", "fold", "fmt", "shuf", "numfmt",
    "split", "cmp", "rmdir", "mkfifo", "mkdir", "node",
    "diff", "cp", "install", "tar", "mv", "rm", "touch", "chmod",
    "java", "python3", "git", "true", "pwd", "base32",
    "sha224sum", "sha384sum", "sha512sum", "pr", "ls",
    "xargs", "iconv", "as", "ld", "nm", "objcopy",
    "objdump", "ranlib", "readelf", "size", "addr2line",
    "c++filt", "elfedit", "cpp",
    "ruby", "dc", "tcl", "free",
];

/// `COMPAT_SUMMARY_CATEGORIES` (validate.sh:1160), in the bash's print order.
pub const CATEGORIES: &[&str] = &[
    "coreutils",
    "interpreters",
    "build-toolchain",
    "text-data",
    "archive-compression",
    "filesystem-storage",
    "process-scheduling",
    "system-introspection",
    "networking",
    "applications",
    "other",
];

/// Port of validate.sh's `compatibility_category` case statement (validate.sh:2422).
///
/// GENERATED FROM THE BASH, not transcribed: the arms below were emitted by
/// parsing the real `case` statement, because a hand-copied 218-label table is
/// unauditable and I got several arms wrong on a first manual pass (`cmp` is
/// filesystem-storage not text-data; `groups` is process-scheduling; `jq`,
/// `openssl` and `sqlite3` are applications). Any label not named here falls to
/// `other`, exactly as the bash's `*)` arm does.
pub fn category_of(label: &str) -> &'static str {
    const COREUTILS: &[&str] = &[
        "arch", "b2sum", "base32", "base64", "basename", "basenc", "bracket", "cat", "cksum",
        "comm", "cp", "csplit", "cut", "date", "dd", "df", "dirname", "du", "echo", "env",
        "expand", "expr", "factor", "fmt", "fold", "head", "id", "install", "join", "ln", "ls",
        "md5sum", "mkdir", "mkfifo", "mktemp", "mv", "nice", "nl", "nohup", "nproc", "numfmt",
        "od", "paste", "pathchk", "pinky", "pr", "printenv", "printf", "ptx", "pwd", "readlink",
        "realpath", "rm", "rmdir", "seq", "sha1sum", "sha224sum", "sha256sum", "sha384sum",
        "sha512sum", "shred", "shuf", "sleep", "sort", "split", "stat", "stdbuf", "sum", "sync",
        "tac", "tee", "test", "timeout", "touch", "tr", "true", "truncate", "tsort", "tty",
        "uname", "unexpand", "uniq", "users", "wc", "wc-lines", "whoami", "xargs", "yes",
    ];
    const INTERPRETERS: &[&str] = &[
        "awk", "bash", "bc", "dc", "java", "lua", "node", "perl", "python3", "ruby", "tcl",
    ];
    const BUILD_TOOLCHAIN: &[&str] = &[
        "addr2line", "ar", "as", "c++filt", "cargo", "clang", "cmake", "cpp", "elfedit", "flex",
        "g++", "gcc", "gcov", "gprof", "javac", "ld", "m4", "make", "nm", "objcopy", "objdump",
        "pkg-config", "ranlib", "readelf", "rustc", "shell-build", "size", "strings", "strip",
    ];
    const TEXT_DATA: &[&str] = &[
        "col", "colrm", "column", "diff", "diff3", "dos2unix", "egrep", "envsubst", "fgrep",
        "file", "find", "grep", "hexdump", "iconv", "msgfmt", "msgunfmt", "patch", "rev", "sed",
        "xxd",
    ];
    const ARCHIVE_COMPRESSION: &[&str] = &[
        "bzip2", "bzip2-roundtrip", "cpio-roundtrip", "crc32", "gzip", "gzip-roundtrip", "tar",
        "tar-roundtrip", "xz", "xz-roundtrip", "zip-unzip", "zstd", "zstd-roundtrip",
    ];
    const FILESYSTEM_STORAGE: &[&str] = &[
        "chmod", "chown", "cmp", "fallocate", "findmnt", "mountpoint", "namei", "setfacl",
        "setfattr",
    ];
    const PROCESS_SCHEDULING: &[&str] = &[
        "chrt", "flock", "getopt", "groups", "ionice", "kill", "logger", "logname", "lsof",
        "pgrep", "pkill", "ps", "taskset", "time", "top",
    ];
    const SYSTEM_INTROSPECTION: &[&str] = &[
        "cal", "free", "getconf", "hostname", "iostat", "lscpu", "lsirq", "lsmod",
        "mpstat-softirqs", "numactl-hardware", "numastat", "pidstat-disk",
        "sar-resource-tables", "sensors-version", "sysctl-random-uuid", "uptime", "uuidgen",
        "vmstat", "vmstat-disk",
    ];
    const NETWORKING: &[&str] = &[
        "curl", "curl-localhost", "ip", "netcat", "socat", "ss", "wget", "wget-localhost",
    ];
    const APPLICATIONS: &[&str] = &[
        "cscope", "git", "jq", "openssl", "sqlite3", "xmllint",
    ];

    if COREUTILS.contains(&label) {
        return "coreutils";
    }
    if INTERPRETERS.contains(&label) {
        return "interpreters";
    }
    if BUILD_TOOLCHAIN.contains(&label) {
        return "build-toolchain";
    }
    if TEXT_DATA.contains(&label) {
        return "text-data";
    }
    if ARCHIVE_COMPRESSION.contains(&label) {
        return "archive-compression";
    }
    if FILESYSTEM_STORAGE.contains(&label) {
        return "filesystem-storage";
    }
    if PROCESS_SCHEDULING.contains(&label) {
        return "process-scheduling";
    }
    if SYSTEM_INTROSPECTION.contains(&label) {
        return "system-introspection";
    }
    if NETWORKING.contains(&label) {
        return "networking";
    }
    if APPLICATIONS.contains(&label) {
        return "applications";
    }
    "other"
}
