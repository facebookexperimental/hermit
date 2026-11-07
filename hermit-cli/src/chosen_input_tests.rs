/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Execute the actual public/raw admission entrypoints in an isolated process.
//! A marker written by the guest distinguishes refusal from a later failure.

use detcore::GuestIds;
use reverie::process::Command;

const CASE: &str = "HERMIT_CHOSEN_INPUT_ADMISSION_TEST_CASE";
const MARKER: &str = "HERMIT_CHOSEN_INPUT_ADMISSION_TEST_MARKER";

fn config(flags: usize) -> detcore::Config {
    detcore::Config {
        guest_ids: GuestIds::Chosen {
            base: 5000,
            refuse_host_ids: true,
        },
        recordreplay_modes: flags & 1 != 0,
        replaying: flags & 2 != 0,
        sequentialize_threads: true,
        max_timeslice: None,
        ..detcore::Config::default()
    }
}

fn guest() -> Command {
    let marker = std::env::var_os(MARKER).unwrap();
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "printf started > \"$1\"", "admission-marker"]);
    command.arg(marker);
    command
}

fn run_refusal_cases(name: &str, cases: &[String], class: &str) {
    for case in cases {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("guest-started");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env(CASE, case)
            .env(MARKER, &marker)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(122), "{case}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(class), "{case}: {stderr}");
        assert!(stderr.contains("guest has not started"), "{case}: {stderr}");
        assert!(
            !stderr.to_ascii_lowercase().contains("desync"),
            "{case}: {stderr}"
        );
        assert!(!marker.exists(), "{case}: the refused guest executed");
    }
}

#[test]
fn binary_chosen_wrappers_and_raw_typed_spawns_refuse_before_target_start() {
    const NAME: &str = "chosen_input_tests::binary_chosen_wrappers_and_raw_typed_spawns_refuse_before_target_start";
    let Ok(case) = std::env::var(CASE) else {
        let cases: Vec<_> = [
            "record-wrapper",
            "replay-wrapper",
            "record-raw",
            "replay-raw",
            "record-closure",
            "replay-closure",
        ]
        .into_iter()
        .flat_map(|route| (0..4).map(move |flags| format!("{route}:{flags}")))
        .collect();
        run_refusal_cases(NAME, &cases, "HERMIT_CHOSEN_BINARY_INPUT_PROVENANCE:");
        return;
    };
    let (route, flags) = case.split_once(':').unwrap();
    let config = config(flags.parse().unwrap());
    let route = route.to_owned();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        match route.as_str() {
            "record-wrapper" => { let _ = hermit_record::spawn_tracer(guest(), config).await; }
            "replay-wrapper" => { let _ = hermit_replay::spawn_tracer(guest(), config, None, false).await; }
            "record-raw" => {
                let _ = reverie_ptrace::TracerBuilder::<detcore::Detcore<hermit_record::Recorder>>::new(guest())
                    .config(config).spawn().await;
            }
            "replay-raw" => {
                let _ = reverie_ptrace::TracerBuilder::<detcore::Detcore<hermit_replay::Replayer>>::new(guest())
                    .config(config).spawn().await;
            }
            "record-closure" => {
                let marker = std::env::var_os(MARKER).unwrap();
                let _ = reverie_ptrace::spawn_fn_with_config::<detcore::Detcore<hermit_record::Recorder>, _>(
                    move || { std::fs::write(marker, b"started").unwrap(); }, config, true,
                ).await;
            }
            "replay-closure" => {
                let marker = std::env::var_os(MARKER).unwrap();
                let _ = reverie_ptrace::spawn_fn_with_config::<detcore::Detcore<hermit_replay::Replayer>, _>(
                    move || { std::fs::write(marker, b"started").unwrap(); }, config, true,
                ).await;
            }
            _ => panic!("unknown route {route}"),
        }
        panic!("{case}: entrypoint returned instead of the named pre-start refusal");
    });
}

#[test]
fn json_chosen_raw_schedule_and_priority_routes_refuse_before_target_start() {
    const NAME: &str = "chosen_input_tests::json_chosen_raw_schedule_and_priority_routes_refuse_before_target_start";
    let Ok(case) = std::env::var(CASE) else {
        let cases: Vec<_> = ["command", "closure", "global"]
            .into_iter()
            .flat_map(|route| {
                ["schedule", "priority", "both-second", "both-first"]
                    .into_iter()
                    .flat_map(move |input| {
                        ["missing", "unknown", "authentic-legacy"]
                            .into_iter()
                            .map(move |metadata| format!("{route}:{input}:{metadata}"))
                    })
            })
            .collect();
        run_refusal_cases(NAME, &cases, "HERMIT_CHOSEN_JSON_INPUT_PROVENANCE:");
        return;
    };
    let parts: Vec<_> = case.split(':').map(str::to_owned).collect();
    let directory = tempfile::tempdir().unwrap();
    let good = directory.path().join("qualified.json");
    let bad = directory.path().join("unqualified.json");
    // Modern fixtures state a backend-local RCB identity and the run claims
    // the same token below, so identity admission passes on any host and
    // the Chosen provenance refusal under test fires. The authentic-legacy
    // bytes stay unwrapped: Chosen validation precedes identity admission
    // on both routes, so they take it too.
    const LOCAL: &str = "test:chosen-input:v1";
    let enveloped = |inner: &serde_json::Value| {
        serde_json::to_vec(&serde_json::json!({
            "rcb_history": {
                "format": 2,
                "identity": {"backend_local": LOCAL},
                "recording": inner
            }
        }))
        .unwrap()
    };
    let mut value = serde_json::json!({
        "per_thread": {}, "global": [],
        "guest_ids": serde_json::to_value(config(0).guest_ids).unwrap(),
        "reserved_input": {"contract": "sealed-private-input-v1", "base": "0x71010000",
            "abi": std::env::consts::ARCH, "page_bytes": unsafe { libc::sysconf(libc::_SC_PAGESIZE) }}
    });
    std::fs::write(&good, enveloped(&value)).unwrap();
    if parts[2] == "missing" {
        value.as_object_mut().unwrap().remove("reserved_input");
    } else {
        value["reserved_input"] = serde_json::json!({"contract": "unknown"});
    }
    if parts[2] == "authentic-legacy" {
        // Complete bytes from the independently retained pre-facility ELF,
        // not a constructed old schema or a stripped modern recording.
        std::fs::write(
            &bad,
            include_bytes!("../tests/fixtures/chosen-before-reserved-input.json"),
        )
        .unwrap();
    } else {
        std::fs::write(&bad, enveloped(&value)).unwrap();
    }
    let mut config = config(0);
    config.backend.rcb_clock_contract = None;
    config.rcb_clock_claim = detcore_model::config::RcbClockClaim::BackendLocal(LOCAL);
    match parts[1].as_str() {
        "schedule" => config.replay_schedule_from = Some(bad),
        "priority" => config.replay_preemptions_from = Some(bad),
        "both-second" => {
            config.replay_schedule_from = Some(good);
            config.replay_preemptions_from = Some(bad);
        }
        "both-first" => {
            config.replay_schedule_from = Some(bad);
            config.replay_preemptions_from = Some(good);
        }
        _ => unreachable!(),
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        if parts[0] == "command" {
            let _ = reverie_ptrace::TracerBuilder::<detcore::Detcore>::new(guest())
                .config(config)
                .spawn()
                .await;
        } else if parts[0] == "closure" {
            let marker = std::env::var_os(MARKER).unwrap();
            let _ = reverie_ptrace::spawn_fn_with_config::<detcore::Detcore, _>(
                move || {
                    std::fs::write(marker, b"started").unwrap();
                },
                config,
                true,
            )
            .await;
        } else {
            let _global =
                <detcore::GlobalState as reverie::GlobalTool>::init_global_state(&config).await;
            start_native_marker();
        }
        panic!("{case}: entrypoint returned instead of the named pre-start refusal");
    });
}

fn start_native_marker() {
    let status = std::process::Command::new("/bin/sh")
        .args(["-c", "printf started > \"$1\"", "admission-marker"])
        .arg(std::env::var_os(MARKER).unwrap())
        .status()
        .unwrap();
    assert!(status.success());
}

/// GlobalTool-only launchers must construct their host state before an RPC
/// server or child. This backstop sees Config flags, not an erased inner Tool
/// role; the typed wrapper/constructor controls above cover that separate fact.
#[test]
fn binary_chosen_config_is_refused_before_an_erased_global_can_launch() {
    const NAME: &str =
        "chosen_input_tests::binary_chosen_config_is_refused_before_an_erased_global_can_launch";
    let Ok(case) = std::env::var(CASE) else {
        run_refusal_cases(
            NAME,
            &["1".to_owned(), "2".to_owned(), "3".to_owned()],
            "HERMIT_CHOSEN_BINARY_INPUT_PROVENANCE:",
        );
        return;
    };
    let config = config(case.parse().unwrap());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let _global =
            <detcore::GlobalState as reverie::GlobalTool>::init_global_state(&config).await;
        start_native_marker();
        panic!("GlobalTool-only construction bypassed the named Config admission");
    });
}

/// Both public-library dispatches reject the actual file before backend
/// selection or command lookup. Namespace records with opaque new metadata
/// retain their old admission and reach the existing operation-owner gate.
#[tokio::test]
async fn json_provenance_admission_precedes_both_library_dispatches() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("schedule.json");
    for ids in [
        GuestIds::Chosen {
            base: 5000,
            refuse_host_ids: true,
        },
        GuestIds::Namespace,
    ] {
        for input in ["schedule", "priority", "both"] {
            let record = serde_json::json!({
                "per_thread": {}, "global": [], "guest_ids": ids,
                "reserved_input": {"contract": "future-input", "opaque": [null, 17]},
            });
            std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
            let mut config = config(0);
            config.guest_ids = ids;
            if input != "priority" {
                config.replay_schedule_from = Some(path.clone());
            }
            if input != "schedule" {
                config.replay_preemptions_from = Some(path.clone());
            }
            let status = crate::dispatch_backend(
                Command::new("/nonexistent/guest"),
                config.clone(),
                false,
                &None,
                crate::Backend::Ptrace,
                None,
            )
            .await
            .unwrap_err();
            let captured = crate::dispatch_output_backend(
                Command::new("/nonexistent/guest"),
                config,
                false,
                &None,
                crate::Backend::Ptrace,
                None,
            )
            .await
            .unwrap_err();
            for error in [status, captured] {
                if matches!(ids, GuestIds::Chosen { .. }) {
                    assert!(
                        error.is::<detcore::preemptions::RecordAdmissionRefusal>(),
                        "{input}: {error:#}"
                    );
                    assert!(
                        error
                            .to_string()
                            .contains("HERMIT_CHOSEN_JSON_INPUT_PROVENANCE:")
                    );
                    assert!(error.to_string().contains("guest has not started"));
                    let classified =
                        crate::error::SerializableError::from(error.context("library admission"));
                    assert_eq!(classified.kind(), crate::error::FailureKind::PolicyRefusal);
                } else {
                    assert!(
                        error
                            .to_string()
                            .contains("ordinary ptrace requires its retained operation owner"),
                        "{input}: {error:#}"
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn authentic_legacy_chosen_json_is_refused_by_both_library_dispatches() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("authentic-legacy.json");
    let marker = directory.path().join("guest-started");
    let bytes = include_bytes!("../tests/fixtures/chosen-before-reserved-input.json");
    std::fs::write(&path, bytes).unwrap();
    let command = || {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf started > \"$1\"", "admission-marker"]);
        command.arg(&marker);
        command
    };
    for route in ["schedule", "priority", "both"] {
        let mut config = config(0);
        if route != "priority" {
            config.replay_schedule_from = Some(path.clone());
        }
        if route != "schedule" {
            config.replay_preemptions_from = Some(path.clone());
        }
        let status = crate::dispatch_backend(
            command(),
            config.clone(),
            false,
            &None,
            crate::Backend::Ptrace,
            None,
        )
        .await
        .unwrap_err();
        let captured = crate::dispatch_output_backend(
            command(),
            config,
            false,
            &None,
            crate::Backend::Ptrace,
            None,
        )
        .await
        .unwrap_err();
        for error in [status, captured] {
            assert!(
                error.is::<detcore::preemptions::RecordAdmissionRefusal>(),
                "{route}: {error:#}"
            );
            assert!(
                error
                    .to_string()
                    .contains("missing reserved_input provenance"),
                "{route}: {error:#}"
            );
            assert!(
                error.to_string().contains("guest has not started"),
                "{route}: {error:#}"
            );
            assert!(!error.to_string().to_ascii_lowercase().contains("desync"));
            assert_eq!(
                crate::error::SerializableError::from(error).kind(),
                crate::error::FailureKind::PolicyRefusal
            );
        }
        assert!(
            !marker.exists(),
            "{route}: legacy replay executed its target"
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}
