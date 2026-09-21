/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The KVM backend's INFO stream must not carry host-irreproducible values.
//!
//! `--verify-strict` compares INFO events under the production canonical policy:
//! it removes the real wall-clock prefix and ordinalizes only explicitly marked
//! host addresses. Every other byte remains exact. An unmarked host wall-clock
//! value inside a message body therefore makes two runs of the same guest differ
//! on that line alone, with no guest cause.
//!
//! The KVM `--verify` path compares this stream directly. These tests guard both
//! prerequisites for that verdict: repeated input must match, and a changed
//! syscall-buffer hash in the retained stream must make verification fail.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;

use tempfile::NamedTempFile;

/// KVM runs are serialized for the same reason `kvm_harder.rs` serializes them.
static KVM_RUN_LOCK: Mutex<()> = Mutex::new(());

const LIFECYCLE_TIMING_EVENT: &str = "reverie-kvm lifecycle phase timings";

const IO_BUFFER_MUTATOR_SOURCE: &str = r#"
typedef unsigned long usize;

static inline long syscall1(long number, long arg1) {
  register long rax __asm__("rax") = number;
  register long rdi __asm__("rdi") = arg1;
  __asm__ volatile("syscall"
                   : "+a"(rax)
                   : "D"(rdi)
                   : "rcx", "r11", "memory");
  return rax;
}

static inline long syscall4(long number, long arg1, long arg2, long arg3,
                            long arg4) {
  register long rax __asm__("rax") = number;
  register long rdi __asm__("rdi") = arg1;
  register long rsi __asm__("rsi") = arg2;
  register long rdx __asm__("rdx") = arg3;
  register long r10 __asm__("r10") = arg4;
  __asm__ volatile("syscall"
                   : "+a"(rax)
                   : "D"(rdi), "S"(rsi), "d"(rdx), "r"(r10)
                   : "rcx", "r11", "memory");
  return rax;
}

__attribute__((noreturn)) void _start(void) {
  static const char path[] = "@STATE_PATH@";
  static const char replacement[16] = {
      66, 66, 66, 66, 66, 66, 66, 66,
      66, 66, 66, 66, 66, 66, 66, 66,
  };
  char observed[16];
  long result = 0;
  long fd = syscall4(257, -100, (long)path, 2 | 02000000, 0);
  if (fd < 0) {
    result = 65;
  } else if (syscall4(17, fd, (long)observed, sizeof(observed), 0) !=
             (long)sizeof(observed)) {
    result = 66;
  } else if (syscall4(18, fd, (long)replacement, sizeof(replacement), 0) !=
             (long)sizeof(replacement)) {
    result = 67;
  } else if (syscall1(74, fd) != 0 || syscall1(3, fd) != 0) {
    result = 68;
  }
  syscall1(231, result);
  __builtin_unreachable();
}
"#;

const SENDMMSG_BUFFER_MUTATOR_SOURCE: &str = r#"
typedef unsigned int u32;
typedef unsigned long usize;

struct iovec {
  void *iov_base;
  usize iov_len;
};

struct msghdr {
  void *msg_name;
  u32 msg_namelen;
  struct iovec *msg_iov;
  usize msg_iovlen;
  void *msg_control;
  usize msg_controllen;
  u32 msg_flags;
};

struct mmsghdr {
  struct msghdr msg_hdr;
  u32 msg_len;
};

static inline long syscall1(long number, long arg1) {
  register long rax __asm__("rax") = number;
  register long rdi __asm__("rdi") = arg1;
  __asm__ volatile("syscall"
                   : "+a"(rax)
                   : "D"(rdi)
                   : "rcx", "r11", "memory");
  return rax;
}

static inline long syscall4(long number, long arg1, long arg2, long arg3,
                            long arg4) {
  register long rax __asm__("rax") = number;
  register long rdi __asm__("rdi") = arg1;
  register long rsi __asm__("rsi") = arg2;
  register long rdx __asm__("rdx") = arg3;
  register long r10 __asm__("r10") = arg4;
  __asm__ volatile("syscall"
                   : "+a"(rax)
                   : "D"(rdi), "S"(rsi), "d"(rdx), "r"(r10)
                   : "rcx", "r11", "memory");
  return rax;
}

static inline long syscall6(long number, long arg1, long arg2, long arg3,
                            long arg4, long arg5, long arg6) {
  register long rax __asm__("rax") = number;
  register long rdi __asm__("rdi") = arg1;
  register long rsi __asm__("rsi") = arg2;
  register long rdx __asm__("rdx") = arg3;
  register long r10 __asm__("r10") = arg4;
  register long r8 __asm__("r8") = arg5;
  register long r9 __asm__("r9") = arg6;
  __asm__ volatile("syscall"
                   : "+a"(rax)
                   : "D"(rdi), "S"(rsi), "d"(rdx), "r"(r10), "r"(r8), "r"(r9)
                   : "rcx", "r11", "memory");
  return rax;
}

__attribute__((noreturn)) void _start(void) {
  static const char path[] = "@STATE_PATH@";
  static const char replacement[16] = {
      66, 66, 66, 66, 66, 66, 66, 66,
      66, 66, 66, 66, 66, 66, 66, 66,
  };
  static int sockets[2];
  static struct iovec iovecs[2];
  static struct mmsghdr message;
  long result = 0;
  long fd = syscall4(257, -100, (long)path, 2 | 02000000, 0);
  long mapping = fd < 0 ? -1 : syscall6(9, 0, 16, 1, 2, fd, 0);
  if (fd < 0 || mapping < 0) {
    result = 65;
  } else if (syscall4(53, 1, 2 | 02000000, 0, (long)sockets) != 0) {
    result = 66;
  } else {
    iovecs[0].iov_base = (void *)mapping;
    iovecs[0].iov_len = 4;
    iovecs[1].iov_base = (void *)(mapping + 4);
    iovecs[1].iov_len = 12;
    message.msg_hdr.msg_iov = iovecs;
    message.msg_hdr.msg_iovlen = 2;
    long sent = syscall4(307, sockets[0], (long)&message, 1, 0);
    if (sent != 1) {
      result = sent < 0 ? 100 - sent : 80 + sent;
    } else if (message.msg_len != 16) {
      result = 70;
    } else if (syscall4(18, fd, (long)replacement, sizeof(replacement), 0) !=
               (long)sizeof(replacement)) {
      result = 68;
    } else if (syscall1(74, fd) != 0) {
      result = 69;
    }
    syscall1(3, sockets[0]);
    syscall1(3, sockets[1]);
  }
  if (fd >= 0) syscall1(3, fd);
  syscall1(231, result);
  __builtin_unreachable();
}
"#;

fn compile_source_guest(name: &str, source_text: &str, extra_args: &[&str]) -> PathBuf {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("kvm-info-log-determinism");
    fs::create_dir_all(&build_root).expect("failed to create guest build directory");
    let source = build_root.join(format!("{name}.c"));
    fs::write(&source, source_text).expect("failed to write guest source");
    let binary = build_root.join(name);
    let output = Command::new("cc")
        .args(["-std=c11", "-O2", "-Wall", "-Wextra", "-Werror"])
        .args(extra_args)
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap_or_else(|error| panic!("failed to compile {name}: {error}"));
    assert!(
        output.status.success(),
        "failed to compile {name}:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
    binary
}

fn compile_guest(name: &str, extra_args: &[&str]) -> PathBuf {
    compile_source_guest(name, "int main(void) { return 0; }\n", extra_args)
}

fn run_kvm_log(binary: &Path, log_level: &str, extra_run_flags: &[&str]) -> String {
    let output = Command::new("timeout")
        .args(["--kill-after", "10s", "90s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log", log_level, "--backend", "kvm"])
        .args(["run", "--strict"])
        .args(extra_run_flags)
        .arg("--")
        .arg(binary)
        .output()
        .unwrap_or_else(|error| panic!("failed to run guest under KVM: {error}"));
    assert!(
        output.status.success(),
        "KVM strict run failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[derive(Debug)]
struct CanonicalInfo {
    message_count: usize,
    text: String,
}

/// Run the same production canonical INFO extraction used by strict
/// verification. Keeping this call on the production implementation prevents
/// the test from drifting to a second approximation of the policy.
fn canonical_info(log: &str) -> CanonicalInfo {
    let mut input = NamedTempFile::new().expect("failed to create temporary KVM log");
    input
        .write_all(log.as_bytes())
        .expect("failed to write temporary KVM log");
    input.flush().expect("failed to flush temporary KVM log");

    let mut output = Vec::new();
    let reported = detcore::logdiff::write_canonical_info(input.path(), &mut output)
        .expect("failed to extract the canonical KVM INFO stream");
    let text = String::from_utf8(output).expect("canonical INFO stream was not UTF-8");
    CanonicalInfo {
        message_count: reported,
        text,
    }
}

/// Compare two captures with the production structured INFO comparator. Unlike
/// `write_canonical_info`, this retains message boundaries, including for
/// multiline messages that contain embedded newlines.
fn compare_canonical_info(left: &str, right: &str) -> detcore::logdiff::LogDiffSummary {
    let mut left_input = NamedTempFile::new().expect("failed to create first temporary KVM log");
    left_input
        .write_all(left.as_bytes())
        .expect("failed to write first temporary KVM log");
    left_input
        .flush()
        .expect("failed to flush first temporary KVM log");
    let mut right_input = NamedTempFile::new().expect("failed to create second temporary KVM log");
    right_input
        .write_all(right.as_bytes())
        .expect("failed to write second temporary KVM log");
    right_input
        .flush()
        .expect("failed to flush second temporary KVM log");

    let options = detcore::logdiff::LogDiffOpts {
        canonicalize_addresses: true,
        comparison: detcore::logdiff::LogComparisonMode::Info,
        no_color: true,
        ..Default::default()
    };
    detcore::logdiff::try_log_diff_detailed(left_input.path(), right_input.path(), &options)
        .expect("failed to compare canonical KVM INFO streams")
}

fn strip_ansi_sgr(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut remaining = input;
    while let Some(start) = remaining.find("\x1b[") {
        output.push_str(&remaining[..start]);
        let escape = &remaining[start + 2..];
        let Some(end) = escape.find("m") else {
            output.push_str(&remaining[start..]);
            return output;
        };
        remaining = &escape[end + 1..];
    }
    output.push_str(remaining);
    output
}

fn run_verify(
    backend: &str,
    binary: &Path,
    state_path: &Path,
    initial: &[u8; 16],
    label: &str,
    extra_run_flags: &[&str],
) -> (Output, serde_json::Value) {
    fs::write(state_path, initial).expect("failed to seed io-buffer state");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("kvm-info-log-determinism");
    let report_path = build_root.join(format!("{label}.json"));
    let log_dir = build_root.join(format!("{label}-logs"));
    let _ = fs::remove_file(&report_path);
    let _ = fs::remove_dir_all(&log_dir);
    fs::create_dir_all(&log_dir).expect("failed to create verification log directory");

    let output = Command::new("timeout")
        .args(["--kill-after", "10s", "90s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log", "info", "--backend", backend])
        .args([
            "run",
            "--strict",
            "--verify",
            "--verify-strict",
            "--keep-logs",
            "--verify-log-dir",
        ])
        .arg(&log_dir)
        .args(extra_run_flags)
        .arg("--verify-json")
        .arg(&report_path)
        .arg("--tmp=/tmp")
        .arg("--")
        .arg(binary)
        .output()
        .unwrap_or_else(|error| {
            panic!("failed to verify io-buffer guest under {backend}: {error}")
        });
    let report = serde_json::from_slice(&fs::read(&report_path).unwrap_or_else(|error| {
        panic!(
            "{backend} verification did not write {}: {error}; stderr:\n{}",
            report_path.display(),
            String::from_utf8_lossy(&output.stderr),
        )
    }))
    .expect("KVM verification report was not valid JSON");
    (output, report)
}

fn run_kvm_verify(
    binary: &Path,
    state_path: &Path,
    initial: &[u8; 16],
    label: &str,
    extra_run_flags: &[&str],
) -> (Output, serde_json::Value) {
    run_verify("kvm", binary, state_path, initial, label, extra_run_flags)
}

#[test]
fn kvm_verify_compares_retained_io_buffer_hashes() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("SKIP kvm_verify_compares_retained_io_buffer_hashes: /dev/kvm is not present");
        return;
    }
    let _guard = KVM_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let state_path = Path::new("/tmp").join(format!(
        "hermit-kvm-iobuf-verification-{}",
        std::process::id()
    ));
    let source = IO_BUFFER_MUTATOR_SOURCE.replace(
        "@STATE_PATH@",
        state_path.to_str().expect("state path was not UTF-8"),
    );
    let binary = compile_source_guest(
        "io-buffer-mutator",
        &source,
        &[
            "-nostdlib",
            "-static",
            "-Wl,-e,_start",
            "-fno-pie",
            "-no-pie",
        ],
    );

    const A_HASH: &str = "991204fba2b6216d476282d375ab88d20e6108d109aecded97ef424ddd114706";
    const B_HASH: &str = "900dfeb7f1b5e344209e2abce56c333dafe606fb3bf59f68ab2b0e2ef8a0662b";
    let (mutation, mutation_report) =
        run_kvm_verify(&binary, &state_path, b"AAAAAAAAAAAAAAAA", "mutation", &[]);
    let mutation_stderr = strip_ansi_sgr(&String::from_utf8_lossy(&mutation.stderr));
    assert!(
        !mutation.status.success(),
        "A-to-B mutation was accepted by KVM verification:\n{mutation_stderr}"
    );
    assert_eq!(mutation_report["verified"], false);
    assert_eq!(mutation_report["verdict"], "diverged");
    assert_eq!(mutation_report["comparison"]["compare_logs"], true);
    assert_eq!(mutation_report["comparison"]["compare_io_buffers"], true);
    assert!(
        mutation_report["compared_log_messages"]["left"]
            .as_u64()
            .is_some_and(|count| count > 0)
    );
    for marker in ["[iobuf]", "pread64", A_HASH, B_HASH] {
        assert!(
            mutation_stderr.contains(marker),
            "KVM mutation failure did not name {marker:?}:\n{mutation_stderr}"
        );
    }

    let (opt_out, opt_out_report) = run_kvm_verify(
        &binary,
        &state_path,
        b"AAAAAAAAAAAAAAAA",
        "opt-out",
        &["--no-detlog-io-buffers"],
    );
    let opt_out_stderr = String::from_utf8_lossy(&opt_out.stderr);
    assert!(
        opt_out.status.success(),
        "the explicit io-buffer opt-out should retain its weaker matched verdict:\n{opt_out_stderr}"
    );
    assert_eq!(opt_out_report["verified"], true);
    assert_eq!(opt_out_report["verdict"], "matched");
    assert_eq!(opt_out_report["bitwise_parity"], false);
    assert_eq!(opt_out_report["comparison"]["compare_logs"], true);
    assert_eq!(opt_out_report["comparison"]["compare_io_buffers"], false);
    assert!(
        opt_out_stderr.contains("output-buffer CONTENT was not compared"),
        "the weaker KVM match must name its missing observation:\n{opt_out_stderr}"
    );

    let (stable, stable_report) =
        run_kvm_verify(&binary, &state_path, b"BBBBBBBBBBBBBBBB", "stable", &[]);
    let stable_stderr = String::from_utf8_lossy(&stable.stderr);
    assert!(
        stable.status.success(),
        "identical B-to-B input failed KVM verification:\n{stable_stderr}"
    );
    assert_eq!(stable_report["verified"], true);
    assert_eq!(stable_report["verdict"], "matched");
    assert_eq!(stable_report["bitwise_parity"], true);
    assert_eq!(stable_report["comparison"]["compare_logs"], true);
    assert!(
        stable_report["compared_log_messages"]["left"]
            .as_u64()
            .is_some_and(|count| count > 0)
    );

    let _ = fs::remove_file(state_path);
}

// This reaches the shared Detcore logging path through ptrace. KVM currently
// returns ENOSYS for sendmmsg, so a KVM version would test syscall support
// rather than the buffer comparison implemented here. The unit tests above
// cover extraction of the completed message prefix independently of backend
// execution.
#[test]
fn ptrace_verify_compares_sendmmsg_iovec_prefixes() {
    let _guard = KVM_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let state_path = Path::new("/tmp").join(format!(
        "hermit-ptrace-sendmmsg-verification-{}",
        std::process::id()
    ));
    let source = SENDMMSG_BUFFER_MUTATOR_SOURCE.replace(
        "@STATE_PATH@",
        state_path.to_str().expect("state path was not UTF-8"),
    );
    let binary = compile_source_guest(
        "sendmmsg-buffer-mutator",
        &source,
        &[
            "-nostdlib",
            "-static",
            "-Wl,-e,_start",
            "-fno-pie",
            "-no-pie",
        ],
    );

    let (mutation, mutation_report) = run_verify(
        "ptrace",
        &binary,
        &state_path,
        b"AAAAAAAAAAAAAAAA",
        "sendmmsg-mutation",
        &[],
    );
    let mutation_stderr = strip_ansi_sgr(&String::from_utf8_lossy(&mutation.stderr));
    assert!(
        !mutation.status.success(),
        "A-to-B sendmmsg mutation was accepted by ptrace verification:\n{mutation_stderr}"
    );
    assert_eq!(mutation_report["verified"], false);
    assert_eq!(
        mutation_report["verdict"], "diverged",
        "ptrace sendmmsg mutation produced no comparison result:\n{mutation_stderr}"
    );
    assert_eq!(mutation_report["comparison"]["compare_io_buffers"], true);
    for marker in ["[iobuf]", "sendmmsg out", "+4->", "+12->"] {
        assert!(
            mutation_stderr.contains(marker),
            "ptrace sendmmsg mutation failure did not name {marker:?}:\n{mutation_stderr}"
        );
    }

    let (stable, stable_report) = run_verify(
        "ptrace",
        &binary,
        &state_path,
        b"BBBBBBBBBBBBBBBB",
        "sendmmsg-stable",
        &[],
    );
    assert!(
        stable.status.success(),
        "identical B-to-B sendmmsg input failed ptrace verification:\n{}",
        String::from_utf8_lossy(&stable.stderr)
    );
    assert_eq!(stable_report["verified"], true);
    assert_eq!(stable_report["verdict"], "matched");

    let _ = fs::remove_file(state_path);
}

#[test]
fn kvm_info_stream_repeats_exactly_across_two_runs() {
    // Host limitation, not a product result: without /dev/kvm there is no KVM
    // backend to measure. Report the skip rather than passing silently.
    if !Path::new("/dev/kvm").exists() {
        eprintln!("SKIP kvm_info_stream_repeats_exactly_across_two_runs: /dev/kvm is not present");
        return;
    }
    let _guard = KVM_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let binary = compile_guest("guest", &[]);
    let first_log = run_kvm_log(&binary, "info", &[]);
    let second_log = run_kvm_log(&binary, "info", &[]);
    let debug_log = run_kvm_log(&binary, "debug", &[]);

    let first = canonical_info(&first_log);
    let second = canonical_info(&second_log);
    let comparison = compare_canonical_info(&first_log, &second_log);

    // Guard the comparison itself: an empty or truncated capture would make the
    // equality below vacuously true, which is the classic false green.
    assert!(
        first.message_count > 50,
        "canonical KVM INFO capture is implausibly short ({} messages); the comparison below \
         would not discriminate anything",
        first.message_count,
    );
    assert_eq!(
        comparison.compared_left, comparison.compared_right,
        "KVM INFO stream changed message count between two runs of the same guest",
    );
    assert_eq!(first.message_count, comparison.compared_left);
    assert_eq!(second.message_count, comparison.compared_right);

    let first_lines = first.text.lines().collect::<Vec<_>>();
    let second_lines = second.text.lines().collect::<Vec<_>>();
    let divergences: Vec<(usize, &str, &str)> = first_lines
        .iter()
        .zip(second_lines.iter())
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(index, (a, b))| (index, *a, *b))
        .collect();

    assert!(
        comparison.matched_with_evidence(),
        "KVM INFO stream is not reproducible across two runs of the same guest. \
         Every line below differs with no guest cause, so it carries a host value \
         that must not be in the compared stream:\n{}",
        divergences
            .iter()
            .map(|(index, a, b)| format!("  line {index}:\n    run 1: {a}\n    run 2: {b}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );

    assert!(
        !first.text.contains(LIFECYCLE_TIMING_EVENT),
        "host lifecycle timings leaked into the canonical INFO stream",
    );
    let debug_timing_lines = debug_log
        .lines()
        .filter(|line| line.contains(" DEBUG ") && line.contains(LIFECYCLE_TIMING_EVENT))
        .collect::<Vec<_>>();
    assert_eq!(
        debug_timing_lines.len(),
        1,
        "expected exactly one DEBUG lifecycle timing event; captured: {debug_timing_lines:?}",
    );
    for field in [
        "prepare_us=",
        "setup_us=",
        "execution_us=",
        "cleanup_us=",
        "teardown_us=",
        "lifecycle_us=",
    ] {
        assert!(
            debug_timing_lines[0].contains(field),
            "DEBUG lifecycle timing event is missing {field}: {}",
            debug_timing_lines[0],
        );
    }
}

#[test]
fn canonical_info_comparison_preserves_multiline_message_boundaries() {
    let left = concat!(
        "2026-08-14T01:00:00.000000Z INFO first\n",
        "INFO continuation\n",
        "2026-08-14T01:00:00.000001Z INFO second\n",
    );
    let right = concat!(
        "2026-08-14T01:00:00.000000Z INFO first\n",
        "2026-08-14T01:00:00.000001Z INFO continuation\n",
        "INFO second\n",
    );

    let flat_left = canonical_info(left);
    let flat_right = canonical_info(right);
    assert_eq!(flat_left.message_count, 2);
    assert_eq!(flat_right.message_count, 2);
    assert_eq!(
        flat_left.text, flat_right.text,
        "fixture must demonstrate why flattened canonical text is insufficient",
    );

    let structured = compare_canonical_info(left, right);
    assert_eq!(structured.compared_left, 2);
    assert_eq!(structured.compared_right, 2);
    assert!(
        structured.diff_found,
        "production comparison must reject a multiline message-boundary shift",
    );
}

#[derive(Debug, PartialEq, Eq)]
struct MemoryDigest {
    kind: &'static str,
    digest: String,
}

fn parse_memory_record(line: &str) -> Result<MemoryDigest, String> {
    let kind = if line.contains(" Stack ") {
        "Stack"
    } else if line.contains(" Heap ") {
        "Heap"
    } else {
        return Err(format!(
            "memory record names neither Stack nor Heap: {line}"
        ));
    };
    let (_, digest) = line
        .rsplit_once("->")
        .ok_or_else(|| format!("memory record has no digest separator: {line}"))?;
    let digest = digest.trim();
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!(
            "memory record has no 64-character hexadecimal content digest: {line}"
        ));
    }
    Ok(MemoryDigest {
        kind,
        digest: digest.to_owned(),
    })
}

/// Every parsed `DETLOG [memory]` content digest, in order.
fn memory_records(info: &str) -> Vec<MemoryDigest> {
    info.lines()
        .filter(|line| line.contains("DETLOG [memory]"))
        .map(|line| parse_memory_record(line).unwrap_or_else(|error| panic!("{error}")))
        .collect()
}

/// KVM must produce identical stack and heap CONTENT hashes across two runs of
/// the same guest.
///
/// SCOPE, stated because it is deliberately narrow and must not be read as more
/// than it is: this uses a **statically linked** guest. A dynamically linked
/// guest FAILS this property today, and not marginally — under KVM,
/// `/bin/echo hello` has 98 of 113 stack-content hashes differing run to run,
/// while ptrace has 0 of 193 differing.
///
/// Those figures and THE HOST THEY WERE MEASURED ON are recorded in
/// `docs/TESTING_ENVIRONMENTS.md`, under "KVM memory-hash repeatability". They
/// live there rather than here because a measurement is only auditable with its
/// provenance, and `scripts/check-portable-paths.sh` correctly refuses a literal
/// hostname in a `.rs` file. The host was not genericised: that would leave a
/// sentence reading as evidence while carrying none.
///
/// The cause is known and tracked, not papered over here: the KVM backend never
/// delivers `rdtsc` to the Reverie `Tool`, so Detcore's existing virtualization
/// never runs and the guest reads a raw host-derived cycle counter, which the
/// dynamic loader then leaves on the stack. See
/// <https://github.com/rrnewton/reverie/issues/448>. A static binary executes
/// zero `rdtsc` (measured: 0, versus 10 for every dynamically linked guest
/// tested), which is exactly why the property holds here and only here.
///
/// So this test pins the boundary of what is true rather than asserting a
/// broader claim. When #448 lands, the static restriction should be removed and
/// this test should pass for a dynamic guest too — that is the intended signal.
#[test]
fn kvm_memory_hashes_repeat_for_a_static_guest() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("SKIP kvm_memory_hashes_repeat_for_a_static_guest: /dev/kvm is not present");
        return;
    }
    let _guard = KVM_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let binary = compile_guest("static_guest", &["-static"]);
    let flags = ["--detlog-stack", "--detlog-heap"];
    let first = canonical_info(&run_kvm_log(&binary, "info", &flags));
    let second = canonical_info(&run_kvm_log(&binary, "info", &flags));

    let first_records = memory_records(&first.text);
    let second_records = memory_records(&second.text);

    // Without this guard the equality below would pass vacuously if the flags
    // stopped emitting records at all — which is precisely how a memory
    // determinism check would silently stop checking anything.
    assert!(
        !first_records.is_empty(),
        "no DETLOG [memory] records were emitted under --detlog-stack --detlog-heap; \
         the comparison below would not discriminate anything",
    );
    assert!(
        first_records.iter().any(|record| record.kind == "Stack")
            && first_records.iter().any(|record| record.kind == "Heap"),
        "expected both Stack and Heap records; got {} record(s): {:?}",
        first_records.len(),
        first_records,
    );
    assert_eq!(
        first_records.len(),
        second_records.len(),
        "KVM emitted a different number of memory records between two runs",
    );

    let divergences: Vec<(usize, &MemoryDigest, &MemoryDigest)> = first_records
        .iter()
        .zip(second_records.iter())
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(index, (a, b))| (index, a, b))
        .collect();

    assert!(
        divergences.is_empty(),
        "{} of {} KVM memory content hashes differ across two runs of the same \
         static guest:\n{}",
        divergences.len(),
        first_records.len(),
        divergences
            .iter()
            .map(|(index, a, b)| {
                format!("  record {index}:\n    run 1: {a:?}\n    run 2: {b:?}")
            })
            .collect::<Vec<_>>()
            .join("\n"),
    );
}

#[test]
fn memory_record_parser_requires_a_content_digest() {
    let without_digest = "INFO detcore: DETLOG [memory][dtid 1] Stack 0x1000-0x2000->";
    assert!(
        parse_memory_record(without_digest).is_err(),
        "a labeled memory record without a digest must not count as evidence",
    );
    let valid = "INFO detcore: DETLOG [memory][dtid 1] Heap 0x1000-0x2000->0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    assert_eq!(
        parse_memory_record(valid),
        Ok(MemoryDigest {
            kind: "Heap",
            digest: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
        }),
    );
}
