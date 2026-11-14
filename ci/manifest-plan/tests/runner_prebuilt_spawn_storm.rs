//! Regression for the runner's prebuilt fixture copy under a spawn storm
//! (dh-8qwyoo): while other threads spawn continuously, `runner::prepare_test`
//! copies a prebuilt cell's fixtures and the program it returns runs at once,
//! as `run_cell` runs it. An in-process `fs::copy` of that program left a
//! window in which a sibling's child held the copy open for writing, and Linux
//! refused to execute it (ETXTBSY).
//!
//! This is its own test binary for the reason given in
//! exec_safe_fs_spawn_storm.rs: the storm must not share a process with the
//! library's CPU-budget tests. It drives the public `prepare_test` rather than
//! the copy helper, so it also fails if the runner stops using the helper.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use hermit_manifest_plan::runner::CellId;
use hermit_manifest_plan::runner::HostLevel;
use hermit_manifest_plan::runner::RunContext;
use hermit_manifest_plan::runner::ScheduledWorkerCapacity;
use hermit_manifest_plan::runner::SelectedCell;
use hermit_manifest_plan::runner::TestRecipe;
use hermit_manifest_plan::runner::prepare_test;
use hermit_manifest_plan::timeouts::TimeoutMultipliers;

const TEST_ID: &str = "fixture/prebuilt";
const DATA: &[u8] = b"fixture data, never executed\n";

/// A fresh directory under the temporary root, removed when dropped.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn prebuilt_cell() -> SelectedCell {
    let test: TestRecipe = serde_json::from_value(serde_json::json!({
        "id": TEST_ID,
        "description": "prebuilt fixture program",
        "lane": "portable",
        "occasional": false,
        "program": "tests/fixture.c",
        "direct": null,
        "observation": {"status": true, "stdout": true, "stderr": true},
        "build": null,
        "modes": {"verify": {"ci": true, "backends_enabled": ["ptrace"]}},
        "proc": null,
    }))
    .unwrap();
    SelectedCell {
        image: None,
        category: "fixture".into(),
        id: CellId {
            test: TEST_ID.into(),
            mode: "verify".into(),
            backend: Some("ptrace".into()),
        },
        test,
        enabled: true,
        timeout_seconds: 15,
        cpu_timeout_seconds: 10,
        host_level: HostLevel::default(),
    }
}

fn prebuilt_context(root: &Path) -> RunContext {
    RunContext {
        command_bindings: BTreeMap::new(),
        root: root.into(),
        hermit_bin: root.join("hermit"),
        result_root: root.join("results"),
        build_root: root.join("build"),
        run_id: "fixture".into(),
        machine_shortname: "fixture-host".into(),
        kernel_version: "7.1.3-fixture".into(),
        host_capabilities: BTreeMap::new(),
        exact_branch_counter: None,
        attempt: 1,
        run_index: None,
        epoch: "2026-01-01T00:00:00Z".into(),
        source_sha: "0".repeat(40),
        source_dirty: false,
        binary_build_sha: None,
        prebuilt: true,
        keep_logs: false,
        capture_custom_runs: false,
        parity_retained: BTreeSet::new(),
        parity_compared: BTreeSet::new(),
        run_verify_strict: false,
        record_verify_strict: false,
        timeout_multipliers: TimeoutMultipliers::default(),
        scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
        isolated_workdir: None,
        shares_ld_so_cache: false,
    }
}

#[test]
fn a_prebuilt_fixture_program_is_never_busy_under_concurrent_spawns() {
    let root = Scratch(std::env::temp_dir().join(format!(
        "runner-prebuilt-spawn-storm-{}",
        std::process::id()
    )));
    let source = root
        .0
        .join("build")
        .join(TEST_ID.replace('/', "-"))
        .join("fixtures");
    std::fs::create_dir_all(&source).unwrap();
    // The sources are never executed, so writing them in-process is safe.
    std::fs::write(source.join("program"), "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(
        source.join("program"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::fs::write(source.join("data"), DATA).unwrap();
    std::fs::set_permissions(source.join("data"), std::fs::Permissions::from_mode(0o644)).unwrap();
    let context = prebuilt_context(&root.0);
    let cell = prebuilt_cell();
    let cell_dir = root.0.join("cell");

    let stop = Arc::new(AtomicBool::new(false));
    let spawners = (0..6)
        .map(|_| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let _ = Command::new("true").status();
                }
            })
        })
        .collect::<Vec<_>>();
    let mut failures = Vec::new();
    for _ in 0..100 {
        let guest = prepare_test(&context, &cell, &cell_dir).unwrap();
        assert_eq!(
            guest,
            [cell_dir
                .join("fixtures/program")
                .to_string_lossy()
                .into_owned()]
        );
        match Command::new(&guest[0]).status() {
            Ok(status) if status.success() => {}
            other => failures.push(format!("{other:?}")),
        }
        let data = cell_dir.join("fixtures/data");
        assert_eq!(std::fs::read(&data).unwrap(), DATA);
        assert_eq!(
            std::fs::metadata(&data).unwrap().permissions().mode() & 0o7777,
            0o644
        );
    }
    stop.store(true, Ordering::Relaxed);
    for spawner in spawners {
        spawner.join().unwrap();
    }
    assert!(failures.is_empty(), "{} busy: {failures:?}", failures.len());
}
