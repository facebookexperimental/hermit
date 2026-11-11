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
            "hermit-detcore-tests",
            "--exclude",
            "hermit-run",
            "--exclude",
            "hermetic_infra_hermit_flaky-tests",
        ]),
        "test.hermit_unit" | "privileged-test.pmu_ptrace_completion_cases" => Some(&[
            "-p",
            "hermit-run",
            "--features",
            "liteinst,kvm-native-test-support",
            "--lib",
            "--bins",
        ]),
        // The hermit_unit selection plus native-exec, prepared in its own
        // cohort (nextest_binaries::NATIVE_EXEC_COHORT) so the feature never
        // enters the default workspace union or the production binary. The
        // node's filters own exactly the cases this feature adds, plus the
        // run-config sample-table cases that check its feature-gated options.
        "test.hermit_native_exec" => Some(&[
            "-p",
            "hermit-run",
            "--features",
            "liteinst,kvm-native-test-support,native-exec",
            "--lib",
            "--bins",
        ]),
        "test.detcore_unit" => Some(&["-p", "hermit-detcore", "--lib", "--bins"]),
        "test.inherited_terminal_read" => Some(&[
            "-p",
            "hermit-run",
            "--features",
            "liteinst",
            "--test",
            "inherited_terminal_read",
        ]),
        "test.detcore_misc"
        | "privileged-build.privileged_tests"
        | "privileged-cpuid.faulting"
        | "privileged-only-cpuid.faulting"
        | "privileged-only-cpuid.faulting_on_host"
        | "super.post_fork_scheduling_diagnostics"
        | "super.network_syscall_determinism_diagnostic" => {
            Some(&["-p", "hermit-detcore-tests", "--test", "tests_misc"])
        }
        "test.detcore_time" | "privileged-test.pmu_detcore_time_cases" => {
            Some(&["-p", "hermit-detcore-tests", "--test", "tests_time"])
        }
        "test.detcore_parallel"
        | "super.weekly_pmu_parallel_memory_diagnostic_mem_race_bottom_detcore"
        | "super.weekly_pmu_parallel_memory_diagnostic_mem_race_default_detcore"
        | "super.weekly_pmu_parallel_memory_diagnostic_mem_race_middle_detcore"
        | "super.weekly_pmu_parallel_memory_diagnostic_mem_race_top_detcore" => {
            Some(&["-p", "hermit-detcore-tests", "--test", "tests_parallelism"])
        }
        "test.hermit_integration" | "test.pmu_integration_cases" => Some(&[
            "-p",
            "hermit-run",
            "--features",
            "liteinst",
            "--test",
            "aio_nr_determinism",
            "--test",
            "arch_prctl",
            "--test",
            "arch_status_determinism",
            "--test",
            "block_inflight_determinism",
            "--test",
            "btrfs_commit_determinism",
            "--test",
            "btrfs_pinned_bytes_determinism",
            "--test",
            "btrfs_reservation_determinism",
            "--test",
            "btrfs_reserved_bytes_determinism",
            "--test",
            "buddyinfo_determinism",
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
            "compression",
            "--test",
            "container_init_deadline",
            "--test",
            "copy_file_range_refusal",
            "--test",
            "cppc_feedback_determinism",
            "--test",
            "cpufreq_avg_determinism",
            "--test",
            "cpuidle_determinism",
            "--test",
            "deadline_slice_cap",
            "--test",
            "dentry_state_determinism",
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
            "flock_exclusion",
            "--test",
            "fp_reduction_determinism",
            "--test",
            "frontier_app_benchmarks",
            "--test",
            "full_summary_futex_order",
            "--test",
            "futex2_refusal",
            "--test",
            "getitimer_determinism",
            "--test",
            "hashseed_determinism",
            "--test",
            "host_kernel_probes",
            "--test",
            "host_security_identity",
            "--test",
            "image_archive",
            "--test",
            "inode_nr_determinism",
            "--test",
            "integration_matrix",
            "--test",
            "io_uring_ring_determinism",
            "--test",
            "ipc_determinism",
            "--test",
            "irq_per_cpu_determinism",
            "--test",
            "kernel_keyring",
            "--test",
            "key_users_determinism",
            "--test",
            "kvm_harder",
            "--test",
            "kvm_lifecycle_timing_log_level",
            "--test",
            "language_runtime_determinism",
            "--test",
            "leveldb",
            "--test",
            "lit",
            "--test",
            "madvise",
            "--test",
            "meminfo_determinism",
            "--test",
            "mmap_determinism",
            "--test",
            "mmap_stress_determinism",
            "--test",
            "mount_introspection",
            "--test",
            "name_to_handle_refusal",
            "--test",
            "netlink_table_determinism",
            "--test",
            "netns_cookie_determinism",
            "--test",
            "node_vmstat_determinism",
            "--test",
            "numa_maps_determinism",
            "--test",
            "optional_memory_features",
            "--test",
            "perf_event_refusal",
            "--test",
            "pidfd_creation",
            "--test",
            "ppoll_simulation",
            "--test",
            "prctl_dumpable_determinism",
            "--test",
            "privileged_observation",
            "--test",
            "proc_fd_link_determinism",
            "--test",
            "proc_fdinfo_determinism",
            "--test",
            "proc_locks_determinism",
            "--test",
            "process_isolation_refusals",
            "--test",
            "procfs_determinism",
            "--test",
            "procfs_positioned_determinism",
            "--test",
            "prodcons_determinism",
            "--test",
            "protocols_determinism",
            "--test",
            "pselect6_simulation",
            "--test",
            "ptrace_refusal",
            "--test",
            "pty_nr_determinism",
            "--test",
            "python_stdlib",
            "--test",
            "random_determinism",
            "--test",
            "random_uuid_determinism",
            "--test",
            "redis_strict",
            "--test",
            "relaxed_flag_matrix",
            "--test",
            "remap_file_pages_refusal",
            "--test",
            "reopened_pipe_progress",
            "--test",
            "robust_futex_owner_death",
            "--test",
            "robust_list_queries",
            "--test",
            "sched_setattr_abi",
            "--test",
            "sched_setattr_noop",
            "--test",
            "scheduler_deadlock",
            "--test",
            "scheduler_policy_queries",
            "--test",
            "self_sched_determinism",
            "--test",
            "self_schedstat_determinism",
            "--test",
            "sigmask_preemption",
            "--test",
            "signal_determinism",
            "--test",
            "smaps_determinism",
            "--test",
            "smaps_rollup_determinism",
            "--test",
            "so_incoming_cpu",
            "--test",
            "socket_cookie_determinism",
            "--test",
            "socket_ioctl_timestamp_determinism",
            "--test",
            "socket_timestamp_determinism",
            "--test",
            "sockstat_determinism",
            "--test",
            "softnet_stat_determinism",
            "--test",
            "sqlite_veryquick",
            "--test",
            "standalone",
            "--test",
            "stress_suite",
            "--test",
            "swaps_determinism",
            "--test",
            "syscall_file_io",
            "--test",
            "syscall_file_metadata",
            "--test",
            "syscall_quick_wins",
            "--test",
            "sysv_legacy_fallbacks",
            "--test",
            "tcp_info_determinism",
            "--test",
            "terminal_child_selectors",
            "--test",
            "thp_stats_determinism",
            "--test",
            "thread_scheduling_fairness",
            "--test",
            "thread_self_procfs_determinism",
            "--test",
            "thread_sync_determinism",
            "--test",
            "uevent_seqnum_determinism",
            "--test",
            "unix_socket_table_determinism",
            "--test",
            "utimensat_mtime",
            "--test",
            "verification_report_cli",
            "--test",
            "verification_report_consumers",
            "--test",
            "verify_claim_names_its_limit",
            "--test",
            "version_provenance",
            "--test",
            "vmstat_determinism",
            "--test",
            "waitid_signal_interrupt",
            "--test",
            "writev_determinism",
            "--test",
            "zero_copy_pipe_fallback",
        ]),
        "test.arbitrary_binaries" => Some(&[
            "-p",
            "hermit-run",
            "--features",
            "liteinst",
            "--test",
            "arbitrary_binaries",
        ]),
        "test.liteinst_allocator" => Some(&[
            "-p",
            "hermit-run",
            "--features",
            "liteinst",
            "--test",
            "liteinst_allocator",
        ]),
        "test.liteinst_stacks" => Some(&[
            "-p",
            "hermit-run",
            "--features",
            "liteinst",
            "--test",
            "liteinst_stacks",
        ]),
        "test.cli"
        | "test.isolated_dbt_workdir"
        | "test.cli_on_host"
        | "privileged-test.pmu_cli_cases"
        | "privileged-test.binder_cli_cases"
        | "privileged-cpuid.cpu_profile_v3"
        | "super.dbt_pipe_backpressure_diagnostic"
        | "super.dbt_failed_exec_recovery_diagnostic"
        | "super.dbt_unsupported_syscall_aggregation_diagnostic"
        | "super.dbt_strict_blocked_stdin_teardown_diagnostic"
        | "super.dbt_guest_stderr_isolation_diagnostic" => Some(&[
            "-p",
            "hermit-run",
            "--features",
            "liteinst",
            "--test",
            "cli",
        ]),
        "privileged-test.cli_kvm"
        | "privileged-only-test.cli_kvm"
        | "privileged-only-test.cli_kvm_on_host" => Some(&[
            "-p",
            "hermit-run",
            "--features",
            "liteinst,kvm-execution-tests",
            "--lib",
            "--test",
            "cli",
        ]),
        // The deadline_slice_cap suite under the KVM node's features, the
        // same feature set as the KVM CLI node above.
        "privileged-test.deadline_slice_cap_kvm" => Some(&[
            "-p",
            "hermit-run",
            "--features",
            "liteinst,kvm-execution-tests",
            "--test",
            "deadline_slice_cap",
        ]),
        "test.sabre_examples" | "sabrecompat.sabre_examples" => Some(&[
            "-p",
            "hermit-run",
            "--features",
            "liteinst",
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
            "hermit-run",
            "--features",
            "liteinst",
            "--test",
            "hermit_modes",
        ]),
        "test.app_strict_verify" | "super.managed_jvm_strict_verify_diagnostics" => Some(&[
            "-p",
            "hermit-run",
            "--features",
            "liteinst",
            "--test",
            "app_strict_verify",
        ]),
        "test.command_strict_verify" => Some(&[
            "-p",
            "hermit-run",
            "--features",
            "liteinst",
            "--test",
            "command_strict_verify",
        ]),
        "test.ignored_syscall_regressions" => Some(&[
            "-p",
            "hermit-run",
            "--features",
            "liteinst",
            "--test",
            "epoll_determinism",
            "--test",
            "rcx_canonicalization",
        ]),
        "test.rr_suite_contract" => Some(&[
            "-p",
            "hermit-run",
            "--features",
            "liteinst",
            "--test",
            "rr_suite",
        ]),
        "quick.detcore_unit" => Some(&["-p", "hermit-detcore", "--lib"]),
        "super.relaxed_hermit_flag_matrix" => Some(&[
            "-p",
            "hermit-run",
            "--features",
            "liteinst",
            "--test",
            "relaxed_flag_matrix",
        ]),
        "super.pselect_signal_interruption_diagnostic" => Some(&[
            "-p",
            "hermit-run",
            "--features",
            "liteinst",
            "--test",
            "pselect6_simulation",
        ]),
        "test.record_replay" | "super.record_replay_matrix_diagnostic" => Some(&[
            "-p",
            "hermit-run",
            "--features",
            "liteinst",
            "--test",
            "record_replay",
        ]),
        "super.ipc_determinism_diagnostic" => Some(&[
            "-p",
            "hermit-run",
            "--features",
            "liteinst",
            "--test",
            "ipc_determinism",
        ]),
        "super.random_source_determinism_diagnostic" => Some(&[
            "-p",
            "hermit-run",
            "--features",
            "liteinst",
            "--test",
            "random_determinism",
        ]),
        "super.threaded_integration_matrix_diagnostic" => Some(&[
            "-p",
            "hermit-run",
            "--features",
            "liteinst",
            "--test",
            "integration_matrix",
        ]),
        "super.weekly_portable_chaos_cases" | "super.weekly_ignored_portable_chaos_cases" => {
            Some(&[
                "-p",
                "hermit-run",
                "--features",
                "liteinst",
                "--test",
                "stress_suite",
            ])
        }
        "super.pmu_analyze_hello_race_stress_calibrated_skid" => Some(&[
            "-p",
            "hermit-run",
            "--features",
            "liteinst",
            "--test",
            "analyze",
        ]),
        "super.full_leveldb_strict_determinism" => Some(&[
            "-p",
            "hermit-run",
            "--features",
            "liteinst",
            "--test",
            "leveldb",
        ]),
        "super.sqlite_veryquick_strict_determinism" => Some(&[
            "-p",
            "hermit-run",
            "--features",
            "liteinst",
            "--test",
            "sqlite_veryquick",
        ]),
        _ => None,
    }
}

/// The KVM gap-ledger runner, which lists and runs a suite through the prepared
/// Nextest executables with the Cargo selection after its `--`.
const KVM_GAP_LEDGER_RUN: &str = "kvm-gap-ledger.rs run";

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
    for marker in [
        "run-nextest-counted.sh",
        "nextest-binaries.rs list",
        KVM_GAP_LEDGER_RUN,
    ] {
        if !command.contains(marker) {
            continue;
        }
        let mut arguments = command_arguments(&command, marker)?;
        if marker == KVM_GAP_LEDGER_RUN {
            // `run -- NEXTEST_ARGS`: the runner hands NEXTEST_ARGS to both its
            // Nextest list and its counted run, and reads its ledger from
            // crate::known_gaps::LEDGER_ENV.
            let separator = arguments
                .iter()
                .position(|argument| argument == "--")
                .ok_or_else(|| format!("{tag} gives {KVM_GAP_LEDGER_RUN} no `--`"))?;
            arguments.drain(..=separator);
        }
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

// `prepare` is one fixed command; only its fixed, profile-matched packaging
// finalizer may follow it, plus, for the pinned workspace prepare node
// alone, its fixed early-artifact gate after that finalizer. A non-default
// cohort's prepare is the one other fixed form (`cohort_prepare_command`):
// prepare under the run's build mode, then assert, with no finalizer.
// Never truncate an arbitrary shell command at `&&`.
fn preparation_profile<'a>(
    tag: &str,
    command: &'a str,
) -> Result<Option<(&'a str, crate::nextest_binaries::PreparedCohort)>, String> {
    let marker = "./ci/nextest-binaries.rs prepare ";
    let Some((_, rest)) = command.split_once(marker) else {
        return Ok(None);
    };
    if command.matches(marker).count() != 1 {
        return Err(format!("{tag} has duplicate prepared commands"));
    }
    let profile = rest.split_whitespace().next().unwrap_or("");
    let cohort = crate::nextest_binaries::NATIVE_EXEC_COHORT;
    if let Ok(expected) = crate::nextest_binaries::cohort_prepare_command(profile, cohort) {
        if expected.strip_prefix(marker) == Some(rest) {
            return Ok(Some((profile, cohort)));
        }
    }
    // The full producer appends `--build-mode "${HERMIT_VALIDATE_BUILD_MODE:-cargo}"`
    // before its finalizer; the bare and mode-less finalized forms stay exact.
    let tails: Vec<String> = crate::nextest_binaries::cargo_profile_for(profile)
        .map(|directory| {
            vec![
                format!(
                    "{profile} --build-mode \"${{HERMIT_VALIDATE_BUILD_MODE:-cargo}}\" && target/{directory}/stage-detcore-liteinst --git target"
                ),
                format!("{profile} && target/{directory}/stage-detcore-liteinst --git target"),
            ]
        })
        .unwrap_or_default();
    let gated = if tag == crate::validation_dag::PINNED_WORKSPACE_PREPARE_TAG {
        tails
            .iter()
            .map(|tail| {
                format!(
                    "{tail}{}",
                    crate::validation_dag::EARLY_ARTIFACT_GATE_SUFFIX
                )
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    if profile.is_empty()
        || (rest != profile
            && !tails.iter().any(|tail| tail == rest)
            && !gated.iter().any(|tail| tail == rest))
    {
        return Err(format!(
            "{tag} has an ambiguous prepared profile or finalizer"
        ));
    }
    Ok(Some((profile, crate::nextest_binaries::DEFAULT_COHORT)))
}

/// A producer of a unifying profile (`nextest_binaries::unifies`) must end
/// with that profile's unified workspace build followed by `prepare`. Any other
/// build -- different features, or a second `-p hermit-run --bin hermit` link --
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
        let Some((profile, cohort)) = preparation_profile(&step.tag(), &command)? else {
            continue;
        };
        if !cohort.is_default() {
            // A cohort producer builds nothing in its prepare node: its
            // compile node built the executables, and widening this payload
            // to the workspace union is exactly what the cohort exists to
            // prevent. Its one fixed form is `cohort_prepare_command`.
            let expected = crate::nextest_binaries::cohort_prepare_command(profile, cohort)?;
            if !command.ends_with(&expected) || command.contains("--workspace") {
                return Err(format!(
                    "{} must end with its cohort prepare `{expected}` and build no workspace selection",
                    step.tag()
                ));
            }
            continue;
        }
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
        if !expected.contains("hermit-run/liteinst") {
            return Err(format!(
                "{} links the E2E Hermit without hermit-run/liteinst: `{expected}`",
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

    use crate::nextest_binaries::COHORT_ENV;
    use crate::nextest_binaries::PreparedCohort;
    use crate::nextest_binaries::SELECTION_ENV;
    use crate::nextest_binaries::config_selections_in_cohort;
    use crate::nextest_binaries::selection_key;
    let by_tag = cfg
        .steps
        .iter()
        .map(|step| (step.tag(), step))
        .collect::<BTreeMap<_, _>>();
    let mut producers = BTreeMap::new();
    for step in &cfg.steps {
        let command = execution_command(step)?;
        if let Some((profile, cohort)) = preparation_profile(&step.tag(), &command)? {
            producers.insert(
                step.tag(),
                (
                    step.cmd.starts_with("./ci/hermetic/run-in-pinned-root.sh "),
                    cohort,
                    config_selections_in_cohort(cfg, profile, cohort)?,
                ),
            );
        }
    }
    for step in &cfg.steps {
        let Some(raw) = step.env.get(SELECTION_ENV) else {
            continue;
        };
        let consumer_cohort =
            PreparedCohort::from_slug(step.env.get(COHORT_ENV).map(String::as_str))
                .map_err(|e| format!("{}: {e}", step.tag()))?;
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
            producers
                .get(tag)
                .is_some_and(|(pinned, cohort, selections)| {
                    *pinned == step.cmd.starts_with("./ci/hermetic/run-in-pinned-root.sh ")
                        && *cohort == consumer_cohort
                        && selections.get(&key) == Some(&args)
                })
        }) {
            return Err(format!(
                "{} can run before any producer in the same filesystem root and cohort {} of its exact Cargo selection {args:?}",
                step.tag(),
                consumer_cohort.name()
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
            // and tight-stack tests (https://github.com/rrnewton/hermit/pull/4053),
            // plus the four targeted-chaos sched_yield tests
            // (https://github.com/rrnewton/hermit/issues/4068), plus the ppoll
            // tight-stack test (https://github.com/rrnewton/hermit/issues/4113),
            // plus the stop-signal SIGHUP poll test
            // (https://github.com/rrnewton/hermit/issues/4111), plus the two
            // single-virtual-CPU procfs tests in procfs_determinism
            // (https://github.com/rrnewton/hermit/issues/4160), plus the
            // random-heuristic sched_yield progress tests, one per chaos setting
            // and guest mode (https://github.com/rrnewton/hermit/issues/4150,
            // https://github.com/rrnewton/hermit/pull/4201), plus the
            // crowded-stack pselect6 record refusal test (Claude review of
            // https://github.com/rrnewton/hermit/pull/4166), plus the single-CPU
            // sysfs numbers and node files test and the directory listing test
            // (https://github.com/rrnewton/hermit/issues/4160), plus the four
            // ppoll_simulation tests (https://github.com/rrnewton/hermit/issues/4042),
            // plus the masked epoll_pwait test
            // (https://github.com/rrnewton/hermit/issues/4041), plus the
            // epoll_pwait2 run test (https://github.com/rrnewton/hermit/issues/4228),
            // less the 13 tests of the retired run_evidence target
            // (https://github.com/rrnewton/hermit/issues/4275),
            // plus the paired user-bind mountinfo test, the test that a user
            // bind changes no other row, and the test that user binds show their
            // targets whatever earlier mounts did
            // (https://github.com/rrnewton/hermit/issues/4266), plus the nine
            // DBT robust-futex owner-death tests and the two ptrace ones
            // (https://github.com/rrnewton/hermit/issues/4238),
            // plus the 67 tests of the 55 binaries no node ran before that pass on the
            // host and in the pinned root
            // (https://github.com/rrnewton/hermit/issues/2791), plus the 32
            // tests of the six DBT binaries that now take the hermetic workdir,
            // plus cppc_feedback_determinism's and meminfo_determinism's one each
            // (https://github.com/rrnewton/hermit/issues/4355), plus redis_strict's
            // three, plus kvm_lifecycle_timing_log_level's and sqlite_veryquick's
            // one each (https://github.com/rrnewton/hermit/issues/4321), plus the
            // two mount-propagation tests
            // (https://github.com/rrnewton/hermit/issues/4167), plus the four DBT
            // and ptrace exec owner-death tests
            // (https://github.com/rrnewton/hermit/issues/4268), plus
            // integration_matrix's one, kvm_harder's four, sigmask_preemption's
            // one and stress_suite's two non-ignored tests
            // (https://github.com/rrnewton/hermit/issues/2791), plus
            // flock_exclusion's 23 (its 24th is ignored until
            // https://github.com/rrnewton/hermit/issues/4173 is fixed), plus
            // frontier_app_benchmarks' 3 and language_runtime_determinism's 4
            // whose programs the pinned root has.
            // Plus signal_determinism's DBT and ptrace SIGKILL tests
            // (https://github.com/rrnewton/hermit/issues/4362), plus
            // the five process-cgroup tests
            // (https://github.com/rrnewton/hermit/issues/4378).
            // Plus robust_futex_owner_death's two DBT fatal-signal tests
            // (https://github.com/rrnewton/hermit/issues/4278).
            // Plus lit's 18 runner and FileCheck-matcher tests, its 19
            // fixtures without a Rust or Go guest less the ignored cat.test,
            // its completeness test, and its 57 fixtures with a Rust guest.
            // Plus signal_determinism's DBT thread-directed SIGKILL refusal test.
            // The local node runs in shards, which together execute the same.
            // The listed population is guarded at run time by the census
            // (listed IDs vs the last landed population), not by a total
            // asserted here; this test keeps the selection/binary identity
            // assertions below.
            assert!(
                !step.env.contains_key("NEXTEST_EXPECTED_EXECUTED"),
                "{tag} declares a retired Nextest count pin"
            );
            // The ten DBT SIGKILL scenarios replace one aggregate test (+9),
            // and container_init_deadline gains the parent-died-before-arming
            // test (+1). And procfs_determinism's directory-alias
            // record/replay test (+1,
            // https://github.com/rrnewton/hermit/issues/4213).
            // Runtime-private maps target/alias/lifetime coverage adds one.
            // And the staged-bind kernel-path and install-time bind tests
            // (+2, K3a of https://github.com/rrnewton/hermit/issues/4213).
            // And proc_locks_determinism's six new tests (+6): 586.
            // And its four descriptor-table ledger tests (+4): 590.
            // signal_determinism's eighteen ptrace_delivery_check_* tests (A5
            // step 3.2) add eighteen, and six more add six, less the spin
            // case, which test.pmu_integration_cases runs, and forced-blocked
            // adds one (+24): 614.
            // And proc_locks_determinism's deferred-refusal split, version-gate
            // and vfork tests (+3): 617.
            // And its fake-/proc-under-chroot test (+1): 618.
            // signal_determinism's five ptrace_signal_state_kept_* tests (A5
            // step 3.0a, +5): 623.
            // Its six ptrace_sigsegv_state_kept_* tests (A5 step 3.0b, +6):
            // 629.
            // Five multithreaded ptrace_delivery_check_* tests (A5 step 3.2b,
            // +5): 634.
            // Three more ptrace_delivery_check_* tests (A5 step 3.2b part 2,
            // +3): 637.
            // Five arrived-first ptrace_delivery_check_* tests (+5): 642.
            // flock_exclusion's three pipe-fork-vfork tests, copied-child
            // pidfd_getfd and recvmsg-import refusals and native-lock control,
            // and proc_locks_determinism's copied-child OFD unlock and
            // stale-flock-record tests (+8): 650.
            // Arrived-first epoll_pwait2 and zero-timeout ppoll tests (+2): 652.
            // A fault at a handler's first instruction (+1): 653.
            // Two chained deliveries at rt_sigreturn's exit (+2): 655.
            // A wait mask differing only in forced signals (+1): 656.
            // The dedicated pselect6 wrapper-validation fixture adds one
            // integration identity beyond the landed signals tests: 657.
            // procfs_determinism's runtime_private_smaps_keep_the_opened_targets_guest_mapping
            // (DBT Smaps projection, +1): 658.
            // Less verify_claim_names_its_limit's
            // under_passthru_opt_an_unobserved_rebinding_names_no_host_input_change,
            // ignored as a known failure by
            // https://github.com/rrnewton/hermit/pull/4559 (-1): 657.
            // procfs_determinism's mapping_capture_live module, the DBT NUMA
            // coherent-capture tests from the dbt NUMA + E5 landing (Hermit
            // b4cd1664 and 74ed9f94; +28): 685.
            // Split the five mapping-capture birth cases into separately bounded
            // test identities, preserving every case and assertion (+4): 689.
            // redis_strict's declared-Redis tests (Hermit 9d901894): the Redis L2
            // test moves there from frontier_app_benchmarks, plus
            // a_declared_redis_pair_is_used_only_when_declared and
            // a_bad_redis_declaration_is_refused_rather_than_replaced (+2): 691.
            // Separately name the optional e9patch source-inspection leg (+1): 692.
            // And the two-run syscall-histogram test in dispatch_stats (+1): 693.
            // random_determinism's
            // a_schedule_replay_hands_the_guest_the_recorded_random_values (+1):
            // 694.
            // fatal_core_capture's
            // a_fault_ends_the_run_while_the_leader_waits_on_a_condition_variable
            // (+1): 695.
            // And robust_futex_owner_death's native and ptrace SIGKILL
            // owner-death tests: the population measures 698 at this
            // integration head (the PR's +2 on its stale base of 694,
            // re-measured here rather than carried over).
            // Recount at 3bdd41fda38: the selection lists 700
            // (`cargo nextest list --profile ci` on the on-host
            // selection), matching the full validate at that tip, whose
            // third shard executed 349 (347 + 2), all passed: 700.
            // Recount at 360e99b2e24: shard 2 grew to 292 (61 + 292 +
            // 349), so the on-host population is 702.
            // The integrated chain grows shard 3 to 361 (61 + 292 + 361),
            // so the on-host population is 714.
            // Shard 2 then grows to 293 (61 + 293 + 361): 715.
            // The DBT robust-futex #[ignore] (issue 5023) drops shard 3
            // to 360 (61 + 293 + 360): 714.
            // The runtime_private maps/smaps #[ignore]s (same issue) drop
            // shard 3 to 358 (61 + 293 + 358): 712.
            // Shard 2 recounts to 294 in run1470 (61 + 294 + 358): 713.
            // The split retains seven package-identity cases in
            // version_provenance and transfers the copied-child TTY case to
            // its DynamoRIO owner: 713 + 7 - 1 = 719.
            // PR 4630's three fatal-core cases grow shard 2: 719 + 3 = 722,
            // measured by the batch3 donor validate at f7ec284e.
            // image_archive's eleven local-image tests join the second
            // shard: 722 + 11 = 733, listed.
            // (Population total intentionally unasserted: census-guarded.)
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
    fn preparation_accepts_only_the_exact_profile_matched_finalizer() {
        for profile in ["full", "hosted-portable"] {
            let bare = format!("prefix && ./ci/nextest-binaries.rs prepare {profile}");
            let finalized =
                format!("{bare} && target/validate/stage-detcore-liteinst --git target");
            for command in [&bare, &finalized] {
                assert_eq!(
                    preparation_profile("producer", command).unwrap(),
                    Some((profile, crate::nextest_binaries::DEFAULT_COHORT))
                );
            }
            for refused in [
                format!("{bare} && true"),
                format!("{finalized} && true"),
                format!("{bare} && target/debug/stage-detcore-liteinst --git target"),
                format!("{finalized} --source-copy target"),
                format!("{bare} && ./ci/nextest-binaries.rs prepare {profile}"),
                format!("{bare} extra"),
            ] {
                assert!(
                    preparation_profile("producer", &refused).is_err(),
                    "{refused}"
                );
            }
        }
        // The pinned workspace prepare node alone may append its fixed
        // early-artifact gate after the finalizer; no other producer may.
        let gated = format!(
            "prefix && ./ci/nextest-binaries.rs prepare full && target/validate/stage-detcore-liteinst --git target{}",
            crate::validation_dag::EARLY_ARTIFACT_GATE_SUFFIX
        );
        assert_eq!(
            preparation_profile(crate::validation_dag::PINNED_WORKSPACE_PREPARE_TAG, &gated)
                .unwrap(),
            Some(("full", crate::nextest_binaries::DEFAULT_COHORT))
        );
        assert!(preparation_profile("producer", &gated).is_err());
        // A non-default cohort's prepare is the one other exact form:
        // prepare under the run's build mode, then assert, no finalizer.
        let cohort_command = format!(
            "prefix && {}",
            crate::nextest_binaries::cohort_prepare_command(
                "full",
                crate::nextest_binaries::NATIVE_EXEC_COHORT
            )
            .unwrap()
        );
        assert_eq!(
            preparation_profile("producer", &cohort_command).unwrap(),
            Some(("full", crate::nextest_binaries::NATIVE_EXEC_COHORT))
        );
        for refused in [
            // Without its assert half.
            "prefix && ./ci/nextest-binaries.rs prepare full --build-mode \"${HERMIT_VALIDATE_BUILD_MODE:-cargo}\" --cohort native-exec"
                .to_string(),
            // With the default cohort's finalizer appended.
            format!("{cohort_command} && target/validate/stage-detcore-liteinst --git target"),
            // An unregistered slug.
            cohort_command.replace("native-exec", "bogus"),
            // A non-unifying profile has no cohort prepare form.
            cohort_command.replace("prepare full", "prepare quick"),
        ] {
            assert!(
                preparation_profile("producer", &refused).is_err(),
                "{refused}"
            );
        }
        for profile in ["quick", "super"] {
            let bare = format!("./ci/nextest-binaries.rs prepare {profile}");
            assert_eq!(
                preparation_profile("producer", &bare).unwrap(),
                Some((profile, crate::nextest_binaries::DEFAULT_COHORT))
            );
            for directory in ["debug", "validate"] {
                let refused =
                    format!("{bare} && target/{directory}/stage-detcore-liteinst --git target");
                assert!(
                    preparation_profile("producer", &refused).is_err(),
                    "{refused}"
                );
            }
        }
        assert!(preparation_profile("producer", "./ci/nextest-binaries.rs prepare ").is_err());
        assert_eq!(preparation_profile("other", "cargo build").unwrap(), None);
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
                "cargo build --locked --profile validate -p hermit-run --bin hermit && cargo build --locked --profile validate --workspace",
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
        let narrowed = mutated(&|cmd| cmd.replace("hermit-run/kvm-execution-tests,", ""));
        assert!(
            narrowed.contains("must end with its unified workspace build"),
            "{narrowed}"
        );
        // The core producer must keep the CLI's LiteInst opt-in from the
        // declared selection union, even if all other features remain.
        let core_installer = mutated(&|cmd| cmd.replace(",hermit-run/liteinst", ""));
        assert!(
            core_installer.contains("must end with its unified workspace build"),
            "{core_installer}"
        );

        // A union that no longer carries the core LiteInst backend: every
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
                    .filter(|feature| *feature != "liteinst")
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
            assert!(!after_cargo.contains("liteinst"), "{after_cargo}");
            // Removing the Cargo opt-in must retain exact preparation and staging.
            assert_eq!(
                before.strip_prefix(before_cargo.as_str()).unwrap(),
                after.strip_prefix(after_cargo.as_str()).unwrap(),
            );
            for step in &mut stripped.steps {
                step.cmd = if step.cmd.contains(&before) {
                    step.cmd.replace(&before, &after)
                } else {
                    step.cmd.replace(&before_cargo, &after_cargo)
                };
            }
        }
        let error = assert_producers_build_the_unified_selection(&stripped).unwrap_err();
        assert!(error.contains("without hermit-run/liteinst"), "{error}");
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
                // The pinned prepare payload's early-artifact gate is
                // admitted only on that node's own tag; strip it so the
                // transplanted command stays a well-formed preparation.
                image
                    .cmd
                    .replace(crate::validation_dag::EARLY_ARTIFACT_GATE_SUFFIX, ""),
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
        let terminal = graph
            .steps
            .iter()
            .find(|step| step.tag() == "test.inherited_terminal_read")
            .unwrap();
        assert_command_selection(terminal).unwrap();
        assert_eq!(
            dagrun::model::command_with_inner_jobs(terminal, "-j", Some(8)),
            terminal.cmd,
            "the eight-CPU terminal node must preserve its single -j 1 argument"
        );
        assert_eq!(
            dagrun::model::env_with_inner_jobs(terminal, "", Some(8)),
            Some(("CARGO_BUILD_JOBS".into(), "8".into()))
        );
        let terminal_selection: Vec<String> =
            serde_json::from_str(&terminal.env[SELECTION_ENV]).unwrap();
        assert_eq!(
            terminal_selection,
            [
                "-p",
                "hermit-run",
                "--features",
                "liteinst",
                "--test",
                "inherited_terminal_read",
            ]
        );
        let terminal_key = crate::nextest_binaries::selection_key(&terminal_selection);
        assert_ne!(terminal_key, hardware_key);
        assert_eq!(full.get(&terminal_key), Some(&terminal_selection));
        assert!(!portable.contains_key(&terminal_key));
        // The KVM gap-ledger node lists and runs its suite through the prepared
        // executables of the selection after its runner's `--`.
        let ledger = graph
            .steps
            .iter()
            .find(|step| step.tag() == "privileged-test.deadline_slice_cap_kvm")
            .unwrap();
        assert_command_selection(ledger).unwrap();
        assert_eq!(
            ledger.env[crate::known_gaps::LEDGER_ENV],
            "hermit-cli/tests/deadline_slice_cap.kvm-gaps.tsv"
        );
        let ledger_selection: Vec<String> =
            serde_json::from_str(&ledger.env[SELECTION_ENV]).unwrap();
        let ledger_key = crate::nextest_binaries::selection_key(&ledger_selection);
        assert_eq!(full.get(&ledger_key), Some(&ledger_selection));
        assert!(!portable.contains_key(&ledger_key));
        let mut narrowed = ledger.clone();
        narrowed.cmd = narrowed
            .cmd
            .replace("--test deadline_slice_cap;", "--test cli;");
        let error = assert_command_selection(&narrowed).unwrap_err();
        assert!(error.contains("command has Cargo selection"), "{error}");
        let mut separatorless = ledger.clone();
        separatorless.cmd = separatorless
            .cmd
            .replace("kvm-gap-ledger.rs run -- ", "kvm-gap-ledger.rs run ");
        let error = assert_command_selection(&separatorless).unwrap_err();
        assert!(error.contains("no `--`"), "{error}");
        // The hardware CLI selection, the inherited-terminal selection and the
        // KVM gap-ledger selection are full-only.
        assert_eq!(full.len(), portable.len() + 3);
        for (key, selection) in &portable {
            assert_eq!(full.get(key), Some(selection));
        }
        for tag in ["test.hermit_unit", "test.hermit_unit_on_host"] {
            let step = graph.steps.iter().find(|step| step.tag() == tag).unwrap();
            let selection: Vec<String> = serde_json::from_str(&step.env[SELECTION_ENV]).unwrap();
            assert!(selection.contains(&"liteinst,kvm-native-test-support".into()));
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
                .ends_with("./ci/nextest-binaries.rs prepare hosted-portable && target/validate/stage-detcore-liteinst --git target")
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
            execution_command(producer).unwrap().ends_with(&format!(
                "./ci/nextest-binaries.rs prepare full --build-mode \"${{HERMIT_VALIDATE_BUILD_MODE:-cargo}}\" && target/validate/stage-detcore-liteinst --git target{}",
                crate::validation_dag::EARLY_ARTIFACT_GATE_SUFFIX
            ))
        );
        producer.cmd = producer.cmd.replace(
            &format!(
                "./ci/nextest-binaries.rs prepare full --build-mode \"${{HERMIT_VALIDATE_BUILD_MODE:-cargo}}\" && target/validate/stage-detcore-liteinst --git target{}",
                crate::validation_dag::EARLY_ARTIFACT_GATE_SUFFIX
            ),
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
            "executable hermit-detcore-tests tests_misc",
            "executable hermit-detcore-tests tests_parallelism",
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
        let args = command_arguments("./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit-run --test cli -E 'test(/a; b/)' -- --ignored; exit $?", "run-nextest-counted.sh").unwrap();
        assert_eq!(
            args,
            [
                "-p",
                "hermit-run",
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
