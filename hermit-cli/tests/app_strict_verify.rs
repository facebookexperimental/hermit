/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! End-to-end determinism tests that run real-world applications under
//! `hermit run --strict --verify` and require a bitwise-identical repeat run
//! (assurance level L2).
//!
//! These deliberately use the built-in `--verify` execution path (Hermit runs
//! the guest twice and diffs the two logs) rather than the manual "run N times
//! and compare stdout" style used elsewhere. They also cover applications that
//! previously had no strict-mode coverage at all:
//!
//! - `curl` and `nginx` were only exercised in default run mode
//!   (`arbitrary_binaries.rs`, `integration_matrix.rs`), never under `--strict`.
//! - `redis-server` and `java` have strict workloads elsewhere
//!   (`redis_strict.rs`, `language_runtime_determinism.rs`); the bounded
//!   version invocations here add a cheap, self-contained L2 smoke check for
//!   process startup and static initialization under the deterministic runtime.
//!
//! Each workload is a bounded, self-contained invocation (no network, no
//! long-running server) so the run terminates and its output depends only on
//! the guest, which is what lets `--verify` reach L2.
//!
//! # Managed runtimes: Go and the JVM
//!
//! This file also covers the Go runtime and the JVM. The results below were
//! measured with the ptrace backend, `--log=off`, and relaxations
//! `--no-virtualize-cpuid --preemption-timeout=disabled` (which keep strict
//! determinism; they only accommodate hosts without CPUID/PMU interception),
//! using Go 1.26.4 (Red Hat 1.26.4-1.el9) and OpenJDK 1.8.0_492.
//!
//! Two distinct outcomes were observed and are encoded as separate tests:
//!
//! - **L2 (bitwise-identical repeat run, `--strict --verify`):** compiled Go
//!   programs (a hello world and a goroutine workload) and the JVM running
//!   compiled classes (`java Hello`, a multi-thread `Threads`, and
//!   `java -version`). The managed runtimes' scheduling, GC, and static
//!   initialization are fully determinized.
//! - **L1 only (output-deterministic under `--strict`, but `--verify`'s
//!   internal two-run log diff diverges):** the toolchain *drivers*
//!   `go version` (the `go` command) and `javac`. Their exit code and
//!   user-visible output are stable across strict runs -- `javac` even emits a
//!   bytewise-identical `.class` -- but Hermit's `--verify` reports
//!   `Failure: nondeterministic`, so they are asserted at L1, not L2.
//!
//! The compiled-guest tests build their guests with the host `go`/`javac`
//! toolchain first and then run the resulting artifact under Hermit, which
//! isolates managed-runtime determinism from compiler determinism.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;
use std::sync::MutexGuard;

/// Serialize the Hermit runs; the deterministic scheduler and PMU counters are
/// process-global resources on the self-hosted runner.
static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());

/// Wall-clock cap for a single `--verify` (two-run) invocation.
const HERMIT_VERIFY_TIMEOUT: &str = "120s";

/// Grace period before `timeout(1)` escalates from SIGTERM to SIGKILL.
const HERMIT_VERIFY_KILL_AFTER: &str = "10s";

fn hermit_run_lock() -> MutexGuard<'static, ()> {
    HERMIT_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Resolve the first candidate path that names an existing regular file.
///
/// These applications are installed by the self-hosted CI job, so a missing
/// binary is a hard error rather than a silent skip.
fn required_app(name: &str, candidates: &[&str]) -> PathBuf {
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .unwrap_or_else(|| {
            panic!(
                "ERROR: required application {name} is missing; expected an executable at one of \
                 {candidates:?} (the self-hosted CI job installs it)"
            )
        })
}

/// Run `hermit run --strict --verify -- <program> <args>` and assert that
/// Hermit's verifier reports a bitwise-identical repeat run (L2).
///
/// `--verify` exits non-zero when the two runs diverge, so a successful exit is
/// the primary determinism signal; the success banner is checked as a guard
/// against the flag silently becoming a no-op.
fn assert_l2_under_strict_verify(program: &Path, args: &[&str]) {
    let _guard = hermit_run_lock();

    let mut command = Command::new("timeout");
    command
        .args([
            "--kill-after",
            HERMIT_VERIFY_KILL_AFTER,
            HERMIT_VERIFY_TIMEOUT,
        ])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=off",
            "run",
            "--strict",
            "--verify",
            // The self-hosted runner exposes real CPUID/PMU; these relaxations
            // keep the test usable on VMs without CPUID interception without
            // weakening determinism (they do not disable strict mode).
            "--no-virtualize-cpuid",
            "--preemption-timeout=disabled",
            "--",
        ])
        .arg(program)
        .args(args);

    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "hermit run --strict --verify was not deterministic (L2) for {rendered}\n\
         status: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status,
    );
    assert!(
        stderr.contains("Determinism verified") || stdout.contains("Determinism verified"),
        "hermit --verify exited 0 but did not report determinism for {rendered}\n\
         stdout:\n{stdout}\nstderr:\n{stderr}",
    );
}

#[test]
#[ignore = "e2e: requires hermit + PMU/mount namespaces + the curl binary"]
fn curl_version_is_deterministic_under_strict_verify() {
    let curl = required_app("curl", &["/usr/bin/curl", "/usr/local/bin/curl"]);
    assert_l2_under_strict_verify(&curl, &["--version"]);
}

#[test]
#[ignore = "e2e: requires hermit + PMU/mount namespaces + the nginx binary"]
fn nginx_version_is_deterministic_under_strict_verify() {
    let nginx = required_app("nginx", &["/usr/sbin/nginx", "/usr/bin/nginx"]);
    assert_l2_under_strict_verify(&nginx, &["-v"]);
}

#[test]
#[ignore = "e2e: requires hermit + PMU/mount namespaces + the redis-server binary"]
fn redis_server_version_is_deterministic_under_strict_verify() {
    let redis_server = required_app(
        "redis-server",
        &["/usr/bin/redis-server", "/usr/local/bin/redis-server"],
    );
    assert_l2_under_strict_verify(&redis_server, &["--version"]);
}

#[test]
#[ignore = "e2e: requires hermit + PMU/mount namespaces + a JVM"]
fn java_version_is_deterministic_under_strict_verify() {
    let java = required_app("java", &["/usr/local/bin/java", "/usr/bin/java"]);
    assert_l2_under_strict_verify(&java, &["-version"]);
}

// ---------------------------------------------------------------------------
// Go and JVM managed-runtime coverage.
//
// The compiled-guest tests build a tiny program with the host toolchain and run
// the artifact under Hermit, isolating managed-runtime determinism from
// compiler determinism. The `go`/`javac`/`java` binaries are installed by the
// self-hosted CI job, so a missing toolchain is a hard error, matching the
// `required_app` policy above.
// ---------------------------------------------------------------------------

const GO_HELLO_SRC: &str = "package main\n\
import \"fmt\"\n\
func main() { fmt.Println(\"hello from go\") }\n";

const GO_GOROUTINES_SRC: &str = r#"package main

import (
	"fmt"
	"sort"
	"sync"
)

func main() {
	const n = 8
	var wg sync.WaitGroup
	results := make([]int, n)
	for i := 0; i < n; i++ {
		wg.Add(1)
		go func(idx int) {
			defer wg.Done()
			results[idx] = idx * idx
		}(i)
	}
	wg.Wait()
	// Sort so the printed output does not depend on completion order.
	sort.Ints(results)
	sum := 0
	for _, v := range results {
		sum += v
	}
	fmt.Printf("goroutines sum=%d results=%v\n", sum, results)
}
"#;

const JAVA_HELLO_SRC: &str = "public class Hello {\n\
    public static void main(String[] args) {\n\
        System.out.println(\"hello from java\");\n\
    }\n\
}\n";

const JAVA_THREADS_SRC: &str = r#"import java.util.Arrays;

public class Threads {
    public static void main(String[] args) throws InterruptedException {
        final int n = 8;
        final int[] results = new int[n];
        Thread[] ts = new Thread[n];
        for (int i = 0; i < n; i++) {
            final int idx = i;
            ts[i] = new Thread(() -> results[idx] = idx * idx);
            ts[i].start();
        }
        for (Thread t : ts) {
            t.join();
        }
        int sum = 0;
        for (int v : results) {
            sum += v;
        }
        System.out.println("threads sum=" + sum + " results=" + Arrays.toString(results));
    }
}
"#;

/// Create (and clean) a per-test build directory under Cargo's target tmpdir.
///
/// Cargo's `CARGO_TARGET_TMPDIR` lives under `target/`, i.e. on the real
/// filesystem rather than Hermit's isolated guest `/tmp`, so guests launched
/// from here (and their classpath directories) are visible to the guest.
fn build_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("app-strict-verify")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir)
        .unwrap_or_else(|error| panic!("failed to create build dir {}: {error}", dir.display()));
    dir
}

/// Run a build command and panic with its full output on failure.
fn run_build(mut command: Command, label: &str) {
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {label} ({rendered}): {error}"));
    assert!(
        output.status.success(),
        "{label} failed ({rendered})\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// Compile a single-file Go program to a native binary with the host toolchain.
fn compile_go(source: &str, bin_name: &str) -> PathBuf {
    let go = required_app("go", &["/usr/bin/go", "/usr/local/bin/go"]);
    let dir = build_dir(bin_name);
    let src = dir.join("main.go");
    fs::write(&src, source).expect("failed to write Go source");
    let bin = dir.join(bin_name);

    let mut command = Command::new(go);
    command
        // Keep the build hermetic and offline: never fetch a toolchain or
        // modules, and keep the cache inside the throwaway build dir.
        .env("GOTOOLCHAIN", "local")
        .env("GO111MODULE", "off")
        .env("GOFLAGS", "")
        .env("GOCACHE", dir.join("gocache"))
        .args(["build", "-o"])
        .arg(&bin)
        .arg(&src);
    run_build(command, &format!("go build {bin_name}"));
    bin
}

/// Compile a single Java class with the host `javac`, returning the classpath
/// directory that holds the resulting `.class`.
fn compile_java(source: &str, class_name: &str) -> PathBuf {
    let javac = required_app("javac", &["/usr/local/bin/javac", "/usr/bin/javac"]);
    let dir = build_dir(class_name);
    let src = dir.join(format!("{class_name}.java"));
    fs::write(&src, source).expect("failed to write Java source");

    let mut command = Command::new(javac);
    command.arg("-d").arg(&dir).arg(&src);
    run_build(command, &format!("javac {class_name}.java"));
    dir
}

/// Run `hermit run --strict -- <program> <args>` once (no `--verify`) and return
/// the process output. Uses the same determinism-preserving relaxations as
/// [`assert_l2_under_strict_verify`].
fn run_once_under_strict(program: &Path, args: &[&str]) -> Output {
    let mut command = Command::new("timeout");
    command
        .args([
            "--kill-after",
            HERMIT_VERIFY_KILL_AFTER,
            HERMIT_VERIFY_TIMEOUT,
        ])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=off",
            "run",
            "--strict",
            "--no-virtualize-cpuid",
            "--preemption-timeout=disabled",
            "--",
        ])
        .arg(program)
        .args(args);

    let rendered = format!("{command:?}");
    command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"))
}

/// Assert assurance level L1 for a driver that does not reach L2: two separate
/// `--strict` runs must both succeed and produce identical stdout, but we do not
/// require `--verify` to agree (its internal two-run log diff diverges for these
/// toolchain drivers).
fn assert_l1_stdout_deterministic(program: &Path, args: &[&str]) {
    let _guard = hermit_run_lock();

    let first = run_once_under_strict(program, args);
    let second = run_once_under_strict(program, args);

    for (label, output) in [("run 1", &first), ("run 2", &second)] {
        assert!(
            output.status.success(),
            "hermit run --strict ({label}) failed for {} {args:?}\nstatus: {}\nstderr:\n{}",
            program.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr),
        );
    }

    assert_eq!(
        first.stdout,
        second.stdout,
        "hermit run --strict produced non-deterministic stdout (not even L1) for {} {args:?}\n\
         run 1 stdout:\n{}\nrun 2 stdout:\n{}",
        program.display(),
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&second.stdout),
    );
}

// --- L2: compiled managed-runtime programs are bitwise deterministic ---

#[test]
#[ignore = "e2e: requires hermit + PMU/mount namespaces + the Go toolchain"]
fn go_hello_is_deterministic_under_strict_verify() {
    let bin = compile_go(GO_HELLO_SRC, "hermit_go_hello");
    assert_l2_under_strict_verify(&bin, &[]);
}

#[test]
#[ignore = "e2e: requires hermit + PMU/mount namespaces + the Go toolchain"]
fn go_goroutines_are_deterministic_under_strict_verify() {
    let bin = compile_go(GO_GOROUTINES_SRC, "hermit_go_goroutines");
    assert_l2_under_strict_verify(&bin, &[]);
}

#[test]
#[ignore = "e2e: requires hermit + PMU/mount namespaces + a JDK"]
fn java_hello_is_deterministic_under_strict_verify() {
    let java = required_app("java", &["/usr/local/bin/java", "/usr/bin/java"]);
    let classpath = compile_java(JAVA_HELLO_SRC, "Hello");
    let classpath = classpath.to_str().expect("classpath is valid UTF-8");
    assert_l2_under_strict_verify(&java, &["-cp", classpath, "Hello"]);
}

#[test]
#[ignore = "e2e: requires hermit + PMU/mount namespaces + a JDK"]
fn java_threads_are_deterministic_under_strict_verify() {
    let java = required_app("java", &["/usr/local/bin/java", "/usr/bin/java"]);
    let classpath = compile_java(JAVA_THREADS_SRC, "Threads");
    let classpath = classpath.to_str().expect("classpath is valid UTF-8");
    assert_l2_under_strict_verify(&java, &["-cp", classpath, "Threads"]);
}

// --- L1: toolchain drivers are output-deterministic but not bitwise (no L2) ---

#[test]
#[ignore = "e2e: requires hermit + PMU/mount namespaces + the Go toolchain"]
fn go_version_is_l1_deterministic_under_strict() {
    // `go version` is output-deterministic under --strict but Hermit's --verify
    // reports it nondeterministic, so it is asserted at L1 only.
    let go = required_app("go", &["/usr/bin/go", "/usr/local/bin/go"]);
    assert_l1_stdout_deterministic(&go, &["version"]);
}

#[test]
#[ignore = "e2e: requires hermit + PMU/mount namespaces + a JDK"]
fn javac_is_l1_deterministic_under_strict() {
    // `javac` produces a bytewise-identical class file across --strict runs, but
    // Hermit's --verify reports it nondeterministic, so it is asserted at L1.
    // Compile into two separate output directories under --strict and compare
    // both the exit status (via `run_once_under_strict`) and the emitted class.
    let javac = required_app("javac", &["/usr/local/bin/javac", "/usr/bin/javac"]);

    let src_dir = build_dir("javac_l1_src");
    let src = src_dir.join("Hello.java");
    fs::write(&src, JAVA_HELLO_SRC).expect("failed to write Java source");

    let _guard = hermit_run_lock();

    let mut class_bytes: Vec<Vec<u8>> = Vec::new();
    for run in 0..2 {
        let out_dir = build_dir(&format!("javac_l1_out{run}"));
        let out_dir_str = out_dir.to_str().expect("out dir is valid UTF-8");
        let src_str = src.to_str().expect("src path is valid UTF-8");
        let output = run_once_under_strict(&javac, &["-d", out_dir_str, src_str]);
        assert!(
            output.status.success(),
            "hermit run --strict javac (run {run}) failed\nstatus: {}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
        );
        let class = out_dir.join("Hello.class");
        class_bytes.push(
            fs::read(&class)
                .unwrap_or_else(|error| panic!("javac did not emit {}: {error}", class.display())),
        );
    }

    assert_eq!(
        class_bytes[0], class_bytes[1],
        "javac emitted a non-deterministic class file across two --strict runs (not even L1)"
    );
}
