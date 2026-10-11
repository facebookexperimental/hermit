// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Authored Cargo build selections for the committed validation graph.
//! Runtime consumers read the selection recorded in that graph. The generator
//! independently checks these declarations against the actual runner commands.

pub(super) fn for_step(tag: &str) -> Option<&'static [&'static str]> {
    match tag {
        "test.regular_crates" | "test.isolated_detcore_workdir" => Some(&[
            "--workspace",
            "--exclude",
            "hermit-detcore",
            "--exclude",
            "hermit",
            "--exclude",
            "hermetic_infra_hermit_flaky-tests",
        ]),
        "test.hermit_unit" | "privileged-test.pmu_ptrace_completion_cases" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends,kvm-native-test-support",
            "--lib",
            "--bins",
        ]),
        "test.detcore_unit" => Some(&["-p", "hermit-detcore", "--lib", "--bins"]),
        "test.detcore_misc"
        | "privileged-build.privileged_tests"
        | "privileged-cpuid.faulting"
        | "privileged-only-cpuid.faulting"
        | "privileged-only-cpuid.faulting_on_host"
        | "super.post_fork_scheduling_diagnostics"
        | "super.network_syscall_determinism_diagnostic" => {
            Some(&["-p", "hermit-detcore", "--test", "tests_misc"])
        }
        "test.detcore_time" | "privileged-test.pmu_detcore_time_cases" => {
            Some(&["-p", "hermit-detcore", "--test", "tests_time"])
        }
        "test.detcore_parallel"
        | "super.weekly_pmu_parallel_memory_diagnostic_mem_race_bottom_detcore"
        | "super.weekly_pmu_parallel_memory_diagnostic_mem_race_default_detcore"
        | "super.weekly_pmu_parallel_memory_diagnostic_mem_race_middle_detcore"
        | "super.weekly_pmu_parallel_memory_diagnostic_mem_race_top_detcore" => {
            Some(&["-p", "hermit-detcore", "--test", "tests_parallelism"])
        }
        "test.hermit_integration" | "test.pmu_integration_cases" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "aio_nr_determinism",
            "--test",
            "arch_status_determinism",
            "--test",
            "chaos_sched_yield_progress",
            "--test",
            "chaos_stress_pmu_detection",
            "--test",
            "child_time_rpc",
            "--test",
            "chown_virtual_root_identity",
            "--test",
            "cli_owned_lifecycle",
            "--test",
            "clock_determinism",
            "--test",
            "clock_discipline_determinism",
            "--test",
            "clock_passthrough",
            "--test",
            "container_init_deadline",
            "--test",
            "cpufreq_avg_determinism",
            "--test",
            "dispatch_stats",
            "--test",
            "epoll_determinism",
            "--test",
            "epoll_pwait_zero_timeout_progress",
            "--test",
            "external_signal_interrupt",
            "--test",
            "fatal_core_capture",
            "--test",
            "file_nr_determinism",
            "--test",
            "first_seen_mtime",
            "--test",
            "fp_reduction_determinism",
            "--test",
            "futex2_refusal",
            "--test",
            "hashseed_determinism",
            "--test",
            "inode_nr_determinism",
            "--test",
            "kernel_keyring",
            "--test",
            "key_users_determinism",
            "--test",
            "mmap_determinism",
            "--test",
            "node_vmstat_determinism",
            "--test",
            "numa_maps_determinism",
            "--test",
            "perf_event_refusal",
            "--test",
            "pidfd_creation",
            "--test",
            "process_isolation_refusals",
            "--test",
            "proc_fdinfo_determinism",
            "--test",
            "proc_locks_determinism",
            "--test",
            "procfs_determinism",
            "--test",
            "procfs_positioned_determinism",
            "--test",
            "pty_nr_determinism",
            "--test",
            "python_stdlib",
            "--test",
            "reopened_pipe_progress",
            "--test",
            "robust_futex_owner_death",
            "--test",
            "run_evidence",
            "--test",
            "self_sched_determinism",
            "--test",
            "self_schedstat_determinism",
            "--test",
            "signal_determinism",
            "--test",
            "smaps_determinism",
            "--test",
            "smaps_rollup_determinism",
            "--test",
            "softnet_stat_determinism",
            "--test",
            "sockstat_determinism",
            "--test",
            "swaps_determinism",
            "--test",
            "thp_stats_determinism",
            "--test",
            "utimensat_mtime",
            "--test",
            "verification_report_cli",
            "--test",
            "verification_report_consumers",
            "--test",
            "verify_claim_names_its_limit",
            "--test",
            "writev_determinism",
            "--test",
            "zero_copy_pipe_fallback",
        ]),
        "test.arbitrary_binaries" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "arbitrary_binaries",
        ]),
        "test.liteinst_allocator" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "liteinst_allocator",
        ]),
        "test.liteinst_stacks" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "liteinst_stacks",
        ]),
        "test.cli"
        | "test.isolated_dbt_workdir"
        | "test.cli_on_host"
        | "privileged-test.pmu_cli_cases"
        | "super.dbt_pipe_backpressure_diagnostic"
        | "super.dbt_failed_exec_recovery_diagnostic"
        | "super.dbt_unsupported_syscall_aggregation_diagnostic"
        | "super.dbt_strict_blocked_stdin_teardown_diagnostic"
        | "super.dbt_guest_stderr_isolation_diagnostic" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "cli",
        ]),
        "privileged-test.cli_kvm"
        | "privileged-only-test.cli_kvm"
        | "privileged-only-test.cli_kvm_on_host" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends,kvm-execution-tests",
            "--lib",
            "--test",
            "cli",
        ]),
        "test.sabre_examples" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "sabre_examples",
        ]),
        "test.hermit_modes"
        | "test.hermit_modes_on_host"
        | "privileged-test.pmu_buck_chaos_cases"
        | "super.chaos_hello_race_verification_diagnostic"
        | "super.weekly_relaxed_default_mode_cases"
        | "super.pmu_buck_chaos_cases"
        | "privileged-only-test.pmu_buck_chaos_cases"
        | "privileged-only-test.pmu_buck_chaos_cases_on_host" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "hermit_modes",
        ]),
        "test.app_strict_verify" | "super.managed_jvm_strict_verify_diagnostics" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "app_strict_verify",
        ]),
        "test.command_strict_verify" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "command_strict_verify",
        ]),
        "test.ignored_syscall_regressions" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "epoll_determinism",
            "--test",
            "rcx_canonicalization",
        ]),
        "test.rr_suite_contract" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "rr_suite",
        ]),
        "quick.detcore_unit" => Some(&["-p", "hermit-detcore", "--lib"]),
        "super.relaxed_hermit_flag_matrix" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "relaxed_flag_matrix",
        ]),
        "super.pselect_signal_interruption_diagnostic" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "pselect6_simulation",
        ]),
        "test.record_replay" | "super.record_replay_matrix_diagnostic" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "record_replay",
        ]),
        "super.ipc_determinism_diagnostic" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "ipc_determinism",
        ]),
        "super.random_source_determinism_diagnostic" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "random_determinism",
        ]),
        "super.threaded_integration_matrix_diagnostic" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "integration_matrix",
        ]),
        "super.weekly_portable_chaos_cases" | "super.weekly_ignored_portable_chaos_cases" => {
            Some(&[
                "-p",
                "hermit",
                "--features",
                "third-party-backends",
                "--test",
                "stress_suite",
            ])
        }
        "super.pmu_analyze_hello_race_stress_calibrated_skid" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "analyze",
        ]),
        "super.full_leveldb_strict_determinism" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "leveldb",
        ]),
        "super.sqlite_veryquick_strict_determinism" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "sqlite_veryquick",
        ]),
        _ => None,
    }
}

/// Recover the exact authored payload from the one supported execution wrapper.
/// Both the generator and its preparation audit inspect the same command bytes
/// the container's final bash executes, including literal shell quoting.
pub(super) fn execution_command(step: &dagrun::model::Step) -> Result<String, String> {
    if !step.cmd.starts_with("./ci/hermetic/run-in-pinned-root.sh ") {
        return Ok(step.cmd.clone());
    }
    let args = shell_words::split(&step.cmd).map_err(|error| format!("{}: {error}", step.tag()))?;
    let boundary = args
        .iter()
        .position(|arg| arg == "--")
        .ok_or_else(|| format!("{} omits its pinned-root command boundary", step.tag()))?;
    match &args[boundary + 1..] {
        [shell, option, guard, argv0, payload]
            if shell == "bash"
                && option == "-c"
                && argv0 == "bash"
                && (guard == crate::validation_dag::PINNED_ROOT_COMMAND_GUARD
                    || guard == crate::validation_dag::LEGACY_PINNED_ROOT_COMMAND_GUARD) =>
        {
            Ok(payload.clone())
        }
        _ => Err(format!(
            "{} has an unrecognized pinned-root command",
            step.tag()
        )),
    }
}

fn command_arguments(command: &str, marker: &str) -> Result<Vec<String>, String> {
    let (_, tail) = command
        .split_once(marker)
        .ok_or_else(|| format!("missing {marker}"))?;
    let tail = tail.replace("${CI:+--profile ci}", "");
    let mut quote = None;
    let mut escape = false;
    let mut end = tail.len();
    for (offset, ch) in tail.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escape = true;
            continue;
        }
        if let Some(active) = quote {
            if ch == active {
                quote = None;
            }
        } else if ch == '\'' || ch == '"' {
            quote = Some(ch);
        } else if matches!(ch, ';' | '&' | '|' | '>' | '<') {
            end = offset;
            break;
        }
    }
    let args = shell_words::split(&tail[..end])
        .map_err(|e| format!("cannot parse authored Nextest arguments: {e}"))?;
    Ok(args)
}

pub(super) fn assert_command_selection(step: &dagrun::model::Step) -> Result<(), String> {
    use crate::nextest_binaries::REQUIRED_ENV;
    use crate::nextest_binaries::SELECTION_ENV;
    use crate::nextest_binaries::split_arguments;
    let tag = step.tag();
    let command = execution_command(step)?;
    let raw = step
        .env
        .get(SELECTION_ENV)
        .ok_or_else(|| format!("{tag} has no prepared build selection"))?;
    let expected: Vec<String> = serde_json::from_str(raw).map_err(|e| format!("{tag}: {e}"))?;
    if step.env.get(REQUIRED_ENV).map(String::as_str) != Some("1") {
        return Err(format!(
            "{tag} does not require prepared Nextest executables"
        ));
    }
    for marker in ["run-nextest-counted.sh", "nextest-binaries.rs list"] {
        if !command.contains(marker) {
            continue;
        }
        let mut arguments = command_arguments(&command, marker)?;
        if marker == "run-nextest-counted.sh" {
            match arguments.first().map(String::as_str) {
                Some("--calibration-host") => {
                    arguments.remove(0);
                }
                Some("--calibration-launch-proof") => {
                    if arguments.get(1).map(String::as_str)
                        != Some(crate::nextest_cohort::PINNED_PROOF_PATH)
                    {
                        return Err(format!("{tag} has an unsupported calibration launch proof"));
                    }
                    arguments.drain(..2);
                }
                _ => {}
            }
        }
        let parsed = split_arguments(&arguments).map_err(|error| format!("{tag}: {error}"))?;
        if parsed.build != expected {
            return Err(format!(
                "{tag} command has Cargo selection {:?}, declared {expected:?}",
                parsed.build
            ));
        }
    }
    let direct = "nextest-binaries.rs executable ";
    if let Some((_, rest)) = command.split_once(direct) {
        if command.matches(direct).count() != 1 {
            return Err(format!("{tag} has ambiguous prepared executable lookups"));
        }
        let arguments = rest
            .split_once(')')
            .ok_or_else(|| format!("{tag} has no executable lookup boundary"))?
            .0;
        let arguments = shell_words::split(arguments).map_err(|e| format!("{tag}: {e}"))?;
        if arguments.len() != 2 {
            return Err(format!(
                "{tag} must name one prepared package and test target"
            ));
        }
        let actual = vec![
            "-p".to_string(),
            arguments[0].clone(),
            "--test".to_string(),
            arguments[1].clone(),
        ];
        if actual != expected {
            return Err(format!(
                "{tag} executable lookup selects {actual:?}, declared {expected:?}"
            ));
        }
    }
    if command.contains("cargo nextest list") || command.contains("cargo nextest run") {
        return Err(format!("{tag} can bypass prepared metadata and compile"));
    }
    Ok(())
}

/// A producer of a unifying profile (`nextest_binaries::unifies`) must end
/// with that profile's unified workspace build followed by `prepare`. Any other
/// build -- different features, or a second `-p hermit --bin hermit` link --
/// makes preparation re-resolve features, recompile, and relink the unhashed
/// executable the tests run. That build also links the Hermit the E2E cells
/// run, so its features must keep every third-party backend. The pinned-root
/// producer runs its build in `validation_dag::PINNED_WORKSPACE_COMPILE_TAG`
/// and its `prepare` in `PINNED_WORKSPACE_PREPARE_TAG`; the pair is judged as
/// the one payload it rejoins to.
pub(super) fn assert_producers_build_the_unified_selection(
    cfg: &dagrun::model::DagConfig,
) -> Result<(), String> {
    for step in &cfg.steps {
        let command = if step.tag() == crate::validation_dag::PINNED_WORKSPACE_PREPARE_TAG {
            crate::validation_dag::pinned_workspace_producer_payload(cfg)?
        } else {
            execution_command(step)?
        };
        let Some((_, profile)) = command.split_once("./ci/nextest-binaries.rs prepare ") else {
            continue;
        };
        if !crate::nextest_binaries::unifies(profile) {
            continue;
        }
        let expected = crate::nextest_binaries::unified_prebuild_command(cfg, profile)?;
        if command.matches("--workspace --all-targets").count() != 1
            || !command.ends_with(&expected)
            || command.contains("--bin hermit")
        {
            return Err(format!(
                "{} must end with its unified workspace build `{expected}` and build nothing else in between",
                step.tag()
            ));
        }
        if !expected.contains("hermit/third-party-backends") {
            return Err(format!(
                "{} links the E2E Hermit without hermit/third-party-backends: `{expected}`",
                step.tag()
            ));
        }
    }
    Ok(())
}

pub(super) fn assert_preparation_dependencies(
    cfg: &dagrun::model::DagConfig,
) -> Result<(), String> {
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;

    use crate::nextest_binaries::SELECTION_ENV;
    use crate::nextest_binaries::config_selections;
    use crate::nextest_binaries::selection_key;
    let by_tag = cfg
        .steps
        .iter()
        .map(|step| (step.tag(), step))
        .collect::<BTreeMap<_, _>>();
    let mut producers = BTreeMap::new();
    for step in &cfg.steps {
        let command = execution_command(step)?;
        if let Some((_, profile)) = command.split_once("./ci/nextest-binaries.rs prepare ") {
            if profile.is_empty() || profile.contains(char::is_whitespace) {
                return Err(format!("{} has an ambiguous prepared profile", step.tag()));
            }
            producers.insert(
                step.tag(),
                (
                    step.cmd.starts_with("./ci/hermetic/run-in-pinned-root.sh "),
                    config_selections(cfg, profile)?,
                ),
            );
        }
    }
    for step in &cfg.steps {
        let Some(raw) = step.env.get(SELECTION_ENV) else {
            continue;
        };
        let args: Vec<String> = serde_json::from_str(raw).map_err(|e| e.to_string())?;
        let key = selection_key(&args);
        let mut ancestors = BTreeSet::new();
        let mut pending = step.deps.clone();
        while let Some(dependency) = pending.pop() {
            if ancestors.insert(dependency.clone()) {
                let ancestor = by_tag
                    .get(&dependency)
                    .ok_or_else(|| format!("{} has missing dependency {dependency}", step.tag()))?;
                pending.extend(ancestor.deps.iter().cloned());
            }
        }
        if !ancestors.iter().any(|tag| {
            producers.get(tag).is_some_and(|(pinned, selections)| {
                *pinned == step.cmd.starts_with("./ci/hermetic/run-in-pinned-root.sh ")
                    && selections.get(&key) == Some(&args)
            })
        }) {
            return Err(format!(
                "{} can run before any producer in the same filesystem root of its exact Cargo selection {args:?}",
                step.tag()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nextest_binaries::REQUIRED_ENV;
    use crate::nextest_binaries::SELECTION_ENV;

    #[test]
    fn child_time_rpc_is_prepared_and_executed_in_both_integration_variants() {
        let graph = dagrun::io::dag_from_json(include_str!("../../dag/validate.json")).unwrap();
        assert_preparation_dependencies(&graph).unwrap();
        for tag in ["test.hermit_integration", "test.hermit_integration_on_host"] {
            let step = graph.steps.iter().find(|step| step.tag() == tag).unwrap();
            assert_command_selection(step).unwrap();
            let args: Vec<String> = serde_json::from_str(&step.env[SELECTION_ENV]).unwrap();
            assert!(
                args.windows(2)
                    .any(|pair| pair == ["--test", "child_time_rpc"])
            );
            assert!(
                step.integration_test_binaries
                    .as_ref()
                    .unwrap()
                    .iter()
                    .any(|binary| binary == "child_time_rpc")
            );
            // 287 listed, less the 25 external_signal_interrupt ptrace cases
            // that need the PMU timer, which test.pmu_integration_cases
            // runs, plus the eight fatal_core_capture tests, plus the
            // scheduler-turn-cost clock-exhaustion test in
            // container_init_deadline, plus the two robust-futex exec tests
            // (https://github.com/rrnewton/hermit/issues/2082), plus the
            // run-mode rt_sigtimedwait SIGCHLD test
            // (https://github.com/rrnewton/hermit/issues/4005), plus its
            // queued-signal counterpart, plus the large-pselect6 mutable-mask
            // and tight-stack tests (https://github.com/rrnewton/hermit/pull/4053).
            assert_eq!(step.env["NEXTEST_EXPECTED_EXECUTED"], "277");
            // The explicit utimensat mtime regression
            // (https://github.com/rrnewton/hermit/issues/3565).
            assert!(
                args.windows(2)
                    .any(|pair| pair == ["--test", "utimensat_mtime"])
            );
            assert!(
                step.integration_test_binaries
                    .as_ref()
                    .unwrap()
                    .iter()
                    .any(|binary| binary == "utimensat_mtime")
            );
            // The canonical first-seen mtime regression
            // (https://github.com/rrnewton/hermit/issues/3639).
            assert!(
                args.windows(2)
                    .any(|pair| pair == ["--test", "first_seen_mtime"])
            );
            assert!(
                step.integration_test_binaries
                    .as_ref()
                    .unwrap()
                    .iter()
                    .any(|binary| binary == "first_seen_mtime")
            );
            assert!(
                args.windows(2)
                    .any(|pair| pair == ["--test", "reopened_pipe_progress"])
            );
            // The ptrace dispatch record and its DEBUG-only control.
            assert!(
                args.windows(2)
                    .any(|pair| pair == ["--test", "dispatch_stats"])
            );
            assert!(
                step.integration_test_binaries
                    .as_ref()
                    .unwrap()
                    .iter()
                    .any(|binary| binary == "reopened_pipe_progress")
            );
            assert!(
                step.integration_test_binaries
                    .as_ref()
                    .unwrap()
                    .iter()
                    .any(|binary| binary == "dispatch_stats")
            );
            assert!(
                args.windows(2)
                    .any(|pair| pair == ["--test", "clock_determinism"])
            );
            assert!(
                step.integration_test_binaries
                    .as_ref()
                    .unwrap()
                    .iter()
                    .any(|binary| binary == "clock_determinism")
            );

            let mut omitted_execution = step.clone();
            omitted_execution.cmd = omitted_execution.cmd.replace("--test child_time_rpc ", "");
            assert!(assert_command_selection(&omitted_execution).is_err());

            let mut omitted_preparation = step.clone();
            let mut args = args;
            let position = args.iter().position(|arg| arg == "child_time_rpc").unwrap();
            args.drain(position - 1..=position);
            omitted_preparation
                .env
                .insert(SELECTION_ENV.into(), serde_json::to_string(&args).unwrap());
            assert!(assert_command_selection(&omitted_preparation).is_err());
        }
    }

    #[test]
    fn a_unifying_producer_must_end_with_exactly_its_union_build() {
        let graph = dagrun::io::dag_from_json(include_str!("../../dag/validate.json")).unwrap();
        assert_producers_build_the_unified_selection(&graph).unwrap();
        let mutated = |edit: &dyn Fn(&str) -> String| {
            let mut changed = graph.clone();
            // The Cargo text lives in the compile half of the pinned producer.
            let producer = changed
                .steps
                .iter_mut()
                .find(|step| step.tag() == crate::validation_dag::PINNED_WORKSPACE_COMPILE_TAG)
                .unwrap();
            producer.cmd = edit(&producer.cmd);
            assert_ne!(
                producer.cmd,
                graph
                    .steps
                    .iter()
                    .find(|s| s.tag() == crate::validation_dag::PINNED_WORKSPACE_COMPILE_TAG)
                    .unwrap()
                    .cmd
            );
            assert_producers_build_the_unified_selection(&changed).unwrap_err()
        };
        // A second Hermit link before the union build: the producer still
        // ends with the union, so only the --bin hermit refusal catches it.
        let relinked = mutated(&|cmd| {
            cmd.replacen(
                "cargo build --locked --profile validate --workspace",
                "cargo build --locked --profile validate -p hermit --bin hermit && cargo build --locked --profile validate --workspace",
                1,
            )
        });
        assert!(
            relinked.contains("must end with its unified workspace build"),
            "{relinked}"
        );
        // A second whole-workspace build before the union build: caught only
        // by the one-workspace-build count.
        let doubled = mutated(&|cmd| {
            cmd.replacen(
                "cargo build --locked --profile validate --workspace",
                "cargo build --locked --profile validate --workspace --all-targets && cargo build --locked --profile validate --workspace",
                1,
            )
        });
        assert!(
            doubled.contains("must end with its unified workspace build"),
            "{doubled}"
        );
        // A workspace build whose features are not the union.
        let narrowed = mutated(&|cmd| cmd.replace("hermit/kvm-execution-tests,", ""));
        assert!(
            narrowed.contains("must end with its unified workspace build"),
            "{narrowed}"
        );

        // A union that no longer carries the third-party backends: every
        // selection drops the feature, and the producer follows the union, so
        // only the backend requirement refuses it.
        let mut stripped = graph.clone();
        for step in &mut stripped.steps {
            let Some(raw) = step.env.get(SELECTION_ENV).cloned() else {
                continue;
            };
            let mut args: Vec<String> = serde_json::from_str(&raw).unwrap();
            if let Some(index) = args.iter().position(|arg| arg == "--features") {
                let kept = args[index + 1]
                    .split(',')
                    .filter(|feature| *feature != "third-party-backends")
                    .collect::<Vec<_>>()
                    .join(",");
                if kept.is_empty() {
                    args.drain(index..=index + 1);
                } else {
                    args[index + 1] = kept;
                }
            }
            step.env
                .insert(SELECTION_ENV.into(), serde_json::to_string(&args).unwrap());
        }
        for profile in ["full", crate::validation_dag::HOSTED_PORTABLE_LABEL] {
            let before =
                crate::nextest_binaries::unified_prebuild_command(&graph, profile).unwrap();
            let after =
                crate::nextest_binaries::unified_prebuild_command(&stripped, profile).unwrap();
            assert!(!after.contains("third-party-backends"), "{after}");
            // The pinned compile half carries only the Cargo build, the text
            // of the union before its `prepare` tail, so it follows that part.
            let cargo_half = |union: &str| {
                union
                    .split_once(" && ./ci/nextest-binaries.rs prepare ")
                    .unwrap()
                    .0
                    .to_owned()
            };
            let (before_cargo, after_cargo) = (cargo_half(&before), cargo_half(&after));
            for step in &mut stripped.steps {
                step.cmd = if step.cmd.contains(&before) {
                    step.cmd.replace(&before, &after)
                } else {
                    step.cmd.replace(&before_cargo, &after_cargo)
                };
            }
        }
        let error = assert_producers_build_the_unified_selection(&stripped).unwrap_err();
        assert!(
            error.contains("without hermit/third-party-backends"),
            "{error}"
        );
    }

    #[test]
    fn prepared_metadata_requires_the_consumers_filesystem_root() {
        let graph = dagrun::io::dag_from_json(include_str!("../../dag/validate.json")).unwrap();
        assert_preparation_dependencies(&graph).unwrap();
        // The local profiles have no host workspace producer since the
        // one-build change of 2026-09-30; the hosted variant is the host-root
        // producer, and its command differs from the pinned payload only in
        // the graph profile it prepares and that profile's unified build.
        let hosted = graph
            .steps
            .iter()
            .find(|step| step.tag() == "build.workspace_on_host")
            .unwrap();
        let image = graph
            .steps
            .iter()
            .find(|step| step.tag() == "build.workspace_in_pinned_root")
            .unwrap();
        // The pinned producer's payload is split across its compile and
        // preparation nodes; rejoined, it is the hosted command.
        let payload = crate::validation_dag::pinned_workspace_producer_payload(&graph).unwrap();
        let unified = |profile| crate::nextest_binaries::unified_prebuild_command(&graph, profile);
        assert_eq!(
            payload,
            hosted.cmd.replace(
                &unified(crate::validation_dag::HOSTED_PORTABLE_LABEL).unwrap(),
                &unified("full").unwrap()
            )
        );
        for (consumer, producer, wrong_command) in [
            (
                "test.regular_crates",
                image.tag(),
                execution_command(image).unwrap(),
            ),
            (
                "test.regular_crates_on_host",
                hosted.tag(),
                image.cmd.clone(),
            ),
        ] {
            let mut wrong_root = graph.clone();
            wrong_root
                .steps
                .iter_mut()
                .find(|step| step.tag() == producer)
                .unwrap()
                .cmd = wrong_command;
            // Check this consumer first while retaining the complete authored
            // preparation population and all other nodes/dependencies.
            wrong_root.steps.sort_by_key(|step| step.tag() != consumer);
            let error = assert_preparation_dependencies(&wrong_root).unwrap_err();
            assert!(
                error.starts_with(consumer) && error.contains("same filesystem root"),
                "{error}"
            );
        }
    }

    #[test]
    fn every_nextest_command_and_inventory_requires_the_declared_preparation() {
        let graph = dagrun::io::dag_from_json(include_str!("../../dag/validate.json")).unwrap();
        assert_preparation_dependencies(&graph).unwrap();
        let original = graph
            .steps
            .iter()
            .find(|step| step.tag() == "privileged-test.cli_kvm")
            .unwrap();
        assert_command_selection(original).unwrap();
        let portable = crate::nextest_binaries::config_selections(&graph, "portable").unwrap();
        let full = crate::nextest_binaries::config_selections(&graph, "full").unwrap();
        let hardware: Vec<String> = serde_json::from_str(&original.env[SELECTION_ENV]).unwrap();
        let hardware_key = crate::nextest_binaries::selection_key(&hardware);
        assert_eq!(full.get(&hardware_key), Some(&hardware));
        assert!(!portable.contains_key(&hardware_key));
        assert_eq!(full.len(), portable.len() + 1);
        for (key, selection) in &portable {
            assert_eq!(full.get(key), Some(selection));
        }
        for tag in ["test.hermit_unit", "test.hermit_unit_on_host"] {
            let step = graph.steps.iter().find(|step| step.tag() == tag).unwrap();
            let selection: Vec<String> = serde_json::from_str(&step.env[SELECTION_ENV]).unwrap();
            assert!(selection.contains(&"third-party-backends,kvm-native-test-support".into()));
            assert!(
                !selection
                    .iter()
                    .any(|arg| arg.contains("kvm-execution-tests"))
            );
        }
        let hosted_producer = graph
            .steps
            .iter()
            .find(|step| step.tag() == "build.workspace_on_host")
            .unwrap();
        assert!(
            hosted_producer
                .cmd
                .ends_with("./ci/nextest-binaries.rs prepare hosted-portable")
        );
        let hosted_artifact = graph
            .steps
            .iter()
            .find(|step| step.tag() == "build.e2e_artifact_on_host")
            .unwrap();
        let hosted_unit = graph
            .steps
            .iter()
            .find(|step| step.tag() == "test.hermit_unit_on_host")
            .unwrap();
        assert!(
            hosted_unit
                .deps
                .iter()
                .any(|dependency| dependency == "build.e2e_artifact_on_host")
        );
        assert!(
            hosted_artifact
                .deps
                .iter()
                .any(|dependency| dependency == "build.workspace_on_host")
        );
        let mut missing_hardware = graph.clone();
        let producer = missing_hardware
            .steps
            .iter_mut()
            .find(|step| step.tag() == "build.workspace_in_pinned_root")
            .unwrap();
        assert!(
            execution_command(producer)
                .unwrap()
                .ends_with("./ci/nextest-binaries.rs prepare full")
        );
        producer.cmd = producer.cmd.replace(
            "./ci/nextest-binaries.rs prepare full",
            "./ci/nextest-binaries.rs prepare portable",
        );
        missing_hardware
            .steps
            .sort_by_key(|step| step.tag() != "privileged-test.cli_kvm");
        let error = assert_preparation_dependencies(&missing_hardware).unwrap_err();
        assert!(
            error.starts_with("privileged-test.cli_kvm") && error.contains("same filesystem root"),
            "{error}"
        );
        let direct = graph
            .steps
            .iter()
            .find(|step| step.tag() == "privileged-only-cpuid.faulting")
            .unwrap();
        assert_command_selection(direct).unwrap();
        let mut wrong_target = direct.clone();
        wrong_target.cmd = wrong_target.cmd.replace(
            "executable hermit-detcore tests_misc",
            "executable hermit-detcore tests_parallelism",
        );
        assert!(assert_command_selection(&wrong_target).is_err());
        let mut missing_direct = direct.clone();
        missing_direct.env.remove(SELECTION_ENV);
        assert!(assert_command_selection(&missing_direct).is_err());
        for mutation in ["required", "selection", "run", "list", "raw-cargo"] {
            let mut changed = original.clone();
            match mutation {
                "required" => {
                    changed.env.remove(REQUIRED_ENV);
                }
                "selection" => {
                    changed.env.insert(SELECTION_ENV.into(), "[]".into());
                }
                "run" => {
                    changed.cmd = changed.cmd.replace(
                        "./ci/run-nextest-counted.sh",
                        "./ci/run-nextest-counted.sh --all-features",
                    );
                }
                "list" => {
                    changed.cmd = changed.cmd.replace(
                        "nextest-binaries.rs list",
                        "nextest-binaries.rs list --all-features",
                    );
                }
                "raw-cargo" => {
                    changed.cmd = changed
                        .cmd
                        .replace("./ci/nextest-binaries.rs list", "cargo nextest list");
                }
                _ => unreachable!(),
            }
            assert!(
                assert_command_selection(&changed).is_err(),
                "accepted {mutation}"
            );
        }
        let mut missing = graph.clone();
        missing
            .steps
            .iter_mut()
            .find(|step| step.tag() == "quick.detcore_unit")
            .unwrap()
            .deps
            .retain(|dep| dep != "quick.build");
        assert!(
            assert_preparation_dependencies(&missing)
                .unwrap_err()
                .contains("quick.detcore_unit")
        );
    }

    #[test]
    fn build_selection_parser_keeps_quoted_filter_punctuation_literal() {
        let args = command_arguments("./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit --test cli -E 'test(/a; b/)' -- --ignored; exit $?", "run-nextest-counted.sh").unwrap();
        assert_eq!(
            args,
            [
                "-p",
                "hermit",
                "--test",
                "cli",
                "-E",
                "test(/a; b/)",
                "--",
                "--ignored"
            ]
        );
    }
}
