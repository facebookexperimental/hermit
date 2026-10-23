/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Actual Detcore/DBT capture and failure controls, not a capture-state model.
//! The existing runner owns process-group cleanup. Native test arguments force
//! specific boundaries; an outer harness timeout is never a successful control.

#![cfg(feature = "dbt")]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::io::Read;
use std::io::Seek;
use std::path::Path;
use std::process::Command;
use std::process::Output;

use reverie_dbt::DbtRunner;

const MARKER: &[u8] = b"MAPPING_CAPTURE_TEST ";

#[derive(Debug)]
struct NativeRecord(BTreeMap<String, String>);

impl NativeRecord {
    fn get(&self, key: &str) -> &str {
        self.0
            .get(key)
            .unwrap_or_else(|| panic!("missing native field {key}: {self:?}"))
    }

    fn unsigned(&self, key: &str) -> u64 {
        self.get(key)
            .parse()
            .unwrap_or_else(|_| panic!("invalid native integer {key}: {self:?}"))
    }

    fn exact_keys(&self, keys: &[&str]) {
        assert_eq!(
            self.0.len(),
            keys.len(),
            "unexpected native fields: {self:?}"
        );
        for key in keys {
            self.get(key);
        }
    }

    fn distinct_executors(&self) {
        let caller = self.unsigned("caller");
        let helper = self.unsigned("helper");
        assert!(
            caller > 0 && helper > 0,
            "missing actual executor: {self:?}"
        );
        assert_ne!(
            caller, helper,
            "caller and helper must be distinct: {self:?}"
        );
    }
}

struct Run {
    outcome: io::Result<Output>,
    records: Vec<NativeRecord>,
}

fn native_records(diagnostics: &[u8]) -> Vec<NativeRecord> {
    diagnostics
        .split(|byte| *byte == b'\n')
        .filter_map(|line| line.strip_prefix(MARKER))
        .map(|line| {
            let text = std::str::from_utf8(line).expect("native capture record is UTF-8");
            let mut fields = BTreeMap::new();
            for field in text.split_ascii_whitespace() {
                let (key, value) = field.split_once('=').expect("native key=value field");
                assert!(!key.is_empty() && !value.is_empty(), "empty native field");
                assert!(
                    fields.insert(key.to_owned(), value.to_owned()).is_none(),
                    "duplicate native field {key}"
                );
            }
            // Retain just the explicit non-sensitive boundary record in the
            // test transcript. Do not dump guest environment or raw proc data.
            eprintln!("MAPPING_CAPTURE_TEST {text}");
            NativeRecord(fields)
        })
        .collect()
}

fn run(mode: Option<&str>, program: &str, args: &[&str]) -> Run {
    run_with_containment(mode, program, args, true, false)
}

fn run_with_containment(
    mode: Option<&str>,
    program: &str,
    args: &[&str],
    isolated: bool,
    terminate: bool,
) -> Run {
    let _serial = super::hermit_run_lock();
    let (drrun, client) = detcore_dbt::prepare_native_client()
        .expect("prepare the normal Detcore-linked native client");
    // Match the strict procfs controls: standard procfs, sequentialized deterministic I/O,
    // fail-closed syscalls, an explicit epoch, disabled timeslice preemption
    // and CPUID virtualization. RCB time remains enabled.
    let config = detcore::Config {
        sequentialize_threads: true,
        deterministic_io: true,
        proc_level: detcore_model::config::ProcLevel::Standard,
        panic_on_unsupported_syscalls: true,
        shutdown_on_unsupported_syscall: true,
        signal_delivery_check_fatal: true,
        max_timeslice: None,
        virtualize_cpuid: false,
        epoch: "2026-01-01T00:00:00Z".parse().unwrap(),
        ..detcore::Config::default()
    };
    let mut config = hermit::prepare_backend_config(config, hermit::Backend::Dbt);
    hermit::declare_dbt_launcher(&mut config, &drrun).expect("declare actual DBT launcher");
    hermit::record_dbt_launched_image(&mut config, Path::new(program), Some(Path::new("/")), None);
    config.validate();
    let environment = BTreeMap::<OsString, OsString>::from([
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("LANG".into(), "C".into()),
        ("LC_ALL".into(), "C".into()),
        ("HERMIT_LOG".into(), "error".into()),
        (
            detcore_dbt::DETCONFIG_ENV.into(),
            hermit::dbt_detconfig_json(&config)
                .expect("encode the declared DBT config")
                .into(),
        ),
    ]);
    // Pending-completion errors can be returned after captured guest output
    // was collected. Keep native records in the runner's separate real channel.
    static NEXT_RUN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("mapping-capture-live");
    std::fs::create_dir_all(&root).expect("retain native control root");
    // PID namespaces can reuse a test PID in the next contained invocation.
    // Keep an exclusively created directory so earlier evidence is never replaced.
    let directory = tempfile::Builder::new()
        .prefix("capture-")
        .tempdir_in(&root)
        .expect("create native control directory")
        .keep();
    let number = NEXT_RUN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = directory.join(format!("{number}-{}.log", mode.unwrap_or("ordinary-exit")));
    let mut diagnostics = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
        .expect("native diagnostic file");
    eprintln!("mapping capture native diagnostics: {}", path.display());
    let mut runner = DbtRunner::new(&drrun, &client)
        .expect("normal DBT runner")
        .isolated_process_group(isolated)
        .terminate_process_group_on_exit(terminate)
        .tool_answers_brk(true)
        .client_argument("-panic-on-unsupported-syscalls")
        .diagnostic_file(&diagnostics)
        .expect("bind native diagnostic channel");
    if let Some(mode) = mode {
        runner = runner
            .client_argument("-test-mapping-capture")
            .client_argument(mode);
    }
    let mut guest = Command::new(program);
    guest.args(args).current_dir("/");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("controller runtime");
    let outcome = runtime.block_on(async {
        match runner
            .output_with_environment_and_global::<detcore::GlobalState>(
                &guest,
                &environment,
                config,
            )
            .await
        {
            Ok((output, mut global)) => {
                // Same scheduler cleanup as the normal CLI, including an
                // ordinary nonzero guest exit that remains an Ok(Output).
                if !output.status.success() {
                    global.force_shutdown_with_error();
                    global.cancel_internal_scheduler().await;
                }
                global
                    .clean_up(false, &None)
                    .await
                    .expect("scheduler cleanup after the guest's output");
                Ok(output)
            }
            Err(error) => Err(error),
        }
    });
    drop(runtime);
    // Preserve the returned outcome before any assertion can unwind. Native
    // diagnostics alone cannot explain a guest that exits before capture.
    match &outcome {
        Ok(output) => {
            std::fs::write(path.with_extension("stdout"), &output.stdout)
                .expect("retain guest stdout");
            std::fs::write(path.with_extension("stderr"), &output.stderr)
                .expect("retain guest stderr");
            std::fs::write(
                path.with_extension("status"),
                format!("{:?}\n", output.status),
            )
            .expect("retain guest status");
        }
        Err(error) => {
            std::fs::write(path.with_extension("error"), format!("{error}\n"))
                .expect("retain runner error");
        }
    }
    diagnostics.rewind().expect("rewind native diagnostics");
    let mut bytes = Vec::new();
    diagnostics
        .read_to_end(&mut bytes)
        .expect("read native diagnostics");
    Run {
        outcome,
        records: native_records(&bytes),
    }
}

fn run_numa(mode: &str) -> Run {
    // The cat image is the actual launch root; no shell, exec wrapper or extra
    // query is inserted in front of the operation being captured.
    run(Some(mode), "/bin/cat", &["/proc/self/numa_maps"])
}

fn restored_record(records: &[NativeRecord]) -> &NativeRecord {
    assert_eq!(
        records.len(),
        1,
        "expected exactly one native record: {records:?}"
    );
    let record = &records[0];
    record.exact_keys(&[
        "phase",
        "result",
        "acquire_ns",
        "exchange_ns",
        "restore_ns",
        "caller",
        "helper",
        "flag_clear",
    ]);
    assert_eq!(record.get("phase"), "restored");
    for field in ["acquire_ns", "exchange_ns", "restore_ns"] {
        record.unsigned(field);
    }
    record.distinct_executors();
    assert_eq!(
        record.get("flag_clear"),
        "1",
        "private fact flag crossed raw transport"
    );
    record
}

fn require_runner_error(outcome: io::Result<Output>) -> io::Error {
    match outcome {
        Err(error) => error,
        Ok(output) => panic!(
            "capture failure was accepted as guest status {:?} (stdout={} bytes, stderr={} bytes)",
            output.status,
            output.stdout.len(),
            output.stderr.len()
        ),
    }
}

#[test]
fn dbt_mapping_capture_requires_enabled_launcher_containment() {
    for terminate in [false, true] {
        let run = run_with_containment(
            None,
            "/bin/cat",
            &["/proc/self/numa_maps"],
            false,
            terminate,
        );
        let error = require_runner_error(run.outcome);
        assert!(
            error
                .to_string()
                .contains("enabled launcher containment is unavailable"),
            "unexpected uncontained refusal (terminate={terminate}): {error}"
        );
        // These are ordinary no-hook invocations. Absence of records is not
        // itself a park-state probe: pre-Requested rejection is source-ordered
        // by the named launcher-containment check exercised above.
        assert!(
            run.records.is_empty(),
            "uncontained invocation produced native test records"
        );
    }
}

#[test]
fn dbt_mapping_capture_restores_before_returning_numa_bytes() {
    let run = run_numa("observe");
    let output = run.outcome.expect("successful actual Detcore NUMA capture");
    assert!(
        output.status.success(),
        "NUMA guest status: {:?}",
        output.status
    );
    assert!(
        !output.stdout.is_empty(),
        "cat produced no NUMA guest bytes"
    );
    assert!(
        output.stderr.is_empty(),
        "unexpected guest stderr ({} bytes)",
        output.stderr.len()
    );
    let text = std::str::from_utf8(&output.stdout).expect("NUMA guest row framing");
    for row in text.lines() {
        let mut fields = row.split_ascii_whitespace();
        let start = fields.next().expect("NUMA start field");
        u64::from_str_radix(start, 16).expect("NUMA hexadecimal start");
        assert!(fields.next().is_some(), "NUMA row has no policy field");
    }
    assert_eq!(restored_record(&run.records).get("result"), "1");
}

fn cancelled(mode: &str) {
    let run = run_numa(mode);
    let record = restored_record(&run.records);
    assert!(
        record.get("result").parse::<i32>().unwrap() < 0,
        "cancellation accepted: {record:?}"
    );
    let error = require_runner_error(run.outcome);
    assert!(
        error
            .to_string()
            .contains("contained mapping capture did not complete restoration"),
        "not a pending-capture failure: {error}"
    );
}

#[test]
fn dbt_mapping_capture_requested_cancellation_refuses_guest_exit() {
    cancelled("cancel-requested");
}

#[test]
fn dbt_mapping_capture_entering_cancellation_refuses_guest_exit() {
    cancelled("cancel-entering");
}

fn terminal(mode: &str) {
    let run = run_numa(mode);
    assert!(
        run.records.is_empty(),
        "terminal path reported normal restoration: {:?}",
        run.records
    );
    let error = require_runner_error(run.outcome);
    assert!(
        error.to_string().contains("phase=controller-read"),
        "terminal fault did not reach the real controller read: {error}"
    );
}

#[test]
fn dbt_mapping_capture_parked_failure_is_a_runner_error() {
    terminal("fail-parked");
}

#[test]
fn dbt_mapping_capture_release_failure_is_a_runner_error() {
    terminal("fail-release");
}

#[test]
fn dbt_mapping_capture_final_owner_change_rejects_read_bytes_after_restoration() {
    // The native control changes only the final copied owner version, after
    // the actual controller Read. The returned bytes must not become Output.
    let run = run_numa("owner-final-changed");
    assert_eq!(restored_record(&run.records).get("result"), "-4");
    let error = require_runner_error(run.outcome);
    assert_eq!(
        error.to_string(),
        "contained mapping capture did not complete restoration (phase=controller-read)"
    );
}

#[test]
fn dbt_mapping_capture_caller_mask_restore_kernel_error_is_terminal() {
    // Inject an invalid address into the actual raw rt_sigprocmask restore.
    // This is kernel-error containment coverage, not async-signal evidence.
    terminal("caller-mask-failure");
}

#[test]
fn dbt_mapping_capture_helper_mask_restore_kernel_error_is_terminal() {
    // Inject an invalid address into the helper's actual raw mask restore.
    // This does not claim delivery or ordering coverage for async signals.
    terminal("helper-mask-failure");
}

#[test]
fn dbt_mapping_capture_preserves_exact_masks_and_test_created_pending_signals() {
    // The test blocks its four chosen signals in an explicit baseline before
    // Closed: SIGUSR1/SIGPIPE are then queued by each executor to itself, and
    // realtime 34/35 arrive from the caller while Parked. "Pending before
    // Closed" refers to these test-created signals, not ambient signals.
    // Native checks include exact physical masks and consumed SI_TKILL sender
    // identities. This does not claim arbitrary handler/allocation coverage.
    let mut run = run_numa("mask-pending");
    let output = run
        .outcome
        .expect("successful capture with pending test signals");
    assert!(
        output.status.success(),
        "NUMA guest status: {:?}",
        output.status
    );
    assert!(
        !output.stdout.is_empty(),
        "cat produced no NUMA guest bytes"
    );
    assert!(
        output.stderr.is_empty(),
        "unexpected guest stderr ({} bytes)",
        output.stderr.len()
    );
    assert_eq!(
        run.records.len(),
        2,
        "expected restored and signal-mask records"
    );
    let index = run
        .records
        .iter()
        .position(|record| record.get("phase") == "signal-masks")
        .expect("missing actual physical-mask and pending-signal record");
    let signals = run.records.remove(index);
    signals.exact_keys(&[
        "phase",
        "caller_full",
        "helper_full",
        "caller_restored",
        "helper_restored",
        "caller_pending",
        "helper_pending",
        "caller_original_mask",
        "caller_baseline_mask",
        "caller_blocked_mask",
        "caller_restored_mask",
        "caller_final_mask",
        "caller_initial_pending",
        "caller_prepared_pending",
        "caller_pending_before",
        "caller_pending_after",
        "caller_consumed",
        "helper_original_mask",
        "helper_baseline_mask",
        "helper_blocked_mask",
        "helper_restored_mask",
        "helper_final_mask",
        "helper_initial_pending",
        "helper_prepared_pending",
        "helper_pending_before",
        "helper_pending_after",
        "helper_consumed",
    ]);
    for field in [
        "caller_full",
        "helper_full",
        "caller_restored",
        "helper_restored",
        "caller_pending",
        "helper_pending",
    ] {
        assert_eq!(
            signals.get(field),
            "1",
            "native signal check failed: {signals:?}"
        );
    }
    // Linux x86-64 signal numbers, matching the explicit raw native fixture.
    let standard = (1_u64 << 9) | (1_u64 << 12); // SIGUSR1 and SIGPIPE.
    let selected = standard | (1_u64 << 33) | (1_u64 << 34);
    let full = !((1_u64 << 8) | (1_u64 << 18)); // Except KILL/STOP.
    for executor in ["caller", "helper"] {
        let number = |field: &str| signals.unsigned(&format!("{executor}_{field}"));
        let original = number("original_mask");
        let baseline = number("baseline_mask");
        let initial_pending = number("initial_pending");
        let pending_before = number("pending_before");
        let pending_after = number("pending_after");
        assert_eq!(baseline, original | selected, "{executor} baseline");
        assert_eq!(number("blocked_mask"), full, "{executor} Closed mask");
        assert_eq!(
            number("restored_mask"),
            baseline,
            "{executor} restored mask"
        );
        assert_eq!(number("final_mask"), original, "{executor} original mask");
        assert_eq!(initial_pending & selected, 0, "{executor} initial pending");
        assert_eq!(
            number("prepared_pending") & selected,
            standard,
            "{executor} test-created pending before Closed"
        );
        assert_eq!(pending_before & selected, selected, "{executor} pending");
        assert_eq!(number("consumed"), selected, "{executor} consumed signals");
        assert_eq!(pending_after & selected, 0, "{executor} selected drain");
        let unrelated = (initial_pending | pending_before) & !selected;
        assert_eq!(
            pending_after & unrelated,
            unrelated,
            "{executor} unrelated pending"
        );
    }
    assert_eq!(restored_record(&run.records).get("result"), "1");
}

#[test]
fn dbt_mapping_capture_extra_helper_exit_does_not_restore_eligibility() {
    let run = run_numa("extra-helper");
    assert_eq!(
        run.records.len(),
        1,
        "missing actual extra-helper lifecycle: {:?}",
        run.records
    );
    let record = &run.records[0];
    record.exact_keys(&["phase", "retired", "caller", "helper"]);
    assert_eq!(record.get("phase"), "extra-helper");
    assert_eq!(record.get("retired"), "1");
    record.distinct_executors();
    require_runner_error(run.outcome);
}

#[test]
fn dbt_mapping_capture_preserves_legitimate_nonzero_guest_exits() {
    for (isolated, terminate) in [(true, false), (false, false), (false, true)] {
        for status in [101, 125] {
            let script = format!("exit {status}");
            let run = run_with_containment(None, "/bin/sh", &["-c", &script], isolated, terminate);
            let output = run
                .outcome
                .expect("ordinary guest exit must remain an output");
            assert_eq!(output.status.code(), Some(status));
            assert!(output.stdout.is_empty() && output.stderr.is_empty());
            assert!(
                run.records.is_empty(),
                "ordinary exit ran a capture test hook"
            );
        }
    }
}

#[test]
fn dbt_mapping_capture_unregistered_token_never_requests_a_park() {
    let run = run_numa("forged-token");
    assert_eq!(run.records.len(), 1, "missing real authorization refusal");
    let record = &run.records[0];
    record.exact_keys(&["phase", "requested"]);
    assert_eq!(record.get("phase"), "authorization-refused");
    assert_eq!(record.unsigned("requested"), 0);
    let error = require_runner_error(run.outcome);
    assert!(error.to_string().contains("phase=prepared"), "{error}");
}

const PROJECTION_GUEST: &str = r#"
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

#define VIEW_CAPACITY (1024u * 1024u)
#define MAP_CAPACITY 8192u
static char maps_view[VIEW_CAPACITY + 1];
static char numa_view[VIEW_CAPACITY + 1];
static char executable[4096];
static struct {
    unsigned long start;
    unsigned long end;
    int executable;
} mappings[MAP_CAPACITY];

static int write_text(int fd, const char *text) {
    size_t length = strlen(text);
    while (length != 0) {
        ssize_t amount = write(fd, text, length);
        if (amount < 0 && errno == EINTR)
            continue;
        if (amount <= 0)
            return 0;
        text += amount;
        length -= (size_t)amount;
    }
    return 1;
}

static int fail(const char *reason) {
    (void)write_text(STDERR_FILENO, reason);
    return 91;
}

static int read_view(int fd, char *buffer) {
    size_t used = 0;
    while (used < VIEW_CAPACITY) {
        ssize_t amount = read(fd, buffer + used, VIEW_CAPACITY - used);
        if (amount < 0 && errno == EINTR)
            continue;
        if (amount < 0)
            return 0;
        if (amount == 0) {
            if (used == 0 || buffer[used - 1] != '\n' ||
                memchr(buffer, '\0', used) != NULL)
                return 0;
            buffer[used] = '\0';
            return 1;
        }
        used += (size_t)amount;
    }
    /* A full buffer has no proved EOF and is not a complete observation. */
    return 0;
}

static int projection(void) {
    ssize_t path_length = readlink("/proc/self/exe", executable,
                                   sizeof(executable) - 1);
    if (path_length <= 0 || (size_t)path_length >= sizeof(executable) - 1)
        return fail("executable link missing or truncated\n");
    executable[path_length] = '\0';
    /* The retained fixture path has no characters procfs must escape. */
    if (strpbrk(executable, " \t\n\\") != NULL)
        return fail("fixture executable path requires procfs escaping\n");

    int maps_fd = open("/proc/self/maps", O_RDONLY | O_CLOEXEC);
    int numa_fd = open("/proc/self/numa_maps", O_RDONLY | O_CLOEXEC);
    if (maps_fd < 0 || numa_fd < 0)
        return fail("cannot open Maps and NUMA observations\n");
    /* Fixed storage and read(2) only between the two complete observations:
       no stdio parsing, allocation, mmap, mprotect, or munmap in this interval. */
    if (!read_view(maps_fd, maps_view) || !read_view(numa_fd, numa_view))
        return fail("Maps or NUMA observation incomplete\n");
    (void)close(maps_fd);
    (void)close(numa_fd);

    size_t map_count = 0;
    size_t executable_count = 0;
    for (char *line = maps_view; *line != '\0';) {
        char *end = strchr(line, '\n');
        if (end == NULL || map_count == MAP_CAPACITY)
            return fail("Maps row framing or capacity\n");
        *end = '\0';
        unsigned long start, finish, offset, inode;
        unsigned int major, minor;
        char permissions[5];
        int path_offset = -1;
        if (sscanf(line, "%lx-%lx %4s %lx %x:%x %lu %n", &start, &finish,
                   permissions, &offset, &major, &minor, &inode,
                   &path_offset) != 7 || path_offset < 0 || start >= finish ||
            strlen(permissions) != 4 ||
            (map_count != 0 && mappings[map_count - 1].end > start))
            return fail("invalid or unordered Maps row\n");
        int is_executable = inode != 0 && permissions[2] == 'x' &&
                            strcmp(line + path_offset, executable) == 0;
        mappings[map_count].start = start;
        mappings[map_count].end = finish;
        mappings[map_count].executable = is_executable;
        executable_count += (size_t)is_executable;
        ++map_count;
        line = end + 1;
    }
    if (map_count == 0 || executable_count == 0)
        return fail("actual executable file mapping missing from Maps\n");

    size_t map_index = 0;
    size_t numa_count = 0;
    size_t executable_numa_count = 0;
    unsigned long previous = 0;
    for (char *line = numa_view; *line != '\0';) {
        char *end = strchr(line, '\n');
        if (end == NULL)
            return fail("NUMA row framing\n");
        *end = '\0';
        unsigned long start;
        char policy[256];
        int fields_offset = -1;
        if (sscanf(line, "%lx %255s %n", &start, policy, &fields_offset) != 2 ||
            fields_offset < 0 || (numa_count != 0 && start <= previous))
            return fail("invalid or unordered NUMA row\n");
        while (map_index < map_count && mappings[map_index].start < start)
            ++map_index;
        if (map_index == map_count || mappings[map_index].start != start)
            return fail("NUMA start absent from guest-visible Maps\n");
        char *saved = NULL;
        for (char *field = strtok_r(line + fields_offset, " \t", &saved);
             field != NULL; field = strtok_r(NULL, " \t", &saved)) {
            if (strncmp(field, "file=", 5) == 0 &&
                strcmp(field + 5, executable) == 0 &&
                mappings[map_index].executable)
                ++executable_numa_count;
        }
        previous = start;
        ++numa_count;
        line = end + 1;
    }
    if (numa_count == 0 || executable_numa_count == 0)
        return fail("actual executable file path missing from NUMA\n");
    return write_text(STDOUT_FILENO, "maps-numa-projection-ok\n") ? 0 : 92;
}

int main(int argc, char **argv) {
    if (argc == 1)
        return projection();
    if (argc != 3 || strcmp(argv[1], "chroot") != 0)
        return fail("invalid fixture arguments\n");
    /* Open pins the target; capture is lazy, on the first read below.  The
       empty new root deliberately has no procfs to reopen accidentally. */
    int fd = open("/proc/self/numa_maps", O_RDONLY | O_CLOEXEC);
    if (fd < 0)
        return fail("cannot preopen NUMA before chroot\n");
    if (chroot(argv[2]) != 0)
        return fail("chroot failed: CAP_SYS_CHROOT is required\n");
    if (chdir("/") != 0)
        return fail("chdir after chroot failed\n");
    ssize_t amount = read(fd, numa_view, sizeof(numa_view));
    (void)amount;
    return fail("NUMA read after chroot returned instead of runner refusal\n");
}
"#;

fn projection_guest() -> &'static Path {
    static GUEST: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    GUEST.get_or_init(|| {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("mapping-capture-live");
        std::fs::create_dir_all(&root).expect("retain projection fixture root");
        let directory = tempfile::Builder::new()
            .prefix("projection-")
            .tempdir_in(&root)
            .expect("create projection fixture directory")
            .keep();
        let source = directory.join("projection.c");
        let executable = directory.join("projection");
        std::fs::write(&source, PROJECTION_GUEST).expect("retain projection guest source");
        let compiled = Command::new("cc")
            .args(["-std=c11", "-O2", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(&source)
            .arg("-o")
            .arg(&executable)
            .output()
            .expect("compile projection guest");
        std::fs::write(directory.join("cc.stdout"), &compiled.stdout)
            .expect("retain projection compiler stdout");
        std::fs::write(directory.join("cc.stderr"), &compiled.stderr)
            .expect("retain projection compiler stderr");
        std::fs::write(
            directory.join("cc.status"),
            format!("{:?}\n", compiled.status),
        )
        .expect("retain projection compiler status");
        assert!(
            compiled.status.success(),
            "projection guest compilation failed; evidence: {}",
            directory.display()
        );
        executable
    })
}

#[test]
fn dbt_mapping_capture_projects_numa_without_test_hooks() {
    let guest = projection_guest().to_str().expect("UTF-8 fixture path");
    let run = run(None, guest, &[]);
    let output = run.outcome.expect("ordinary no-hook NUMA projection");
    assert!(
        output.status.success(),
        "projection status: {:?}",
        output.status
    );
    assert_eq!(output.stdout, b"maps-numa-projection-ok\n");
    assert!(
        output.stderr.is_empty(),
        "unexpected projection guest stderr"
    );
    assert!(
        run.records.is_empty(),
        "ordinary projection ran a native test hook"
    );
}

#[test]
fn dbt_mapping_capture_refuses_changed_reader_root_without_test_hooks() {
    let guest = projection_guest();
    let root = tempfile::Builder::new()
        .prefix("chroot-")
        .tempdir_in(guest.parent().expect("fixture directory"))
        .expect("create empty changed root")
        .keep();
    // This is an actual chroot and requires CAP_SYS_CHROOT. Missing privilege
    // must fail this control, never skip it or count an ordinary guest failure.
    let run = run(
        None,
        guest.to_str().expect("UTF-8 fixture path"),
        &["chroot", root.to_str().expect("UTF-8 changed-root path")],
    );
    assert!(
        run.records.is_empty(),
        "changed-root control ran a native test hook"
    );
    let error = require_runner_error(run.outcome);
    assert!(
        error
            .to_string()
            .contains("mapping reader root or mount namespace differs"),
        "not an authenticated reader-root refusal: {error}"
    );
}

const BIRTH_GUEST: &str = r#"
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

static int ready_pipe[2];
static int release_pipe[2];
static pthread_t siblings[8];
static char numa[4096];

static int write_all(int fd, const char *bytes, size_t length) {
    while (length != 0) {
        ssize_t amount = write(fd, bytes, length);
        if (amount < 0 && errno == EINTR)
            continue;
        if (amount <= 0)
            return 0;
        bytes += amount;
        length -= (size_t)amount;
    }
    return 1;
}

static int fail(const char *reason) {
    (void)write_all(STDERR_FILENO, reason, strlen(reason));
    return 91;
}

static int read_byte(int fd) {
    char byte;
    ssize_t amount;
    do {
        amount = read(fd, &byte, 1);
    } while (amount < 0 && errno == EINTR);
    return amount == 1 && byte == 'x';
}

static void *sibling(void *unused) {
    (void)unused;
    if (!write_all(ready_pipe[1], "x", 1) || !read_byte(release_pipe[0]))
        _exit(93);
    return NULL;
}

static int reap(pid_t child) {
    int status;
    pid_t result;
    do {
        result = waitpid(child, &status, 0);
    } while (result < 0 && errno == EINTR);
    return result == child && WIFEXITED(status) && WEXITSTATUS(status) == 0;
}

static int retain_stream(const char *path, int target) {
    int fd = open(path, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, 0600);
    if (fd < 0)
        return 0;
    int ok = dup2(fd, target) == target;
    if (close(fd) != 0)
        ok = 0;
    return ok;
}

int main(int argc, char **argv) {
    if (argc != 5)
        return fail("invalid birth fixture arguments\n");
    /* A Runner error has no Output value. Retain the real guest streams in
       owned files before setup so refusal cannot discard their evidence. */
    if (!retain_stream(argv[3], STDOUT_FILENO) ||
        !retain_stream(argv[4], STDERR_FILENO))
        return fail("cannot retain birth guest streams\n");

    unsigned int count = 0;
    const char *receipt = NULL;
    if (strcmp(argv[1], "fork") == 0) {
        pid_t child = fork();
        if (child == 0)
            _exit(0);
        if (child < 0 || !reap(child))
            return fail("fork child did not complete and reap\n");
        receipt = "fork-retired=1\n";
    } else if (strcmp(argv[1], "failed-clone") == 0) {
        /* Linux requires CLONE_VM when CLONE_SIGHAND is requested. This is
           an actual failed birth attempt, not an invented eligibility bit. */
        errno = 0;
        long child = syscall(SYS_clone, (unsigned long)(CLONE_SIGHAND | SIGCHLD),
                             0UL, 0UL, 0UL, 0UL);
        int error = errno;
        if (child == 0)
            _exit(0);
        if (child > 0) {
            (void)reap((pid_t)child);
            return fail("invalid clone unexpectedly created a child\n");
        }
        if (child != -1 || error != EINVAL)
            return fail("invalid clone did not return EINVAL\n");
        receipt = "clone-failed=EINVAL\n";
    } else {
        if (strcmp(argv[1], "threads-1") == 0) {
            count = 1;
            receipt = "siblings-ready=1\n";
        } else if (strcmp(argv[1], "threads-2") == 0) {
            count = 2;
            receipt = "siblings-ready=2\n";
        } else if (strcmp(argv[1], "threads-8") == 0) {
            count = 8;
            receipt = "siblings-ready=8\n";
        } else {
            return fail("unknown birth fixture mode\n");
        }
        if (pipe(ready_pipe) != 0 || pipe(release_pipe) != 0)
            return fail("cannot create sibling readiness pipes\n");
        for (unsigned int index = 0; index < count; ++index) {
            if (pthread_create(&siblings[index], NULL, sibling, NULL) != 0)
                return fail("cannot create all requested siblings\n");
        }
        for (unsigned int index = 0; index < count; ++index) {
            if (!read_byte(ready_pipe[0]))
                return fail("sibling did not report readiness\n");
        }
        /* No release byte is written until after the NUMA read below; all
           requested siblings are alive during that attempted capture. */
    }

    int proof = open(argv[2], O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, 0600);
    if (proof < 0 || !write_all(proof, receipt, strlen(receipt)) || close(proof) != 0)
        return fail("cannot retain successful birth setup\n");
    if (!write_all(STDOUT_FILENO, receipt, strlen(receipt)))
        return fail("cannot report successful birth setup\n");

    int fd = open("/proc/self/numa_maps", O_RDONLY | O_CLOEXEC);
    if (fd < 0)
        return fail("cannot open NUMA after birth setup\n");
    ssize_t amount = read(fd, numa, sizeof(numa));
    (void)amount;
    (void)close(fd);
    for (unsigned int index = 0; index < count; ++index) {
        if (!write_all(release_pipe[1], "x", 1))
            return fail("cannot release sibling after unexpected NUMA return\n");
    }
    for (unsigned int index = 0; index < count; ++index) {
        if (pthread_join(siblings[index], NULL) != 0)
            return fail("cannot join sibling after unexpected NUMA return\n");
    }
    /* A guest errno, ordinary exit, or successful read is not the required
       typed Runner refusal. Keep all such outcomes as failed controls. */
    return fail("NUMA after birth returned instead of runner refusal\n");
}
"#;

fn birth_guest() -> &'static Path {
    static GUEST: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    GUEST.get_or_init(|| {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("mapping-capture-live");
        std::fs::create_dir_all(&root).expect("retain birth fixture root");
        let directory = tempfile::Builder::new()
            .prefix("birth-")
            .tempdir_in(&root)
            .expect("create birth fixture directory")
            .keep();
        let source = directory.join("birth.c");
        let executable = directory.join("birth");
        std::fs::write(&source, BIRTH_GUEST).expect("retain birth guest source");
        let compiled = Command::new("cc")
            .args([
                "-std=c11", "-O2", "-g", "-Wall", "-Wextra", "-Werror", "-pthread",
            ])
            .arg(&source)
            .arg("-o")
            .arg(&executable)
            .output()
            .expect("compile birth guest");
        std::fs::write(directory.join("cc.stdout"), &compiled.stdout)
            .expect("retain birth compiler stdout");
        std::fs::write(directory.join("cc.stderr"), &compiled.stderr)
            .expect("retain birth compiler stderr");
        std::fs::write(
            directory.join("cc.status"),
            format!("{:?}\n", compiled.status),
        )
        .expect("retain birth compiler status");
        assert!(
            compiled.status.success(),
            "birth guest compilation failed; evidence: {}",
            directory.display()
        );
        executable
    })
}

fn check_birth_attempt_keeps_the_domain_unavailable(case: &str, setup: &str) {
    let guest = birth_guest();
    let directory = tempfile::Builder::new()
        .prefix(&format!("{case}-"))
        .tempdir_in(guest.parent().expect("birth fixture directory"))
        .expect("create birth control evidence directory")
        .keep();
    let setup_path = directory.join("setup");
    let stdout_path = directory.join("stdout");
    let stderr_path = directory.join("stderr");
    eprintln!("mapping capture birth evidence: {}", directory.display());
    let run = run(
        Some("birth-domain"),
        guest.to_str().expect("UTF-8 birth fixture path"),
        &[
            case,
            setup_path.to_str().expect("UTF-8 birth setup path"),
            stdout_path.to_str().expect("UTF-8 birth stdout path"),
            stderr_path.to_str().expect("UTF-8 birth stderr path"),
        ],
    );
    assert_eq!(
        std::fs::read(&setup_path).expect("actual successful birth setup receipt"),
        setup.as_bytes(),
        "incomplete or incorrect birth setup for {case}"
    );
    assert_eq!(
        std::fs::read(&stdout_path).expect("retained actual birth guest stdout"),
        setup.as_bytes(),
        "birth guest did not reach its exact pre-NUMA boundary for {case}"
    );
    assert!(
        std::fs::read(&stderr_path)
            .expect("retained actual birth guest stderr")
            .is_empty(),
        "birth guest reported a setup or unexpected-return error for {case}"
    );

    let error = require_runner_error(run.outcome);
    let mut withdrawals = 0;
    let mut native_refusals = 0;
    for record in &run.records {
        match record.get("phase") {
            "birth-withdraw" => {
                record.exact_keys(&["phase", "syscall", "requested"]);
                let syscall = record.unsigned("syscall");
                assert!(
                    [libc::SYS_clone, libc::SYS_fork, libc::SYS_clone3]
                        .into_iter()
                        .any(|expected| syscall == expected as u64),
                    "not an actual fixture birth syscall: {record:?}"
                );
                assert_eq!(record.unsigned("requested"), 0);
                withdrawals += 1;
            }
            "birth-refused" => {
                record.exact_keys(&["phase", "requested"]);
                assert_eq!(record.unsigned("requested"), 0);
                native_refusals += 1;
            }
            _ => panic!("unexpected birth control phase for {case}: {record:?}"),
        }
    }
    assert!(withdrawals > 0, "no real pre-syscall withdrawal for {case}");
    assert!(native_refusals <= 1, "repeated capture refusal for {case}");
    let error_text = error.to_string();
    let shared_refusal = error_text.contains("runtime-private numa_maps view unavailable:");
    let pending_refusal = error_text
        .contains("contained mapping capture did not complete restoration (phase=prepared)")
        || error_text.contains(
            "contained mapping capture did not complete restoration (phase=controller-read)",
        );
    assert!(
        shared_refusal || pending_refusal,
        "not a named mapping-domain refusal for {case}: {error}"
    );
    // Prepare can refuse before native operation0. An absent birth-refused
    // marker is therefore distinct from a successful native capture; all
    // accepted record variants above require requested=0 and forbid a
    // restored-success marker. Preserve the actual reason, not a relabel.
    eprintln!(
        "birth control {case}: native_operation0_refused={} runner_error={error}",
        native_refusals == 1
    );
}

#[test]
fn dbt_mapping_capture_birth_fork_keeps_the_domain_unavailable() {
    check_birth_attempt_keeps_the_domain_unavailable("fork", "fork-retired=1\n");
}

#[test]
fn dbt_mapping_capture_birth_failed_clone_keeps_the_domain_unavailable() {
    check_birth_attempt_keeps_the_domain_unavailable("failed-clone", "clone-failed=EINVAL\n");
}

#[test]
fn dbt_mapping_capture_birth_one_thread_keeps_the_domain_unavailable() {
    check_birth_attempt_keeps_the_domain_unavailable("threads-1", "siblings-ready=1\n");
}

#[test]
fn dbt_mapping_capture_birth_two_threads_keeps_the_domain_unavailable() {
    check_birth_attempt_keeps_the_domain_unavailable("threads-2", "siblings-ready=2\n");
}

#[test]
fn dbt_mapping_capture_birth_eight_threads_keeps_the_domain_unavailable() {
    check_birth_attempt_keeps_the_domain_unavailable("threads-8", "siblings-ready=8\n");
}

const TOPOLOGY_GUEST: &str = r#"
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <unistd.h>

#define CAPACITY (1024u * 1024u)
static char first[CAPACITY];
static char next[CAPACITY];

static int fail(void) {
    static const char message[] = "private-split NUMA comparison failed\n";
    ssize_t written = write(STDERR_FILENO, message, sizeof(message) - 1);
    (void)written;
    return 91;
}

/* Failure-only evidence: these are the two buffers already returned by the
   unchanged observations. No additional proc file is opened or read. A missing
   end marker means an output write failed; it never changes the failure code. */
static int write_evidence(const char *bytes, size_t length) {
    unsigned interruptions = 0;
    while (length != 0) {
        ssize_t written = write(STDOUT_FILENO, bytes, length);
        if (written < 0 && errno == EINTR && interruptions++ < 8)
            continue;
        if (written <= 0)
            return 0;
        bytes += (size_t)written;
        length -= (size_t)written;
    }
    return 1;
}

static void retain_mismatch(size_t first_length, size_t next_length) {
    static const char begin[] = "NUMA_FIRST_BEGIN\n";
    static const char between[] = "NUMA_FIRST_END\nNUMA_NEXT_BEGIN\n";
    static const char end[] = "NUMA_NEXT_END\n";
    if (!write_evidence(begin, sizeof(begin) - 1) ||
        !write_evidence(first, first_length) ||
        !write_evidence(between, sizeof(between) - 1) ||
        !write_evidence(next, next_length))
        return;
    (void)write_evidence(end, sizeof(end) - 1);
}

static int observe(char *buffer, size_t *length) {
    int fd = open("/proc/self/numa_maps", O_RDONLY | O_CLOEXEC);
    if (fd < 0)
        return 0;
    size_t used = 0;
    while (used < CAPACITY) {
        ssize_t amount = read(fd, buffer + used, CAPACITY - used);
        if (amount < 0 && errno == EINTR)
            continue;
        if (amount < 0)
            break;
        if (amount == 0) {
            int complete = used != 0 && buffer[used - 1] == '\n' &&
                           memchr(buffer, '\0', used) == NULL;
            if (close(fd) != 0)
                return 0;
            *length = used;
            return complete;
        }
        used += (size_t)amount;
    }
    (void)close(fd);
    return 0; /* Error or full buffer without proved EOF. */
}

int main(void) {
    /* Keep the guest observation storage resident before the first capture.
       Volatile writes cannot be removed by the optimizer. Otherwise the first
       read itself faults anonymous BSS pages and legitimately adds the retained
       kernelpagesize_kB field to the next kernel NUMA view. */
    long page_size = sysconf(_SC_PAGESIZE);
    if (page_size <= 0 || (unsigned long)page_size > CAPACITY)
        return fail();
    for (size_t offset = 0; offset < CAPACITY; offset += (size_t)page_size) {
        ((volatile char *)first)[offset] = 0;
        ((volatile char *)next)[offset] = 0;
    }
    ((volatile char *)first)[CAPACITY - 1] = 0;
    ((volatile char *)next)[CAPACITY - 1] = 0;
    size_t first_length = 0;
    /* The guest never allocates or changes mappings between observations.
       Each close/open forces a fresh NUMA snapshot, rather than rereading one
       descriptor's already-rendered contents. The native test alone changes
       its certified-private VMA protections before each of these captures. */
    if (!observe(first, &first_length))
        return fail();
    for (int repetition = 0; repetition < 2; ++repetition) {
        size_t length = 0;
        if (!observe(next, &length))
            return fail();
        if (length != first_length || memcmp(first, next, length) != 0) {
            retain_mismatch(first_length, length);
            return fail();
        }
    }
    static const char success[] = "numa-private-split-ok\n";
    ssize_t written = write(STDOUT_FILENO, success, sizeof(success) - 1);
    return written == (ssize_t)(sizeof(success) - 1) ? 0 : 92;
}
"#;

fn topology_guest() -> &'static Path {
    static GUEST: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    GUEST.get_or_init(|| {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("mapping-capture-live");
        std::fs::create_dir_all(&root).expect("retain topology fixture root");
        let directory = tempfile::Builder::new()
            .prefix("topology-")
            .tempdir_in(&root)
            .expect("create topology fixture directory")
            .keep();
        let source = directory.join("topology.c");
        let executable = directory.join("topology");
        std::fs::write(&source, TOPOLOGY_GUEST).expect("retain topology guest source");
        let compiled = Command::new("cc")
            .args(["-std=c11", "-O2", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(&source)
            .arg("-o")
            .arg(&executable)
            .output()
            .expect("compile topology guest");
        std::fs::write(directory.join("cc.stdout"), &compiled.stdout)
            .expect("retain topology compiler stdout");
        std::fs::write(directory.join("cc.stderr"), &compiled.stderr)
            .expect("retain topology compiler stderr");
        std::fs::write(
            directory.join("cc.status"),
            format!("{:?}\n", compiled.status),
        )
        .expect("retain topology compiler status");
        assert!(
            compiled.status.success(),
            "topology guest compilation failed; evidence: {}",
            directory.display()
        );
        executable
    })
}

#[test]
fn dbt_mapping_capture_private_vma_split_preserves_projected_numa() {
    let guest = topology_guest().to_str().expect("UTF-8 fixture path");
    let run = run(Some("private-split"), guest, &[]);
    let output = run.outcome.expect("three actual private-topology captures");
    assert!(
        output.status.success(),
        "topology status: {:?}",
        output.status
    );
    assert_eq!(output.stdout, b"numa-private-split-ok\n");
    assert!(output.stderr.is_empty(), "unexpected topology guest stderr");
    assert_eq!(
        run.records.len(),
        6,
        "three restored and three topology records"
    );
    let mut topology = Vec::new();
    let mut restored = 0;
    for record in &run.records {
        match record.get("phase") {
            "restored" => {
                assert_eq!(
                    restored_record(std::slice::from_ref(record)).get("result"),
                    "1"
                );
                restored += 1;
            }
            "private-split" => topology.push(record),
            other => panic!("unexpected topology control phase: {other}"),
        }
    }
    assert_eq!(restored, 3);
    assert_eq!(topology.len(), 3);
    for (index, record) in topology.iter().enumerate() {
        record.exact_keys(&[
            "phase",
            "step",
            "address",
            "page_size",
            "owned",
            "left_start",
            "left_end",
            "left_prot",
            "middle_start",
            "middle_end",
            "middle_prot",
            "right_start",
            "right_end",
            "right_prot",
        ]);
        assert_eq!(record.unsigned("step"), index as u64 + 1);
        assert_eq!(
            record.get("owned"),
            "1",
            "missing actual private-range proof"
        );
    }
    let address = topology[0].unsigned("address");
    let page = topology[0].unsigned("page_size");
    assert!(address != 0 && page.is_power_of_two());
    assert_eq!(address % page, 0);
    let boundary = |pages: u64| {
        address
            .checked_add(page.checked_mul(pages).expect("page-count overflow"))
            .expect("topology address overflow")
    };
    let extent = |record: &NativeRecord, name: &str| {
        (
            record.unsigned(&format!("{name}_start")),
            record.unsigned(&format!("{name}_end")),
            record.unsigned(&format!("{name}_prot")),
        )
    };
    let initial = extent(topology[0], "left");
    assert!(initial.0 <= address && initial.1 >= boundary(7));
    assert_eq!(
        initial.2, 1,
        "initial actual kernel protection must be READ"
    );
    assert_eq!(extent(topology[0], "middle"), initial);
    assert_eq!(extent(topology[0], "right"), initial);
    for record in &topology[1..] {
        assert_eq!(record.unsigned("address"), address);
        assert_eq!(record.unsigned("page_size"), page);
    }
    assert_eq!(extent(topology[1], "left"), (initial.0, boundary(2), 1));
    assert_eq!(extent(topology[1], "middle"), (boundary(2), boundary(5), 3));
    assert_eq!(extent(topology[1], "right"), (boundary(5), initial.1, 1));
    // mprotect restores permissions; Linux need not remerge anonymous VMAs.
    // Prove complete original-span coverage by actual whole-owned READ extents,
    // with no holes or partial overlaps. Keep the real middle RW split above
    // and all three exact guest byte comparisons unchanged.
    let mut covered = initial.0;
    let mut previous = None;
    for (name, sample) in [
        ("left", address),
        ("middle", boundary(2)),
        ("right", boundary(5)),
    ] {
        let current = extent(topology[2], name);
        assert!(initial.0 <= current.0 && current.0 <= sample && sample < current.1);
        assert!(current.1 <= initial.1);
        assert_eq!(current.2, 1, "restored kernel protection must be READ");
        if current.0 < covered {
            assert_eq!(
                Some(current),
                previous,
                "partially overlapping restored VMAs"
            );
        } else {
            assert_eq!(current.0, covered, "hole in restored private span");
        }
        covered = current.1;
        previous = Some(current);
    }
    assert_eq!(covered, initial.1, "incomplete restored private span");
}

fn take_phase(records: &mut Vec<NativeRecord>, phase: &str) -> NativeRecord {
    assert_eq!(
        records
            .iter()
            .filter(|record| record.get("phase") == phase)
            .count(),
        1,
        "expected one {phase} record: {records:?}"
    );
    let index = records
        .iter()
        .position(|record| record.get("phase") == phase)
        .unwrap();
    records.remove(index)
}

fn require_numa_output(outcome: io::Result<Output>) {
    let output = outcome.expect("successful actual NUMA capture");
    assert!(
        output.status.success(),
        "NUMA guest status: {:?}",
        output.status
    );
    assert!(
        !output.stdout.is_empty(),
        "missing actual NUMA guest output"
    );
    assert!(output.stderr.is_empty(), "unexpected NUMA guest stderr");
    let text = std::str::from_utf8(&output.stdout).expect("NUMA guest row framing");
    for row in text.lines() {
        let mut fields = row.split_ascii_whitespace();
        u64::from_str_radix(fields.next().expect("NUMA start"), 16)
            .expect("NUMA hexadecimal start");
        assert!(fields.next().is_some(), "NUMA row without a policy");
    }
}

fn transport_record(records: &mut Vec<NativeRecord>) -> NativeRecord {
    let record = take_phase(records, "transport");
    record.exact_keys(&[
        "phase",
        "send_calls",
        "recv_calls",
        "send_success",
        "recv_success",
        "send_bytes",
        "recv_bytes",
        "max_send",
        "max_recv",
        "partial",
        "last_result",
    ]);
    for field in [
        "send_calls",
        "recv_calls",
        "send_success",
        "recv_success",
        "send_bytes",
        "recv_bytes",
        "max_send",
        "max_recv",
    ] {
        record.unsigned(field);
    }
    record
        .get("last_result")
        .parse::<i64>()
        .expect("signed transport result");
    record
}

fn require_controlled_signal_masks(signals: &NativeRecord) {
    signals.exact_keys(&[
        "phase",
        "caller_full",
        "helper_full",
        "caller_restored",
        "helper_restored",
        "caller_pending",
        "helper_pending",
        "caller_original_mask",
        "caller_baseline_mask",
        "caller_blocked_mask",
        "caller_restored_mask",
        "caller_final_mask",
        "caller_initial_pending",
        "caller_prepared_pending",
        "caller_pending_before",
        "caller_pending_after",
        "caller_consumed",
        "helper_original_mask",
        "helper_baseline_mask",
        "helper_blocked_mask",
        "helper_restored_mask",
        "helper_final_mask",
        "helper_initial_pending",
        "helper_prepared_pending",
        "helper_pending_before",
        "helper_pending_after",
        "helper_consumed",
    ]);
    assert_eq!(signals.get("phase"), "signal-masks");
    for field in [
        "caller_full",
        "helper_full",
        "caller_restored",
        "helper_restored",
        "caller_pending",
        "helper_pending",
    ] {
        assert_eq!(signals.get(field), "1", "native signal check: {signals:?}");
    }
    // Same controlled baseline as the existing mask-pending identity: these
    // standard signals were deliberately queued before Closed, not inherited
    // from arbitrary ambient handlers. EPIPE must not consume the queued PIPE.
    let standard = (1_u64 << 9) | (1_u64 << 12);
    let selected = standard | (1_u64 << 33) | (1_u64 << 34);
    let full = !((1_u64 << 8) | (1_u64 << 18));
    for executor in ["caller", "helper"] {
        let number = |field: &str| signals.unsigned(&format!("{executor}_{field}"));
        let original = number("original_mask");
        let baseline = number("baseline_mask");
        let initial_pending = number("initial_pending");
        let pending_before = number("pending_before");
        let pending_after = number("pending_after");
        assert_eq!(baseline, original | selected, "{executor} signal baseline");
        assert_eq!(number("blocked_mask"), full, "{executor} Closed mask");
        assert_eq!(
            number("restored_mask"),
            baseline,
            "{executor} restored mask"
        );
        assert_eq!(number("final_mask"), original, "{executor} original mask");
        assert_eq!(initial_pending & selected, 0, "{executor} initial pending");
        assert_eq!(number("prepared_pending") & selected, standard);
        assert_eq!(pending_before & selected, selected, "{executor} pending");
        assert_eq!(number("consumed"), selected, "{executor} consumed signals");
        assert_eq!(pending_after & selected, 0, "{executor} selected drain");
        let unrelated = (initial_pending | pending_before) & !selected;
        assert_eq!(
            pending_after & unrelated,
            unrelated,
            "{executor} unrelated pending"
        );
    }
}

#[test]
fn dbt_mapping_capture_preflight_faults_never_request_a_park() {
    // Explicit preflight fault controls; neither a failed birth nor actual
    // concurrent registry contention is established by these modes.
    for mode in ["stale-nonce", "generation-exhausted", "owner-busy"] {
        let run = run_numa(mode);
        assert_eq!(run.records.len(), 1, "unexpected records for {mode}");
        let record = &run.records[0];
        record.exact_keys(&["phase", "requested", "reason"]);
        assert_eq!(record.get("phase"), "preflight-refused");
        assert_eq!(record.unsigned("requested"), 0);
        assert_eq!(record.get("reason"), mode);
        require_runner_error(run.outcome);
    }
}

#[test]
fn dbt_mapping_capture_partial_frames_complete_actual_transport() {
    let mut run = run_numa("partial-frames");
    let transport = transport_record(&mut run.records);
    assert_eq!(transport.get("partial"), "1");
    // Successful returns and transferred bytes cover only Closed exchange,
    // excluding authorization. Header/body calls alone cannot satisfy this:
    // each direction must transfer more than 11 bytes in chunks of at most 7.
    for (calls, successes, bytes, maximum) in [
        ("send_calls", "send_success", "send_bytes", "max_send"),
        ("recv_calls", "recv_success", "recv_bytes", "max_recv"),
    ] {
        let calls = transport.unsigned(calls);
        let successes = transport.unsigned(successes);
        let bytes = transport.unsigned(bytes);
        let maximum = transport.unsigned(maximum);
        assert!(bytes > 11, "insufficient actual transfer: {transport:?}");
        assert!((1..=7).contains(&maximum), "non-partial I/O: {transport:?}");
        assert!(successes > 2, "header/body calls alone: {transport:?}");
        assert!(
            successes >= bytes.div_ceil(7),
            "missing transfers: {transport:?}"
        );
        assert!(
            successes <= bytes,
            "non-positive successful I/O: {transport:?}"
        );
        assert!(
            calls >= successes,
            "successes exceed attempts: {transport:?}"
        );
    }
    assert!(transport.get("last_result").parse::<i64>().unwrap() > 0);
    assert_eq!(restored_record(&run.records).get("result"), "1");
    require_numa_output(run.outcome);
}

#[test]
fn dbt_mapping_capture_primary_shutdowns_restore_and_refuse() {
    // Native controls use real shutdown of the authenticated primary socket;
    // they do not replace/close it or substitute an error-returning transport.
    for (mode, expected) in [("transport-epipe", -32_i64), ("transport-eof", 0)] {
        let mut run = run_numa(mode);
        if mode == "transport-epipe" {
            let signals = take_phase(&mut run.records, "signal-masks");
            require_controlled_signal_masks(&signals);
        }
        let transport = transport_record(&mut run.records);
        assert_eq!(transport.get("partial"), "0");
        assert_eq!(
            transport.get("last_result").parse::<i64>().unwrap(),
            expected
        );
        if mode == "transport-epipe" {
            assert!(transport.unsigned("send_calls") > 0, "no actual EPIPE send");
        } else {
            assert!(
                transport.unsigned("recv_calls") > 0,
                "no actual EOF receive"
            );
        }
        assert_eq!(restored_record(&run.records).get("result"), "-3");
        require_runner_error(run.outcome);
    }
}

#[test]
fn dbt_mapping_capture_closed_observer_rejects_actual_allocation() {
    for (mode, injected) in [("closed-observer", false), ("closed-allocation", true)] {
        let mut run = run_numa(mode);
        let observer = take_phase(&mut run.records, "closed-observer");
        observer.exact_keys(&[
            "phase",
            "allocations",
            "frees",
            "mappings",
            "protections",
            "storage_owned",
        ]);
        assert_eq!(observer.get("storage_owned"), "1");
        let allocations = observer.unsigned("allocations");
        let frees = observer.unsigned("frees");
        let mappings = observer.unsigned("mappings");
        let protections = observer.unsigned("protections");
        if injected {
            // The injected dr_global_alloc/free may grow the heap as well;
            // no zero mapping/protection count is assumed for this fault.
            assert!(
                allocations > 0 && frees > 0,
                "actual allocation/free not observed"
            );
            assert_eq!(restored_record(&run.records).get("result"), "-4");
            require_runner_error(run.outcome);
        } else {
            assert_eq!((allocations, frees, mappings, protections), (0, 0, 0, 0));
            assert_eq!(restored_record(&run.records).get("result"), "1");
            require_numa_output(run.outcome);
        }
    }
}

#[test]
fn dbt_mapping_capture_stale_generation_after_read_is_terminal() {
    // Corrupts only the saved caller generation after the actual Read. This
    // is not evidence for delayed signal delivery or a real failed birth.
    terminal("stale-generation");
}

fn require_incomplete_phase(outcome: io::Result<Output>, phase: &str) {
    let error = require_runner_error(outcome);
    assert!(
        error.to_string().contains(&format!(
            "contained mapping capture did not complete restoration (phase={phase})"
        )),
        "not the expected incomplete {phase} capture: {error}"
    );
}

#[test]
fn dbt_mapping_capture_rejects_application_role_and_contains_helper_exit() {
    let mut app = run_numa("app-role");
    let record = take_phase(&mut app.records, "app-role");
    record.exact_keys(&["phase", "refused"]);
    assert_eq!(record.get("refused"), "1");
    assert_eq!(restored_record(&app.records).get("result"), "1");
    require_numa_output(app.outcome);

    let helper = run_numa("helper-exit");
    assert_eq!(helper.records.len(), 1, "unexpected helper-exit records");
    helper.records[0].exact_keys(&["phase"]);
    assert_eq!(helper.records[0].get("phase"), "helper-exit");
    // The sole record comes from the explicit protected raw diagnostic. A
    // normal restored record would contradict this terminal control.
    require_incomplete_phase(helper.outcome, "controller-read");
}

#[test]
fn dbt_mapping_capture_revalidates_final_owner_registry_and_image() {
    for (mode, owner_held, registry_held, image_changed, phase) in [
        ("owner-final-busy", 1, 0, 0, "controller-read"),
        ("registry-busy", 0, 1, 0, "prepared"),
        ("image-changed", 0, 0, 1, "controller-read"),
    ] {
        let mut run = run_numa(mode);
        let record = take_phase(&mut run.records, "final-input");
        record.exact_keys(&["phase", "owner_held", "registry_held", "image_changed"]);
        assert_eq!(record.unsigned("owner_held"), owner_held);
        assert_eq!(record.unsigned("registry_held"), registry_held);
        assert_eq!(record.unsigned("image_changed"), image_changed);
        assert_eq!(restored_record(&run.records).get("result"), "-4");
        require_incomplete_phase(run.outcome, phase);
    }
}

fn raw_leaves_record(records: &mut Vec<NativeRecord>) -> NativeRecord {
    let record = take_phase(records, "raw-leaves");
    record.exact_keys(&[
        "phase",
        "seen",
        "blocked",
        "injected",
        "caller_stack_start",
        "caller_stack_end",
        "helper_stack_start",
        "helper_stack_end",
        "storage_owned",
    ]);
    let caller = (
        record.unsigned("caller_stack_start"),
        record.unsigned("caller_stack_end"),
    );
    let helper = (
        record.unsigned("helper_stack_start"),
        record.unsigned("helper_stack_end"),
    );
    assert!(caller.0 > 0 && caller.0 < caller.1, "empty caller stack");
    assert!(helper.0 > 0 && helper.0 < helper.1, "empty helper stack");
    assert_ne!(caller, helper, "caller/helper storage witnesses alias");
    assert_eq!(record.get("storage_owned"), "1");
    record.unsigned("seen");
    record.unsigned("blocked");
    record.unsigned("injected");
    record
}

#[test]
fn dbt_mapping_capture_raw_leaves_require_the_private_flag_clear() {
    // These bits identify actual raw operations observed by the SDK: SEND,
    // RECEIVE, and CLOCK. WAIT/POLL are timing-dependent on a successful run.
    const SEND: u64 = 1 << 0;
    const RECEIVE: u64 = 1 << 1;
    const CLOCK: u64 = 1 << 3;

    let mut clean = run_numa("raw-flag-clear");
    let record = raw_leaves_record(&mut clean.records);
    assert_eq!(
        record.unsigned("seen") & (SEND | RECEIVE | CLOCK),
        SEND | RECEIVE | CLOCK
    );
    assert_eq!(record.unsigned("blocked"), 0);
    assert_eq!(record.unsigned("injected"), 0);
    assert_eq!(restored_record(&clean.records).get("result"), "1");
    require_numa_output(clean.outcome);

    let mut held = run_numa("raw-flag-held");
    let record = raw_leaves_record(&mut held.records);
    assert_eq!(record.unsigned("seen") & SEND, SEND);
    assert_eq!(
        record.unsigned("blocked"),
        1,
        "first blocked leaf must be SEND enum+1"
    );
    assert_eq!(record.unsigned("injected"), 1);
    // This explicit mutation is the only case that expects flag_clear=0.
    // Do not weaken the shared restored_record() flag-clear assertion.
    let restored = take_phase(&mut held.records, "restored");
    assert!(held.records.is_empty(), "unexpected held-flag records");
    restored.exact_keys(&[
        "phase",
        "result",
        "acquire_ns",
        "exchange_ns",
        "restore_ns",
        "caller",
        "helper",
        "flag_clear",
    ]);
    assert_eq!(restored.get("result"), "-3");
    assert_eq!(restored.get("flag_clear"), "0");
    for field in ["acquire_ns", "exchange_ns", "restore_ns"] {
        restored.unsigned(field);
    }
    restored.distinct_executors();
    require_incomplete_phase(held.outcome, "prepared");
}

#[test]
fn dbt_mapping_capture_stalled_and_invalid_frames_remain_runner_errors() {
    const RECEIVE: u64 = 1 << 1;
    const POLL: u64 = 1 << 2;
    const CLOCK: u64 = 1 << 3;
    const RESPONSE_CAPACITY: u64 = 2_162_688;

    let mut stalled = run_numa("transport-stall");
    let raw = raw_leaves_record(&mut stalled.records);
    assert_eq!(
        raw.unsigned("seen") & (RECEIVE | POLL | CLOCK),
        RECEIVE | POLL | CLOCK,
        "stalled receive did not exercise the bounded raw wait path"
    );
    let transport = transport_record(&mut stalled.records);
    assert_eq!(transport.get("partial"), "0");
    assert!(
        transport.unsigned("recv_calls") > 0,
        "no actual stalled receive"
    );
    assert_eq!(transport.get("last_result").parse::<i64>().unwrap(), -11);
    assert_eq!(restored_record(&stalled.records).get("result"), "-3");
    require_incomplete_phase(stalled.outcome, "prepared");

    for (mode, kind, result) in [
        ("transport-oversize", "oversize", "-3"),
        ("transport-malformed", "malformed", "1"),
    ] {
        let mut run = run_numa(mode);
        let fault = take_phase(&mut run.records, "transport-fault");
        fault.exact_keys(&[
            "phase",
            "kind",
            "received_length",
            "fault_value",
            "response_length",
        ]);
        assert_eq!(fault.get("kind"), kind);
        let received = fault.unsigned("received_length");
        assert!((1..=RESPONSE_CAPACITY).contains(&received));
        if kind == "oversize" {
            assert_eq!(fault.unsigned("fault_value"), RESPONSE_CAPACITY + 1);
            assert_eq!(fault.unsigned("response_length"), 0);
        } else {
            // The witness retains the original first byte. Native control
            // replaces it with 255 after receiving the complete real frame.
            assert!(fault.unsigned("fault_value") < 255);
            assert_eq!(fault.unsigned("response_length"), received);
        }
        let transport = transport_record(&mut run.records);
        assert_eq!(transport.get("partial"), "0");
        assert!(
            transport.unsigned("recv_success") > 0,
            "no real response read"
        );
        assert_eq!(restored_record(&run.records).get("result"), result);
        // A native result of 1 establishes restoration, not acceptance of a
        // deliberately malformed response. Rust decoding must still refuse.
        require_incomplete_phase(run.outcome, "controller-read");
    }
}

const GUEST_VIEWS: &str = r#"
/* Run through the ordinary DBT capture fixture with no native fault mode.
 * This is a guest-owned mapping,
 * deliberately distinct from the runtime-private seven-page control. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#define CAPACITY (1024u * 1024u)
static char maps_before[CAPACITY];
static char maps_after[CAPACITY];
static char numa_before[CAPACITY];
static char numa_after[CAPACITY];

struct extent {
    uint64_t start;
    uint64_t end;
    const char *permissions;
};

static int write_all(int fd, const char *bytes, size_t length) {
    while (length != 0) {
        ssize_t n = write(fd, bytes, length);
        if (n < 0 && errno == EINTR)
            continue;
        if (n <= 0)
            return 0;
        bytes += (size_t)n;
        length -= (size_t)n;
    }
    return 1;
}

static int fail(const char *stage) {
    (void)write_all(STDERR_FILENO, stage, strlen(stage));
    (void)write_all(STDERR_FILENO, "\n", 1);
    return 91;
}

static int success(const char *marker) {
    return write_all(STDOUT_FILENO, marker, strlen(marker)) ? 0 : 92;
}

static void prefault(char *buffer, size_t page) {
    for (size_t i = 0; i < CAPACITY; i += page)
        ((volatile char *)buffer)[i] = 0;
    ((volatile char *)buffer)[CAPACITY - 1] = 0;
}

/* A fresh descriptor requests a fresh complete observation. A full buffer
 * without observed EOF is a failure, as are truncation and embedded NULs.
 * No allocation, stdio parsing, or mapping change occurs in this function. */
static int read_view(const char *path, char *buffer, size_t *length) {
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0)
        return 0;
    size_t used = 0;
    while (used < CAPACITY) {
        ssize_t n = read(fd, buffer + used, CAPACITY - used);
        if (n < 0 && errno == EINTR)
            continue;
        if (n < 0)
            break;
        if (n == 0) {
            int complete = used != 0 && buffer[used - 1] == '\n' &&
                           memchr(buffer, '\0', used) == NULL;
            if (close(fd) != 0)
                return 0;
            *length = used;
            return complete;
        }
        used += (size_t)n;
    }
    (void)close(fd);
    return 0;
}

static int hex(const char **position, const char *end, uint64_t *value) {
    const char *p = *position;
    uint64_t number = 0;
    unsigned digits = 0;
    while (p < end) {
        unsigned digit;
        if (*p >= '0' && *p <= '9')
            digit = (unsigned)(*p - '0');
        else if (*p >= 'a' && *p <= 'f')
            digit = (unsigned)(*p - 'a') + 10;
        else if (*p >= 'A' && *p <= 'F')
            digit = (unsigned)(*p - 'A') + 10;
        else
            break;
        if (number > (UINT64_MAX - digit) / 16)
            return 0;
        number = number * 16 + digit;
        ++p;
        ++digits;
    }
    if (digits == 0)
        return 0;
    *position = p;
    *value = number;
    return 1;
}

/* Check actual Maps extents intersecting the guest-owned seven-page span.
 * The guards make the initial full extent exact, avoiding an assumption about
 * merging with unrelated neighboring anonymous mappings. */
static int maps_extents(const char *view, size_t length,
                        const struct extent *expected, size_t count) {
    const char *end = view + length;
    size_t found = 0;
    uint64_t previous_end = 0;
    while (view < end) {
        const char *line_end = memchr(view, '\n', (size_t)(end - view));
        const char *p = view;
        uint64_t start, finish;
        if (line_end == NULL || !hex(&p, line_end, &start) ||
            p == line_end || *p++ != '-' || !hex(&p, line_end, &finish) ||
            start >= finish || start < previous_end ||
            p == line_end || *p++ != ' ' || line_end - p < 5 || p[4] != ' ')
            return 0;
        if (start < expected[count - 1].end && finish > expected[0].start) {
            if (found == count || start != expected[found].start ||
                finish != expected[found].end ||
                memcmp(p, expected[found].permissions, 4) != 0)
                return 0;
            ++found;
        }
        previous_end = finish;
        view = line_end + 1;
    }
    return found == count;
}

/* NUMA supplies starts, never inferred ends. Check its exact ordered starts
 * in the already-validated guest Maps span; policy/body bytes are not edited. */
static int numa_starts(const char *view, size_t length, uint64_t span_end,
                       const struct extent *expected, size_t count) {
    const char *end = view + length;
    size_t found = 0;
    uint64_t previous = 0;
    int have_previous = 0;
    while (view < end) {
        const char *line_end = memchr(view, '\n', (size_t)(end - view));
        const char *p = view;
        uint64_t start;
        if (line_end == NULL || !hex(&p, line_end, &start) ||
            (have_previous && start <= previous) || p == line_end ||
            (*p != ' ' && *p != '\t'))
            return 0;
        while (p < line_end && (*p == ' ' || *p == '\t'))
            ++p;
        if (p == line_end)
            return 0;
        if (start >= expected[0].start && start < span_end) {
            if (found == count || start != expected[found].start)
                return 0;
            ++found;
        }
        previous = start;
        have_previous = 1;
        view = line_end + 1;
    }
    return found == count;
}

static int maps_stable(void) {
    size_t before_length, after_length, numa_length;
    /* This comparison brackets the first coherent NUMA capture. It compares
     * all guest Maps bytes, not a filtered subset or only row counts. */
    if (!read_view("/proc/self/maps", maps_before, &before_length) ||
        !read_view("/proc/self/numa_maps", numa_before, &numa_length) ||
        !read_view("/proc/self/maps", maps_after, &after_length))
        return fail("maps-stability observation incomplete");
    if (before_length != after_length ||
        memcmp(maps_before, maps_after, before_length) != 0)
        return fail("coherent capture changed guest Maps bytes");
    return success("mapping-capture-maps-stable-ok\n");
}

static int guest_split(size_t page) {
    size_t total = 9 * page;
    void *reservation = mmap(NULL, total, PROT_NONE,
                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (reservation == MAP_FAILED)
        return fail("guest split reservation failed");
    char *guest = (char *)reservation + page;
    if (mprotect(guest, 7 * page, PROT_READ) != 0)
        return fail("guest initial READ protection failed");
    /* Establish residency before observation1 without dirtying the pages. */
    for (size_t i = 0; i < 7 * page; i += page)
        (void)((const volatile unsigned char *)guest)[i];

    uint64_t start = (uint64_t)(uintptr_t)guest;
    struct extent initial[] = {{start, start + 7 * page, "r--p"}};
    struct extent split[] = {
        {start, start + 2 * page, "r--p"},
        {start + 2 * page, start + 5 * page, "rw-p"},
        {start + 5 * page, start + 7 * page, "r--p"},
    };
    size_t maps_length0, maps_length1, numa_length0, numa_length1;
    if (!read_view("/proc/self/maps", maps_before, &maps_length0) ||
        !read_view("/proc/self/numa_maps", numa_before, &numa_length0) ||
        !maps_extents(maps_before, maps_length0, initial, 1) ||
        !numa_starts(numa_before, numa_length0, initial[0].end, initial, 1))
        return fail("initial guest extent or NUMA start not observed");

    if (mprotect(guest + 2 * page, 3 * page, PROT_READ | PROT_WRITE) != 0)
        return fail("actual guest middle RW split failed");
    if (!read_view("/proc/self/maps", maps_after, &maps_length1) ||
        !read_view("/proc/self/numa_maps", numa_after, &numa_length1) ||
        !maps_extents(maps_after, maps_length1, split, 3) ||
        !numa_starts(numa_after, numa_length1, initial[0].end, split, 3))
        return fail("actual guest split disappeared from Maps or NUMA");
    if (numa_length0 == numa_length1 &&
        memcmp(numa_before, numa_after, numa_length0) == 0)
        return fail("guest split left identical NUMA output");
    if (munmap(reservation, total) != 0)
        return fail("guest reservation cleanup failed");

    /* stdio occurs only after both complete captures and every assertion. */
    if (printf("mapping-capture-guest-split-ok start=%llx split1=%llx "
               "split2=%llx end=%llx\n",
               (unsigned long long)start,
               (unsigned long long)split[1].start,
               (unsigned long long)split[2].start,
               (unsigned long long)initial[0].end) < 0 || fflush(stdout) != 0)
        return 92;
    return 0;
}

int main(int argc, char **argv) {
    long raw_page = sysconf(_SC_PAGESIZE);
    if (argc != 2 || raw_page <= 0 || (unsigned long)raw_page > CAPACITY)
        return fail("invalid fixture arguments or page size");
    size_t page = (size_t)raw_page;
    prefault(maps_before, page);
    prefault(maps_after, page);
    prefault(numa_before, page);
    prefault(numa_after, page);
    if (strcmp(argv[1], "maps-stable") == 0)
        return maps_stable();
    if (strcmp(argv[1], "guest-split") == 0)
        return guest_split(page);
    return fail("unknown fixture mode");
}

"#;

fn guest_views() -> &'static Path {
    static GUEST: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    GUEST.get_or_init(|| {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("mapping-capture-live");
        std::fs::create_dir_all(&root).expect("retain guest-views fixture root");
        let directory = tempfile::Builder::new()
            .prefix("guest-views-")
            .tempdir_in(&root)
            .expect("create guest-views fixture directory")
            .keep();
        eprintln!(
            "mapping capture guest-view fixture: {}",
            directory.display()
        );
        let source = directory.join("guest-views.c");
        let executable = directory.join("guest-views");
        std::fs::write(&source, GUEST_VIEWS).expect("retain guest-views guest source");
        let compiled = Command::new("cc")
            .args(["-std=c11", "-O2", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(&source)
            .arg("-o")
            .arg(&executable)
            .output()
            .expect("compile guest-views guest");
        std::fs::write(directory.join("cc.stdout"), &compiled.stdout)
            .expect("retain guest-views compiler stdout");
        std::fs::write(directory.join("cc.stderr"), &compiled.stderr)
            .expect("retain guest-views compiler stderr");
        std::fs::write(
            directory.join("cc.status"),
            format!("{:?}\n", compiled.status),
        )
        .expect("retain guest-views compiler status");
        assert!(
            compiled.status.success(),
            "guest-views guest compilation failed; evidence: {}",
            directory.display()
        );
        executable
    })
}

#[test]
fn dbt_mapping_capture_leaves_guest_maps_unchanged() {
    let guest = guest_views();
    let run = run(
        None,
        guest.to_str().expect("UTF-8 guest-view fixture"),
        &["maps-stable"],
    );
    assert!(run.records.is_empty(), "no native test hook is permitted");
    let output = run.outcome.expect("ordinary coherent capture");
    assert!(
        output.status.success(),
        "guest Maps control: {:?}",
        output.status
    );
    assert_eq!(output.stdout, b"mapping-capture-maps-stable-ok\n");
    assert!(output.stderr.is_empty(), "guest Maps control stderr");
}

#[test]
fn dbt_mapping_capture_preserves_actual_guest_vma_split() {
    let guest = guest_views();
    let run = run(
        None,
        guest.to_str().expect("UTF-8 guest-view fixture"),
        &["guest-split"],
    );
    assert!(run.records.is_empty(), "no native test hook is permitted");
    let output = run.outcome.expect("ordinary guest split capture");
    assert!(
        output.status.success(),
        "guest split control: {:?}",
        output.status
    );
    assert!(output.stderr.is_empty(), "guest split control stderr");
    let text = std::str::from_utf8(&output.stdout).expect("guest split marker UTF-8");
    assert!(
        text.ends_with('\n') && text.lines().count() == 1,
        "one complete marker"
    );
    let fields: Vec<_> = text.trim_end().split(' ').collect();
    assert_eq!(fields.len(), 5, "exact guest split field set");
    assert_eq!(fields[0], "mapping-capture-guest-split-ok");
    let mut values = Vec::new();
    for (field, name) in fields[1..]
        .iter()
        .zip(["start=", "split1=", "split2=", "end="])
    {
        let value = u64::from_str_radix(field.strip_prefix(name).expect("exact field order"), 16)
            .expect("guest extent hexadecimal");
        values.push(value);
    }
    // The C assertions used this kernel's page size and exact Maps extents.
    // Check the retained address witness independently after successful exit.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    assert!(page > 0);
    let page = page as u64;
    assert!(values[0] > 0 && values[0] % page == 0);
    assert_eq!(values[1], values[0] + 2 * page);
    assert_eq!(values[2], values[0] + 5 * page);
    assert_eq!(values[3], values[0] + 7 * page);
}
