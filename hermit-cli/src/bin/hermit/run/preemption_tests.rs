/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::time::Duration;
use std::time::UNIX_EPOCH;

use clap::CommandFactory;
use clap::Parser;
use hermit::Backend;

use super::PolicyRefusal;
use super::RunOpts;

const TIMERLESS: [Backend; 5] = [
    Backend::Dbt,
    Backend::Sabre,
    Backend::Liteinst,
    Backend::InGuestTrap,
    Backend::Kvm,
];

// Exercise the process-boundary parser, including config expansion and source
// capture. Derived Parser::parse_from alone deliberately has unknown provenance.
fn parse_raw(argv: Vec<OsString>) -> RunOpts {
    let command = crate::Args::command();
    let expanded = crate::run_config::expand_argv(&command, &argv).unwrap();
    let argv = expanded.as_deref().unwrap_or(&argv);
    let matches = command.try_get_matches_from(argv).unwrap();
    let args = crate::args_from_matches_with_clock(&matches, || {
        UNIX_EPOCH + Duration::from_secs(1_000_000_000)
    })
    .unwrap();
    let crate::Subcommand::Run(mut run) = args.command else {
        panic!("expected run subcommand");
    };
    run.backend = args.global.backend;
    *run
}

fn parse(backend: Backend, options: &[&str]) -> RunOpts {
    let mut argv = vec![
        "hermit".into(),
        format!("--backend={}", backend.as_str()).into(),
        "run".into(),
    ];
    argv.extend(options.iter().map(OsString::from));
    argv.extend(["--", "/nonexistent-preemption-admission-guest"].map(OsString::from));
    parse_raw(argv)
}

// Config has no PartialEq. Debug includes the runtime-only fields serde skips,
// so this snapshot checks the complete configuration, including clock policy.
fn config_snapshot(run: &RunOpts) -> String {
    format!("{:#?}", run.det_opts.det_config)
}

fn assert_refuses(run: &RunOpts, setting: &str) {
    let config_before = config_snapshot(run);
    let error = run.resolve_preemption_settings().unwrap_err();
    let message = format!("{error:#}");
    assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{message}");
    assert!(message.contains("unsupported_preemption"), "{message}");
    assert!(
        message.contains(&format!("--backend={}", run.selected_backend().as_str())),
        "{message}"
    );
    assert!(message.contains(setting), "{message}");
    assert!(message.contains("the guest has not started"), "{message}");
    assert_eq!(config_snapshot(run), config_before);
}

#[test]
fn inherited_maximum_uses_each_timerless_backends_capability() {
    for backend in TIMERLESS {
        let run = parse(backend, &[]);
        assert!(!run.max_timeslice_explicit, "{backend:?}");
        assert_eq!(
            run.det_opts
                .det_config
                .max_timeslice
                .map(|maximum| maximum.get()),
            Some(200_000_000),
            "{backend:?}"
        );
        let config_before = config_snapshot(&run);
        run.resolve_preemption_settings().unwrap();
        assert_eq!(config_snapshot(&run), config_before, "{backend:?}");
        // Validation repeats this read-only check after the early launch gate.
        run.resolve_preemption_settings().unwrap();
        assert_eq!(config_snapshot(&run), config_before, "{backend:?}");
    }
}

#[test]
fn explicit_maximum_is_refused_even_when_equal_to_the_parser_default() {
    for backend in TIMERLESS {
        for options in [
            vec!["--max-timeslice=1000"],
            vec!["--max-timeslice=200000000"],
            vec!["--preemption-timeout=200000000"],
            vec!["--max-timeslice=1000", "--no-rcb-time"],
        ] {
            let run = parse(backend, &options);
            assert!(run.max_timeslice_explicit, "{backend:?}: {options:?}");
            assert_refuses(&run, "--max-timeslice");
        }
    }
}

#[test]
fn ptrace_and_e9patch_preprocessing_preserve_positive_maximums() {
    for backend in [Backend::Ptrace, Backend::E9patch] {
        for options in [vec![], vec!["--max-timeslice=1000"]] {
            let run = parse(backend, &options);
            let config_before = config_snapshot(&run);
            assert!(run.det_opts.det_config.max_timeslice.is_some());
            run.resolve_preemption_settings().unwrap();
            assert_eq!(config_snapshot(&run), config_before);
            let configured_maximum = run.det_opts.det_config.max_timeslice;
            for timer_supported in [true, false] {
                let mut validated = run.clone();
                validated
                    .validate_args_with_perf_support(timer_supported)
                    .unwrap();
                let expected_maximum = if timer_supported {
                    configured_maximum
                } else {
                    None
                };
                assert_eq!(
                    validated.det_opts.det_config.max_timeslice,
                    expected_maximum
                );
                assert_eq!(
                    validated.timer_downgrade,
                    if timer_supported {
                        None
                    } else {
                        Some(super::TimerUnavailable::NoPerfEvents)
                    }
                );
                assert_eq!(
                    validated.preemption_summary(),
                    detcore_model::summary::PreemptionSummary {
                        max_timeslice_ns: expected_maximum.map(|maximum| maximum.get()),
                        timer_enabled: Some(timer_supported),
                    }
                );
                let expected_display = if timer_supported {
                    format!(
                        "Configured maximum timeslice: {} virtual ns; timer: enabled",
                        configured_maximum.unwrap().get()
                    )
                } else {
                    "Configured maximum timeslice: disabled; timer: none".to_owned()
                };
                assert_eq!(validated.preemption_summary().to_string(), expected_display);
            }
        }
    }
}

#[test]
fn disabled_maximum_preserves_cooperative_and_boundary_event_options() {
    for backend in TIMERLESS {
        for disabled in [
            "--max-timeslice=disabled",
            "--max-timeslice=0",
            "--preemption-timeout=0",
        ] {
            let run = parse(
                backend,
                &[
                    disabled,
                    "--chaos",
                    "--chaos-per-thread-slowdown",
                    "--target-timeslice=1000",
                    "--target-timeslice-syscalls-only",
                    "--record-preemptions-to=unused-recording",
                    "--replay-schedule-from=unused-schedule",
                    "--stacktrace-event=1",
                    "--chaos-slowdown-max-factor=4.5",
                    "--chaos-epoch-length-ns=1000",
                ],
            );
            let config_before = config_snapshot(&run);
            run.resolve_preemption_settings().unwrap();
            assert_eq!(config_snapshot(&run), config_before);
            assert!(run.max_timeslice_explicit);
            assert_eq!(run.det_opts.det_config.max_timeslice, None);
            assert!(run.det_opts.det_config.target_timeslice.is_some());
            assert!(run.det_opts.det_config.chaos);
            assert!(run.det_opts.det_config.chaos_per_thread_slowdown);
            assert!(run.det_opts.det_config.record_preemptions_to.is_some());
            assert!(run.det_opts.det_config.replay_schedule_from.is_some());
        }
    }
}

#[test]
fn explicit_timer_controls_refuse_independently_of_a_disabled_maximum() {
    for (options, setting) in [
        (vec!["--imprecise-timers"], "--imprecise-timers"),
        (vec!["--interrupt-at=3:1000"], "--interrupt-at"),
        (vec!["--preemption-stacktrace"], "--preemption-stacktrace"),
        (
            vec!["--preemption-stacktrace-log-file=unused-stacktrace"],
            "--preemption-stacktrace-log-file",
        ),
        (vec!["--panic-on-rbc-overshoot"], "--panic-on-rbc-overshoot"),
        (vec!["--panic-on-rcb-overshoot"], "--panic-on-rcb-overshoot"),
        (vec!["--skid-margin=50"], "--skid-margin"),
        (
            vec!["--replay-preemptions-from=unused-preemption-recording"],
            "--replay-preemptions-from",
        ),
    ] {
        let inherited = parse(Backend::Dbt, &options);
        assert_refuses(&inherited, setting);
        assert_eq!(
            inherited
                .det_opts
                .det_config
                .max_timeslice
                .map(|maximum| maximum.get()),
            Some(200_000_000)
        );
        let options: Vec<_> = ["--max-timeslice=disabled"]
            .into_iter()
            .chain(options)
            .collect();
        let run = parse(Backend::Dbt, &options);
        assert_refuses(&run, setting);
        parse(Backend::Ptrace, &options)
            .resolve_preemption_settings()
            .unwrap();
    }
}

fn write_input(directory: &Path, input: &str, stored: Option<&str>) -> std::path::PathBuf {
    let options: Vec<_> = stored.into_iter().collect();
    let run = parse(Backend::Dbt, &options);
    let saved = run.saved_run_config().unwrap();
    match input {
        "--config" => {
            let path = directory.join("run.yaml");
            crate::run_config::write(&path, &saved).unwrap();
            path
        }
        "--from" => {
            let (global, run_inputs) = crate::run_config::to_manifest_parts(&saved).unwrap();
            let manifest = crate::manifest::Manifest::new(
                hermit::Id::unique(),
                crate::manifest::Kind::InputsOnly,
                crate::manifest::Command {
                    program: saved.program,
                    args: saved.args,
                    cwd: None,
                },
                crate::manifest::Inputs {
                    global,
                    run: run_inputs,
                    host_drawn: Default::default(),
                },
            );
            let path = directory.join("id").join(crate::manifest::MANIFEST_NAME);
            crate::manifest::write(&path, &manifest).unwrap();
            path.parent().unwrap().to_path_buf()
        }
        _ => panic!("unexpected input option"),
    }
}

#[test]
fn config_and_from_preserve_maximum_provenance_and_cli_precedence() {
    for input in ["--config", "--from"] {
        for (stored, overriding, refuses) in [
            (None, None, false),
            (Some("--max-timeslice=200000000"), None, true),
            (
                Some("--max-timeslice=200000000"),
                Some("--preemption-timeout=disabled"),
                false,
            ),
            (
                Some("--max-timeslice=disabled"),
                Some("--max-timeslice=200000000"),
                true,
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = write_input(directory.path(), input, stored);
            let mut argv = vec!["hermit".into(), "run".into(), input.into(), path.into()];
            argv.extend(overriding.map(OsString::from));
            let run = parse_raw(argv);
            assert_eq!(run.selected_backend(), Backend::Dbt);
            assert_eq!(
                run.max_timeslice_explicit,
                stored.is_some() || overriding.is_some()
            );
            if refuses {
                assert_refuses(&run, "--max-timeslice");
            } else {
                let config_before = config_snapshot(&run);
                run.resolve_preemption_settings().unwrap();
                assert_eq!(config_snapshot(&run), config_before);
                assert_eq!(
                    run.det_opts
                        .det_config
                        .max_timeslice
                        .map(|maximum| maximum.get()),
                    if stored.is_none() && overriding.is_none() {
                        Some(200_000_000)
                    } else {
                        None
                    }
                );
            }
        }
    }
}

#[test]
fn non_utf8_program_does_not_erase_explicit_maximum_provenance() {
    let mut argv: Vec<_> = [
        "hermit",
        "--backend=dbt",
        "run",
        "--max-timeslice=200000000",
        "--",
    ]
    .map(OsString::from)
    .into();
    argv.push(OsString::from_vec(
        b"/nonexistent-preemption-admission-guest-\xff".to_vec(),
    ));
    let run = parse_raw(argv);
    assert!(
        run.config_capture.is_none(),
        "the unrelated UTF-8 capture must fail"
    );
    assert!(run.max_timeslice_explicit);
    assert_refuses(&run, "--max-timeslice");
}

#[test]
fn inherited_maximum_stays_omitted_in_saved_config_and_reports_no_timer() {
    // Keep this test's identity while guarding the revised contract: an omitted
    // maximum remains inherited; timer capability never injects "disabled".
    let mut run = parse(Backend::Dbt, &[]);
    let config_before = config_snapshot(&run);
    let saved_before = run.saved_run_config().unwrap();
    assert!(!saved_before.run.contains_key("max-timeslice"));
    run.resolve_preemption_settings().unwrap();
    assert_eq!(config_snapshot(&run), config_before);
    let saved = run.saved_run_config().unwrap();
    assert_eq!(
        serde_json::to_value(&saved).unwrap(),
        serde_json::to_value(&saved_before).unwrap()
    );
    assert!(!saved.run.contains_key("max-timeslice"));
    let (_, manifest_inputs) = crate::run_config::to_manifest_parts(&saved).unwrap();
    assert!(!manifest_inputs.contains_key("max-timeslice"));
    run.validate_args().unwrap();
    assert_eq!(
        run.det_opts
            .det_config
            .max_timeslice
            .map(|maximum| maximum.get()),
        Some(200_000_000)
    );
    // Reproduction commands omit defaults so replay does not turn an inherited
    // maximum into an explicit unsupported timer request. The summary reports
    // the configured value separately from the timer mechanism.
    assert!(!run.to_string().contains("--max-timeslice"));
    let summary = run.preemption_summary();
    assert_eq!(summary.max_timeslice_ns, Some(200_000_000));
    assert_eq!(summary.timer_enabled, Some(false));
    assert_eq!(
        summary.to_string(),
        "Configured maximum timeslice: 200000000 virtual ns; timer: none"
    );
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("effective.yaml");
    crate::run_config::write(&path, &saved).unwrap();
    let reloaded = parse_raw(vec![
        "hermit".into(),
        "run".into(),
        "--config".into(),
        path.into(),
    ]);
    assert!(!reloaded.max_timeslice_explicit);
    let config_before = config_snapshot(&reloaded);
    reloaded.resolve_preemption_settings().unwrap();
    assert_eq!(config_snapshot(&reloaded), config_before);
    assert_eq!(
        reloaded
            .det_opts
            .det_config
            .max_timeslice
            .map(|maximum| maximum.get()),
        Some(200_000_000)
    );
}

#[test]
fn direct_parser_without_source_capture_does_not_claim_an_inherited_default() {
    let mut run = RunOpts::parse_from(["run", "/nonexistent-preemption-admission-guest"]);
    run.backend = Some(Backend::Dbt);
    assert!(run.max_timeslice_explicit);
    assert_refuses(&run, "--max-timeslice");
}
