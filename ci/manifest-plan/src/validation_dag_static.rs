// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Typed, generator-only definitions for authored validation steps.
//!
//! This module is private to `hermit-manifest-plan`: runtime validation cannot
//! import it and consumes only `ci/dag/validate.json`. The maintenance generator
//! combines these authored steps with the corpus-derived partitions, then emits
//! that one committed DAG. Keeping the source typed and independent makes an
//! arbitrary command, dependency, or cap edit in the generated file stale.

use std::collections::BTreeMap;

use dagrun::model::CmdType;
use dagrun::model::DagConfig;
use dagrun::model::DagManifest;
use dagrun::model::ResourceHint;
use dagrun::model::ResultManifest;
use dagrun::model::Step;
use dagrun::model::StepClass;
use dagrun::model::StructuredTestResultsManifest;

pub(super) const RUST_SCRIPT_PRODUCER_COMMAND: &str = r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; AGENT_UTILS_RS_ENSURE_ONLY=1 ./agent-utils/rs/bin/dagrun && ./ci/prepare-rust-scripts.sh"########;
const RUST_SCRIPT_PRODUCER_DESCRIPTION: &str = r########"DISCOVERED PRODUCER 2026-08-31: every tracked rust-script entrypoint is generated as a Cargo package, checked with the existing clippy policy, built as a release executable, and, when it carries tests, built as a test harness. The outputs are copied into target/ci/rust-scripts after checkout and pin verification, and all compiling consumers wait for this node. Repository graph commands resolve rust-script through ci/rust-script-bin, which executes only the published binaries and never invokes Cargo; sources owned by external operational tooling remain that tooling's responsibility. Two concurrent rust-script --force probes sharing one cache measured a real Cargo build-directory lock wait; separate caches removed the wait but duplicated compilation. Keeping one writer and read-only consumers makes compilation placement deterministic without creating per-node target directories. RESOURCE BOUNDS measured 2026-09-14 at Hermit 7b8be60708c29b4926f36f06be8793523841d6ba: the old 2-GiB hard cap killed the cold width-8 producer in run 1812 and in a focused width-4 control. With the unchanged 28-entrypoint/17-harness workload and width 8, three ordinary cold runs with memory.max=6442450944 bytes passed with cgroup peaks 3836551168, 3845783552, and 3841757184 bytes; three pinned-root cold runs under that same memory.max passed with peaks 3803856896, 3721408512, and 3691466752 bytes. All six recorded memory.events max=0, oom=0, and oom_kill=0. The 4-GiB baseline rounds above the measured high-water mark for scheduler accounting; the 6-GiB hard cap adds more than 2.4 GiB of runaway headroom while fitting the project's 8-GiB minimum runner. preferred_inner_jobs=8 remains paired with CARGO_BUILD_JOBS so the scheduler reserves the same eight-CPU width Cargo actually uses. WALL BOUND 2026-10-04: the producer's wall bound was 900 seconds, the width-8 wall equivalent of its unchanged 7200-second CPU bound. Across 22 local full validations of Hermit main between 2026-10-01 and 2026-10-04 the ordinary producer's wall peaked at 644.496 seconds (median 201.548), and the pinned-root producer's at 527.414 seconds. The peak run, at Hermit 972dd0ead8c440da3716e7c3ccf47f5eac34cc6e, used 1253.8 CPU seconds, 14% above the 1097.5-second median, over 3.2 times the median wall, so it reflects fewer available cores rather than more work. 900 seconds was 1.40 times that peak, below the project's rule that a limit be at least 1.5 times the largest observed use, so the wall bound is now 1200 seconds, the next 300-second bucket above 1.5 times 644.496 seconds (966.7). The ordinary, pinned-root, hosted-privileged on-host and quick/super producers share this constant. Quick and super use a distinct 1200-second CPU budget and zero duration estimate: 1200 is the next project 300-second bucket above 1.5 times the largest measured 737.730-second CPU sample. The Step constructor orders these as wall, CPU, then memory."########;
pub(super) const RUST_SCRIPT_PRODUCER_WALL_SECONDS: i64 = 1200;
/// Wall bound of the authored `build.workspace` node, which the pinned-root
/// Cargo half (`build.workspace_compile_in_pinned_root`) inherits unchanged.
pub(super) const WORKSPACE_WALL_SECONDS: i64 = 1200;
/// Wall bound of `build.workspace_on_host`, the hosted twin of
/// `build.workspace`; see HOSTED_WORKSPACE_WALL_DESCRIPTION.
pub(super) const HOSTED_WORKSPACE_WALL_SECONDS: i64 = 2100;
const _: () = assert!(HOSTED_WORKSPACE_WALL_SECONDS >= WORKSPACE_WALL_SECONDS);
pub(super) const HOSTED_WORKSPACE_WALL_DESCRIPTION: &str = r########" HOSTED WALL BOUND 2026-10-06: this hosted twin's wall is 2100 seconds; the local node keeps 1200, sized for its 32-worker preference, and the command, CPU bound and memory hints here are the local node's. GitHub's hosted portable job runs this node on whatever hardware the runner pool assigns, and its scheduler caps the node to the run's three-CPU budget (logged as "exceeds the run total CPU-core budget --max-cpus 3; capping its guest width and per-step cpu.max to 3"). In https://github.com/rrnewton/hermit/actions/runs/37383177918 (Hermit f43c990b3c96, AMD EPYC 9V45) the node passed in 752.75 seconds: Cargo reported 73 seconds for detcore-dbt and 643 for the workspace (37 crates compiled after a partial cache restore), and the rest, chiefly preparation, took about 37. In https://github.com/rrnewton/hermit/actions/runs/37440093209 (Hermit 47896b1a15dd, AMD EPYC 7763) the same phases took 120 and 1048 seconds (33 crates compiled), 1.6 times longer each, and the node was killed at 1200.07 seconds, 20 seconds into the dev-profile nextest-cpu-wrapper build that took 18 seconds on the faster host; at the same ratio it needed about 1240. The two runs built different Hermit revisions, so this is not an isolated comparison; but the slower run compiled fewer crates and both Cargo phases slowed by the same factor, which points to the runner rather than the build. 2100 is the next 300-second bucket above 1.5 times both the killed run's lower bound of 1200 seconds (1800.1) and the 1240-second estimate (1860). Under the three-CPU cpu.max cap, 2100 seconds of wall allow at most 6300 CPU seconds, inside the unchanged 7200-second CPU bound. The host pressure graph (ci/compat-envelope/pressure-test.rs) clones this node as its build.workspace and so carries the same wall."########;
/// Wall bound of the authored `check.script_unit_tests` node, which the local
/// lanes run.
pub(super) const SCRIPT_UNIT_TESTS_WALL_SECONDS: i64 = 900;
/// Wall bound of `check.script_unit_tests_on_host`, the hosted twin of
/// `check.script_unit_tests`; see HOSTED_SCRIPT_UNIT_TESTS_WALL_DESCRIPTION.
pub(super) const HOSTED_SCRIPT_UNIT_TESTS_WALL_SECONDS: i64 = 1500;
const _: () = assert!(HOSTED_SCRIPT_UNIT_TESTS_WALL_SECONDS >= SCRIPT_UNIT_TESTS_WALL_SECONDS);
pub(super) const HOSTED_SCRIPT_UNIT_TESTS_WALL_DESCRIPTION: &str = r########" HOSTED WALL BOUND 2026-10-06: this hosted twin's wall is 1500 seconds; the local node keeps 900, and the command, CPU bound and memory hints here are the local node's. GitHub's hosted portable job runs this node beside check.lint_checks and the selftest.* nodes, and its scheduler caps the node's width (HERMIT_SCRIPT_TEST_JOBS) at the run's three-CPU budget (--max-cpus 3). That run is unboxed: no cgroup holds the node to three CPUs, and its log reports that the node's CPU bound cannot be enforced there, so only the wall applies. In https://github.com/rrnewton/hermit/actions/runs/37543769782 (Hermit d7441caea790) the node was killed at 900.16 seconds. Its other units had finished by 615 seconds; the one test still running was the scripts/validate.rs submodule service-result fixture, started at about 630 seconds, whose child compiles a copied validate.rs with rust-script under its own 300-second bound. That child had run 266.96 seconds and had been compiling its last crate, the validate binary (28.6 seconds locally at three CPUs), for 11.83 seconds, so it would have reached its own bound first and no node wall would have let that revision pass. On the host recorded for this node in docs/TESTING_ENVIRONMENTS.md ("Named measurement hosts"), a run of this node at three CPUs with a cold rust-script cache took 724.57 seconds, and the hosted run reached the same milestones 1.5 to 2.6 times later. The fixture child now compiles at Cargo opt-level 0: a cold three-CPU build of its package fell from 147.71 to 50.85 seconds, and the fixture alone at four CPUs from 146.19 to 84.17. Scaling the hosted child's compile by that ratio and taking 2.6 times the fixture's local non-compile time (about 34 seconds) gives an estimate of about 850 seconds for the hosted node; that is not a hosted measurement, and 900 seconds would leave less than the project's 50% headroom over it. 1500 is the next 300-second bucket above 1.5 times both the measured lower bound of 900.16 seconds (1350.2) and the 850-second estimate (1275). At a width of three, 1500 seconds of wall come to about 4500 CPU seconds, inside the unchanged 7200-second CPU bound wherever a run can enforce it."########;
pub(super) const RUST_SCRIPT_PRODUCER_RSS_BASELINE_BYTES: i64 = 4 * 1024 * 1024 * 1024;
pub(super) const RUST_SCRIPT_PRODUCER_HARD_MEM_MAX_BYTES: i64 = 6 * 1024 * 1024 * 1024;
pub(super) const RUST_SCRIPT_PRODUCER_INNER_JOBS: i64 = 8;
pub(super) const RUST_SCRIPT_PRODUCER_QUICK_SUPER_CPU_SECONDS: i64 = 1200;
pub(super) const MANIFEST_GATE_INNER_JOBS: i64 = 2;
pub(super) const MANIFEST_GATE_CPU_SECONDS: i64 = 120;
const MANIFEST_GATE_DESCRIPTION: &str = r########"SELF-TESTS MOVED OUT 2026-09-29: every product node waits on this gate, and the local validation of Hermit 98a621f42935d585e1306262b11d64488dd51614 was killed here by the 900-second CPU cap while the gate was still running repository-tool self-tests. Those self-tests (scorecard, pressure_test, validate_rs, manifest_cli, dbt_budget; TOOL_SELF_TESTS in validation_dag.rs) now run as selftest.<name> leaf nodes that still decide the verdict but block nothing. The gate keeps only what product nodes consume: the manifest-plan audits run in process (DAG/manifest correspondence, budget ordering, determinism-stress evidence, CLI brackets and the expected-cell plan, which define the cells and commands the product nodes execute), and `generate-test-footprints --check`, which proves the committed ci/test-footprints.json still matches ci/dag/validate.json and Cargo metadata, so hosted test selection picks product nodes from current data. Measured 2026-09-29 on the measurement host recorded for gate.manifest in docs/TESTING_ENVIRONMENTS.md ("Named measurement hosts") at load average 215-233: `test-harness validate` took 2.88 s wall and 2.53 CPU seconds (1.99 user, 0.54 system, 29440 KiB largest process RSS), against 664 s of wall before it was killed at 900 CPU seconds with the self-tests included. The 120 CPU seconds are 47 times that sample and bound a hang, not the work; the ordinary and quick/super gates share the constant. The 900-second wall is unchanged because the committed portable preflight critical path is built from it (3780 seconds, then 3900 once setup.manifest_plan's wall cap went from 180 to 300 seconds). HERMIT_VALIDATE_AUDIT_JOBS=1 and the two-core width are retained; with one remaining child audit they no longer change the schedule. The history below describes the former seven-audit gate. AUDIT WIDTH CONTRACT 2026-09-25: RUN1902 admitted this gate to a one-core cgroup but inherited CARGO_BUILD_JOBS=4 from the outer validation environment. test-harness treated that unrelated value as the admitted width, launched two heavyweight metadata audits, and the scorecard's unchanged five-second snapshot command control was starved and killed after producing no output. The retained step profile recorded 0.9264 effective cores and 380.573 seconds throttled. Cold isolated controls at Hermit 22e9e0b5 and ecb3c9a5 passed with the original five-second boundary; concurrent one-core controls stretched the scorecard from 349.258/376.097 seconds to 553.087/531.102 seconds. preferred_inner_jobs=2 reserves the measured two-core width for each audit's internal Cargo and helper work, while jobs_env=CARGO_BUILD_JOBS carries a smaller admitted width into every child. The ordinary and quick/super variants set HERMIT_VALIDATE_AUDIT_JOBS=1 so their top-level audits run serially inside the shared aggregate budget (seven when measured; six since slice S13 of https://github.com/rrnewton/hermit/issues/3301 retired the tests/backend-parity/split_asymmetric_pr.py self-test): the exact concurrent scheduler path consumed 601.132 CPU seconds and was killed by the unchanged 600-second cap, while isolated scorecard and pressure controls consumed 327.344 and 70.699 CPU seconds. The separately bounded hosted-privileged variant retains its prior two-worker schedule. jobs_flag is explicitly empty so dagrun does not also append its default -j argument to test-harness. Commands, audit population, wall/CPU caps, and scorecard deadlines are unchanged. CPU BOUND 2026-09-28: the ordinary and quick/super gates now carry 900 CPU seconds; the hosted-privileged variant keeps 600 with its 180-second wall. The audit work did not grow. At Hermit 81ca8822, b280bc48 and 1bd45c15 each audit's waited user+sys CPU agreed within 1% (scorecard 254.6-256.6, pressure 62.5-63.0, validate.rs 28.4-28.8 seconds), and across the 46 retained local validation profiles of this step from 2026-09-22 to 2026-09-28 user CPU stayed at or below 316.2 seconds while system CPU ranged from 32 to 374 seconds. The variable part is process creation. One complete gate at 81ca8822 runs 4011 git commands (scorecard 3182, validate.rs 509, pressure 204, DBT budget 65), and the local validation host resolves git to a telemetry wrapper that measured 51-85 ms of CPU per call against 5.1-5.2 ms for the git it wraps, spawning five further git processes and a detached logger for each call. On an unthrottled host the complete gate consumed 512.0 (b280bc48) and 535.7 (1bd45c15) CPU seconds with that wrapper first on PATH, and 305.5 and 300.7 with the wrapped git first. Three consecutive local runs on 2026-09-28 reached the old 600-second cap inside audits 5-7 with 340-374 seconds of system CPU. The largest observed user CPU, 316.2 seconds, times one plus the largest observed system/user ratio, 1.655, is 839.5 CPU seconds; 900 leaves 60.5 seconds above that product. It remains a runaway bound: a two-core spin is killed after 450 seconds, half of the unchanged 900-second wall."########;

/// The controlled writer a static validation step invokes.
///
/// This is authored metadata, not command-string inference. The generator audits
/// the command independently so adding or removing a writer cannot silently leave
/// this declaration stale.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum StructuredResultProducerKind {
    Nextest,
    TestHarness,
    Envelope,
    Applications,
}

impl StructuredResultProducerKind {
    /// The exact structured-result schema this producer writes: schema 4 for a
    /// harness node of a bucket in
    /// [`crate::validation_dag::DIAGNOSTIC_MANIFEST_BUCKETS`], which runs with
    /// `--diagnostic-results`; the default schema 2 for every other producer.
    pub(super) fn declaration(
        self,
        owner: String,
        manifest_category: Option<&str>,
    ) -> StructuredTestResultsManifest {
        let diagnostic = self == Self::TestHarness
            && manifest_category.is_some_and(|category| {
                crate::validation_dag::DIAGNOSTIC_MANIFEST_BUCKETS.contains(&category)
            });
        if diagnostic {
            StructuredTestResultsManifest::diagnostic(owner)
        } else {
            StructuredTestResultsManifest::current(owner)
        }
    }

    pub(super) const ALL: [Self; 4] = [
        Self::Nextest,
        Self::TestHarness,
        Self::Envelope,
        Self::Applications,
    ];

    pub(super) const fn command_marker(self) -> &'static str {
        match self {
            Self::Nextest => "run-nextest-counted.sh",
            Self::TestHarness => "target/debug/test-harness run",
            Self::Envelope => "write-structured-test-counts.sh",
            Self::Applications => "tests/e2e/lib/applications/run_all.sh",
        }
    }

    pub(super) const fn tags(self) -> &'static [&'static str] {
        match self {
            Self::Nextest => NEXTEST_RESULT_PRODUCERS,
            Self::TestHarness => TEST_HARNESS_RESULT_PRODUCERS,
            Self::Envelope => ENVELOPE_RESULT_PRODUCERS,
            Self::Applications => APPLICATION_RESULT_PRODUCERS,
        }
    }
}

pub(super) const NEXTEST_RESULT_PRODUCERS: &[&str] = &[
    "test.isolated_dbt_workdir",
    "test.isolated_detcore_workdir",
    "privileged-only-test.cli_kvm",
    "privileged-only-test.cli_kvm_on_host",
    "privileged-only-test.pmu_buck_chaos_cases",
    "privileged-only-test.pmu_buck_chaos_cases_on_host",
    "privileged-test.cli_kvm",
    "privileged-test.pmu_buck_chaos_cases",
    "privileged-test.pmu_cli_cases",
    "privileged-test.pmu_detcore_time_cases",
    "privileged-test.pmu_ptrace_completion_cases",
    "quick.detcore_unit",
    "super.chaos_hello_race_verification_diagnostic",
    "super.dbt_failed_exec_recovery_diagnostic",
    "super.dbt_guest_stderr_isolation_diagnostic",
    "super.dbt_pipe_backpressure_diagnostic",
    "super.dbt_strict_blocked_stdin_teardown_diagnostic",
    "super.dbt_unsupported_syscall_aggregation_diagnostic",
    "super.full_leveldb_strict_determinism",
    "super.ipc_determinism_diagnostic",
    "super.managed_jvm_strict_verify_diagnostics",
    "super.network_syscall_determinism_diagnostic",
    "super.pmu_analyze_hello_race_stress_calibrated_skid",
    "super.pmu_buck_chaos_cases",
    "super.post_fork_scheduling_diagnostics",
    "super.pselect_signal_interruption_diagnostic",
    "super.random_source_determinism_diagnostic",
    "super.record_replay_matrix_diagnostic",
    "super.relaxed_hermit_flag_matrix",
    "super.sqlite_veryquick_strict_determinism",
    "super.threaded_integration_matrix_diagnostic",
    "super.weekly_ignored_portable_chaos_cases",
    "super.weekly_pmu_parallel_memory_diagnostic_mem_race_bottom_detcore",
    "super.weekly_pmu_parallel_memory_diagnostic_mem_race_default_detcore",
    "super.weekly_pmu_parallel_memory_diagnostic_mem_race_middle_detcore",
    "super.weekly_pmu_parallel_memory_diagnostic_mem_race_top_detcore",
    "super.weekly_portable_chaos_cases",
    "super.weekly_relaxed_default_mode_cases",
    "test.app_strict_verify",
    "test.arbitrary_binaries",
    "test.cli",
    "test.liteinst_allocator",
    "test.liteinst_stacks",
    "test.cli_on_host",
    "test.record_replay",
    "test.command_strict_verify",
    "test.detcore_misc",
    "test.detcore_parallel",
    "test.detcore_time",
    "test.detcore_unit",
    "test.hermit_integration",
    "test.hermit_modes",
    "test.hermit_modes_on_host",
    "test.hermit_unit",
    "test.ignored_syscall_regressions",
    "test.pmu_integration_cases",
    "test.regular_crates",
    "test.rr_suite_contract",
    "test.sabre_examples",
    "test.app_strict_verify_on_host",
    "test.arbitrary_binaries_on_host",
    "test.command_strict_verify_on_host",
    "test.detcore_misc_on_host",
    "test.detcore_parallel_on_host",
    "test.detcore_time_on_host",
    "test.detcore_unit_on_host",
    "test.hermit_integration_on_host",
    "test.hermit_unit_on_host",
    "test.ignored_syscall_regressions_on_host",
    "test.regular_crates_on_host",
    "test.rr_suite_contract_on_host",
    "test.sabre_examples_on_host",
];

pub(super) const TEST_HARNESS_RESULT_PRODUCERS: &[&str] = &[
    // The Buck import twins validation_dag::materialize_buck_e2e adds.
    "e2e.manifest_applications_buck",
    "e2e.manifest_bin_c_buck",
    "e2e.manifest_c_programs_buck",
    "e2e.manifest_chaos_c_buck",
    "e2e.manifest_compat_buck",
    "e2e.manifest_data_handling_buck",
    "e2e.manifest_debugger_c_buck",
    "e2e.manifest_determinism_stress_buck",
    "e2e.manifest_determinism_stress_c_buck",
    "e2e.manifest_language_runtimes_buck",
    "e2e.manifest_shared_futex_c_buck",
    "e2e.manifest_system_utils_buck",
    "e2e.manifest_util_c_buck",
    "privileged-e2e.manifest_applications_buck",
    "privileged-e2e.manifest_c_programs_buck",
    "privileged-e2e.manifest_system_utils_buck",
    "e2e.manifest_applications",
    "e2e.manifest_applications_on_host",
    "e2e.manifest_compat",
    "e2e.manifest_compat_on_host",
    "e2e.manifest_bin_c",
    "e2e.manifest_bin_c_on_host",
    "e2e.manifest_c_programs",
    "e2e.manifest_c_programs_on_host",
    "e2e.manifest_chaos_c",
    "e2e.manifest_chaos_c_on_host",
    "e2e.manifest_data_handling",
    "e2e.manifest_data_handling_on_host",
    "e2e.manifest_debugger_c",
    "e2e.manifest_debugger_c_on_host",
    "e2e.manifest_determinism_stress",
    "e2e.manifest_determinism_stress_c",
    "e2e.manifest_determinism_stress_c_on_host",
    "e2e.manifest_determinism_stress_on_host",
    "e2e.manifest_language_runtimes",
    "e2e.manifest_language_runtimes_on_host",
    "e2e.manifest_shared_futex_c",
    "e2e.manifest_shared_futex_c_on_host",
    "e2e.manifest_system_utils",
    "e2e.manifest_system_utils_on_host",
    "e2e.manifest_util_c",
    "e2e.manifest_util_c_on_host",
    "portablecompat.manifest_compat",
    "privileged-e2e.manifest_applications",
    "privileged-e2e.manifest_c_programs",
    "privileged-e2e.manifest_system_utils",
    "privileged-only-e2e.manifest_applications",
    "privileged-only-e2e.manifest_applications_on_host",
    "privileged-only-e2e.manifest_c_programs",
    "privileged-only-e2e.manifest_c_programs_on_host",
    "privileged-only-e2e.manifest_system_utils",
    "privileged-only-e2e.manifest_system_utils_on_host",
    "quick.e2e_verify",
    "rrcompat.manifest_compat",
    "sabrecompat.manifest_compat",
    "strictcompat.manifest_compat",
];

/// Nodes that write their rows through `ci/write-structured-test-counts.sh`:
/// the envelope levels and, since 2026-10, each super stress probe, whose
/// repetitions are its rows.
pub(super) const ENVELOPE_RESULT_PRODUCERS: &[&str] = &[
    "test.envelope_levels",
    "test.envelope_levels_on_host",
    "superstress.ptrace_strict_verify",
    "superstress.ptrace_pipeline",
    "superstress.ptrace_record_replay",
    "superstress.kvm_verify",
    "superstress.dbt_verify",
];
pub(super) const APPLICATION_RESULT_PRODUCERS: &[&str] =
    &["test.applications_e2e", "test.applications_e2e_on_host"];

pub(super) const PMU_MEMORY_FAILURE_FAMILY: &str = "super.Weekly PMU parallel memory diagnostic";
pub(super) const PMU_MEMORY_FAILURE_FAMILY_MEMBERS: &[&str] = &[
    "super.weekly_pmu_parallel_memory_diagnostic_mem_race_bottom_detcore",
    "super.weekly_pmu_parallel_memory_diagnostic_mem_race_default_detcore",
    "super.weekly_pmu_parallel_memory_diagnostic_mem_race_middle_detcore",
    "super.weekly_pmu_parallel_memory_diagnostic_mem_race_top_detcore",
];

/// Exact selected populations, independent of Nextest's output parser, make
/// an empty or narrowed run refuse. Update these only after enumerating the
/// corresponding shipped command and accounting for changed test identities.
pub(super) const NEXTEST_EXPECTED_COUNTS: &[(&str, u64)] = &[
    // Two actual-leaf matrices, five shared qualifier controls and the real
    // Gitless source-copy factory regression; confirm with the prepared listing.
    ("test.liteinst_allocator", 8),
    // Eleven unchanged M2 cases plus one constructor row and two refusal controls.
    ("test.liteinst_stacks", 14),
    // run_dbt_binds_in_a_user_namespace_of_its_own retains both prior
    // identities: 2 + 1 = 3.
    ("test.isolated_dbt_workdir", 3),
    ("test.isolated_detcore_workdir", 1),
    // Prepared inventories retain all prior identities and add four reporting
    // tests to regular crates and nine to Hermit's library/binary selection.
    // Nine admission-context/nested-ID controls extend the measured 533-test set.
    // Six portable-context controls extend the measured 542-test set.
    // Five owned CPU-reader controls retain all 548 prior selected identities.
    // Five CPU-evidence tests plus one schema-ingress test retain all 553 prior IDs.
    // Three policy controls retain all 559 prior identities.
    // Three real CPU-emission lifecycle controls retain all 562 prior identities.
    // The runtime manifest-root regression retains all 565 prior identities.
    // Eleven record-workload preparation controls retain all 566 prior IDs.
    // Seven census/finalized-run/scope controls retain all 577 current-main IDs.
    // Three epoch controls and the dagrun-preparation placement control retain all 606 prior IDs.
    // Six expected-guest-exit controls retain all 610 prior identities.
    // The manifest-gate width contract retains all 616 prior identities.
    // The GlobalTime sub-microsecond elapsed-time and environment-free config
    // fingerprint regressions retain all 617 prior selected identities.
    // Two validate-runner tests and two stress_series declared_guest_exit
    // tests retain all 619 prior identities (`cargo nextest list` measured 623).
    // The release-profile GlobalTime behind-baseline refusal test retains all
    // 623 prior identities.
    // The failed_match series-evidence test retains all 624 prior identities.
    // The parity overhaul (d550979ad0, 71b5bca69b, b9ec113b5f, b280bc4807) adds
    // 24 tests (15 parity, 3 logdiff_report, 2 runner, one each in schema10,
    // validation_dag, test-harness and cli_help) and removes 9 ptrace
    // parity-rerun tests: 625 + 24 - 9 = 640 in `cargo nextest list`.
    // The parity review follow-ups add 5 tests (2 parity, 1 runner, 2 cli_help)
    // and retain all 640 prior identities (`cargo nextest list` measured 645).
    // Seven parity post-pass tests and the test-harness post-pass test retain
    // all 645 prior identities (`cargo nextest list` measured 653).
    // The post-pass review fixes add 10 tests (7 parity, 3 test-harness) and
    // retain all 653 prior identities (`cargo nextest list` measured 663).
    // The series parity post-pass adds one parity test (a rejected operand is
    // unavailable) and retains all 663 prior identities (measured 664).
    // e8007f971a7 adds relative_artifacts_and_hermit_paths_are_still_measured
    // and ad21724d5f6 adds only_a_program_name_without_a_slash_is_left_for_path;
    // both retain all 664 prior identities (`cargo nextest list` measured 666).
    // 7ce5e3b6bc6 counts ten backend-parity-c fold tests (measured 676).
    // Slice S13 of https://github.com/rrnewton/hermit/issues/3301 adds 21
    // hermit-manifest-plan tests and retains all 676 prior identities
    // (`cargo nextest list --profile ci -p hermit-manifest-plan` measured 490
    // at fdb12e99110 and 511 with S13; no other selected crate changed):
    // runner.rs lib: a_vdso_getrandom_golden_must_declare_its_kernel_floor,
    //   expected_stdout_cannot_pass_a_diverged_comparison_or_failed_status,
    //   expected_stdout_contains_cannot_pass_a_diverged_or_failed_run,
    //   expected_stdout_contains_declarations_are_verify_only_and_non_empty,
    //   expected_stdout_contains_requires_the_marker_in_the_compared_stdout,
    //   expected_stdout_declarations_are_verify_only_and_name_enabled_backends,
    //   expected_stdout_names_the_run_that_differs,
    //   expected_stdout_passes_only_when_both_runs_print_it,
    //   expected_stdout_without_compared_outputs_is_never_a_pass,
    //   only_a_verify_cell_reads_its_own_backends_expected_stdout,
    //   only_a_verify_cell_reads_its_own_backends_expected_stdout_contains;
    // main.rs bin: accepts_verify_expected_stdout_contains_for_an_enabled_backend,
    //   accepts_verify_expected_stdout_for_an_enabled_backend,
    //   rejects_empty_expected_stdout_contains,
    //   rejects_expected_stdout_contains_for_a_disabled_backend,
    //   rejects_expected_stdout_contains_outside_verify,
    //   rejects_expected_stdout_for_a_disabled_backend,
    //   rejects_expected_stdout_outside_verify,
    //   rejects_non_string_expected_stdout;
    // backend_parity_c_fold:
    //   every_manifest_node_with_a_dbt_cell_orders_after_the_dbt_runtime_abi_check,
    //   the_privileged_c_programs_description_names_every_selected_cell.
    // Slice S8 of https://github.com/rrnewton/hermit/issues/3301 adds 10
    // hermit-manifest-plan lib tests and retains all 697 prior identities
    // (`cargo nextest list --profile ci -p hermit-manifest-plan` measured 521
    // against 511; no other selected crate changed):
    // parity.rs: equal_inputs_need_equalization_on_both_sides_and_one_guest_view,
    //   equalized_launches_earn_clean_credit_and_no_others_do;
    // runner.rs: file_digest_matches_a_whole_file_digest,
    //   verify_cells_give_every_bindable_backend_the_same_guest_inputs,
    //   ptrace_given_kvm_inputs_matches_its_own_on_add_key_enosys,
    //   ptrace_given_sabre_inputs_matches_its_own_on_bpf_enosys,
    //   ptrace_given_liteinst_inputs_matches_its_own_on_adjtimex_deterministic,
    //   ptrace_given_kvm_inputs_matches_its_own_on_ioctl_fioclex,
    //   ptrace_given_liteinst_inputs_matches_its_own_on_mmap_determinism,
    //   ptrace_given_sabre_inputs_matches_its_own_on_epoll_determinism.
    // The hosted-routing series adds two hermit-manifest-plan tests and
    // retains all 707 prior identities (`cargo nextest list` measured 709):
    // validation_dag::tests::
    //   hosted_portable_omits_the_excluded_backends_everywhere_and_locally_keeps_them;
    // test-harness bin:
    //   committed_hosted_portable_harness_commands_exclude_each_backend_once.
    // The inherited-repository-location series
    // (https://github.com/rrnewton/hermit/issues/3362) adds one
    // hermit-manifest-plan lib test and retains all 709 prior identities
    // (`cargo nextest list --profile ci -p hermit-manifest-plan` measured 524
    // against 523; no other selected crate gains or loses a test):
    // nextest_binaries::tests::
    //   scratch_repository_tests_ignore_inherited_git_location_variables.
    // Moving the tool self-tests out of gate.manifest (d7138e26, under
    // https://github.com/rrnewton/hermit/issues/3381) adds two
    // hermit-manifest-plan tests and retains all 710 prior identities
    // (`cargo nextest list --profile ci -p hermit-manifest-plan` measured 526
    // against 524; no other selected crate gains or loses a test):
    // validation_dag::tests::tool_self_test_guard_refuses_each_planted_drift;
    // cli_help: test_harness_selftest_refuses_anything_but_one_known_name.
    // Change-set selection for selftest.scorecard_commands (under
    // https://github.com/rrnewton/hermit/issues/3381) adds seven
    // hermit-manifest-plan tests and retains all 712 prior identities:
    // self_test_selection::tests::{
    //   a_change_outside_the_triggers_skips_and_says_so_in_one_line,
    //   a_change_under_a_trigger_runs, anything_unresolved_or_forced_runs,
    //   change_set_reads_the_branch_diff_and_untracked_paths_and_runs_main,
    //   change_set_runs_in_a_shallow_clone,
    //   change_set_lists_deleted_renamed_and_submodule_paths,
    //   every_trigger_names_a_tracked_path}
    // (seven tests; 712 + 7 = 719).
    // The scorecard regression tier's prepared helper adds two and retains
    // all 719 prior identities:
    // validation_dag::tests::only_the_scorecard_regression_tier_takes_the_prepared_helper;
    // cli_help: test_harness_selftest_scorecard_refuses_a_missing_helper.
    // The chaos-timeout reason regressions
    // (runner::tests::a_chaos_timeout_leads_the_reason_and_is_not_counted_as_a_pass
    // and runner::tests::a_chaos_timeout_keeps_an_earlier_seeds_failure_reason)
    // retain all 721 prior identities.
    // The replay-epoch fix (https://github.com/rrnewton/hermit/issues/3411)
    // adds three hermit-verify common::opts::tests (one_captured_epoch_is_given_to_every_run,
    // an_explicit_epoch_is_never_replaced_or_duplicated,
    // an_environment_epoch_is_inherited_without_reading_the_clock) and retains
    // all 723 prior identities.
    // hermit-verify's
    // log_diff_canonicalizes_host_addresses_only_when_the_child_offers_it
    // (https://github.com/rrnewton/hermit/issues/3412) retains all 726 prior
    // identities.
    // hermit-manifest-plan's hermit_backend_is_passed_before_the_run_subcommand
    // (https://github.com/rrnewton/hermit/pull/3439) retains all 727 prior
    // identities.
    // Six nextest_binaries::tests for the unified Nextest preparation
    // (one_selection_is_its_own_union,
    // several_selections_unify_to_the_workspace_with_qualified_features,
    // only_validate_profile_profiles_unify_their_listing,
    // a_union_refuses_what_it_cannot_reproduce_exactly,
    // selection_membership_follows_cargo_target_selection,
    // selection_membership_refuses_an_empty_or_unknown_selection) retain all
    // 728 prior identities.
    // nextest_binaries::tests::a_one_selection_listing_must_keep_every_executable_cargo_listed
    // and nextest_build_selections::tests::a_unifying_producer_must_end_with_exactly_its_union_build
    // retain all 734 prior identities.
    // The manifest-runner extensions' runner::tests
    // (verify_extensions_are_verify_only_reasoned_and_allowlisted,
    // declared_relaxations_are_recorded_on_the_cell,
    // labels_are_lowercase_dashed_words_and_unique,
    // labels_select_only_the_tests_that_carry_them,
    // a_verify_cell_carries_its_hermit_args_env_and_comparator,
    // a_stripped_cell_passes_only_a_matched_report_and_a_strict_one_still_needs_canonical)
    // and the manifest front door's
    // accepts_the_verify_extensions_in_a_verify_mode,
    // rejects_the_verify_extensions_outside_verify, accepts_test_labels and
    // rejects_a_misspelled_labels_key retain all 736 prior identities.
    // The validation inventory's four validation_inventory::tests
    // (labels_select_the_closure_and_groups_follow_dependency_order,
    // a_shared_paragraph_is_printed_once_per_group,
    // ascii_prepends_the_group_graph, arguments_are_parsed_strictly) retain
    // all 746 prior identities.
    // The diagnostic-cell and stripped-ledger tests
    // (only_a_reasoned_product_failure_of_a_diagnostic_cell_is_a_diagnostic_failure,
    // a_declared_stripped_match_is_a_weak_tier_match_never_canonical,
    // a_stripped_divergence_is_sticky_and_stays_at_the_weak_tier,
    // only_a_declared_complete_stripped_pass_is_a_match,
    // a_declared_stripped_cell_is_retained_and_verified_as_weak_ordinary_evidence,
    // the_weak_tier_is_refused_unless_it_is_exactly_a_stripped_verify_comparison,
    // only_a_diagnostic_results_run_may_select_a_diagnostic_cell)
    // retain all 750 prior identities.
    // The compatibility-corpus fold's six tests
    // (manifest_corpus::tests::a_corpus_row_expands_into_one_verify_cell_with_the_lane_settings,
    // a_malformed_corpus_is_refused, placeholders_resolve_or_refuse,
    // a_direct_shell_word_expands_its_placeholders_in_the_shell,
    // runner::tests::a_shipped_compat_row_keeps_the_strict_compatibility_probe_flags,
    // and test-harness a_no_retry_cell_is_not_retried_after_a_product_failure)
    // retain all 757 prior identities.
    // test-harness no_retry_flag_turns_framework_retries_off retains all 763 prior identities.
    // runner::tests::a_relative_hermit_bin_names_a_checkout_path, added when the
    // portable-strict-compat-only lane became one manifest bucket (fold 2 of
    // https://github.com/rrnewton/hermit/issues/3448), retains all 764 prior
    // identities.
    // The Buck/RE harness (https://github.com/rrnewton/hermit/pull/3507) adds 13
    // tests that retain all 765 prior identities: detcore-model
    // host_capability::tests::a_missing_kvm_device_is_proof_of_absence; runner::tests::
    // a_source_sha_replaces_git_for_a_snapshot_root and
    // source_sha_must_be_one_full_lowercase_commit; hermit-manifest-plan
    // snapshot_population_lists_every_file_under_tests and
    // source_snapshot_selects_the_filesystem_population; test-harness
    // a_kvm_cell_is_withheld_where_kvm_is_proven_absent,
    // a_lone_host_inapplicable_cell_fails_the_run_in_tpx_too,
    // an_unwritable_count_file_fails_the_run_in_tpx_too,
    // every_kvm_plan_row_requires_the_kvm_host_capability,
    // source_snapshot_flags_parse_once,
    // tpx_json_reports_each_final_cell_with_the_run_verdict and
    // tpx_json_reports_missing_cells_and_run_failures_as_failed; and cli_help
    // test_harness_source_snapshot_flags_redirect_misuse.
    // https://github.com/rrnewton/hermit/pull/3544 adds three runner tests
    // (a_replay_cell_passes_on_the_inherited_exit_only_when_matched_and_exact,
    // a_replay_cell_inherits_its_verify_guest_inputs_and_expectations and
    // replay_argv_carries_the_inherited_verify_environment) that retain all
    // 778 prior identities.
    // ci/manifest-plan dagrun_pin every_dagrun_dependency_uses_the_agent_utils_gitlink
    // retains all 778 prior identities.
    // ci/manifest-plan dagrun_pin every_spelling_of_a_dagrun_declaration_is_found_and_refused
    // retains all 779 prior identities.
    // https://github.com/rrnewton/hermit/pull/3522 adds
    // runner::tests::a_passing_patching_backend_report_must_carry_a_consistent_dispatch_record,
    // which retains all 783 prior identities.
    // Slice S12 of https://github.com/rrnewton/hermit/issues/3301 adds 31
    // hermit-manifest-plan test identities and removes one, which S12
    // replaced with a rewritten test under a new name: 784 - 1 + 31 = 814,
    // and 783 of the 784 prior identities are retained. No other selected
    // crate gains or loses a test. parity.rs goes from 36 lib tests to 64.
    // The rewritten test,
    // parity::tests::a_rejected_operand_takes_the_harness_class_with_the_callers_reason,
    // replaces
    // parity::tests::a_rejected_operand_is_unavailable_with_the_callers_reason,
    // and these 28 are new:
    // parity::tests::{
    //   a_cell_the_expected_scope_owes_but_the_status_omits_is_record_missing,
    //   a_comparison_no_record_can_carry_is_log_diff_failed_with_both_logs,
    //   a_complete_status_with_a_count_mismatch_is_refused,
    //   a_diverged_report_under_any_typed_result_leaves_its_side_without_a_log,
    //   a_failed_status_owes_record_missing_rows_for_its_unrecorded_scope,
    //   a_hanging_writer_is_killed_at_its_bound,
    //   a_ledger_row_that_disagrees_with_its_record_is_refused,
    //   a_ledger_row_with_a_malformed_emitted_at_is_refused,
    //   a_log_diff_verdict_that_is_not_a_measurement_is_log_diff_failed,
    //   a_mean_credit_below_one_never_prints_as_full_credit,
    //   a_mismatched_operand_is_never_compared_even_with_both_logs_retained,
    //   a_parity_record_breaking_a_credit_invariant_is_refused_by_that_invariant,
    //   a_post_pass_failed_outside_post_pass_owes_its_scope,
    //   a_real_post_pass_reads_back_through_ledger_sources,
    //   a_refused_node_owes_record_missing_rows_for_its_expected_cells,
    //   a_report_that_is_not_cross_backend_evidence_is_log_diff_failed,
    //   a_result_root_is_removed_when_its_test_ends,
    //   a_schema_one_status_is_accepted_by_its_count,
    //   a_timeout_is_unavailable_with_its_own_class,
    //   a_writer_that_cannot_append_is_named_and_leaves_the_rows,
    //   an_absent_status_owes_its_expected_cells,
    //   an_invalid_record_refuses_the_node,
    //   appended_rows_reach_the_writer_with_the_runs_identity,
    //   emitted_at_is_read_as_an_rfc3339_utc_instant,
    //   every_class_a_post_pass_decides_is_typed_and_counted_once,
    //   inputs_not_equalized_is_for_a_backend_that_cannot_be_equalized_only,
    //   ledger_verdicts_order_from_most_to_least_adverse,
    //   staging_input_never_removes_a_file_it_did_not_create};
    // the new tests/parity_export.rs integration binary adds two:
    // parity_export:
    //   export_prints_one_row_per_owed_cell_and_passes_real_records_through,
    //   export_refuses_an_inconsistent_node_and_prints_no_rows.
    // Deleting the E2E runner's ptrace-golden normalization producer
    // (https://github.com/rrnewton/hermit/issues/3301) deletes its one test,
    // runner::tests::ptrace_golden_normalization_is_bounded_and_accounts_a_timeout,
    // and retains the other 813 prior identities (`cargo nextest list
    // --profile ci` over this node's whole selection measured the -1 when the
    // change was written, not on this base; no other selected crate gains or
    // loses a test).
    // The import-mode change of https://github.com/rrnewton/hermit/pull/3542
    // adds six tests and retains all 813 prior identities: test-harness's
    // import_mode_republishes_rows_and_a_missing_cell_is_an_error,
    // import_mode_host_inapplicable_claim_needs_this_machines_confirmation and
    // import_mode_marks_the_rows_of_a_dirty_checkout_dirty, and runner's
    // a_source_sha_is_refused_where_git_tracks_the_files,
    // a_source_sha_is_accepted_outside_any_checkout and
    // a_source_sha_is_refused_through_a_symlink_to_tracked_files.
    // `cargo nextest list --profile ci` over this node's selection measured
    // 819 at 9e7dd6e33cf6fc76f3b9a744f50441120b32f0d6.
    // runner::tests::a_cell_labelled_by_its_mode_belongs_to_that_run_type_alone,
    // manifest_corpus::tests::a_focused_run_type_adds_labelled_cells_and_records_red_ones
    // and validation_dag::tests::steps_sharing_a_result_file_never_run_in_one_run_type,
    // added when the sabre-compat-only lane became compat.yaml cells (fold 3 of
    // https://github.com/rrnewton/hermit/issues/3448), retain all 819 prior
    // identities.
    // manifest_corpus::tests::a_variant_adds_one_labelled_test_per_row_on_the_corpus_backend
    // and a_malformed_variant_is_refused, added when the strict-compat-only lane
    // became compat.yaml cells (fold 4 of the same issue), and
    // the_strict_variant_runs_exactly_the_strict_corpus_programs, which binds
    // that variant to ci/compat/corpus-strict.json, retain all 822 prior
    // identities.
    // manifest_corpus::tests::a_replay_variant_runs_only_its_replay_cell_with_its_budget
    // and a_replay_variant_with_verify_settings_is_refused, added when the
    // rr-compat-only lane became compat.yaml replay cells (fold 5 of the same
    // issue), and
    // the_rr_variant_keeps_the_rr_lane_programs_and_gates_only_those_that_replay,
    // which pins that variant at the retired lane's 139 programs, retain all 825
    // prior identities.
    // Six later tests retain all 828 prior identities:
    // cpu_evidence::tests::each_live_source_is_named_and_the_cgroup_counter_cannot_fall
    // (dfc22fc36a19); detcore-sabre's glibc_compat::tests::
    // dl_find_object_finds_this_code_and_its_unwind_tables,
    // isoc23_strtol_accepts_a_binary_prefix_in_bases_0_and_2 and
    // isoc23_strtol_matches_strtol_without_a_binary_prefix (4e05a36ee985); and
    // runner::tests::a_slow_failed_sample_does_not_use_up_the_unavailable_grace
    // (047c3d019b6b) and
    // a_slow_census_that_keeps_failing_still_stops_the_command (5c4251450a21).
    // `cargo nextest list --profile ci` over this node's selection measured 834
    // at 5c4251450a21, and the full validation of c1312a563dc5, which has the
    // first four, ran 832.
    // runner::tests::only_a_typed_skid_overshoot_and_nothing_else_is_a_skid_only_row,
    // runner::tests::a_skid_attempt_that_breaks_a_declared_stdout_assertion_is_not_skid_only,
    // runner::tests::a_skid_row_records_the_cells_declared_stdout and
    // test-harness
    // tests::a_skid_overshoot_only_verify_attempt_earns_one_counted_skid_retry,
    // added with the SKID-RETRY of
    // https://github.com/rrnewton/hermit/issues/1845, retain all 834 prior
    // identities.
    // validation_dag::tests::no_dag_text_names_a_user_home_path retains all
    // 838 prior identities (`cargo nextest list --profile ci` over this
    // node's selection measured 839).
    // runner::tests::a_retained_verify_pass_is_redecided_from_its_own_evidence,
    // added when a pressure-test history's skid-recovered PASS became
    // re-decided from its retained evidence (the same issue), retains all 839.
    // validation_dag::tests::committed_buck_e2e_selection_replaces_22_nodes_with_18,
    // added with the full-buck-e2e label (Buck as the E2E runner), retains all
    // 840 prior identities.
    // detcore-model's procfs::tests::ephemeral_host_seed_mount_class_is_the_seed_name
    // and seed_filter_drops_only_seed_rows_and_keeps_order
    // (https://github.com/rrnewton/hermit/pull/3219) retain all 841 prior
    // identities (`cargo nextest list --profile ci` measured 843).
    // detcore-model's procfs::tests::named_host_seeds_are_excluded
    // (https://github.com/rrnewton/hermit/pull/3219) retains all 843 prior
    // identities (`cargo nextest list --profile ci` measured 844).
    // hermit-manifest-plan's cli_help::sync_cells_refuses_bad_flags_and_checks_the_committed_tree
    // and test-harness's sync_cells_round_trips_an_unflip_and_reflip_byte_for_byte
    // and sync_cells_round_trips_a_parity_selected_kvm_cell, added in 32053b6d
    // (https://github.com/rrnewton/hermit/issues/3606) without a pin change;
    // `cargo nextest list --profile ci` over this node's selection measured 847.
    // test-harness's sync_cells_records_disabling_an_optional_cell, added in
    // 6166181d8f (the same issue) without a pin change, retains all 847 prior
    // identities (`cargo nextest list --profile ci` measured 848).
    // hermit-manifest-plan's
    // result_owner_index_answers_exactly_what_result_manifest_owner_answers
    // (76521affca) and the two imported-Buck-log parity tests
    // an_imported_row_is_measured_only_from_its_restored_logs and
    // an_imported_log_index_is_trusted_whole_or_not_at_all (9fd0c01a39), both
    // landed without a pin change, retain all 848 prior identities
    // (`cargo nextest list --profile ci` measured 851).
    // Measuring each budgeted invocation's live CPU from a cgroup of its own
    // adds 33 identities and removes one, so 850 of the 851 prior identities
    // are retained and the count rises by 32.
    // cpu_evidence::tests::each_live_source_is_named_and_the_cgroup_counter_cannot_fall
    // is replaced by
    // each_live_source_is_named_and_a_cgroup_last_sample_cannot_fall_below_its_high_water,
    // a name that says what it checks. The others are
    // cpu_evidence::tests::a_falling_cgroup_history_is_refused_by_every_reader,
    // a_flat_cgroup_history_keeps_distinct_high_water_and_last_points and
    // a_cgroup_membership_stop_charges_nothing_and_only_follows_a_cgroup_meter;
    // invocation_cgroup::tests::the_process_group_scan_marker_accepts_only_exactly_one,
    // the_unified_cgroup_entry_maps_to_sys_fs_cgroup_or_is_classified,
    // a_missing_or_non_cgroup_parent_is_eligible_and_a_file_parent_is_not,
    // a_symlinked_parent_cgroup_is_refused_and_not_eligible,
    // a_read_only_parent_cgroup_is_eligible_for_the_fallback,
    // a_fresh_invocation_cgroup_counts_its_process_monotonically_and_is_removed,
    // empty_cgroups_nested_by_the_command_are_removed_by_finish_and_by_drop and
    // a_populated_nested_cgroup_is_killed_and_removed_by_finish;
    // runner::tests::monotonic_cpu_sampler_refuses_a_decrease_and_keeps_the_high_value,
    // a_decreasing_cgroup_sample_is_refused_after_the_unavailable_grace,
    // a_transient_cgroup_decrease_is_graced_until_the_counter_recovers,
    // a_boxed_run_whose_invocation_cgroup_cannot_be_created_still_stops_the_cell,
    // a_boxed_run_without_an_invocation_cgroup_refuses_quick_commands_before_any_runs,
    // the_fallback_marker_does_not_hide_an_ineligible_cgroup_failure,
    // a_malformed_fallback_marker_refuses_the_budgeted_command_before_launch,
    // a_run_without_cgroups_measures_with_the_process_group_scan_under_its_own_source_name,
    // a_budgeted_cell_row_names_the_invocation_cgroup_as_its_cpu_source,
    // an_escaped_setsid_burner_is_charged_and_killed_through_the_invocation_cgroup,
    // a_leader_that_joins_the_runners_process_group_is_killed_when_the_stop_grace_ends,
    // group_leaving_leader_helper (the child that test re-executes; without its
    // environment variable it returns at once),
    // the_stop_kills_the_invocation_cgroup_before_it_waits_for_a_leader_that_left_its_group,
    // a_cell_whose_command_leaves_empty_nested_cgroups_passes_and_leaves_nothing_behind,
    // a_populated_nested_cgroup_is_killed_and_removed_when_the_wall_budget_stops_the_command,
    // a_process_that_moves_out_of_its_invocation_cgroup_is_stopped_and_refused,
    // the_cgroup_membership_check_refuses_every_process_it_sees_outside_the_cgroup,
    // a_membership_refusal_stops_the_invocation_and_charges_nothing and
    // the_real_membership_readers_find_this_process_and_its_group; and
    // test-harness
    // tests::validation_levels_privileged_dag_keeps_available_boxing_and_marks_only_the_unboxed_launch
    // and validation_levels_run_dag_audit_pins_both_privileged_launches.
    // `cargo nextest list --profile ci -p hermit-manifest-plan` measured 654
    // at e3424b2c0f29 and 686 with these commits on it; no other selected
    // crate changes.
    // runner::tests::a_failed_spawn_names_the_exec_or_the_cgroup_join_whichever_failed
    // retains all 883.
    // runner::tests::the_process_group_is_read_after_the_last_parenthesis_of_any_command_name
    // and runner::tests::a_process_named_with_bytes_that_are_not_utf8_does_not_fail_the_scan
    // retain all 884 (`cargo nextest list --profile ci -p hermit-manifest-plan`
    // measured 687 at 7e2fc5cfd65d and 689 with them).
    // The three parity import tests of 9af999677494,
    // parity::tests::an_imported_pair_earns_clean_credit_only_when_both_cells_ran_alike,
    // parity::tests::an_imported_reference_never_reuses_an_earlier_golden and
    // test-harness tests::parity_compare_finds_an_imported_runs_logs_through_its_summary,
    // retain all 886, and
    // runner::tests::a_retained_command_line_must_be_the_executor_invocation_its_row_records
    // retains all 889 (`cargo nextest list --profile ci -p hermit-manifest-plan`
    // measured 692 at 9af999677494 and 693 with it; the whole selection lists 890).
    // ledger::admission::tests::main_ancestor_floor_is_canonical_only_as_the_targets_own_floor
    // of 6bc5540bb6a7 retains all 890 (the validate node at b3037b11fa56 executed
    // 694 hermit-manifest-plan tests and 891 in the whole selection, all passing).
    // Seven runner::tests for the process-group scan's stat re-read (one per arm
    // of member_process_group, plus the real existence reader) retain all 891
    // (`cargo nextest list --profile ci -p hermit-manifest-plan` lists 701).
    // stress_series::tests::pressure_history_admits_a_nonzero_match_only_as_declared
    // retains all 898 (the same listing gives 702 with it).
    // manifest_metadata::tests::a_reproducer_selects_exactly_its_cell_including_focused_run_types
    // retains all 899 (the same listing gives 703 with it).
    // runner::tests::a_replay_cell_is_not_retried_after_a_product_failure and
    // test-harness tests::a_replay_cell_is_not_retried_after_a_product_failure
    // retain all 900 (the same listing gives 705 with them).
    // Folding the rr-compat-only run type into each row's own test removes
    // manifest_corpus::tests::a_replay_variant_runs_only_its_replay_cell_with_its_budget,
    // a_replay_variant_with_verify_settings_is_refused and
    // the_rr_variant_keeps_the_rr_lane_programs_and_gates_only_those_that_replay,
    // rewritten under new names as
    // a_replay_run_type_adds_a_labelled_replay_cell_to_each_rows_own_test,
    // a_malformed_replay_run_type_is_refused and
    // the_replay_cells_keep_the_rr_lane_programs_and_gate_only_those_that_replay,
    // and adds
    // runner::tests::a_replay_cell_that_declares_an_empty_env_runs_without_its_verify_env
    // and the front door's tests::accepts_an_env_in_a_replay_mode and
    // rejects_an_env_outside_verify_and_replay, so 899 of the 902 prior
    // identities are retained: 902 - 3 + 6 = 905 (the same listing gives 708).
    // stress_series::tests::timed_out_match_evidence_is_the_timeout_and_nothing_else
    // and stress_series::tests::declared_guest_exit_holds_a_timed_out_sibling_to_the_timeout_rule
    // retain all 905: 905 + 2 = 907 (the same listing gives 710 with them).
    // The network trace and record/replay data model
    // (https://github.com/rrnewton/hermit/pull/3777) adds 16 detcore-model
    // tests and retains all 907 prior identities: six
    // network_trace::tests::network_trace_v2_* tests and ten
    // network_engine::tests (907 + 16 = 923).
    // Opt-in network record and replay
    // (https://github.com/rrnewton/hermit/pull/3778) adds ten tests and
    // retains all 923 prior identities: eight network_engine::tests
    // (requests_record_pulls_at_the_time_they_answer,
    // shut_rd_delivers_queued_bytes_before_end_of_file,
    // every_error_names_a_remedy,
    // finished_replay_accepts_a_complete_run_even_with_input_unread,
    // finished_replay_refuses_unsent_output_and_unmade_connections,
    // record_refuses_a_peer_naming_no_single_host,
    // replay_refuses_an_input_held_back_by_bytes_never_sent and
    // replay_that_sends_what_gates_an_input_never_stalls),
    // network_trace::tests::traceable_peer_names_one_host_including_ipv4_mapped_addresses
    // and fd::tests::open_file_ids_display_their_kind_sequence_and_creator
    // (923 + 10 = 933, measured with cargo nextest list --profile ci).
    // The post-facto network replay fixes add seven tests and retain all
    // 933 prior identities: six network_engine::tests
    // (so_error_and_send_release_a_due_error_as_its_first_observer,
    // refused_nonblocking_sends_replay_as_eagain_at_the_same_offset,
    // finished_replay_refuses_an_unmade_refused_send,
    // record_refuses_a_send_interleaved_with_a_waiting_send,
    // record_refuses_a_send_that_overtook_a_zero_byte_wait and
    // record_refuses_a_refusal_pushed_during_a_wait) and
    // network_trace::tests::network_trace_v3_holds_refused_sends_that_v2_rejects
    // (933 + 7 = 940).
    // The fixes to the post-facto review of
    // https://github.com/rrnewton/hermit/pull/3791 add four tests and retain
    // all 940: network_engine::tests::
    // replay_accepts_a_waited_fragment_only_after_as_many_waits_and_as_long and
    // apply_stamps_a_waited_send_with_the_global_time_of_its_request, and
    // network_trace::tests::network_trace_v4_holds_send_waits_that_v2_and_v3_cannot
    // and network_trace_v4_validation_rejects_each_malformed_wait (940 + 4 = 944).
    // The three host-input-change retry tests (runner::tests::
    // only_a_typed_host_input_change_is_a_host_input_row and
    // a_host_input_attempt_that_breaks_a_declared_stdout_assertion_is_not_host_input_only,
    // and test-harness's
    // tests::a_host_input_change_earns_one_retry_even_on_a_no_retry_cell)
    // retain all 944 prior identities (947, measured with cargo nextest list).
    // detcore-model's config::tests::config_fingerprint_includes_the_inode_rpc_identity
    // retains all 947 (948, measured with cargo nextest list).
    // The three hermit-test-workdir bind tests (DBT --bind support) retain all
    // 948: 951, measured with cargo nextest list --profile ci --workspace
    // --exclude hermit-detcore --exclude hermit
    // --exclude hermetic_infra_hermit_flaky-tests.
    // Fourteen backend-capability controls (detcore-dbt's
    // tests::dbt_execution_model_reads_the_dbt_backend_capabilities,
    // tests::a_dbt_run_without_a_cli_configuration_keeps_its_previous_backend_facts,
    // tests::the_cli_configuration_round_trips_through_its_environment_encoding
    // and tests::a_cli_configuration_keeps_the_backend_facts_it_carries;
    // detcore-model's
    // config::tests::a_config_without_backend_capabilities_deserializes_as_ptrace,
    // config::tests::config_fingerprint_encodes_the_backend_capabilities_optional_field,
    // config::tests::legacy_backend_json_round_trips_every_backend_capabilities_constant,
    // config::tests::legacy_backend_keys_decode_as_the_fields_they_were,
    // config::tests::a_legacy_backend_key_of_the_wrong_type_fails_the_parse,
    // config::tests::legacy_positions_are_the_encoded_key_order,
    // config::tests::a_legacy_positional_array_reads_as_the_object_it_lists,
    // config::tests::a_malformed_legacy_positional_array_fails_the_parse,
    // config::tests::a_legacy_configuration_decodes_whatever_the_ambient_hermit_settings
    // and config::tests::keys_the_legacy_form_did_not_name_are_ignored)
    // retain all 951 prior identities: 951 + 14 = 965.
    // manifest-plan's service_result::tests::
    // delegated_writeback_is_schema_6_only_and_never_changes_the_exit retains
    // all 965 (966, measured by a full validation's executed count).
    // detcore-model's config::tests::record_host_inputs_never_enters_the_legacy_form
    // retains all 966 (967; each was measured as 965 + 1 on its own).
    // detcore-sabre's
    // tests::detlog_forwarding_follows_the_coordinators_per_target_policy
    // retains all 967 prior identities: 967 + 1 = 968.
    // manifest-plan's validation_dag::tests::
    // pinned_workspace_compile_does_not_wait_for_the_rust_script_tools retains
    // all 968: 969 (a full validation executed 969 against the 968 pin).
    // Three exact-branch-counter tests
    // (https://github.com/rrnewton/hermit/issues/3794: detcore-model's
    // host_capability::tests::exact_branch_counter_needs_evidence,
    // manifest-plan's runner::tests::exact_branch_counter_is_read_from_the_typed_host_report
    // and test-harness's
    // tests::strict_run_pmu_clock_cells_are_withheld_where_the_binary_reports_an_inexact_counter)
    // retain all 969: 969 + 3 = 972.
    // detcore-model's network_trace::tests::network_trace_above_64_mib_round_trips
    // and network_trace::tests::frames_are_the_header_then_the_bincode_payload
    // (https://github.com/rrnewton/hermit/issues/3803) retain all 972
    // (oversized_length_is_rejected_before_allocation is renamed
    // hostile_length_is_truncation_without_allocating_it): 972 + 2 = 974.
    // manifest-plan's
    // runner::tests::every_row_records_the_binarys_exact_branch_counter_verdict
    // retains all 974 (975, measured with cargo nextest list).
    // canonical_verdict::tests::the_exact_branch_counter_verdict_is_optional_and_strictly_typed,
    // which manifest-plan path-includes, retains all 975 (976, measured with
    // cargo nextest list).
    // detcore-sabre's tests::coordinator_fingerprint_survives_plugin_reinitialization
    // retains all 976 (977, measured with cargo nextest list --profile ci).
    // detcore-model's config::tests::a_saved_shared_dequeue_timers_field_is_ignored
    // and config::tests::a_controlled_key_that_contradicts_the_capabilities_fails_the_parse
    // retain all 977 (979, measured with cargo nextest list --profile ci).
    // validate_plan::tests::manifest_audit_uses_its_measured_cold_cache_cpu_budget_only,
    // which manifest-plan now path-includes from scripts/lib with the in-process
    // generator, retains all 979 (980, measured with cargo nextest list --profile ci).
    // manifest-plan's
    // runner::tests::a_diverged_chaos_cell_is_a_determinism_failure_as_in_verify
    // retains all 980: 981.
    // detcore-model's config::tests::process_exits_complete_asynchronously_never_enters_the_legacy_form
    // (the C3.5 pin bump) retains all 981: 982.
    // manifest-plan's
    // stress_series::tests::ineligible_execution_path_evidence_is_the_passed_sabre_attempt_and_nothing_else
    // retains all 982: 983.
    // manifest-plan's
    // validation_dag::tests::hosted_script_unit_tests_twin_changes_only_its_identity_and_wall
    // retains all 983: 984.
    // detcore-dbt's tests::only_a_linux_error_from_a_handler_reaches_the_guest and
    // tests::a_failed_thread_start_ends_the_run_instead_of_failing_the_syscall
    // retain all 984: 986.
    // ledger::framework_retries::tests::every_retried_cell_is_counted_with_its_first_attempt_and_selected_outcome
    // and ledger::framework_retries::tests::an_unreadable_history_refuses_rather_than_undercounting
    // (https://github.com/rrnewton/hermit/issues/1845) retain all 986: 988.
    // detcore-model's config::tests::
    // blocked_wait_signal_interruption_never_enters_the_legacy_form and
    // guest_may_inherit_a_terminal_never_enters_the_legacy_form
    // (https://github.com/rrnewton/hermit/pull/3361) retain all 988: 990
    // (measured with cargo nextest list --profile ci).
    // hermit-manifest-plan's nextest_binaries::tests::
    // a_changed_prepared_input_names_each_field_that_moved (2a43eb81) retains
    // all 990: 991 (validate run
    // validate-claude-coord-2a43eb81acf6-1791366692674881018-635384-a5df6dd9 executed 991).
    // hermit-manifest-plan's validation_dag::tests::
    // buck_cells_hand_validate_node_their_wall_bound retains all 991: 992 (validate run
    // validate-tickhub-ops-2-8d85cb2d50e0-1791374991259209356-157714-59e7d85a executed 992).
    // Making the parity population the verify matrix (every test whose ptrace
    // verify cell full validation selects, on every candidate backend, in
    // place of tests/e2e/parity-selection.yaml) keeps hermit-manifest-plan at
    // 726 tests and this node at 991, with changed identities. parity.rs
    // removes parity::tests::{the_initial_selection_is_exactly_its_rule,
    // a_selection_with_nothing_selectable_is_a_vacuous_snapshot,
    // the_selection_loader_refuses_what_it_cannot_measure,
    // a_selection_under_the_manifest_directory_is_refused} and adds
    // parity::tests::{the_snapshot_marks_exactly_the_population_selected,
    // a_disabled_candidate_of_a_population_test_is_a_parity_cell,
    // the_population_counts_every_candidate_zero_and_excludes_only_reference_failures};
    // parity::tests::the_scope_is_every_cell_with_a_planned_side_and_dbt_retains_nothing
    // is renamed ..._and_each_planned_side_is_retained. test-harness removes
    // tests::sync_cells_round_trips_a_parity_selected_kvm_cell and renames
    // tests::the_full_profile_reports_each_selected_parity_cell_exactly_once
    // to ..._each_population_parity_cell_exactly_once. The new
    // tests/parity_population.rs adds
    // the_parity_population_is_every_selected_ptrace_verify_test_on_every_candidate
    // and every_candidate_verify_column_is_a_parity_backend: 726 - 5 + 5 = 726
    // (`cargo nextest list --profile ci -p hermit-manifest-plan` lists 726).
    // parity::tests::an_unsampled_enabled_and_selected_candidate_is_outside_the_population_not_zero,
    // for the pressure test's not-sampled candidates, retains all 991: 992
    // (the same listing gives 727 with it).
    // The review fixes of 326e0b23 add
    // parity::tests::a_red_candidate_is_a_typed_zero_however_its_runs_compared and
    // parity::tests::a_not_sampled_row_is_admitted_only_from_the_pressure_test,
    // and rename runner::tests::selected_portable_parity_candidates_preserve_identical_guest_arguments
    // to runner::tests::population_parity_candidates_preserve_identical_guest_arguments:
    // 992 + 2 = 994 (the same listing gives 729).
    // main::tests::rejects_io_buffer_comparison_relaxation_with_reason, for the
    // refusal of compare_io_buffers=false with a reason, retains all 995: 996.
    // config::tests::target_timeslice_syscalls_only_never_enters_the_legacy_form
    // (lane qemu-rcb, https://github.com/rrnewton/hermit/pull/3859) retains
    // all 996: 997.
    // runner::tests::a_child_still_in_the_root_cgroup_while_forked_is_read_again
    // retains all 997: 998, listed with this node's arguments.
    // happens_before::tests::has_syscall_count_anchor_at_is_exact (detcore-model,
    // https://github.com/rrnewton/hermit/issues/3877) retains all 998: 999.
    // detcore-dbt's tests::a_recorded_determinism_loss_becomes_an_evidence_record
    // retains all 999: 1000, listed with this node's arguments.
    // config::tests::in_guest_detlog_forward_policy_never_enters_the_legacy_form
    // (detcore-model) retains all 1000: 1001, listed with this node's arguments.
    // validation_dag::tests::committed_full_selections_survive_a_slow_preparation
    // (hermit-manifest-plan, https://github.com/rrnewton/hermit/issues/3896)
    // retains all 1001: 1002, measured by the validate of 7ec8f5fb. Renamed
    // committed_selections_survive_a_slow_preparation when it widened to every
    // label (https://github.com/rrnewton/hermit/issues/3906): still 1002.
    // config::tests::replaying_never_enters_the_legacy_form and
    // config::tests::replaying_round_trips_through_reverie_bincode
    // (https://github.com/rrnewton/hermit/pull/3871) retain all 1002: 1004.
    // validation_dag::tests::test_free_declarations_name_count_free_committed_nodes
    // (https://github.com/rrnewton/hermit/issues/3915) retains all 1004: 1005.
    // config::tests::{scheduler_turn_cost_never_enters_the_legacy_form,
    // scheduler_turn_charge_refuses_a_scaled_charge_below_one_nanosecond} and
    // time::global_time_tests::{scheduler_turn_cost_sets_the_charge_per_turn,
    // the_largest_scheduler_turn_charge_advances_every_turn,
    // a_configured_turn_charge_refuses_to_saturate_the_clock,
    // the_default_turn_charge_still_saturates} (lane qemu-rcb,
    // https://github.com/rrnewton/hermit/pull/3885) retain all 1005: 1011.
    // happens_before::tests::syscall_fd_anchor_normalizes_and_displays,
    // syscall_fd_anchor_is_refused_where_it_cannot_apply and
    // count_syscall_occurrences_is_exact_per_thread (detcore-model) retain all
    // 1011: 1014.
    // Removed with the test-free declarations it checked (the zero-test run
    // refusal they existed to lift is gone): back to the same 1004.
    // runner::tests::an_in_guest_trap_dispatch_record_must_report_no_patched_site
    // and config::tests::in_guest_site_patching_off_never_enters_the_legacy_form
    // (`--backend=in-guest-trap`, step C4 of
    // https://github.com/rrnewton/hermit/issues/3520) retain all 1011: 1013,
    // listed with this node's arguments.
    // tests::an_in_guest_liteinst_cell_needs_cpuid_faulting (hermit-manifest-plan
    // test-harness; liteinst and in-guest-trap cells need CPUID faulting and
    // route locally) retains all 1013: 1014, listed with this node's arguments.
    // record_workloads::tests::standalone_population_is_shared_across_processes_and_pruned
    // (hermit-manifest-plan includes ci/record-replay-workloads.rs,
    // https://github.com/rrnewton/hermit/issues/3946) retains all 1017: 1018.
    // Removing the retired ptrace reference run's evidence types
    // (https://github.com/rrnewton/hermit/issues/3301) deletes nine
    // hermit-manifest-plan tests and adds two: backend_parity::tests (5, with
    // backend_parity.rs) and ledger::schema10::tests::{
    // reference_refusal_and_cross_divergence_never_change_the_candidate_verdict,
    // parity_attempt_decoder_requires_every_nullable_key_and_binds_raw_reports,
    // plans_retained_before_issue_3301_still_verify_their_parity_relations,
    // legacy_reference_failure_and_unavailable_cross_comparison_remain_visible}
    // go; ledger::schema10::tests::{
    // retired_backend_parity_evidence_is_refused_and_its_absence_accepted,
    // a_row_retained_with_the_retired_reference_run_is_excluded} come; and
    // cpu_evidence::tests::role_references_are_local_ordered_and_not_numeric_pid_identity
    // is renamed role_order_follows_preparation_and_timeouts_and_refuses_the_retired_reference.
    // `cargo nextest list --profile ci -p hermit-manifest-plan`: 737 -> 730,
    // so 1018 - 7 = 1011.
    // manifest_corpus::tests::{an_additional_backend_adds_full_validation_cells_with_their_own_flags,
    // a_malformed_additional_backend_is_refused} and
    // runner::tests::{corpus_additional_backend_flags_meet_the_manifest_validator,
    // a_per_backend_hermit_args_reason_covers_exactly_the_flagged_backends}
    // (hermit-manifest-plan, the compat corpus's additional backends,
    // https://github.com/rrnewton/hermit/issues/3745) retain all 1011: 1015,
    // listed with this node's arguments.
    // config::tests::{a_futex_wake_yields_only_when_it_woke_a_waiter,
    // a_futex_wake_yields_under_replay_too,
    // futex_wake_yields_never_enters_the_legacy_form} (lane qemu-rcb,
    // https://github.com/rrnewton/hermit/issues/3874) retain all 1015: 1018.
    // config::tests::inherited_seccomp_never_enters_the_legacy_form,
    // config::tests::inherited_seccomp_round_trips_through_reverie_bincode and
    // config::tests::inherited_seccomp_is_read_from_a_status_file (detcore-model)
    // retain all 1018: 1021, listed with this node's arguments.
    // runner::tests::a_probed_in_guest_cell_runs_without_a_preemption_timer
    // (hermit-manifest-plan; a probe of a disabled in-guest LiteInst cell runs
    // with --max-timeslice=disabled) retains all 1021: 1022.
    // glibc_compat::tests::{a_panic_unwinds_through_a_frame_in_a_new_dlmopen_namespace,
    // a_panic_unwinds_through_a_frame_in_the_callers_namespace,
    // a_constructor_waiting_for_a_workers_first_unwind_does_not_time_out,
    // the_first_unwind_keeps_a_pending_dlerror,
    // a_panic_in_an_earlier_constructor_unwinds_through_a_new_dlmopen_namespace,
    // the_dl_iterate_phdr_fallback_finds_this_code_and_its_unwind_tables,
    // a_newlm_panic_is_caught_past_an_object_named_libc_with_an_unreadable_dynamic_section,
    // a_newlm_panic_is_caught_with_libc_loaded_under_another_name,
    // a_newlm_panic_is_caught_through_a_forwarding_interposer}
    // (detcore-sabre, the guest preloads' _dl_find_object,
    // https://github.com/rrnewton/hermit/issues/3980) retain all 1022: 1031,
    // listed with this node's arguments.
    // config::tests::a_config_fingerprint_mismatch_names_both_fingerprints
    // (lane qemu-rcb, https://github.com/rrnewton/hermit/issues/3986) retains
    // all 1031: 1032.
    // The native per-physical-network namespace regression: 1023, listed.
    // happens_before::tests::{version_2_fields_need_version_2,
    // version_2_refuses_unknown_fields,
    // futex_op_matches_the_low_32_bits_of_the_operation,
    // from_refusals_name_the_event,
    // relative_anchors_count_strictly_after_their_base} (detcore-model,
    // relative happens-before anchors) retain all 1033: 1038.
    // happens_before::tests::a_duplicated_field_is_refused (detcore-model,
    // Codex review of https://github.com/rrnewton/hermit/pull/4004) retains
    // all 1038: 1039.
    // runner::tests::a_guest_permission_denied_under_a_first_run_rejection_is_a_product_crash
    // (hermit-manifest-plan, https://github.com/rrnewton/hermit/issues/4029)
    // retains all 1039: 1040.
    // The required allocator prerequisite mutation test retains all 1039 prior identities: 1040.
    // The four retained cell-results tests in ledger::tests
    // (https://github.com/rrnewton/dev-hermit/issues/552) retain all 1042: 1046.
    // happens_before::tests::{a_posthook_anchor_is_counted_at_entry_and_refused_on_no_return_syscalls,
    // a_posthook_anchor_shares_its_counter_and_starts_windows_at_its_entry}
    // (https://github.com/rrnewton/hermit/issues/3929) retain all 1046: 1048.
    // The four retained-census, transcoding, depth and key-order tests in
    // ledger::tests (https://github.com/rrnewton/dev-hermit/issues/552)
    // retain all 1048: 1052.
    // The DBT startup-authority refusal regression retains all 1048 prior identities.
    // The depth-boundary test in ledger::tests and the repeated-census-key and
    // extension-count tests in ledger::admission::tests retain all 1053: 1056.
    // runner::tests::a_typed_run_failure_is_a_product_crash_with_its_reason
    // (hermit-manifest-plan, the run_failed verification reason) retains all
    // 1048: 1049.
    // config::tests::backend_config_json_{preserves_every_backend_and_representable_bytes,
    // round_trips_each_independent_capability,keeps_legacy_object_and_array_semantics,
    // requires_each_typed_capability_once} (detcore-model), plus
    // tests::{versioned_config_preserves_independent_descriptor_facts_in_the_runtime,
    // a_rejected_versioned_config_keeps_the_existing_parse_fallback_policy}
    // (detcore-dbt), retain all 1058 prior cases: 1064.
    // stress_series::tests::failed_match_pressure_evidence_is_a_non_canonical_failed_attempt
    // retains all 1058: 1059.
    ("test.regular_crates", 1065),
    // Three tracing PID-alignment tests added in f9383156 retain all 707 prior IDs.
    // Twelve epoch controls and the LiteInst stderr-pressure control retain all 710 prior IDs.
    // Two real readv import-permission companions retain all 748 prior identities.
    // Two proc-fallback container tests and one broken-stderr warning test
    // retain all 750 prior identities in the prepared Nextest inventory.
    // Three logdiff_report schema-2 tests and the bin/hermit matched-prefix
    // report test (d550979ad0) retain all 753 prior identities.
    // The five PMU-subject ptrace_completion::tests::real_random_ cases move
    // to privileged-test.pmu_ptrace_completion_cases, and the namespace-only
    // perf-probe control is added: 757 - 5 + 1 = 753.
    // The nosuid,nodev image-root regression
    // (container::tests::image_container_accepts_a_rootfs_on_a_nosuid_nodev_filesystem)
    // retains all 753 prior identities (`cargo nextest list --profile ci`
    // measured 754; https://github.com/rrnewton/hermit/issues/3334).
    // Four container-init stop-signal bridge tests (container::tests::
    // a_namespace_init_honours_stop_signals_sent_before_it_arms,
    // the_process_holding_the_bridge_still_dies_from_stop_signals,
    // the_bridge_leaves_ignored_and_handled_signals_alone_and_restores_the_default,
    // the_bridge_is_not_installed_by_a_process_that_is_pid_1) retain all 754
    // prior identities (`cargo nextest list --profile ci` measured 758;
    // https://github.com/rrnewton/hermit/issues/3354).
    // subcommand_level_backend_is_a_usage_error_naming_the_global_form,
    // misplaced_backend_is_only_moved_when_unambiguous and
    // global_backend_with_namespace_only_is_a_usage_error,
    // analyze_and_bisect_accept_every_run_backend and
    // global_backend_reaches_every_trial_run
    // (https://github.com/rrnewton/hermit/pull/3439) retain all 758 prior
    // identities.
    // instruction_map::tests::decodes_instructions_that_end_at_or_cross_a_4gib_host_address
    // (https://github.com/rrnewton/hermit/issues/3462) retains all 763 prior
    // identities.
    // version::tests::fbcode_version_leads_with_the_crate_version and
    // version::tests::fbcode_version_marks_missing_build_facts_unknown
    // (https://github.com/rrnewton/hermit/pull/3511) retain all 764 prior
    // identities.
    // https://github.com/rrnewton/hermit/pull/3544 adds
    // record_start::tests::record_networking_defaults_to_local_like_run,
    // record_start::tests::a_gdb_checked_recording_refuses_local_networking,
    // replay::tests::only_an_autopilot_replay_of_a_local_recording_is_local and
    // metadata::tests::metadata_without_a_network_choice_loads_as_unknown, which
    // retain all 766 prior identities.
    // https://github.com/rrnewton/hermit/pull/3580 adds
    // event::tests::fd_set_bytes_rounds_up_to_whole_longs,
    // recorder::network::tests::{capture_select_keeps_sets_and_timeout_on_success,
    // select_capture_is_clamped_to_the_descriptor_table,
    // capture_select_keeps_the_readable_prefix_and_drops_an_unreadable_timeout,
    // capture_select_omits_sets_the_kernel_did_not_copy_out} and
    // replayer::network::tests::{replay_select_restores_sets_and_exact_timeout,
    // replay_select_restores_only_the_recorded_prefix_before_efault}, which
    // retain all 770 prior identities.
    // https://github.com/rrnewton/hermit/issues/3537 adds
    // recorder::mmap::tests::{parses_maps_lines,
    // refill_ranges_clip_to_the_advised_range,
    // refill_ranges_cover_shared_files_but_not_shared_anonymous_memory,
    // this_hosts_shared_anonymous_memory_is_on_the_shmem_device,
    // wipeonfork_prefix_stops_at_the_first_private_file_mapping} and
    // replayer::mmap::tests::{differing_pages_merges_adjacent_changed_pages,
    // differing_pages_treats_unread_bytes_as_changed}, which retain all 777
    // prior identities.
    // https://github.com/rrnewton/hermit/pull/3598 adds
    // event_stream::tests::path_query_and_mutation_syscalls_have_kernel_arities,
    // which retains all 784 prior identities.
    // version::tests::cargo_version_names_a_stamped_revision and
    // version::tests::cargo_version_of_an_unstamped_build_says_dev_build
    // (https://github.com/rrnewton/hermit/pull/3547) retain all 785 prior
    // identities.
    // verify::tests::stripped_success_does_not_claim_bitwise_parity_but_strict_success_does
    // (newcomer-audit verify-claim wording) retains all 766 prior identities.
    // tests::top_level_help_states_what_the_defaults_actually_are and
    // record_start::tests::completion_hint_offers_plain_playback_before_the_gdb_session
    // (newcomer-audit CLI help and replay-hint wording) retain all 767 prior
    // identities.
    // https://github.com/rrnewton/hermit/pull/3522 adds
    // backend_stats::tests::a_requested_summary_collects_without_debug_logging
    // and verify::tests::report_carries_each_run_dispatch_record and removes
    // backend_stats::tests::baseline_ptrace_snapshot_is_explicit, because
    // ptrace now reports real dispatch counters instead of "metrics=none".
    // The other 789 identities are retained.
    // Keeping only the golden run-1 log after a matched verify
    // (https://github.com/rrnewton/hermit/issues/3301) adds five bin/hermit
    // retention tests (verify::tests::
    // requested_logs_survive_a_match_that_did_not_compare_them,
    // requested_logs_survive_a_refused_comparison,
    // an_overridden_match_keeps_both_logs_in_the_failure_directory;
    // tests::dbt_backend_divergence_overrides_a_log_match_for_retention;
    // run::tests::skid_overshoot_overrides_a_log_match_for_retention) and
    // retains all 791 prior identities (`cargo nextest list --profile ci`
    // measured the +5 when the change was written, not on this base).
    // https://github.com/rrnewton/hermit/pull/3603 adds
    // record_replay_path::tests::procfs_symlinks_are_refused_only_when_requested
    // (replay-root containment refuses procfs symlinks) and retains all 796
    // prior identities.
    // In-guest LiteInst (https://github.com/rrnewton/hermit/pull/3635) adds
    // eighteen tests and retains all 797 prior identities (`cargo nextest list
    // --profile ci` measured +18): run::liteinst_in_guest_refuses_options_it_cannot_honour,
    // run::liteinst_host_hybrid_is_not_subject_to_the_in_guest_refusals,
    // error::tests::in_guest_liteinst_refusal_is_serialized_as_a_policy_refusal,
    // five interp::tests::startup_* ELF-reader tests, two
    // script::test::kernel_script_* #! parser tests, and eight
    // tests::in_guest_liteinst_* / tests::liteinst_runtime_selector_* tests.
    // tests::kvm_reports_stored_metadata_timestamps_only_for_sequential_tool_threads,
    // added with KVM's stored host file timestamps (562b7dd7a635), retains
    // all 815 prior identities.
    // verify::tests::skid_overshoot_refusal_keeps_a_refused_comparisons_reason
    // (a skid overshoot no longer erases a refused comparison's reason, so the
    // SKID-RETRY of https://github.com/rrnewton/hermit/issues/1845 cannot
    // retry it) retains all 816 prior identities.
    // tests::identity_capture_excludes_host_seed_mounts_and_keeps_order
    // (https://github.com/rrnewton/hermit/pull/3219) retains all 817 prior
    // identities (`cargo nextest list --profile ci` lists 823 = 818 plus the
    // five real_random_ PMU cases this node skips).
    // The four dbt_detconfig_tests::* cases, which pin HERMIT_DBT_DETCONFIG to
    // its 806cf2fa38d1 bytes after the deleted Detcore switches
    // (https://github.com/rrnewton/hermit/issues/3765), retain all 818 prior
    // identities: 818 + 4 = 822.
    // The six --record-networking/--replay-networking parsing tests of
    // https://github.com/rrnewton/hermit/pull/3778 in hermit's run.rs
    // (network_trace_is_off_without_a_networking_flag,
    // record_networking_selects_record_mode_and_host_networking,
    // record_networking_refuses_a_missing_directory_and_conflicting_flags,
    // replay_networking_reads_the_trace_before_the_guest_starts,
    // replay_networking_adopts_the_trace_epoch_and_refuses_another and
    // network_trace_is_reached_through_a_descriptor_opened_before_the_container)
    // retain all 822 prior identities: 822 + 6 = 828 (`cargo nextest list
    // --profile ci` lists 833 = 828 plus the five real_random_ PMU cases this
    // node skips).
    // Making LiteInst in-guest only
    // (https://github.com/rrnewton/hermit/issues/3520) deletes the
    // ptrace-hosted hybrid and eight unit tests whose subject it was:
    // tests::liteinst_pin_guard::{a_runtime_with_no_recorded_revision_is_refused,
    // a_runtime_from_another_revision_is_refused_naming_both,
    // a_matching_runtime_is_accepted,
    // the_loader_consults_the_guard_before_anything_else,
    // a_versioned_soname_finds_its_marker},
    // tests::liteinst_runtime_selector_accepts_only_unset_or_one,
    // run::skid_margin_override_is_available_to_liteinst_host_hybrid and
    // run::liteinst_host_hybrid_is_not_subject_to_the_in_guest_refusals. It
    // renames liteinst_host_backend_preserves_ptrace_rcb_timeslices to
    // liteinst_backend_config_preserves_the_requested_timeslice and
    // liteinst_public_dispatch_runs_ptrace_host_hybrid to
    // liteinst_public_dispatch_runs_in_guest_detcore, and retains the other
    // 812 identities unchanged (`cargo nextest list --profile ci` with this
    // node's filters lists 814, the two renamed tests included). The parent
    // pinned 818 but listed 822: its four dbt_detconfig_tests::* cases, which
    // pin HERMIT_DBT_DETCONFIG after the deleted Detcore switches
    // (https://github.com/rrnewton/hermit/issues/3765), were not counted, so
    // this is 822 - 8 = 814.
    // Both together: 822 + 6 (https://github.com/rrnewton/hermit/pull/3778)
    // - 8 (https://github.com/rrnewton/hermit/issues/3520) = 820.
    // The thirteen host_input_change::tests (naming a host file that changed
    // during one verification run) retain all 820 prior identities (833,
    // measured with cargo nextest list and this node's filters).
    // Four backend-capability controls (tests::backend_capabilities_match_the_golden_table,
    // tests::backend_capabilities_are_the_ones_each_backend_reports,
    // tests::dbt_detconfig_bytes_are_unchanged_by_backend_capabilities and
    // tests::every_backend_encodes_its_previous_backend_keys) retain all 833
    // prior identities: 833 + 4 = 837.
    // run::tests::forwarded_detlogs_respect_the_log_bound_and_keep_its_truncation_marker
    // (https://github.com/rrnewton/hermit/issues/3520, C5) retains all 837:
    // 837 + 1 = 838.
    // tests::sabre_detlog_forwarding_answers_each_target_as_the_cli_filter_does
    // retains all 838 prior identities: 838 + 1 = 839.
    // host_input_change::tests::a_replacement_where_the_guest_rebound_the_path_is_not_named
    // and host_input_change::tests::a_log_with_mutation_lines_is_complete_when_its_end_counts_them
    // retain all 839 (841, measured with cargo nextest list).
    // tests::liteinst_detlog_forwarding_hands_the_guest_its_per_target_policy
    // and tests::detlog_forward_policy_matches_real_detlog_emission_under_field_filters
    // retain all 841 (843, measured with cargo nextest list).
    // Six exact-branch-counter tests
    // (https://github.com/rrnewton/hermit/issues/3794: host_capabilities::tests::
    // only_a_failed_validation_of_an_armed_counter_is_inexact,
    // a_validation_failure_kind_carries_no_measured_count and
    // cpu_identity_reads_the_first_processor; run::
    // strict_refuses_an_inexact_branch_counter_on_the_pmu_backends,
    // strict_refusal_needs_strict_a_pmu_backend_and_an_inexact_counter and
    // strict_run_refuses_an_inexact_branch_counter_before_it_looks_at_the_program)
    // retain all 843: 843 + 6 = 849.
    // run::passthru_opt_leaves_guest_syscalls_unobserved_on_every_backend
    // retains all 849 (850, measured with cargo nextest list).
    // canonical_verdict::tests::the_exact_branch_counter_verdict_is_optional_and_strictly_typed
    // and verify::tests::a_recorded_branch_counter_verdict_is_stamped_on_every_report_once
    // retain all 859 (861, measured with cargo nextest list).
    // run.rs the_ptrace_runtime_guards_reverie_private_page retains all 861 (862).
    // The 40 --max-log-bytes unit tests of
    // https://github.com/rrnewton/hermit/pull/3686 retain all 862 prior
    // identities: 862 + 40 = 902 (`cargo nextest list --profile ci` with this
    // node's filters lists 902). They are 25 tracing::tests
    // (max_log_bytes_accepts_binary_suffixes,
    // max_log_bytes_refusals_redirect_to_a_working_value,
    // byte_sizes_render_in_the_accepted_spelling,
    // the_budget_reports_the_crossing_exactly_once,
    // capped_writer_counts_what_it_passes_through,
    // capped_writer_counts_bytes_the_file_bound_discards,
    // crossing_the_cap_exits_with_the_log_cap_status_and_says_why,
    // a_refund_after_the_crossing_does_not_reopen_the_budget,
    // crossing_the_cap_exits_even_when_no_sink_can_take_the_message,
    // a_cap_diagnostic_to_a_departed_reader_raises_no_signal,
    // the_diagnostic_signal_guard_consumes_only_the_signal_its_write_raised,
    // the_proc_fd_path_names_the_descriptor,
    // the_crossing_message_ends_with_the_class_line_at_every_limit,
    // a_total_past_u64_max_crosses_even_the_largest_limit,
    // a_log_ending_with_the_stop_message_reads_as_truncated,
    // the_stop_message_is_not_appended_after_the_truncation_marker,
    // a_failed_exit_timer_start_exits_with_the_log_cap_status_at_once,
    // a_panic_after_the_cap_ended_the_run_still_exits_with_the_log_cap_status,
    // the_outer_log_cap_diagnostic_is_attempted_at_most_once,
    // a_write_in_progress_at_the_crossing_withholds_the_final_message,
    // a_write_admitted_before_the_crossing_is_waited_for_even_after_a_refund,
    // only_the_first_write_past_the_limit_claims_the_crossing,
    // the_drain_wait_gives_up_at_its_bound,
    // capped_writer_ends_every_admission_it_starts and
    // the_final_message_is_the_last_line_when_writers_race_the_crossing);
    // run::tests::forwarded_records_alone_cross_the_log_cap;
    // run::{log_cap_is_refused_where_the_guest_could_outlive_hermit,
    // log_cap_is_accepted_where_hermit_takes_the_guest_down_with_it,
    // log_cap_refusal_table_covers_every_backend};
    // container::tests::{log_cap_crossed_before_the_container_init_arms_stops_the_init,
    // log_cap_crossed_before_the_namespace_only_guest_arms_stops_the_exec};
    // analyze::rundata::tests::{analyze_trials_charge_the_invocations_log_budget,
    // log_cap_refusal_reaches_analyze_and_bisect_trials};
    // verify::tests::{log_cap_retention_report_survives_a_stderr_without_a_reader,
    // log_cap_retention_report_does_not_wait_on_a_full_stderr_pipe,
    // retention_report_reads_as_before_when_stderr_has_room};
    // analyze::phases::tests::a_failing_search_round_ends_the_search_with_its_error;
    // analyze::minimize::tests::a_failing_minimize_trial_ends_minimization_with_its_error;
    // and schedule_search::tests::{a_failing_trial_ends_the_schedule_search_with_its_error,
    // a_failing_jitter_replay_returns_its_error}.
    // The shared physical-exit watcher's four runtime-mode tests and the three
    // in_guest_exits admission tests retain all 902 prior IDs: 909.
    // The exit watchdog's six tests (in_guest_exits) retain all 909: 915.
    // The two verification-latch tests and the in-guest kernel-floor test retain all 915: 918.
    // The watcher's two auto-reap tests retain all 918: 920.
    // Three C3.5 fix-forward tests (a refused admission, a brief exit-path sleep, a recorded failure failing the run) retain all 920: 923.
    // metadata::tests::record_version_rejects_pre_request_ordinal_inode_streams
    // and replay::tests::replay_refuses_a_recording_from_before_inode_request_ordinals
    // (https://github.com/rrnewton/hermit/issues/2897) retain all 923: 925.
    // The --strict inexact-branch-counter refusal in analyze, bisect and record
    // start (https://github.com/rrnewton/hermit/issues/3810):
    // analyze::rundata::tests::{strict_trials_refuse_an_inexact_branch_counter,
    // strict_analyze_refuses_an_inexact_branch_counter_before_its_workspace},
    // bisect::tests::strict_bisect_refuses_an_inexact_branch_counter_before_reading_schedules
    // and record_start::tests::{strict_record_refuses_an_inexact_branch_counter_before_it_records,
    // record_refusal_needs_strict_and_an_inexact_counter} retain all 925: 930
    // (measured with cargo nextest list).
    // Re-listed with the admission-failed hook test: 926.
    // Re-listed with the coordinator-failure and late-watch kill tests: 928.
    // tests::backend_blocked_wait_signal_contract_is_explicit,
    // tests::a_launch_may_pass_a_terminal_unless_it_proves_it_has_none and
    // tests::only_gated_backends_record_an_inherited_terminal
    // (https://github.com/rrnewton/hermit/pull/3361) retain all 902 (905, measured with cargo
    // nextest list).
    // The 933 pinned before https://github.com/rrnewton/hermit/pull/3361 and its three tests above,
    // re-listed together: 936 (measured with cargo nextest list --profile ci).
    // The verify signal report's six verify::tests (signal_termination_report_*
    // and a_log_tail_starts_at_a_whole_line) retain all 936: 942 (measured
    // with cargo nextest list).
    // Run options that analyze and bisect trials do not apply are refused, and
    // their --skid-margin is installed
    // (https://github.com/rrnewton/hermit/issues/3835):
    // analyze::rundata::tests::{trials_refuse_run_options_only_main_applies,
    // trials_refuse_run_options_only_main_applies_before_their_workspace,
    // analyzer_reproducer_retains_the_trial_timeout,
    // analyze_installs_the_trials_skid_margin} and
    // bisect::tests::{bisect_refuses_run_options_its_replays_do_not_apply,
    // bisect_installs_the_replays_skid_margin} retain all 942: 948 (measured
    // with cargo nextest list).
    // analyze::rundata::tests::{trials_refuse_outputs_they_overwrite,
    // trials_refuse_a_timeout_their_backend_cannot_enforce} (rel-041's review
    // of https://github.com/rrnewton/hermit/pull/3836) retain all 948: 950.
    // The three birth-identity exit-watch tests retain all 950: 953, re-listed.
    // verify::tests::signal_reports_fill_a_pipe_that_cannot_take_them_whole
    // retains all 953: 954, re-listed with this node's arguments.
    // bisect::tests::{bisect_replays_from_the_epoch_its_schedules_were_recorded_under,
    // bisect_refuses_schedules_recorded_under_two_epochs}
    // (https://github.com/rrnewton/hermit/issues/3835) retain all 954: 956,
    // measured with cargo nextest list.
    // analyze::phases::tests::analyze_replays_a_record_from_the_epoch_it_was_recorded_under
    // (https://github.com/rrnewton/hermit/issues/3870) retains all 956: 957,
    // measured with cargo nextest list.
    // run::display_runopts_without_a_pmu_profile retains all 957: 958, listed
    // with this node's arguments.
    // run::tests::a_green_run_removes_only_its_own_fatal_cores: 959.
    // analyze::phases::tests::analyze_replays_a_network_trace_from_the_epoch_it_was_recorded_under
    // (https://github.com/rrnewton/hermit/issues/3875) retains all 959: 960,
    // measured with cargo nextest list.
    // metadata::tests::a_record_without_a_timer_has_no_timeslice retains
    // all 960: 961, listed with this node's arguments.
    // backends::tests::a_dbt_run_whose_evidence_records_a_determinism_loss_is_not_compared
    // retains all 961: 962, listed with this node's arguments.
    // analyze::phases::tests::analyze_refuses_the_unimplemented_schedule_inputs_before_any_trial
    // retains all 962: 963, measured with cargo nextest list.
    // metadata::tests::record_version_rejects_pre_accept_streams and five
    // recorder::network::tests::accept_copy_out_* tests
    // (https://github.com/rrnewton/hermit/pull/3871) retain all 963: 969,
    // listed with this node's arguments.
    // Two replayer::shutdown_replay_tests (the same pull request) retain all
    // 969: 971.
    // Two replayer::accept_stand_in_tests (the same pull request) retain all
    // 971: 973.
    // Two more replayer::accept_stand_in_tests and five recorder::network
    // tests of the user read and the accepted socket's peer address (the same
    // pull request) retain all 973: 980.
    // scheduler_turn_cost_parses_round_trips_and_is_refused_for_dbt (lane
    // qemu-rcb, https://github.com/rrnewton/hermit/pull/3885) retains all 980:
    // 981.
    // Thirteen run_config::tests and three run::saved_config_* tests of the
    // loadable run config (`hermit run --config` / `--save-config`) retain all
    // 981: 997.
    // happens_before::tests::launch_refuses_every_anchor_it_cannot_enforce
    // retains all 997: 998.
    // metadata::tests::record_version_rejects_backgrounded_accept_streams
    // (https://github.com/rrnewton/hermit/pull/3908) retains all 998: 999,
    // listed with this node's arguments less the five skipped real_random_
    // tests.
    // `--backend=in-guest-trap` (step C4 of
    // https://github.com/rrnewton/hermit/issues/3520) adds seven:
    // tests::{in_guest_trap_is_in_guest_liteinst_and_forces_site_patching_off,
    // in_guest_trap_refuses_a_guest_environment_that_turns_site_patching_on},
    // backend_stats::tests::{in_guest_trap_reports_the_liteinst_source_under_its_own_name,
    // in_guest_trap_rejects_a_source_of_another_runtime},
    // error::tests::in_guest_trap_refusal_is_serialized_as_a_policy_refusal,
    // tests::in_guest_trap_is_rejected_outside_run (bin hermit) and
    // run::in_guest_trap_refuses_the_options_liteinst_refuses (bin hermit),
    // retaining all 997: 1004, listed with this node's arguments.
    // tests::{an_in_guest_trap_run_that_patched_a_site_is_refused_after_it_ends,
    // detcore_checks_the_variable_the_in_guest_runtime_reads} (the
    // in-guest-trap coordinator postcondition) retain all 1004: 1006, listed
    // with this node's arguments.
    // sabre_bootstrap::tests::frame_descriptor_v2_decodes_r10 retains all
    // 997: 998.
    // run::{happens_before_refusal_accepts_e9patch_and_refuses_in_guest_backends,
    // happens_before_spec_load_errors_are_refusals_except_host_read_failures}
    // (bin hermit, https://github.com/rrnewton/hermit/issues/3943) retain all
    // 1009: 1011.
    // Three host_seccomp::tests and run_config's global-option-after-the-
    // subcommand test (the inherited seccomp filter refusal,
    // https://github.com/rrnewton/hermit/issues/3942) retain all 1009: 1013.
    // metadata::tests::record_version_rejects_newest_first_futex_wake_streams
    // (lane qemu-rcb, https://github.com/rrnewton/hermit/issues/3917) retains
    // all 1015: 1016.
    // futex_wake_yields_parses_round_trips_and_is_refused_where_inert (lane
    // qemu-rcb, https://github.com/rrnewton/hermit/issues/3874) retains all
    // 1016: 1017.
    // metadata::tests::record_version_rejects_pre_vfork_release_edge_streams
    // (https://github.com/rrnewton/hermit/issues/3984) retains all 1017: 1018.
    // run::a_version_2_spec_is_refused_when_validation_turns_the_timer_off
    // (relative happens-before anchors) retains all 1018: 1019.
    // metadata::tests::record_version_rejects_pre_child_subreaper_streams
    // (https://github.com/rrnewton/hermit/issues/3997) retains all 1019: 1020.
    // And the two signal-target metadata tests (https://github.com/rrnewton/hermit/issues/3963): 1022.
    // And metadata::tests::record_version_rejects_streams_that_held_a_committed_sigchld_in_a_futex_wait
    // (review of https://github.com/rrnewton/hermit/pull/4039): 1023.
    // canonical_verdict::tests::run_failure_round_trips_and_names_no_guest_disposition
    // and verify::tests::a_typed_run_failure_replaces_the_stamp_and_an_untyped_error_does_not
    // (the run_failed verification reason) retain all 1023: 1025.
    // metadata::tests::record_version_rejects_streams_from_before_raw_clone_processes
    // (https://github.com/rrnewton/hermit/issues/4012) retains all 1025: 1026.
    // dbt_detconfig_tests::{current_dbt_uses_exact_descriptor_facts_without_host_timed_turns,
    // dbt_detconfig_keeps_descriptor_facts_independent,
    // representable_descriptor_facts_keep_the_current_config_bytes,
    // versioned_dbt_config_rejects_unknown_or_malformed_envelopes}
    // retain all 1025 prior cases: 1029. Historical-byte and legacy-projection
    // test renames preserve their cases and do not add to this count.
    // And metadata::tests::record_version_rejects_streams_that_answered_a_futex_rewait_as_woken
    // (https://github.com/rrnewton/hermit/issues/4033): 1031.
    // And metadata::tests::record_version_rejects_streams_that_read_a_futex_wait_word_through_the_tracer
    // (review of https://github.com/rrnewton/hermit/pull/4061): 1032.
    // And metadata::tests::record_version_rejects_streams_whose_pidfd_signals_woke_no_receiver
    // (https://github.com/rrnewton/hermit/issues/4099): 1033.
    // only_a_group_kill_from_hermits_group_is_refused
    // (https://github.com/rrnewton/hermit/issues/4065) retains all 1033: 1034.
    // And metadata::tests::record_version_rejects_streams_that_answered_a_zombie_kill_with_esrch
    // (https://github.com/rrnewton/hermit/issues/4091): 1035.
    // And metadata::tests::pre_epoll_pwait_recordings_are_refused_by_the_version_gate: 1036.
    // And metadata::tests::record_version_rejects_streams_that_answered_a_zombie_sigqueueinfo_with_esrch
    // (https://github.com/rrnewton/hermit/issues/4091): 1037.
    // Three physical constructor-registration controls retain all prior cases.
    // tests::backend_timer_delivery_is_explicit
    // (https://github.com/rrnewton/hermit/issues/4068) retains all 1040: 1041.
    // And metadata::tests::record_version_rejects_streams_that_answered_a_thread_id_kill_with_esrch
    // (https://github.com/rrnewton/hermit/issues/4094): 1042.
    // And metadata::tests::pre_yield_readmission_recordings_are_refused_by_the_version_gate
    // (Codex review of https://github.com/rrnewton/hermit/pull/4120): 1043.
    // And metadata::tests::pre_tight_stack_ppoll_recordings_are_refused_by_the_version_gate
    // (Claude review of https://github.com/rrnewton/hermit/pull/4127): 1044.
    // And metadata::tests::record_version_rejects_streams_that_answered_a_gone_sigqueueinfo_before_its_siginfo
    // (https://github.com/rrnewton/hermit/issues/4124): 1045.
    // The two SIGKILL-retirement metadata/replay tests (https://github.com/rrnewton/hermit/issues/3994) retain all 1045: 1047.
    // And metadata::tests::pre_epoll_pwait2_recordings_are_refused_by_the_version_gate
    // (https://github.com/rrnewton/hermit/issues/4088): 1048.
    // And metadata::tests::record_version_rejects_streams_whose_stop_signals_armed_no_orphaned_group_record
    // (https://github.com/rrnewton/hermit/issues/4111): 1049.
    ("test.hermit_unit", 1049),
    // Fifteen stage-two child-publication controls retain all 728 prior IDs.
    // Five resource-limit controls retain all 743 prior identities.
    // Three descriptor-import error controls retain all 780 prior identities.
    // Four process-retirement fence controls and four uncontrolled-retirement
    // controls retain all 789 prior identities.
    // The fractional-boot /proc/uptime and round-up sysinfo(2) uptime
    // regressions retain all 797 prior identities.
    // The fixed-boot-instant /proc/stat btime regression retains all 799 prior
    // identities.
    // The every-representable-offset btime regression and the btime-only-for-
    // /proc/stat gate retain all 800 prior identities (`cargo nextest list`
    // measured 802).
    // Five exec POSIX timer lifecycle tests retain all 802 prior identities.
    // The logdiff matched-prefix test (d550979ad0) retains all 807 prior IDs.
    // Two matched-prefix/verdict agreement tests retain all 808 prior IDs.
    // Thirty-three exec transfer, teardown and refusal controls retain all 810 IDs.
    // Two empty-queue fizzle shutdown tests (scheduler::test::
    // sabre_last_exit_logs_one_fizzle_wherever_the_final_wait_status_lands and
    // redundant_exit_hook_after_exit_group_logs_one_fizzle_wherever_it_lands)
    // retain all 843 prior identities (`cargo nextest list --profile ci`
    // measured 845; https://github.com/rrnewton/hermit/issues/3360).
    // Three inject_fstat scratch tests (syscalls::files::inject_fstat_scratch::
    // writable_stack_scratch_is_used_while_its_guard_is_live,
    // faulting_stack_scratch_falls_back_to_a_transient_page and
    // descriptor_is_closed_when_no_scratch_can_be_found) retain all 845 prior
    // identities (`cargo nextest list --profile ci` measured 848;
    // https://github.com/rrnewton/hermit/issues/3328).
    // preemptions::tests::recorded_epoch_round_trips_and_legacy_records_have_none
    // (https://github.com/rrnewton/hermit/issues/3411) retains all 848 prior
    // identities.
    // tool_global::tests::external_scheduler_announces_before_its_future_is_polled
    // (https://github.com/rrnewton/hermit/issues/3463) retains all 849 prior
    // identities.
    // io_buffers::event_tests::
    //   backend_runtime_bootstrap_syscall_is_handled_but_not_charged_to_guest_time
    // and io_buffers::event_tests::
    //   backend_runtime_bootstrap_window_charges_time_reads_and_caps_uncharged_syscalls
    // (https://github.com/rrnewton/hermit/pull/3430) retain all 850 prior
    // identities.
    // The wait4 argument-validation precedence test
    // (wait4_argument_validation_follows_linux_precedence) retains all 852
    // prior identities.
    // Two smaps page-accounting controls (procfs::tests) retain all 853 prior
    // identities: 853 + 2 = 855.
    // Seven failed-syscall display controls retain all 855 prior identities:
    // 855 + 7 = 862.
    // Six failed-gettimeofday tv-store tests retain all 862 prior identities:
    // 862 + 6 = 868.
    // Two time-probe confirmation controls (the time(NULL) control probe and
    // the unchanged-stopped-word check) and two EFAULT failed-syscall display
    // controls retain all 868 prior identities: 868 + 4 = 872.
    // Two unreadable-stopped-word controls (the unmapped-word check and the
    // memory-map availability check) retain all 872 prior identities:
    // 872 + 2 = 874.
    // Two first-seen mtime tests (tool_global::tests::
    // only_exact_canonical_host_mtimes_are_kept and
    // first_seen_mtime_is_resolved_by_the_first_stat) retain all 874 prior
    // identities: 874 + 2 = 876.
    // Two bootstrap-turn scheduler-time tests (scheduler::test::
    // bootstrap_syscall_turn_withholds_scheduler_time_except_for_polling_retries
    // and io_buffers::event_tests::
    // bootstrap_syscall_marks_its_resource_requests_only_while_it_is_uncharged,
    // https://github.com/rrnewton/hermit/issues/3517) retain all 876 prior
    // identities: 876 + 2 = 878.
    // Three ephemeral host seed mount tests (procfs::tests::
    // seed_churn_does_not_change_guest_mountinfo_membership and
    // retained_row_with_seed_parent_still_snapshots, and the source guard
    // syscalls::files::procfs_wiring_guard::
    // snapshot_initializer_excludes_host_seed_mounts_at_both_captures,
    // https://github.com/rrnewton/hermit/pull/3219) retain all 878 prior
    // identities: 878 + 3 = 881 (`cargo nextest list --profile ci` measured 881).
    // The 23 directory-stream tests (eleven dirents::test, one
    // io_buffers::tests and eleven syscalls::files::test, among them
    // a_copy_error_other_than_a_fault_is_returned and
    // a_fault_of_the_whole_copy_copies_nothing;
    // https://github.com/rrnewton/hermit/pull/3226) retain all 881 prior
    // identities: 881 + 23 = 904 (`cargo nextest list --profile ci` measured 904).
    // The 27 serial KVM child-wait unit tests of
    // https://github.com/rrnewton/hermit/pull/3266 (11 scheduler::parked_tests
    // child-wait tests, 15 syscalls::threads::kvm_waitid::tests and
    // tool_local::timeslice_tests::
    // child_cpu_publication_replacement_rejects_stale_owner_and_snapshot)
    // retain all 904 prior identities: measured 931 = 904 + 27.
    // The sixteen syscalls::network_trace::tests of
    // https://github.com/rrnewton/hermit/pull/3778 (sockaddr_round_trips_both_families,
    // sockaddr_matches_the_libc_layout, other_families_and_short_buffers_are_not_traced,
    // lowat_normalisation_follows_linux,
    // poll_reports_requested_events_and_the_unmaskable_ones,
    // host_pulls_classify_like_the_recorder,
    // sockets_outside_a_channel_are_refused_only_where_they_reach_the_network,
    // a_channel_admits_only_descriptor_and_configuration_calls,
    // ipv6_addresses_without_a_scope_id_are_traced_as_linux_accepts_them,
    // sockaddr_copy_in_follows_linux, abstract_unix_addresses_are_recognised,
    // rights_are_found_in_every_control_message,
    // every_timeout_but_zero_sets_one, signal_driven_io_requests_are_recognised,
    // interface_and_route_ioctls_are_recognised and
    // host_network_state_paths_are_recognised) retain all 931 prior
    // identities (931 + 16 = 947, measured with cargo nextest list --profile ci).
    // scheduler::test::backoff_sleep_doubles_each_round_then_caps and
    // iovecs::tests::backend_user_address_limit_selects_the_policy retain all
    // 947 prior identities: 947 + 2 = 949, measured with cargo nextest list
    // --profile ci -p hermit-detcore --lib --bins.
    // tool_global::tests::files_on_different_devices_with_one_inode_number_stay_distinct
    // and syscalls::namespace::tests::anonymous_object_devices_are_the_pipe_and_socket_filesystems
    // retain all 949 (951, measured with cargo nextest list).
    // tool_global::tests::host_input_observations_are_kept_by_the_run_and_written_at_its_end
    // retains all 951 (952, measured with cargo nextest list).
    // detlog::tests::forwarded_line_names_the_emitting_module and
    // detlog::tests::forwarding_failures_become_loss_notices
    // (https://github.com/rrnewton/hermit/issues/3520, C5) retain all 952:
    // 952 + 2 = 954.
    // detlog::tests::forward_policy_answers_per_target_as_the_subscriber_does,
    // detlog::tests::forward_policy_most_specific_target_decides and
    // detlog::tests::forward_policy_encoding_round_trips_and_refuses_malformed_values
    // retain all 954 prior identities: 954 + 3 = 957.
    // syscalls::files::rebound_path_tests::an_operand_that_cannot_be_established_is_reported_as_the_root
    // retains all 957 (958, measured with cargo nextest list).
    // Four tool_global::tests mount-ID tests and two procfs::tests mountinfo
    // tests (mount IDs numbered by first observation, hermit 565ec411cd21)
    // retain all 958: 958 + 6 = 964, the count a full validate run measured
    // at a25b3fcd3805 ("Starting 964 tests across 2 binaries").
    // syscalls::network_trace::tests::select_sets_report_each_set_s_events_as_fs_select_c_does
    // retains all 964 (965).
    // detlog::tests::forwarded_records_are_written_at_points_the_schedule_fixes
    // and detlog::tests::split_forwarded_message_reads_the_sender_tag (967).
    // syscalls::files::untraced_code_tests' three tests retain all 967 (970).
    // logdiff::test::a_log_stopped_by_the_cap_is_refused_like_a_truncated_one
    // (https://github.com/rrnewton/hermit/pull/3686) retains all 970 prior
    // identities: 970 + 1 = 971, measured with cargo nextest list --profile ci
    // -p hermit-detcore --lib --bins.
    // scheduler::parked_tests::signal_control_is_installed_when_offered_in_a_sequentialized_run,
    // scheduler::parked_tests::signal_control_cannot_be_installed_after_the_run_started,
    // scheduler::parked_tests::backends_without_signal_control_never_enter_the_controlled_loop
    // and scheduler::parked_tests::real_timers_follow_the_installed_control
    // retain all 971: 971 + 4 = 975, measured with cargo nextest list
    // --profile ci -p hermit-detcore --lib --bins.
    // procfs::tests::proc_modules_does_not_depend_on_the_host_module_set
    // (https://github.com/rrnewton/hermit/issues/3815) retains all 975 (976,
    // measured with cargo nextest list).
    // Eleven C3.5 exit-hold tests (scheduler and tool_global) retain all 976: 987.
    // Six exit_dependencies tests (rulings A and D.1) retain all 987: 993.
    // The completion-before-registration test (C3.5 fix-forward) retains all 993: 994.
    // io_buffers::event_tests::rng_readv_event_digest_reads_exactly_when_the_wide_read_fails
    // (https://github.com/rrnewton/hermit/issues/3823) retains all 994 (995,
    // measured with cargo nextest list --profile ci).
    // io_buffers::event_tests::rng_readv_event_digest_reads_words_when_wide_and_exact_reads_fail,
    // io_buffers::tests::read_written_reads_every_extent_of_a_write_only_mapping
    // and io_buffers::tests::read_written_fails_with_the_wide_error_when_no_method_can_read
    // (write-only destinations, https://github.com/rrnewton/hermit/issues/3823)
    // retain all 995 (998, measured with cargo nextest list --profile ci).
    // io_buffers::tests::poll_family_extents_use_the_low_32_bits_of_nfds
    // retains all 998 (999, measured with cargo nextest list --profile ci).
    // tool_global::tests::a_host_dependent_new_inode_does_not_renumber_later_files
    // (https://github.com/rrnewton/hermit/issues/2897) retains all 999 (1000,
    // measured with cargo nextest list --profile ci).
    // tool_global::tests::an_mtime_update_does_not_number_files (the same
    // issue) retains all 1000 (1001, measured with cargo nextest list --profile ci).
    // The descriptor-alias interning test retains all 998: 999, re-listed.
    // Both 999 counts above, re-listed together: 1000.
    // With the two inode tests above, the descriptor-alias test retains all 1001: 1002.
    // https://github.com/rrnewton/hermit/pull/3361
    // (https://github.com/rrnewton/hermit/issues/3146): 66 blocked-wait and
    // host-timed signal tests, with none removed, retain all 975 prior
    // identities (1041, measured with cargo nextest list --profile ci):
    // scheduler::test, 26 tests:
    //   a_child_exit_sigchld_goes_to_the_first_thread_that_does_not_block_it,
    //   a_child_exit_timer_commits_a_deferred_creator_delivery_without_sending,
    //   a_child_exit_timer_names_the_thread_that_created_the_child,
    //   a_child_exit_wakes_no_parked_futex_waiter,
    //   a_default_job_control_stop_leaves_a_rearming_futex_wait_parked,
    //   a_host_timed_signal_never_ends_a_parked_futex_wait_of_its_process,
    //   a_launch_that_may_pass_a_terminal_holds_the_terminal_signals_from_the_start,
    //   a_parked_futex_waiter_is_admitted_on_its_mask_alone,
    //   a_scheduler_that_does_not_model_signal_targets_reads_and_records_no_sleeping_mask,
    //   a_sigchld_never_ends_a_parked_futex_wait,
    //   a_sigchld_target_sleeping_outside_the_scheduler_uses_the_mask_recorded_at_its_commit,
    //   a_signal_caught_after_a_futex_wait_began_ends_it,
    //   a_signal_ignored_after_a_futex_wait_began_keeps_its_deadline,
    //   a_signal_that_cannot_wake_a_background_call_leaves_it_in_its_pool,
    //   a_signal_to_a_thread_sleeping_outside_the_scheduler_is_requeued_after_its_own_stop,
    //   a_signal_to_an_unposted_thread_outside_the_background_pools_still_asserts,
    //   a_signal_woken_futex_waiter_runs_before_an_already_runnable_thread,
    //   an_expired_release_barrier_keeps_the_arm_and_refuses_the_run,
    //   an_unplaced_sigchld_delivery_is_deferred_behind_a_runnable_sibling,
    //   an_unreadable_signal_state_ends_a_parked_futex_wait,
    //   non_interrupting_signal_leaves_a_parked_futex_waiter_parked,
    //   pending_signal_does_not_rewrite_a_woken_futex_waiter,
    //   pending_signal_interrupts_a_parked_futex_waiter,
    //   signaled_background_threads_are_released_together_in_thread_id_order,
    //   the_signaled_background_barrier_runs_before_any_timer_pop,
    //   unreported_signal_state_keeps_futex_waiter_parked_for_cross_task_signals;
    // syscalls::files::test, four tests:
    //   a_hangup_after_the_open_does_not_hide_the_acquired_terminal,
    //   an_open_acquires_a_controlling_terminal_only_as_a_session_leader,
    //   async_io_arming_calls_name_their_host_timed_signals,
    //   terminal_control_calls_name_the_terminal_signals;
    // syscalls::helpers::kernel_signal_wait_failures, 29 tests:
    //   a_consumed_sigchld_that_stops_an_injection_is_refused,
    //   a_default_ignored_signal_that_stops_an_injection_is_held,
    //   a_held_sigchld_is_never_replaced_by_another_stop,
    //   a_held_signal_refusal_follows_the_unsupported_operation_policy,
    //   a_mask_change_that_cannot_run_ends_the_run_instead_of_resuming_the_guest,
    //   a_mask_that_signals_keep_stopping_ends_the_run_instead_of_resuming_the_guest,
    //   a_pending_caught_sigchld_ends_a_select_wait_before_the_injection,
    //   a_pending_caught_sigchld_ends_a_select_wait_but_no_gated_wait,
    //   a_pending_caught_signal_ends_the_wait_before_the_injection,
    //   a_restoration_refusal_replaces_the_result_unless_the_wait_refused_first,
    //   a_second_unidentified_stop_is_refused,
    //   a_stack_with_no_scratch_room_leaves_the_wait_under_the_guest_mask,
    //   a_stop_by_the_preemption_signal_still_restarts_a_wait_without_a_deadline,
    //   a_stop_is_identified_only_by_exactly_its_signal,
    //   a_stop_of_the_mask_restoration_that_replaces_a_held_sigchld_is_refused,
    //   a_stop_that_cannot_be_identified_keeps_a_timed_wait_and_its_deadline,
    //   a_thread_that_no_longer_exists_ends_its_wait_with_erestartnointr,
    //   a_timed_wait_ends_for_a_held_signal_that_would_end_it,
    //   a_timed_wait_refuses_to_let_another_stop_replace_a_held_sigchld,
    //   a_wait_that_reaches_the_stop_bound_holding_a_sigchld_is_refused,
    //   a_wait_without_a_deadline_ends_for_a_held_signal_that_would_end_it,
    //   an_identified_stop_of_the_mask_restoration_that_replaces_a_held_sigchld_is_refused,
    //   an_ignored_signal_that_stops_the_mask_change_does_not_end_the_wait,
    //   an_unidentified_stop_at_the_stop_bound_is_refused,
    //   an_unidentified_stop_over_a_held_sigchld_is_refused,
    //   an_unreadable_signal_state_of_a_live_thread_ends_the_run,
    //   next_dequeued_follows_the_kernel_order,
    //   only_a_wait_without_a_deadline_ends_after_the_absorbed_stop_bound,
    //   restore_repeats_the_mask_change_until_the_kernel_reports_it;
    // syscalls::helpers::tests, three tests:
    //   a_held_signal_is_reported_unless_it_is_sigstop,
    //   a_restart_block_resumes_only_at_the_interrupted_call,
    //   kernel_restart_errno_matches_linux_restart_policy;
    // syscalls::threads::tests, four tests:
    //   a_child_exit_signal_other_than_sigchld_is_host_timed_for_its_creator,
    //   a_rearming_wait_is_not_ended_by_a_default_job_control_stop,
    //   kernel_signal_state_parses_proc_status,
    //   only_unblocked_caught_or_fatal_signals_interrupt_a_wait;
    // The 1002 pinned before https://github.com/rrnewton/hermit/pull/3361 and its 66 tests above,
    // re-listed together: 1068 (measured with cargo nextest list --profile ci).
    // Seven SIGCHLD Phase A scheduler tests
    // (sigchld_notification_classification_follows_do_notify_parent,
    // proc_status_signal_masks_parse_as_hex,
    // child_exit_notification_is_classified_only_when_every_input_is_known,
    // child_exit_notification_reads_the_saved_guest_mask,
    // ignored_child_exit_holds_turns_instead_of_arming_the_timer,
    // a_publication_reported_before_the_grant_installs_no_hold and
    // child_exit_notification_is_classified_only_in_covered_parent_states)
    // retain all 1068: 1075, measured with cargo nextest list --profile ci.
    // an_exit_that_names_another_process_installs_no_hold retains all 1075:
    // 1076, measured with cargo nextest list --profile ci.
    // The four recvmsg unobserved-digest tests (header, short iovec, unreadable buffer, other calls still fail) retain all 1076: 1080, re-listed.
    // The two unobserved-record logdiff tests (anchored counting, ordinary --verify compares it) retain all 1080: 1082, re-listed.
    // tool_global::tests::backend_exit_reports_take_the_path_the_backend_capabilities_select
    // retains all 1082: 1083, measured with cargo nextest list --profile ci.
    // syscalls::time::tests::failed_gettimeofday_repair_is_skipped_only_for_kvm
    // is deleted with the capability it tested, which reverie-kvm's time(2)
    // and Guest::storable_memory_ranges replace: 1083 - 1 = 1082, measured with
    // cargo nextest list --profile ci -p hermit-detcore --lib --bins.
    // io_buffers::event_tests::procfs_reads_refuse_an_impossible_count_before_asking_the_user_address_limit
    // retains all 1082: 1082 + 1 = 1083, measured with cargo nextest list
    // --profile ci -p hermit-detcore --lib --bins. Two iovecs::tests are
    // renamed in the same change (backend_user_address_limit_selects_the_policy
    // -> guest_reported_user_address_limit_is_the_one_enforced and
    // rng_iovecs_native_oracle_failures_keep_their_tool_identity ->
    // rng_iovecs_limit_query_failures_keep_their_tool_identity), count unchanged.
    // Five open file description tests in detcore fd.rs (shared-OFD step B2:
    // four on shared handles, one on argument conversion order), re-listed: 1088.
    // syscalls::helpers::kernel_signal_wait_failures::a_sigchld_after_an_unidentified_stop_is_refused_before_it_replaces_it
    // retains all 1088: 1088 + 1 = 1089, measured with cargo nextest list
    // --profile ci -p hermit-detcore --lib --bins.
    // The non-modelling-scheduler background-signal test (P0 flake fix), re-listed: 1090.
    // Seven shared open file tests (shared-OFD step B5a: five in shared_open_files.rs, two in tool_global.rs), re-listed: 1096.
    // tool_global::tests::{a_host_reusing_a_freed_inode_does_not_renumber_the_new_file,
    // an_unlinked_file_keeps_its_number_through_its_descriptors,
    // retiring_and_forgetting_consume_no_counter_values}
    // (https://github.com/rrnewton/hermit/issues/3840) retain all 1097 (1100,
    // measured with cargo nextest list).
    // tool_global::tests::{a_pending_mtime_stays_with_its_file_after_the_last_name_goes,
    // a_stale_directory_entry_does_not_renumber_a_held_file} (the reviews of
    // https://github.com/rrnewton/hermit/pull/3849) retain all 1100 (1102,
    // measured with cargo nextest list).
    // tool_global::tests::a_stale_directory_entry_keeps_its_file_after_the_inode_is_reused
    // (codex re-review of https://github.com/rrnewton/hermit/pull/3849)
    // retains all 1102: 1103.
    // tool_global::tests::a_write_after_the_last_name_reaches_the_snapshot_record
    // (claude re-check of https://github.com/rrnewton/hermit/pull/3849) retains
    // all 1103: 1104.
    // tool_global::tests::a_snapshot_lookup_does_not_scan_the_inodes_history
    // (codex re-check of https://github.com/rrnewton/hermit/pull/3849) retains
    // all 1104: 1105.
    // Thirteen SIGALRM ledger tests in scheduler::parked_tests (signal phase 1, step
    // I1a), re-listed: 1110.
    // Five SIGALRM ledger control tests (three in scheduler::parked_tests, two
    // in tool_global; signal phase 1 step I1b), re-listed: 1123.
    // tool_local::timeslice_tests::instruction_trap_* (four tests, lane
    // qemu-rcb, https://github.com/rrnewton/hermit/pull/3859) retain all 1123:
    // 1127.
    // Eleven fatal_core::tests (core layout, zstd frames, caps, deadline,
    // dumping-thread and unreadable-memory cases): 1138.
    // Four more fatal_core::tests (dump rule, lock-wait deadline, repeated
    // process ID, Full-tier timeout): 1142.
    // Seven signal phase 1 table tests (five in sigalrm_phase1, two in
    // tool_global; step I1b part 2), re-listed: 1134.
    // procfs::tests::self_sched_hides_schedstats_fields retains all 1149:
    // 1150, listed with this node's arguments.
    // scheduler::test::a_signal_sent_at_an_rt_sigsuspend_request_arms_the_call_it_wakes
    // retains all 1150: 1151, listed with this node's arguments.
    // Four fatal_core::tests (orphaned temp files, the helper-thread wait, a
    // panicking helper, an abandoned capture): 1151 + 4 = 1155.
    // syscalls::signal::rt_sigsuspend_tests::rt_sigsuspend_sleeps_under_the_mask_detcore_read
    // retains all 1151: 1152, listed with this node's arguments.
    // Six detlog forwarding tests (the full-socket-buffer receiver, per-thread
    // counts, the completeness rule, count-slot churn, released-slot reuse,
    // the required forwarder; SaBRe tool-output socket) retain all 1155: 1161,
    // listed with this node's arguments.
    // scheduler::parked_tests::the_runtimes_sigalrm_publications_and_questions
    // and sigalrm_phase1::tests::a_signalfd_in_the_kernels_table_is_found
    // (signal phase 1 step I3) retain all 1150: 1152, re-listed.
    // scheduler::test::empty_queue_step_readmits_a_parked_thread_whose_gate_opened,
    // empty_queue_step_reports_a_parked_thread_whose_gate_cannot_open and
    // signal_to_a_thread_held_at_a_gate_leaves_it_held
    // (https://github.com/rrnewton/hermit/issues/3149) retain all 1164: 1167.
    // syscalls::signal::rt_sigsuspend_tests::rt_sigsuspend_scratch_never_lands_on_the_callers_mask
    // retains all 1156: 1157, listed with this node's arguments.
    // The two effective exit-signal tests of
    // https://github.com/rrnewton/hermit/issues/3895 retain all 1168: 1170,
    // listed with this node's arguments.
    // scheduler::test::a_guest_signal_to_an_rt_sigsuspend_sleeper_arms_the_release_barrier
    // retains all 1168: 1169, listed with this node's arguments.
    // util::find_in_directory_tests::find_in_directory_visits_every_name_and_stops_at_the_first_find
    // retains all 1170: 1171, listed with this node's arguments.
    // scheduler::parked_tests::a_due_sigalrm_entry_is_taken_once_and_only_when_due
    // and completed_syscall_projection_tests::a_completed_syscall_shows_its_result_and_return_address
    // (signal phase 1 step I4) retain all 1171: 1173, listed with this node's
    // arguments.
    // The nine scheduler::child_exit_sigchld tests and
    // scheduler::test::a_child_exit_timer_sends_nothing_after_the_kernels_copy
    // (one SIGCHLD per child exit) retain all 1174: 1184, run with this node's
    // arguments.
    // scheduler::test::a_refused_turn_charge_returns_before_an_ordinary_grant and
    // scheduler::parked_tests::a_refused_turn_charge_returns_before_a_controlled_observation
    // (lane qemu-rcb, https://github.com/rrnewton/hermit/pull/3885) retain all
    // 1184: 1186.
    // scheduler::test::checkpoint_fires_count_and_occurrence_anchors_reached_at_one_syscall
    // and unfired_occurrence_anchor_is_reported_by_name retain all 1186: 1188.
    // Eight scheduler rejoin tests, two rejoin_log tests and four
    // syscalls::accept_in_turn tests
    // (https://github.com/rrnewton/hermit/pull/3908) retain all 1188: 1202,
    // listed with this node's arguments.
    // in_guest_site_patching::tests::only_zero_satisfies_a_request_for_site_patching_off
    // (the in-guest-trap Tool image check) retains all 1186: 1187, listed
    // with this node's arguments.
    // The rt_sigsuspend-mask subset of
    // https://github.com/rrnewton/hermit/pull/3224 retains all 1203 and adds
    // ten tests: 1213, listed with this node's arguments.
    // scheduler::test, nine tests:
    //   a_sibling_signal_sent_at_an_rt_sigsuspend_request_arms_the_call_it_wakes,
    //   a_sibling_signal_sent_before_the_rt_sigsuspend_grant_requeues_the_waiter_first,
    //   child_exit_signal_does_not_replace_a_posted_vfork_continuation,
    //   child_exit_signal_leaves_vfork_parent_waiting_for_its_continuation,
    //   rt_sigsuspend_mask_uses_kernel_sigset_bits_and_cannot_block_kill_or_stop,
    //   signal_blocked_by_the_rt_sigsuspend_mask_leaves_the_wait_in_its_pool,
    //   signal_leaves_blocking_external_io_waiting_for_its_own_report,
    //   signal_leaves_rt_sigsuspend_in_its_pool_on_a_backend_without_a_guaranteed_report,
    //   signal_requeues_rt_sigsuspend_without_counterfeiting_its_report;
    // syscalls::signal::tests, one test:
    //   the_installed_mask_cannot_block_kill_or_stop.
    // syscalls::namespace::tests::a_failed_device_probe_is_kept_without_the_c_library_message
    // (in-guest LiteInst start-up out of the guest's heap) retains all 1213:
    // 1214, listed with this node's arguments.
    // scheduler::test::a_futex_wake_takes_the_longest_waiter_first (lane
    // qemu-rcb, https://github.com/rrnewton/hermit/issues/3917) retains all
    // 1214: 1215.
    // scheduler::test::{a_futex_wake_of_several_takes_the_longest_waiters_in_order,
    // a_partial_bitset_wake_keeps_the_other_waiters_in_arrival_order,
    // exit_wakes_take_the_longest_waiter_first} (the same change) retain all
    // 1215: 1218.
    // scheduler::test::a_futex_waker_is_requeued_behind_the_waiters_it_woke
    // (lane qemu-rcb, https://github.com/rrnewton/hermit/issues/3874) retains
    // all 1218: 1219.
    // procfs_inode::tests::{a_procfs_entry_names_its_path_whatever_its_host_inode,
    // a_path_names_its_process_and_thread,
    // a_path_within_procfs_does_not_depend_on_the_mount,
    // dot_entries_name_the_directory_and_its_parent,
    // mountinfo_names_procfs_mounts_by_type,
    // only_anonymous_devices_with_1024_byte_blocks_may_be_procfs,
    // other_directories_keep_host_keys, path_keys_cannot_equal_procfs_inodes,
    // procfs_entries_are_named_only_where_thread_ids_are_host_ids,
    // this_process_reads_its_own_procfs_entries} and tool_global::tests::{
    // a_numbered_host_inode_keeps_its_number_when_its_path_is_named,
    // a_snapshot_entry_names_the_task_that_held_its_id,
    // a_rebuilt_procfs_entry_keeps_its_number,
    // a_reused_task_id_names_new_procfs_entries, procfs_aliases_share_one_number,
    // a_write_to_a_rebuilt_procfs_entry_sets_the_mtime_of_its_number}
    // (rebuilt procfs entries keep their numbers) retain all 1219: 1235.
    // procfs_inode::tests::{an_entry_of_a_task_that_has_exited_is_not_named,
    // a_path_linux_marks_deleted_names_nothing,
    // a_procfs_object_is_told_by_its_filesystem_type} (entries of exited tasks
    // and procfs files outside the guest's mounts) retain all 1235: 1238.
    // procfs_inode::tests::a_failed_host_call_leaves_the_callers_errno (the
    // guest's errno under SaBRe) retains all 1238: 1239.
    // procfs_inode::tests::a_mount_table_read_whose_close_is_refused_is_an_error
    // (a mount table read under a filter that refuses close) retains all
    // 1239: 1240.
    // procfs_inode::tests::{mount_points_are_read_in_the_frame_of_the_guests_root,
    // a_stale_table_names_nothing_rather_than_another_entry} (mount points read
    // in the frame of the guest's root, matched by mount ID) retain all 1240:
    // 1242.
    // procfs_inode::tests::{a_mount_moved_by_a_rename_is_found_where_it_moved,
    // a_mount_point_read_in_another_frame_names_nothing,
    // an_open_returning_its_own_number_ran_only_if_it_took_that_descriptor,
    // only_a_close_returning_0_ran, a_process_holds_the_descriptors_it_has_open}
    // (a mount point is checked where the reader finds it, and an injected
    // open or close is accepted only when it ran) retain all 1242: 1247.
    // procfs_inode::tests::a_process_holds_the_descriptors_it_has_open is
    // removed with the guess it tested, now that an injected open a signal
    // stopped reports a restart, and
    // an_open_returning_its_own_number_ran_only_if_it_took_that_descriptor
    // becomes an_open_ran_unless_it_reports_a_restart: 1246.
    // procfs_inode::tests::{a_mount_table_read_that_is_always_interrupted_is_an_error,
    // a_mount_table_open_that_is_always_interrupted_is_an_error,
    // an_interrupted_call_is_made_again_a_bounded_number_of_times,
    // a_file_longer_than_one_read_is_read_whole} (the mount table's open and
    // reads are tried again a bounded number of times) retain all 1246: 1250.
    // The poll-deadline wake (https://github.com/rrnewton/hermit/issues/3952)
    // adds eleven, retaining all 1250: 1261. scheduler::runqueue::tests::
    // {restore_poller_priority_moves_one_poller_to_the_back_of_its_level,
    // only_first_entry_heuristics_are_priority_ordered,
    // a_deadline_restored_poller_counts_as_a_poller_until_it_runs,
    // a_poll_upgrade_ends_a_restored_pollers_poller_status} and
    // scheduler::parked_tests::{a_due_poll_deadline_restores_the_backed_off_poller_before_selection,
    // stale_poll_deadlines_are_dropped_and_suspended_ones_kept,
    // poll_deadlines_are_recorded_only_where_they_can_matter,
    // the_controlled_loop_restores_a_due_poller_in_maintenance,
    // the_controlled_loop_restores_a_poller_whose_deadline_a_refresh_crosses,
    // a_deadline_restored_poller_gates_a_held_sigchld_as_while_backed_off,
    // a_deadline_restored_poller_gates_a_finished_background_call_as_while_backed_off}.
    // The cross-task wake of an emulated pause or nanosleep
    // (https://github.com/rrnewton/hermit/issues/3982) adds eleven, retaining
    // all 1230, listed with this node's arguments. scheduler::test::
    // {a_caught_or_fatal_cross_task_signal_ends_a_parked_pause,
    // a_cross_task_signal_ends_a_pause_still_on_the_run_queue,
    // a_blocked_or_ignored_cross_task_signal_leaves_a_pause_parked,
    // a_cross_task_signal_withdrawn_before_the_drain_leaves_the_sleep_parked,
    // a_signal_ignored_when_sent_never_ends_the_sleep,
    // a_held_signal_never_ends_the_sleep,
    // an_unreadable_sleeper_state_never_wakes_the_sleep,
    // a_default_stop_ends_a_pause_but_not_a_nanosleep,
    // a_cross_task_signal_after_a_nanosleep_deadline_leaves_it_to_complete,
    // sleeps_that_cross_task_signals_do_not_reach}, and
    // syscalls::threads::tests::a_thread_reported_under_another_pid_is_read_and_alive:
    // 1241.
    // The scalar SIGALRM admission/neighbor regression retains all 1272:
    // 1273, measured with this node's Nextest listing.
    // procfs::tests::{cgroups_hides_the_host_cgroup_counts,
    // cgroups_leaves_unknown_formats_untouched} (lane qemu-rcb,
    // https://github.com/rrnewton/hermit/issues/3987) retain all 1273: 1275.
    // a_killed_vfork_parent_takes_its_barrier_with_it,
    // a_killed_vfork_parent_with_an_unregistered_child_leaves_a_tombstone,
    // a_killed_vfork_parents_barrier_lasts_until_its_childs_release_edge,
    // a_vfork_childs_exit_grant_releases_its_dead_parents_barrier and
    // a_new_vfork_by_a_reused_tid_clears_a_stale_tombstone,
    // a_vfork_parent_retired_after_its_childs_exec_releases_the_barrier,
    // a_released_vfork_barrier_ends_at_the_parents_continuation and
    // a_new_vfork_by_a_reused_tid_replaces_a_kept_barrier
    // (https://github.com/rrnewton/hermit/issues/3984) retain all 1275: 1283.
    // Ten SIGALRM sleep boundary/resource tests retain all 1283: 1293.
    // scheduler::test::{a_hold_is_charged_from_its_first_park_and_refused_past_its_budget,
    // a_released_hold_and_a_version_1_hold_are_never_refused,
    // a_non_leader_exec_on_a_relative_anchors_thread_is_refused,
    // an_unfired_relative_anchor_names_its_base,
    // exec_reconnect_refuses_a_relative_anchor_only_across_a_non_leader_exec,
    // the_identity_refusal_grants_no_further_turn,
    // a_removed_held_thread_leaves_no_hold_behind,
    // an_opened_gate_is_not_refused_before_its_thread_runs_again} (relative
    // happens-before anchors) retain all 1293: 1301.
    // scheduler::test::{a_pending_vfork_barrier_past_its_valve_refuses_the_run_and_stays,
    // a_released_vfork_barrier_past_its_valve_refuses_the_run_and_stays,
    // a_vfork_barrier_event_inside_the_valve_proceeds_and_stops_the_clock}
    // (https://github.com/rrnewton/hermit/issues/3999) retain all 1301: 1304.
    // scheduler::tests::{a_futex_wake_count_of_zero_or_below_wakes_one_waiter,
    // a_futex_wake_bitset_count_of_zero_wakes_the_first_matching_waiter}
    // (https://github.com/rrnewton/hermit/issues/3957) retain all 1304: 1306.
    // The fourteen child-subreaper re-parenting scheduler tests and the
    // subreaper prctl backend and mode gate
    // (https://github.com/rrnewton/hermit/issues/3997) retain all 1306: 1321.
    // scheduler::parked_tests::{an_expired_vfork_barrier_ends_the_run_through_the_daemon_loop,
    // the_vfork_barrier_valve_is_thirty_seconds} (the valve's consumer and
    // bound) retain all 1321: 1323.
    // And a_background_call_with_an_unknown_sleeping_mask_is_never_armed (review of https://github.com/rrnewton/hermit/pull/3989): 1324.
    // scheduler::test::{a_child_wait_settles_only_on_children_held_or_in_futex_waits,
    // a_chain_of_child_waits_down_to_a_held_child_settles,
    // a_process_armed_with_a_parent_death_sigkill_is_not_stuck} and
    // tool_global::test::sigkill_notifications_record_the_doomed_process
    // (https://github.com/rrnewton/hermit/issues/3904) retain all 1324: 1328.
    // And a_committed_sigchld_ends_a_gated_wait_unless_a_host_timed_source_can_post_it and a_precise_futex_wait_holds_a_committed_signal_only_if_a_host_timed_source_can_post_it (https://github.com/rrnewton/hermit/issues/4005): 1330.
    // a_posthook_checkpoint_fires_only_on_a_final_result,
    // io_buffers::event_tests::a_posthook_anchor_fires_on_an_observer_errno_like_a_success
    // and scheduler::test::{an_interrupted_posthook_anchor_is_refused_and_does_not_fire,
    // an_interrupted_posthook_refusal_grants_no_turn}
    // (https://github.com/rrnewton/hermit/issues/3929) retain all 1330: 1334.
    // scheduler::tests::{a_futex_requeue_wakes_the_oldest_and_moves_the_next_in_order,
    // futex_requeue_counts_follow_linux,
    // a_futex_requeue_onto_itself_keeps_waiters_in_place},
    // syscalls::threads::tests::{futex_commands_are_classified_as_linux_dispatches_them,
    // futex_wake_op_decodes_and_applies_as_linux} and
    // memory::tests::a_dontunmap_remap_keeps_both_aliases_of_the_object
    // (https://github.com/rrnewton/hermit/pull/4020) retain all 1334: 1340,
    // listed with `cargo nextest list -p hermit-detcore --lib --bins`.
    // syscalls::threads::tests::an_interrupted_shared_key_probe_is_never_retried_or_taken_as_an_answer
    // (https://github.com/rrnewton/hermit/pull/4020) retains all 1340: 1341.
    // scheduler::tests::a_terminal_deadlock_report_reaches_the_installed_hook_until_it_is_removed
    // (run_failed with deadlock) retains all 1341: 1342.
    // tool_local::file_metadata_tests::rediscovery_preserves_known_and_invalidated_flock_history
    // retains all 1342 prior cases: 1343.
    // And a_background_call_with_a_temporary_mask_holds_the_schedule_until_the_kernel_installs_it (https://github.com/rrnewton/hermit/issues/3993): 1344.
    // And a_live_thread_whose_mask_cannot_be_read_holds_its_background_entry,
    // an_unreadable_own_mask_at_the_grant_still_arms_the_entry_barrier and
    // an_unproved_background_entry_ends_the_run_by_name_after_the_valve
    // (Codex review of https://github.com/rrnewton/hermit/pull/4053): 1347.
    // syscalls::threads::tests::futex_timeout_clamps_a_huge_valid_timespec_like_ktime
    // (Codex re-check of https://github.com/rrnewton/hermit/pull/4061) retains
    // all 1347: 1348.
    // tool_global::tests::only_hermits_own_thread_group_is_hermits
    // (https://github.com/rrnewton/hermit/issues/4066) retains all 1348: 1349.
    // The eleven real-time expiration fate tests of scheduler::tests
    // (a_real_time_expiration_every_thread_blocks_is_kept_pending,
    // a_real_time_expiration_one_thread_unblocks_is_deliverable,
    // a_real_time_expiration_is_judged_by_a_held_threads_own_mask,
    // a_real_time_expiration_with_a_thread_outside_the_schedule_is_deliverable,
    // a_real_time_expiration_with_an_unreadable_thread_is_deliverable,
    // an_ignored_real_time_expiration_is_discarded_where_a_thread_takes_it,
    // a_real_time_expiration_a_signalfd_or_a_handler_return_could_see_is_deliverable,
    // a_real_time_expiration_after_a_call_outside_the_schedule_is_deliverable,
    // a_real_time_expiration_of_a_process_without_live_threads_is_discarded,
    // without_signal_target_modelling_the_arming_thread_decides_as_before and
    // a_pending_real_time_expiration_is_discarded_by_ignore_and_by_exit;
    // https://github.com/rrnewton/hermit/issues/4089) retain all 1349: 1360.
    // The chaos yield-epoch tests (scheduler::runqueue::tests, scheduler::test,
    // syscalls::threads::tests; https://github.com/rrnewton/hermit/issues/4068)
    // retain all 1360: 1384.
    // And scheduler::runqueue::tests::threads_queued_by_a_yield_are_not_work_a_background_call_waits_behind
    // (https://github.com/rrnewton/hermit/issues/4092): 1385.
    // The 29 SIGKILL-retirement, signalfd-reconciliation and iovec tests (https://github.com/rrnewton/hermit/issues/3994) retain all 1385: 1414.
    // scheduler::test::a_sigkill_arm_on_an_rt_sigsuspend_sleeper_ends_with_its_exit
    // (https://github.com/rrnewton/hermit/issues/3941): 1415.
    // The 14 own-process signal and orphaned-group tests (https://github.com/rrnewton/hermit/issues/4034) retain all 1415: 1429.
    ("test.detcore_unit", 1429),
    // tight_stack_openat::openat_succeeds_without_writable_stack_below_rsp
    // retains all 27 prior selected identities under the unchanged skip filters
    // (measured 28; https://github.com/rrnewton/hermit/issues/3328).
    // The wait4 argument-error regression
    // (wait4_argument_errors_match_linux_and_preserve_children) retains all 28
    // prior identities selected after the node's five named skips.
    // The 32 readdir_order tests
    // (https://github.com/rrnewton/hermit/pull/3226) retain all 29 prior
    // identities selected after the node's five named skips (measured 61).
    // The four procfs_caller_buffer tests
    // (https://github.com/rrnewton/hermit/issues/3815) retain all 61 prior
    // identities selected after the node's five named skips (measured 65).
    // Seven more procfs_caller_buffer tests (destination protection, partial
    // copies, the failed-read offset and the self-maps capture) retain all 65
    // (measured 72).
    // Three more (a first self-maps read on a tight stack, its destination
    // checks, and the user-address-limit check at end of file) retain all 72
    // (measured 75).
    // One more (a 1-, 4- and 8-byte self-maps read on a tight stack before an
    // unmapped page) retains all 75 (measured 76).
    // random_read_before_an_unmapped_page (1-, 4- and 8-byte random reads
    // before an unmapped page; https://github.com/rrnewton/hermit/issues/3823)
    // retains all 76 (measured 77).
    // pipe_read_into_a_write_only_page (the same issue, for a non-random read)
    // retains all 77 (measured 78).
    // readdir_order's host_dependent_inode_sighting_does_not_renumber_a_later_listing
    // (https://github.com/rrnewton/hermit/issues/2897) retains all 78
    // (measured 79).
    // readdir_order's mapped_hard_link_does_not_renumber_a_later_listing (the
    // same issue) retains all 79 (measured 80).
    // readdir_order's stdin_alias_does_not_renumber_a_later_listing (the same
    // issue) retains all 80 (measured 81).
    // readdir_order's mapped_stdin_does_not_renumber_a_later_listing,
    // mapped_stdin_reports_the_inode_fdinfo_reports and
    // another_process_stdin_pipe_reads_as_the_readers_own (stdio identities
    // resolve to fd 0's inode) retain all 81 (measured 84).
    // readdir_order's stdio_empty_path_stat_reports_the_inode_fdinfo_reports
    // (https://github.com/rrnewton/hermit/issues/3855) retains all 84
    // (measured 85).
    // inode_reuse::{removing_a_last_name_retires_its_number_and_nothing_else,
    // a_reused_inode_gets_a_fresh_number_however_it_is_first_reached}
    // (https://github.com/rrnewton/hermit/issues/3840) retain all 85 (measured
    // 87).
    // inode_reuse::nothing_is_retired_without_sequentialized_threads (the same
    // reviews) retains all 87 (measured 88).
    // a_futex_wake_reaches_the_longest_waiter_while_another_waits_again (lane
    // qemu-rcb, https://github.com/rrnewton/hermit/issues/3917) retains all 88:
    // 89.
    // procfs_inode::{a_rebuilt_proc_fd_entry_keeps_its_number,
    // a_rebuilt_entry_reached_through_self_keeps_its_number,
    // a_procfs_entry_reached_by_two_paths_keeps_one_number,
    // an_entry_detcore_cannot_name_records_a_determinism_loss,
    // an_overwritten_path_does_not_name_the_entry_it_now_leads_to,
    // the_proc_root_link_count_does_not_count_host_processes,
    // the_proc_root_link_count_through_the_cwd_does_not_count_host_processes,
    // a_write_to_a_procfs_file_moves_its_mtime} retain all 89 (measured 97).
    // procfs_inode::{a_listing_keeps_its_keys_after_its_task_exits,
    // a_close_that_never_runs_ends_in_a_determinism_loss,
    // a_close_that_a_filter_refuses_ends_in_a_determinism_loss}
    // (keys decided with the snapshot; the O_PATH descriptor is closed or the
    // run records a loss) retain all 97 (measured 100).
    // procfs_inode::naming_an_entry_leaves_the_guests_errno (the guest's errno
    // under SaBRe) retains all 100 (measured 101).
    // procfs_inode::a_refused_close_ends_in_a_determinism_loss_whatever_a_lookup_says
    // (a refused close is a loss whatever a later look finds) retains all 101
    // (measured 102).
    // procfs_inode::a_descriptor_numbered_as_openat_is_detcores_when_it_was_free
    // (an O_PATH descriptor numbered as openat's own system call ran) retains
    // all 102 (measured 103).
    // procfs_inode::without_sequentialized_threads_a_path_is_not_opened_again
    // (no path is opened again while the guest's threads are not
    // sequentialized) retains all 103 (measured 104).
    // procfs_inode::{without_sequentialized_threads_a_descriptor_is_not_named,
    // without_sequentialized_threads_the_working_directory_is_not_named} (no
    // procfs entry is named while the guest's threads are not sequentialized,
    // whatever names it) retain all 104 (measured 106).
    // readdir_order's twelve procfs descriptor-entry identity tests (the
    // review cases of https://github.com/rrnewton/hermit/pull/3972): 118.
    // subreaper's eleven child-subreaper guest tests
    // (https://github.com/rrnewton/hermit/issues/3997): 129.
    // raw_clone's five raw-clone process tests
    // (https://github.com/rrnewton/hermit/issues/4012) retain all 129: 134.
    ("test.detcore_misc", 134),
    ("test.detcore_parallel", 5),
    // The previously unenrolled tests_time target contributes all 28 measured IDs.
    // Two seccomp-EFAULT failed-gettimeofday regressions retain all 28 prior
    // identities: 28 + 2 = 30.
    // The unreadable-mapped-tv seccomp regression retains all 30 prior
    // identities: 30 + 1 = 31.
    // The 29 cases that enable max_timeslice need a PMU and move to
    // privileged-test.pmu_detcore_time_cases, because the hosted runner has none
    // (https://github.com/rrnewton/hermit/issues/3663). The two that disable it
    // stay: 31 - 29 = 2, and 2 + 29 = 31 still run under the full label.
    ("test.detcore_time", 2),
    // 402ba973 adds two clock_determinism tests, retaining all 158 prior IDs:
    // default_virtual_epoch_tracks_invocation_start_and_is_reported and
    // explicit_virtual_epoch_reproduces_identical_observed_time.
    // The read-only proc chroot identity test retains all 170 prior identities.
    // 108e98c1e63 added run_evidence::tracing_time_mask_replaces_only_the_event_time
    // without raising this count (listed 172 against 171). The container-init
    // test container_init_honours_signals_sent_before_it_arms
    // (https://github.com/rrnewton/hermit/issues/3354) adds one more. Both
    // retain all 171 prior identities (`cargo nextest list --profile ci`
    // measured 173).
    // Four clock_determinism replay-epoch tests
    // (replay_with_an_omitted_epoch_starts_from_the_recorded_epoch,
    // replay_of_a_host_clock_recording_reuses_its_epoch,
    // replay_refuses_an_explicit_epoch_that_contradicts_the_recording and
    // replay_without_virtual_time_adopts_the_recorded_epoch;
    // https://github.com/rrnewton/hermit/issues/3411) retain all 173 prior
    // identities.
    // clock_passthrough adds
    // record_captures_host_clock_reads_and_replay_returns_them and
    // run_without_virtual_time_reads_the_host_clock
    // (https://github.com/rrnewton/hermit/issues/1176); both retain all 177
    // prior identities.
    // The four verify-claim wording tests in verify_claim_names_its_limit,
    // previously listed as undeclared in ci/undeclared-test-binaries.tsv,
    // retain all 179 prior identities.
    // reopened_pipe_progress adds six reopened-pipe progress and host-pipe
    // full-read tests (https://github.com/rrnewton/hermit/pull/3534); they
    // retain all 183 prior identities.
    // The dispatch_stats binary adds ptrace_reports_seccomp_stops_and_no_patching
    // and debug_log_carries_the_report_without_a_summary_file; both retain all
    // 189 prior identities.
    // first_seen_mtime adds the canonical first-seen mtime and cp -p tests
    // (https://github.com/rrnewton/hermit/issues/3639); they retain all 191
    // prior identities.
    // utimensat_mtime adds
    // explicit_mtimes_are_reported_by_stat_and_verify_strictly and
    // a_renamed_over_target_does_not_get_the_explicit_mtime
    // (https://github.com/rrnewton/hermit/issues/3565); they retain all 193
    // prior identities.
    // verify_claim_names_its_limit adds
    // a_host_file_replaced_during_run_1_is_named_as_the_cause,
    // a_divergence_with_no_host_file_change_names_no_host_input_change and
    // a_guest_that_replaces_a_file_after_diverging_names_no_host_input_change;
    // they retain all 195 prior identities (198, measured with cargo nextest
    // list).
    // a_guest_that_rebinds_the_path_itself_names_no_host_input_change and
    // a_replacement_where_the_guest_tried_to_rebind_the_path_names_no_host_input_change
    // retain all 198 (200, measured with cargo nextest list).
    // under_passthru_opt_an_unobserved_rebinding_names_no_host_input_change,
    // under_passthru_opt_a_host_replacement_is_reported_but_not_named and
    // a_rebinding_through_reverie_private_page_names_no_host_input_change
    // retain all 200 (203, measured with cargo nextest list).
    // verify_claim_names_its_limit's three private-page rewriting tests and
    // reverie_private_page_is_the_range_hermit_guards retain all 203 (207).
    // https://github.com/rrnewton/hermit/pull/3361
    // (https://github.com/rrnewton/hermit/issues/3146): the new
    // external_signal_interrupt binary adds all 68 of its tests (its LiteInst
    // cells are restoration targets of
    // https://github.com/rrnewton/hermit/issues/3745), and all 207 prior
    // identities are retained (275, measured with cargo nextest list).
    // signal_determinism's six verify signal-report tests (verify_reports_*,
    // verify_writes_no_signal_report_when_the_signaled_runs_verify and
    // verify_signal_report_cannot_replace_the_disposition_on_a_full_stderr)
    // retain all 275 (281, measured with cargo nextest list).
    // verify_signal_report_cannot_hold_the_disposition_on_a_full_blocking_stderr
    // retains all 281 (282),
    // verify_signal_report_cannot_hold_agreed_guest_stdout all 282 (283), and
    // the three stdout-file tests (one control, two at the file-size limit)
    // all 283 (286).
    // external_signal_interrupt's cells_give_the_guest_a_minimal_environment
    // retains all 286 (287, measured with cargo nextest list).
    // The 25 external_signal_interrupt ptrace cases that need the PMU timer
    // move to test.pmu_integration_cases: 287 - 25 = 262, listed
    // with this node's filter.
    // The eight fatal_core_capture tests: 270.
    // container_init_deadline::virtual_clock_exhaustion_under_a_scheduler_turn_cost_ends_the_run
    // (lane qemu-rcb, https://github.com/rrnewton/hermit/pull/3885) retains all
    // 270: 271.
    // robust_futex_owner_death::ptrace_wakes_the_waiter_when_an_exec_ends_the_owner
    // (https://github.com/rrnewton/hermit/issues/2082) retains all 271: 272.
    // And robust_futex_owner_death::ptrace_wakes_nobody_when_an_exec_leaves_the_owner_word_alone:
    // 273.
    // And external_signal_interrupt::ptrace_rt_sigtimedwait_is_ended_by_the_sigchld_of_an_exiting_child
    // (https://github.com/rrnewton/hermit/issues/4005): 274.
    // And external_signal_interrupt::ptrace_rt_sigtimedwait_takes_a_queued_signal_before_the_sigchld_of_an_exiting_child
    // (review of https://github.com/rrnewton/hermit/pull/4039): 275.
    // And external_signal_interrupt::ptrace_large_pselect6_sleeps_under_its_entry_mask_while_a_sibling_rewrites_it
    // (Codex review of https://github.com/rrnewton/hermit/pull/4053): 276.
    // And external_signal_interrupt::ptrace_large_pselect6_on_a_stack_with_no_scratch_behaves_as_natively
    // (Codex re-check of https://github.com/rrnewton/hermit/pull/4053): 277.
    // And the four targeted-chaos sched_yield tests in chaos_sched_yield_progress
    // (https://github.com/rrnewton/hermit/issues/4068): 281.
    // And external_signal_interrupt::ptrace_ppoll_on_a_stack_with_no_scratch_behaves_as_natively
    // (https://github.com/rrnewton/hermit/issues/4113): 282.
    // And external_signal_interrupt::ptrace_a_stop_signal_send_keeps_a_sighup_poll_to_its_deadline
    // (https://github.com/rrnewton/hermit/issues/4111): 283.
    ("test.hermit_integration", 283),
    ("test.arbitrary_binaries", 4),
    // Every record_replay identity but one (`cargo nextest list` lists 110):
    // the --skip waiver of record_node_eventfd_epoll_sequence
    // (https://github.com/rrnewton/hermit/issues/3782) leaves 109. The waiver
    // is gone since the rejoin log of
    // https://github.com/rrnewton/hermit/pull/3908 fixed that replay.
    // network_trace::curl_recording_replays_offline_with_identical_logs runs
    // since the pinned root provides curl
    // (https://github.com/rrnewton/hermit/issues/3786).
    // record_pipe_read_after_clearing_nonblocking_waits_for_its_writer adds
    // one and retains all 106 prior identities.
    // replay_mkdir_beneath_a_directory_that_only_existed_at_record_time and
    // replay_mkdirat_beneath_a_directory_that_only_existed_at_record_time add
    // two and retain all 104 prior identities. The three
    // network_trace::tcp_backpressure_* tests are added by the post-facto
    // network replay fixes. The fixes to the post-facto review of
    // https://github.com/rrnewton/hermit/pull/3791 add three more and retain
    // all 100 prior identities:
    // tcp_backpressure_replay_completes_a_waited_send_when_the_recording_did,
    // tcp_backpressure_record_refuses_a_waiting_send_whose_descriptor_was_replaced
    // and tcp_backpressure_records_the_bytes_each_resumed_send_injected.
    // network_trace::tcp_select_family_records_and_replays_offline
    // (https://github.com/rrnewton/hermit/issues/3804) adds one and retains
    // all 107 prior identities.
    // network_trace::record_reads_poll_family_nfds_as_linux_does adds one and
    // retains all 109 prior identities (`cargo nextest list` lists 111).
    // replay_receives_an_ignored_sigchld_at_the_recorded_boundary adds one
    // and retains all 110 (`cargo nextest list` lists 112).
    // record_pipe_blocking_read_waits_for_forked_writer,
    // record_socketpair_blocking_read_waits_for_forked_writer and
    // record_socketpair_read_after_clearing_nonblocking_waits_for_its_writer
    // add three and retain all 111 (`cargo nextest list` lists 115).
    // record_tcp_accept_replays_from_the_log
    // (https://github.com/rrnewton/hermit/issues/3864) adds one and retains
    // all 114 (`cargo nextest list` lists 116).
    // replays_of_a_threaded_tcp_accept_recording_agree
    // (https://github.com/rrnewton/hermit/pull/3871) adds one and retains all
    // 115 (`cargo nextest list` lists 117).
    // record_accept_copies_the_peer_address_out_as_linux_does,
    // record_accept_reads_the_address_capacity_after_the_wait,
    // replay_applies_a_socketpair_shutdown_before_a_live_recvmmsg,
    // recording_refuses_sendmmsg_on_an_accepted_connection and
    // recording_refuses_recvmmsg_on_an_accepted_connection
    // (https://github.com/rrnewton/hermit/pull/3871) add five and retain all
    // 116 (`cargo nextest list` lists 122).
    // replays_of_a_threaded_tcp_accept_recording_agree_for_a_received_listener
    // (the same pull request) adds one and retains all 121 (`cargo nextest
    // list` lists 123).
    // record_accept_on_a_thread_with_its_own_fd_table_names_its_peer (the
    // same pull request) adds one and retains all 122 (`cargo nextest list`
    // lists 124).
    // replay_reapplies_socket_calls_against_the_calling_threads_own_fd_table
    // (the same pull request) adds one and retains all 123 (`cargo nextest
    // list` lists 125).
    // Fourteen blocking-accept and rejoin-log tests
    // (record_replay_blocking_accept_*, record_replay_accept_entry_cases_*,
    // record_blocking_accept_*, record_accept_*, recording_refuses_* and
    // replay_refuses_a_missing_or_unexhausted_rejoin_log,
    // https://github.com/rrnewton/hermit/pull/3908) add fourteen and retain
    // all 124 (`cargo nextest list` lists 139).
    // record_workloads::tests::standalone_population_is_shared_across_processes_and_pruned
    // (https://github.com/rrnewton/hermit/issues/3946) adds one and retains all
    // 138 (`cargo nextest list` lists 140).
    // record_node_eventfd_epoll_sequence runs again, its --skip waiver gone
    // (https://github.com/rrnewton/hermit/issues/3782): all 140.
    // record_forked_child_writes_into_a_redirected_stdout_pipe
    // (https://github.com/rrnewton/hermit/issues/3964) adds one and retains all
    // 140 (`cargo nextest list` lists 141).
    // record_pselect_whose_mask_blocks_the_scheduler_timer_signal,
    // record_pselect_whose_mask_blocks_a_signal_a_sibling_takes,
    // record_pselect_with_a_write_only_mask_wrapper,
    // record_pselect_whose_mask_a_sibling_rewrites_before_the_call_runs and
    // record_sigtimedwait_ends_for_a_caught_sigchld_from_a_child_exit
    // (https://github.com/rrnewton/hermit/issues/3963 and its review) add five
    // and retain all 141 (`cargo nextest list` lists 146).
    // record_forked_child_writes_to_a_saved_stdout_after_its_parent_redirected_it
    // and record_forked_child_moves_the_root_stdout_offset_as_in_the_recording
    // (https://github.com/rrnewton/hermit/issues/4006 and its review) add two
    // and retain all 146 (`cargo nextest list` lists 148).
    // record_pselect_whose_mask_unblocks_the_scheduler_timer_signal and
    // record_pselect_whose_mask_a_sibling_rewrites_while_it_sleeps
    // (https://github.com/rrnewton/hermit/issues/3992 and its review) add two
    // and retain all 148 (`cargo nextest list` lists 150).
    // record_pselect_whose_mask_unblocks_the_timer_signal_on_a_tight_stack
    // (follow-up to https://github.com/rrnewton/hermit/pull/4052): 151.
    // record_sigsuspend_while_a_timer_signal_is_due
    // (https://github.com/rrnewton/hermit/issues/3993) adds one and retains all
    // 151 (`cargo nextest list` lists 152).
    // record_zero_timeout_pselect_whose_mask_unblocks_a_pending_signal and
    // record_zero_timeout_ppoll_whose_mask_unblocks_a_pending_signal
    // (https://github.com/rrnewton/hermit/issues/4054): 154.
    // record_ppoll_whose_mask_blocks_the_scheduler_timer_signal and
    // record_ppoll_whose_mask_unblocks_the_scheduler_timer_signal
    // (https://github.com/rrnewton/hermit/issues/4042): 156.
    // record_zero_timeout_pselect_whose_mask_a_sibling_toggles and
    // record_zero_timeout_ppoll_whose_mask_unblocks_a_pending_signal_on_a_tight_stack
    // (review of https://github.com/rrnewton/hermit/pull/4084): 158.
    // record_pselect_whose_mask_a_sibling_clears_before_the_grant (re-review of
    // https://github.com/rrnewton/hermit/pull/4084): 159.
    // record_epoll_pwait_on_a_ready_pipe, record_masked_epoll_pwait_on_a_ready_pipe,
    // record_epoll_pwait_woken_by_a_writer_thread,
    // record_epoll_pwait_whose_mask_blocks_the_scheduler_timer_signal,
    // record_epoll_pwait_whose_mask_unblocks_the_scheduler_timer_signal,
    // record_epoll_pwait_whose_mask_unblocks_a_pending_signal,
    // record_epoll_pwait_whose_mask_a_sibling_zeroes,
    // record_epoll_pwait_whose_mask_unblocks_a_pending_signal_on_a_tight_stack,
    // record_epoll_pwait_whose_mask_unblocks_a_pending_signal_24_bytes_into_its_stack,
    // record_epoll_pwait_whose_events_buffer_sits_in_the_red_zone,
    // record_masked_epoll_pwait_on_a_pipe_descriptor and
    // record_pselect_whose_mask_unblocks_the_timer_signal_24_bytes_into_its_stack
    // (epoll_pwait recorded, https://github.com/rrnewton/hermit/issues/4086, and
    // https://github.com/rrnewton/hermit/pull/4087's reviews): 171.
    // record_pselect_beside_a_sibling_spinning_on_sched_yield
    // (https://github.com/rrnewton/hermit/issues/4092): 172.
    // And record_pselect_beside_two_siblings_spinning_on_sched_yield (Claude
    // review of https://github.com/rrnewton/hermit/pull/4120): 173.
    // The five ppoll tight-stack tests (record_*_24_bytes_into_its_page,
    // record_*_128_bytes_into_its_page, record_unmasked_zero_timeout_ppoll_16_bytes_into_its_page
    // and record_masked_ppoll_whose_pollfd_is_in_the_red_zone,
    // https://github.com/rrnewton/hermit/issues/4113): 178.
    // And recording_refuses_a_masked_pselect_8_bytes_into_its_stack
    // (https://github.com/rrnewton/hermit/issues/4112): 179.
    // And record_masked_ppoll_whose_timeout_is_in_the_red_zone (Claude review of
    // https://github.com/rrnewton/hermit/pull/4127): 180.
    // record_epoll_pwait2_on_a_ready_pipe, record_epoll_pwait2_woken_by_a_writer_thread,
    // record_epoll_pwait2_whose_mask_unblocks_a_pending_signal,
    // record_epoll_pwait2_whose_mask_unblocks_a_pending_signal_on_a_tight_stack
    // and ..._24_bytes_into_its_stack (epoll_pwait2 recorded,
    // https://github.com/rrnewton/hermit/issues/4088): 185.
    // And record_epoll_pwait2_whose_mask_a_sibling_zeroes,
    // record_masked_epoll_pwait2_whose_timeout_is_in_the_red_zone,
    // record_zero_timeout_epoll_pwait2_beside_a_busy_sibling and
    // record_epoll_pwait2_checks_its_timeout_before_its_mask (Claude review of
    // https://github.com/rrnewton/hermit/pull/4128): 189.
    ("test.record_replay", 189),
    // Seven proc-fallback, warning, and record/replay tests retain all 80
    // selected identities under the unchanged shipped CLI skip filters.
    // The successful-exec POSIX timer regression retains all 87 prior CLI cases.
    // The PMU-subject skid-overshoot case moves to privileged-test.pmu_cli_cases.
    // The nonleader-exec exit-only variant retains all 87 prior selected CLI
    // identities; its four PMU-subject siblings run in privileged-test.pmu_cli_cases.
    // run_timeout_refusal_does_not_depend_on_backend_availability
    // (https://github.com/rrnewton/hermit/issues/3418) retains all 88 prior
    // selected CLI identities.
    // log_diff_compares_the_log_files_of_two_separate_runs,
    // log_diff_does_not_count_the_epoch_notice_as_evidence
    // (https://github.com/rrnewton/hermit/issues/3410) and
    // relaxed_log_diff_canonicalizes_marked_host_addresses_on_request
    // (https://github.com/rrnewton/hermit/issues/3412) retain all 89 prior
    // selected CLI identities.
    // liteinst_runtime_staging_does_not_require_a_git_checkout and
    // hermit_dap_skip_never_applies_to_a_binary_named_hermit_dap
    // (https://github.com/rrnewton/hermit/issues/3419) retain all 92 prior
    // selected CLI identities.
    // a_teardown_stall_after_publication_completes_within_the_default_budget,
    // a_child_that_outlives_the_finalize_budget_is_cancelled_with_the_budget_named
    // and a_malformed_finalize_budget_is_refused
    // (https://github.com/rrnewton/hermit/issues/3414) retain all 94 prior
    // selected CLI identities.
    // run_rejects_subcommand_level_backend and
    // analyze_rejects_backend_in_its_run_arguments
    // (https://github.com/rrnewton/hermit/pull/3439) retain all 97 prior
    // selected CLI identities.
    // run_verify_from_a_working_directory_under_host_tmp,
    // run_refuses_a_summary_json_hidden_by_the_private_tmp and
    // run_ptrace_backend_engagement_from_a_working_directory_under_host_tmp
    // (https://github.com/rrnewton/hermit/issues/3260) retain all 99 prior
    // selected CLI identities.
    // version_names_a_revision_only_when_the_build_was_stamped
    // (https://github.com/rrnewton/hermit/pull/3547) retains all 102 prior
    // selected CLI identities.
    // hermit_dap_attach_keeps_the_thread_across_continue_and_stack_trace and
    // hermit_dap_replay_steps_back_and_reverse_continues_through_a_recording
    // (https://github.com/rrnewton/reverie/pull/885) retain all 103 prior
    // selected CLI identities.
    // hermit_dap_replay_refusal_reaches_the_client,
    // hermit_dap_replay_steps_back_through_stops_off_line_breakpoints and
    // hermit_dap_replay_steps_back_through_repeated_and_shared_addresses
    // (https://github.com/rrnewton/hermit/pull/3545) retain all 105 prior
    // selected CLI identities.
    // hermit_dap_replay_step_back_in_recursion_lands_exactly and
    // hermit_dap_replay_step_back_skips_later_passes_of_a_one_line_loop
    // (https://github.com/rrnewton/hermit/pull/3545) retain all 108 prior
    // selected CLI identities.
    // hermit_dap_replay_step_back_between_sibling_calls_lands_exactly,
    // hermit_dap_replay_step_back_refuses_a_sibling_call_with_the_same_stack_pointer,
    // hermit_dap_replay_refused_step_back_stays_at_a_mid_line_stop and
    // hermit_dap_replay_reverse_continue_ends_the_session_when_it_cannot_return
    // (https://github.com/rrnewton/hermit/pull/3545) retain all 110 prior
    // selected CLI identities.
    // hermit_dap_replay_step_back_before_an_exit_lands_exactly,
    // hermit_dap_replay_refused_step_back_before_an_exit_stays_at_the_stop and
    // hermit_dap_replay_continue_through_deep_recursion_is_fast_and_lands_exactly
    // (https://github.com/rrnewton/hermit/pull/3545) retain all 114 prior
    // selected CLI identities.
    // hermit_dap_replay_step_back_refuses_an_activation_that_differs_only_at_frame_7
    // (https://github.com/rrnewton/hermit/pull/3545) retains all 117 prior
    // selected CLI identities.
    // The real-ptrace golden-log retention test
    // ptrace_keep_logs_retains_only_the_golden_log_after_a_match
    // (https://github.com/rrnewton/hermit/issues/3301) retains all 118 prior
    // selected identities (`cargo nextest list --profile ci` measured the +1
    // when the change was written, not on this base). It needs no PMU:
    // without perf counters the run continues with --max-timeslice=disabled.
    // liteinst_backend_stats_report_the_guests_own_dispatch_paths
    // (https://github.com/rrnewton/hermit/pull/3564) retains all 119 prior
    // selected CLI identities.
    // liteinst_in_guest_refuses_a_maximum_timeslice_before_dispatch,
    // liteinst_in_guest_selector_rejects_unknown_values and
    // liteinst_in_guest_refuses_verify_without_reading_stdin
    // (https://github.com/rrnewton/hermit/pull/3635) retain all 120 prior
    // selected CLI identities (`cargo nextest list --profile ci` measured +3).
    // Its two other CLI tests are #[ignore]d: they need the in-guest runtime
    // library, which this selection does not build.
    // Making LiteInst in-guest only
    // (https://github.com/rrnewton/hermit/issues/3520) deletes
    // run_liteinst_verifies_detcore_backend,
    // liteinst_backend_stats_report_the_guests_own_dispatch_paths and
    // liteinst_in_guest_selector_rejects_unknown_values, which drove the
    // deleted ptrace-hosted hybrid or its selector. It renames
    // run_liteinst_rejects_a_non_runtime_override_before_activation_claim and
    // run_liteinst_rejects_an_inert_dso_before_activation_claim to
    // ..._before_dispatch and retains the other 118 identities unchanged
    // (`cargo nextest list --profile ci` with this node's skips lists 120,
    // the two renamed tests included). Three
    // in-guest CLI tests are #[ignore]d at this point, for the reason above.
    // The same change moves the liteinst_advanced tests that in-guest LiteInst
    // can run into this binary, as the 23 tests of
    // liteinst_in_guest_programs:: (hermit-cli/tests/common/
    // liteinst_in_guest_programs.rs; python_random_example is not among them,
    // because in-guest LiteInst crashes on Python's hashlib), and restores
    // liteinst_backend_stats_report_the_guests_own_dispatch_paths retargeted to
    // in-guest LiteInst: 144 = 120 + 23 + 1 (`cargo nextest list --profile ci`
    // with this node's skips lists 144 non-ignored matches). They load the
    // libdetcore_liteinst.so that build.workspace_compile_in_pinned_root's
    // `cargo build --profile validate --workspace --all-targets` writes beside
    // target/validate/hermit, the CARGO_BIN_EXE_hermit they run.
    // run_liteinst_finds_the_runtime_staged_as_an_installed_resource, which
    // checks that an installation's packaged rsrcs/libdetcore_liteinst.so is
    // found and is consulted before the Cargo artifact directory, retains all
    // 144 prior identities (`cargo nextest list --profile ci` with this node's
    // skips lists 145).
    // liteinst_in_guest_programs::liteinst_in_guest_python_random_example
    // (restored as an ordinary test) and
    // liteinst_in_guest_programs::liteinst_in_guest_cpuid_in_a_late_loaded_library_runs
    // retain all 145 prior identities: 147. Both need the pinned Reverie to
    // emulate CPUID in code mapped after the in-guest runtime started.
    // liteinst_in_guest_verify_compares_the_records_the_guest_forwards,
    // liteinst_in_guest_verify_survives_a_guest_stderr_without_a_reader and
    // liteinst_in_guest_verify_with_records_past_the_log_bound_is_no_result
    // (https://github.com/rrnewton/hermit/issues/3520, C5) retain all 147
    // prior identities (`cargo nextest list --profile ci` with this node's
    // skips lists 150). liteinst_in_guest_verify_log_keeps_records_in_ptraces_order
    // retains all 150: 150 + 1 = 151, measured with the same listing.
    // dbt_rdtsc_is_answered_by_detcore_as_under_ptrace retains all 152: 153,
    // listed with the build.workspace_on_host features and this node's
    // arguments.
    // ptrace_verification_reports_carry_the_exact_branch_counter_verdict
    // retains all 153 (154, measured with cargo nextest list).
    // The eleven --max-log-bytes CLI tests of
    // https://github.com/rrnewton/hermit/pull/3686
    // (max_log_bytes_aborts_a_run_whose_stderr_log_runs_away,
    // max_log_bytes_aborts_a_run_whose_log_file_runs_away,
    // max_log_bytes_leaves_a_run_under_the_cap_alone_and_refuses_zero,
    // max_log_bytes_exits_promptly_when_stderr_is_a_full_pipe_nobody_reads,
    // max_log_bytes_keeps_the_cap_status_when_stderr_has_no_reader,
    // max_log_bytes_is_refused_where_the_guest_could_outlive_hermit,
    // max_log_bytes_is_refused_under_no_namespace_on_the_ptrace_runtime,
    // max_log_bytes_stopped_log_is_refused_by_log_diff,
    // max_log_bytes_is_refused_for_sabre_strace_and_leaves_other_strace_alone,
    // max_log_bytes_verify_keeps_the_cap_status_when_stderr_loses_its_reader
    // and max_log_bytes_verify_exits_promptly_when_stderr_fills_after_run1)
    // retain all 154 prior identities: 154 + 11 = 165 (`cargo nextest list
    // --profile ci` with this node's skips lists 165).
    // max_log_bytes_exits_promptly_when_perf_is_unavailable_and_stderr_is_a_full_pipe
    // (the hosted-runner hang of GitHub run
    // https://github.com/rrnewton/hermit/actions/runs/37489014478) retains
    // all 165: 165 + 1 = 166, listed the same way.
    // The two in-guest FUSE tests (ruling A) and the userfaultfd test (ruling D.2): 169.
    // The exit-reaping and unscheduled-death probes (C3.5 evidence): 171.
    // The sixteen capped-run diagnostic tests (fifteen max_log_bytes_* cases
    // in cli.rs, one of them the verify Run2 banner, and
    // liteinst_in_guest_programs::liteinst_in_guest_max_log_bytes_exits_promptly_when_stderr_is_a_full_pipe)
    // retain all 171: 171 + 16 = 187, listed the same way.
    // Five more capped-run diagnostic tests (the --print-verify-logs echo, the
    // unreadable run-1 summary warning, the stderr log sink without
    // --log-file, and --stacktrace-event with and without a thread exit)
    // retain all 187: 187 + 5 = 192, listed the same way. The sixth,
    // max_log_bytes_preemption_stacktrace_exits_promptly_when_stderr_is_a_full_pipe,
    // needs the PMU timer, so this node skips it by exact name and
    // privileged-test.pmu_cli_cases runs it.
    // max_log_bytes_print_verify_logs_exits_promptly_when_run1_log_is_unreadable_and_stderr_is_a_full_pipe
    // (the --print-verify-logs read-error warning) retains all 192:
    // 192 + 1 = 193, listed the same way.
    // The two fail-closed stderr-writer tests (statx denied, and the terminal
    // query denied with stderr a full terminal) retain all 193: 193 + 2 = 195,
    // listed the same way.
    // Four more stderr-writer tests (the terminal query answered with ENOTTY,
    // statx denied with stderr read, and every file-type query on stderr
    // denied or feigning success with stderr a full pipe) retain all 195:
    // 195 + 4 = 199, listed the same way.
    // max_log_bytes_without_a_log_file_exits_promptly_when_the_stat_of_stderr_never_returns_and_the_log_fills_the_stderr_pipe
    // retains all 199: 199 + 1 = 200, listed the same way.
    // max_log_bytes_without_a_log_file_exits_promptly_when_every_file_type_query_answers_eintr_and_the_log_fills_the_stderr_pipe
    // retains all 200: 200 + 1 = 201, listed the same way.
    // liteinst_in_guest_dup_aliases_share_one_cursor_after_fork retains all
    // 201: 202, re-listed. It starts an in-guest LiteInst guest, so it joins
    // the hosted CPUID-faulting exact-name filterset and test.cli_on_host
    // stays 169.
    // liteinst_in_guest_close_range_releases_ports_like_ptrace retains all
    // 202: 203, re-listed. It starts an in-guest LiteInst guest, so it joins
    // the hosted CPUID-faulting exact-name filterset and test.cli_on_host
    // stays 169.
    // liteinst_replay_with_a_conflicting_epoch_is_refused_in_every_build and
    // liteinst_verify_with_a_quiet_log_level_is_refused_in_every_build retain
    // all 203: 205, re-listed.
    // run_ptrace_returns_efault_for_gettimeofday_into_an_unreadable_page,
    // run_dbt_fails_when_a_child_ends_on_a_backend_failure_its_parent_ignores,
    // run_dbt_fails_when_a_forked_child_ends_on_a_backend_failure_before_exec and
    // run_dbt_verify_keeps_the_diagnostic_of_a_backend_failure retain all 205:
    // 209.
    // verify_digest_records_an_unobservable_recvmsg_and_keeps_the_kernel_result retains all 209: 210, re-listed. It starts no LiteInst guest, so test.cli_on_host selects it too.
    // liteinst_in_guest_programs::liteinst_in_guest_user_address_limit_queries_keep_the_guest_errno
    // retains all 210: 211, measured with cargo nextest list --profile ci.
    // happens_before_inert_spec_adds_no_per_syscall_scheduler_turns and
    // happens_before_edge_reverses_two_processes_writes
    // (https://github.com/rrnewton/hermit/issues/3877) retain all 211: 213.
    // a_sigchld_sent_at_an_rt_sigsuspend_request_wakes_the_call retains all
    // 213: 214, listed with this node's arguments.
    // a_shared_sigsuspend_mask_rewritten_before_the_call_runs_does_not_stall_the_run
    // retains all 214: 215, listed with this node's arguments.
    // liteinst_in_guest_refuses_a_guest_socket_at_the_forwarding_number
    // retains all 214: 215, measured with cargo nextest list --profile ci.
    // liteinst_in_guest_programs::liteinst_in_guest_sigalrm_handler_is_virtual_and_published
    // retains all 211: 212, re-listed. It starts an in-guest LiteInst guest, so
    // it joins the hosted CPUID-faulting exact-name filterset and
    // test.cli_on_host stays 171.
    // The thread-aware happens-before checkpoint and its parked-thread fixes
    // (https://github.com/rrnewton/hermit/issues/3149) add seven tests:
    // happens_before_spec_adds_checkpoint_turns_only_on_named_threads,
    // happens_before_edge_reverses_two_threads_writes,
    // happens_before_source_then_blocking_wait_lets_the_target_pass,
    // happens_before_gate_that_cannot_open_reports_a_deadlock,
    // happens_before_sigchld_to_a_held_thread_waits_for_its_gate,
    // happens_before_alarm_at_a_held_thread_ends_in_a_deadlock_report and
    // happens_before_process_exit_with_a_held_worker_exits_zero retain all 217: 224.
    // a_sigsuspend_mask_on_a_shared_stack_below_the_red_zone_is_copied_elsewhere
    // retains all 215: 216, listed with this node's arguments.
    // a_childs_exit_signal_follows_its_exec_and_clone_parent
    // (https://github.com/rrnewton/hermit/issues/3895) retains all 225: 226,
    // listed with this node's arguments.
    // liteinst_in_guest_programs::liteinst_in_guest_tool_directory_reads_stay_out_of_the_guest_heap
    // retains all 226: 227, listed with this node's arguments. It joins the same
    // hosted CPUID-faulting filterset, so test.cli_on_host does not change.
    // liteinst_in_guest_programs::liteinst_in_guest_sigalrm_handler_runs_at_syscall_completion,
    // liteinst_in_guest_sigalrm_pending_at_handler_return_runs_before_the_next_syscall,
    // liteinst_in_guest_sigalrm_committed_at_an_instruction_trap_runs_before_exit and
    // liteinst_in_guest_an_undeliverable_sigalrm_is_a_loss_and_stops_the_guest (signal
    // phase 1 step I4) retain all 227: 231, listed with this node's arguments. They join
    // the same hosted filterset; test.cli_on_host does not change.
    // a_child_exit_is_notified_once_with_linuxs_siginfo retains all 231: 232,
    // run with this node's arguments. It starts no LiteInst guest, so
    // test.cli_on_host selects it too.
    // run_config_saved_by_one_run_reproduces_it and
    // run_config_refuses_an_unknown_key_with_the_working_spelling retain all
    // 232: 234. Neither starts a LiteInst guest, so test.cli_on_host selects both.
    // The syscall-occurrence anchor tests
    // happens_before_fd_anchors_order_two_threads_without_calibration,
    // happens_before_fd_anchor_that_never_fires_is_refused_by_name,
    // happens_before_fd_anchor_counts_past_the_first_occurrence,
    // happens_before_before_anchor_that_never_fires_is_refused_by_name,
    // happens_before_hold_in_a_vfork_child_is_refused_by_name,
    // happens_before_fd_anchor_is_refused_without_interception and
    // happens_before_unsupported_fd_anchor_is_refused_before_reading_stdin
    // retain all 234: 241.
    // liteinst_in_guest_programs::{in_guest_trap_dispatch_record_reports_no_patched_sites,
    // in_guest_trap_verifies_a_program_with_no_site_patched,
    // in_guest_trap_sets_site_patching_off_and_refuses_a_caller_who_turns_it_on,
    // in_guest_trap_refuses_a_guest_library_that_turns_site_patching_back_on}
    // (`--backend=in-guest-trap`, step C4 of
    // https://github.com/rrnewton/hermit/issues/3520) retain all 234: 238,
    // listed with this node's arguments. Each starts an in-guest LiteInst
    // guest, so they join the hosted CPUID-faulting exact-name filterset and
    // test.cli_on_host does not change.
    // run_dbt_numbers_syscalls_like_ptrace_from_the_initial_execve retains all
    // 234: 235 (cargo nextest list with this node's arguments). It starts no
    // LiteInst guest, so test.cli_on_host selects it too.
    // happens_before_unusable_spec_is_a_named_policy_refusal
    // (https://github.com/rrnewton/hermit/issues/3943) retains all 246: 247.
    // run_refuses_an_inherited_seccomp_filter and
    // run_under_an_inherited_seccomp_filter_with_the_unsafe_override_is_recorded
    // retain all 246: 248. Neither starts a LiteInst guest, so
    // test.cli_on_host selects both.
    // liteinst_in_guest_programs::{liteinst_in_guest_runtime_leaves_the_guest_heap_as_the_program_made_it,
    // liteinst_in_guest_fork_child_heap_is_the_parents,
    // liteinst_in_guest_admitted_sigalrm_leaves_the_guest_heap_as_the_program_made_it,
    // liteinst_in_guest_runtime_calls_none_of_the_converted_libc_wrappers}
    // (in-guest LiteInst start-up out of the guest's heap) retain all 246:
    // 250, listed with this node's arguments. Each starts an in-guest LiteInst
    // guest, so they join the hosted CPUID-faulting exact-name filterset and
    // test.cli_on_host does not change.
    // run_dbt_guest_sees_ptraces_environment_before_and_after_exec and
    // run_dbt_verifies_a_guest_that_prints_its_environment retain all 253:
    // 255 (cargo nextest list with this node's arguments). Neither starts a
    // LiteInst guest, so test.cli_on_host selects both.
    // liteinst_in_guest_programs::{liteinst_in_guest_self_signal_deaths_are_scheduled_and_verify,
    // liteinst_in_guest_a_root_that_aborts_ends_with_its_own_status} (a
    // process that signals itself to death deregisters first) retain all
    // 255: 257. Each starts an in-guest LiteInst guest, so they join the
    // hosted CPUID-faulting exact-name filterset and test.cli_on_host does not
    // change.
    // The runtime-owned self-signal exclusion test retains all 257: 258.
    // Both in-guest backends must report a loss for SIGSEGV and SIGSYS; this
    // test also joins the hosted CPUID-faulting exact-name filterset.
    // The scalar SIGALRM lifecycle and admission-control regressions retain
    // all 258: 260, measured with this node's Nextest listing. Both require
    // CPUID faulting and join only the hosted exact-name exclusions.
    // a_killed_vfork_parent_does_not_stop_the_schedule
    // and a_vfork_child_that_kills_its_parent_and_execs_lets_its_own_child_run
    // (https://github.com/rrnewton/hermit/issues/3984) retain all 260: 262.
    // Fatal, noninterrupting and excluded-running alarm regressions retain
    // all 262: 265. Each needs CPUID faulting and is excluded only on hosted.
    // liteinst_in_guest_programs::{liteinst_in_guest_checks_and_hides_the_config_fingerprint,
    // liteinst_runtime_refuses_another_trees_fingerprint_before_connecting}
    // (lane qemu-rcb, https://github.com/rrnewton/hermit/issues/3986) retain
    // all 265: 267, measured with `cargo nextest list --profile ci` and this
    // node's arguments.
    // The DBT network namespace collision regression: 266, listed.
    // happens_before_{relative_anchor_holds_the_first_call_after_its_base,
    // unfired_relative_anchor_is_refused_with_its_base,
    // hold_budget_ends_a_hold_a_spinner_never_releases,
    // version_2_load_refusals_name_the_event_and_run_nothing,
    // version_2_without_perf_counters_is_refused_before_stdin,
    // relative_anchor_state_is_not_inherited_by_a_new_thread} retain all
    // 268: 274. None starts a LiteInst guest or needs CPUID faulting.
    // skid_retry_admits_only_a_typed_skid_overshoot_refusal, the retry
    // predicate's unit test (https://github.com/rrnewton/hermit/issues/1845),
    // runs no guest: 268.
    // a_futex_wake_count_of_zero_or_negative_wakes_one_waiter
    // (https://github.com/rrnewton/hermit/issues/3957) retains all 275: 276.
    // happens_before_cycle_through_a_child_wait_is_refused_by_name and
    // happens_before_held_child_killed_by_another_process_is_not_a_deadlock
    // (https://github.com/rrnewton/hermit/issues/3904) retain all 276: 278.
    // a_real_time_signal_timer_is_refused_by_name_when_it_expires
    // (https://github.com/rrnewton/hermit/issues/3893) retains all 278: 279.
    // a_process_directed_signal_to_several_threads_is_refused_by_name
    // (https://github.com/rrnewton/hermit/issues/4034) retains all 279: 280.
    // a_signal_to_a_process_group_is_refused_by_name
    // (https://github.com/rrnewton/hermit/issues/4046) retains all 280: 281.
    // happens_before_posthook_{before_anchor_orders_the_completed_write,
    // after_anchor_holds_the_thread_after_its_call, edge_passes_strict_verify,
    // anchor_on_an_interrupted_call_is_refused_by_name,
    // child_wait_cycle_is_refused_by_name,
    // read_with_a_held_writer_ends_at_the_timeout,
    // anchor_with_replay_schedule_is_refused,
    // anchor_of_a_thread_that_exits_mid_call_never_fires} and
    // happens_before_alarm_at_a_thread_held_at_a_posthook_gate_leaves_it_held
    // (https://github.com/rrnewton/hermit/issues/3929) retain all 281: 290.
    // futex_requeue_and_wake_op_match_linux, futex_lock_pi_is_refused_by_name,
    // futex_requeue_and_wake_op_key_and_access_their_words_as_linux and
    // a_wake_op_on_a_file_backed_shared_word_is_refused_by_name
    // (https://github.com/rrnewton/hermit/pull/4020) retain all 290: 294.
    // verify_tmpdir_does_not_reach_the_guest_environment,
    // a_killed_verify_leaves_its_logs_in_verify_tmpdir_not_tmpdir and
    // an_unusable_verify_tmpdir_is_a_clean_error_naming_the_variable
    // (https://github.com/rrnewton/dev-hermit/issues/553) retain all 294: 297.
    // group_kills_never_reach_the_host_without_sequentialized_threads
    // (https://github.com/rrnewton/hermit/issues/4057) retains all 297: 298.
    // a_futex_waiter_woken_for_a_signal_another_thread_takes_rereads_its_word
    // (https://github.com/rrnewton/hermit/issues/4033) retains all 298: 299.
    // Six happens-before cases whose version-2 specs need the PMU preemption
    // timer (happens_before_{relative_anchor_holds_the_first_call_after_its_base,
    // relative_anchor_state_is_not_inherited_by_a_new_thread,
    // unfired_relative_anchor_is_refused_with_its_base,
    // hold_budget_ends_a_hold_a_spinner_never_releases,
    // posthook_anchor_on_an_interrupted_call_is_refused_by_name,
    // hold_in_a_vfork_child_is_refused_by_name}) move to
    // privileged-test.pmu_cli_cases (hosted portable run
    // https://github.com/rrnewton/hermit/actions/runs/38110887257): 299 - 6 =
    // 293, listed with this node's arguments.
    // a_futex_wait_on_an_unreadable_word_is_efault
    // (https://github.com/rrnewton/hermit/issues/4030) retains all 293: 294.
    // a_pidfd_signal_to_several_threads_is_refused_by_name
    // (https://github.com/rrnewton/hermit/issues/4047) retains all 294: 295.
    // signals_to_hermits_own_tasks_are_dropped_in_every_mode,
    // a_nested_pid_namespace_signals_its_own_tasks and
    // a_signal_to_a_newborn_thread_is_never_taken_for_hermits
    // (https://github.com/rrnewton/hermit/issues/4066) retain all 295: 298.
    // a_namespace_only_guests_kill_0_never_reaches_hermits_group and
    // a_namespace_only_run_on_a_terminal_keeps_its_terminal_behaviour
    // (https://github.com/rrnewton/hermit/issues/4065) retain all 298: 300.
    // a_pending_real_time_timer_expiration_is_refused_only_where_it_is_observed
    // (https://github.com/rrnewton/hermit/issues/4089) retains all 300: 301.
    // a_cross_process_sigkill_verifies_at_bitwise_parity and a_recorded_kill_of_a_victim_in_a_background_read_is_refused_at_replay (https://github.com/rrnewton/hermit/issues/3994) retain all 301: 303.
    // The four own-process signal CLI tests (https://github.com/rrnewton/hermit/issues/4034) retain all 303: 307.
    ("test.cli", 307),
    // sabre_and_ptrace_detlogs_agree_through_post_exec and
    // detlog_records_drop_only_the_timestamp_and_suffix retain all 7 prior
    // identities.
    // sabre_reports_a_replaced_host_file_without_naming_it_the_cause,
    // sabre_names_no_host_input_change_for_another_divergence and
    // sabre_names_no_host_input_change_for_a_guest_that_replaced_a_file_after_diverging
    // retain all 9 (12, measured with cargo nextest list).
    // sabre_forwards_a_target_scoped_detlog_filter_as_ptrace_applies_it
    // retains all 12 prior identities: 13.
    // sabre_forked_child_gets_no_plugin_warning_on_guest_stderr retains all
    // 13: 14.
    // sabre_verify_log_keeps_forwarded_records_in_ptraces_order and
    // sabre_verify_survives_a_guest_dup_onto_the_forwarding_socket retain all
    // 14: 16 (measured with cargo nextest list).
    // sabre_verify_refuses_when_forwarded_records_go_missing retains all 16:
    // 17. sabre_verify_writes_no_tool_record_into_a_guest_stderr_file retains
    // all 17: 18.
    // sabre_verify_sends_no_tool_record_to_a_guest_socket_at_the_passed_number
    // retains all 18: 19.
    // sabre_verify_never_loses_an_exec_image_that_drops_its_forwarding_settings
    // retains all 19: 20.
    // sabre_runs_a_guest_rdtsc_before_and_after_its_first_syscall retains all
    // 20: 21 (measured with cargo nextest list).
    // sabre_virtualizes_an_rdtsc_in_a_guest_signal_handler and
    // sabre_refuses_a_guest_rdtsc_before_its_plugin_starts retain all 21: 23.
    // sabre_refuses_an_rdtsc_that_reenters_a_tool_call retains all 23: 24.
    // sabre_guest_startup_reads_detcores_stack_limit and
    // sabre_refuses_a_guest_limit_change_before_its_plugin_starts retain all
    // 24: 26.
    // sabre_guest_startup_limit_reads_through_the_sigill_marker retains all
    // 26: 27.
    ("test.sabre_examples", 27),
    ("test.hermit_modes", 21),
    ("test.app_strict_verify", 8),
    ("test.command_strict_verify", 9),
    ("test.ignored_syscall_regressions", 4),
    ("test.rr_suite_contract", 1),
    ("privileged-test.pmu_buck_chaos_cases", 6),
    // PMU-subject cases moved out of test.hermit_unit and test.cli so the
    // hosted lane, which has no PMU, does not select them (see
    // https://github.com/rrnewton/hermit/actions/runs/36499357369).
    ("privileged-test.pmu_ptrace_completion_cases", 5),
    // Four PMU-subject ptrace nonleader-exec cases join the skid-overshoot case.
    // run_chaos_preemption_replay_reuses_the_recorded_epoch
    // (https://github.com/rrnewton/hermit/issues/3413) joins the five PMU cases.
    // max_log_bytes_preemption_stacktrace_exits_promptly_when_stderr_is_a_full_pipe
    // joins them: 6 + 1 = 7, listed with this node's filterset.
    // poll_timeout_returns_on_time_while_another_thread_spins
    // (https://github.com/rrnewton/hermit/issues/3952) joins them: 7 + 1 = 8.
    // The six version-2 happens-before cases that need the preemption timer
    // (hosted portable run
    // https://github.com/rrnewton/hermit/actions/runs/38110887257) join them:
    // 8 + 6 = 14, listed with this node's filterset.
    ("privileged-test.pmu_cli_cases", 14),
    // The 29 tests_time cases that enable max_timeslice and therefore need the
    // PMU-backed RCB clock/timer, moved out of test.detcore_time and its hosted
    // twin (https://github.com/rrnewton/hermit/issues/3663); measured with the
    // node's exact filter, 29 run and the 2 PMU-free cases are filtered out.
    // rdtsc_inside_a_spinlock_does_not_end_a_target_timeslice (lane qemu-rcb,
    // https://github.com/rrnewton/hermit/pull/3859) sets max_timeslice, so this
    // node selects it and retains all 29: 30.
    // scheduler_turn_cost_sets_the_clock_jump_across_a_turn,
    // scheduler_turn_cost_runs_are_deterministic and
    // a_finite_poll_times_out_with_a_small_scheduler_turn_cost (lane qemu-rcb,
    // https://github.com/rrnewton/hermit/pull/3885) inherit the default
    // max_timeslice, so this node selects them and retains all 30: 33.
    // a_futex_wake_hands_a_contended_mutex_to_the_waiter,
    // a_futex_wake_handoff_is_deterministic and
    // a_futex_wake_yield_survives_a_preemption_record_and_replay (lane
    // qemu-rcb, https://github.com/rrnewton/hermit/issues/3874) enable
    // max_timeslice, so this node selects them and retains all 33: 36.
    ("privileged-test.pmu_detcore_time_cases", 36),
    // The 25 external_signal_interrupt ptrace cases that need the PMU timer,
    // moved out of test.hermit_integration and its hosted twin; listed with
    // the node's exact filter, 25 run.
    ("test.pmu_integration_cases", 25),
    // Exec timer and nonleader-exec refusal regressions extend 33 KVM cases
    // plus the unchanged setup control.
    // Two KVM gettimeofday EFAULT regressions retain all 36 prior identities.
    // The twelve serial KVM waitid/wait4 regressions of
    // https://github.com/rrnewton/hermit/pull/3266 (ten waitid copy-out and
    // sibling-reaping cases and two wait4 cases) retain all 38 prior
    // identities: measured 50 = 38 + 12, of which 49 are run_kvm_ tests.
    // The wait4 __WNOTHREAD owner-selection regression and the two parked
    // foreign-waiter contention regressions (waitid and wait4) of the same
    // pull request retain all 50 prior identities: measured 53 = 50 + 3, of
    // which 52 are run_kvm_ tests.
    // run_kvm_numbers_guest_tasks_as_the_ptrace_backend retains all 53:
    // 53 + 1 = 54, of which 53 are run_kvm_ tests.
    // run_kvm_names_the_execve_filename_in_at_execfn_as_ptrace_does retains
    // all 54: 54 + 1 = 55, of which 54 are run_kvm_ tests.
    // run_kvm_futex_wait_on_an_unreadable_word_matches_linux
    // (https://github.com/rrnewton/hermit/issues/4030) retains all 55:
    // 55 + 1 = 56, of which 55 are run_kvm_ tests.
    ("privileged-test.cli_kvm", 56),
    // The same three https://github.com/rrnewton/hermit/issues/3260 tests as
    // test.cli; all 99 prior identities retained. The same
    // https://github.com/rrnewton/hermit/pull/3547 version test as test.cli
    // retains all 102.
    // The same two hermit-dap end-to-end tests from
    // https://github.com/rrnewton/reverie/pull/885 as test.cli; all 103 prior
    // identities retained. The same thirteen hermit-dap replay tests from
    // https://github.com/rrnewton/hermit/pull/3545 as test.cli (added three,
    // two, four, three and one at a time); all 105, 108, 110, 114 and 117
    // prior identities retained at each step.
    // The host node carries the identical selection.
    // liteinst_backend_stats_report_the_guests_own_dispatch_paths
    // (https://github.com/rrnewton/hermit/pull/3564), as in test.cli; all 119
    // prior identities retained.
    // The three https://github.com/rrnewton/hermit/pull/3635 in-guest LiteInst
    // refusal tests, as in test.cli; all 120 prior identities retained.
    // The host twin loses the same three
    // https://github.com/rrnewton/hermit/issues/3520 tests as test.cli, and
    // gains the same 24 in-guest LiteInst tests (120 + 24 = 144), whose runtime
    // build.workspace_on_host builds beside target/validate/hermit.
    // The same installed-resource test as test.cli; all 144 prior identities
    // retained. The same two late-mapped-CPUID tests as test.cli; all 145
    // prior identities retained. The same three forwarded-record --verify
    // tests as test.cli; all 147 prior identities retained. The same
    // forwarded-record order test as test.cli; all 150 retained.
    // The 31 in-guest LiteInst cases that need CPUID faulting (the 26 of
    // GitHub portable run 37383177918, whose hosted runner has none, and the
    // five liteinst_in_guest_verify_ cases above, which run guests under the
    // same runtime) leave the host twin through an exact-name filterset;
    // test.cli keeps them. Listed at this tip with the
    // build.workspace_on_host features (hermit/kvm-native-test-support,
    // hermit/third-party-backends) and this node's arguments: 152 before,
    // 121 after, and the removed set is exactly those 31.
    // The same DBT rdtsc test as test.cli retains all 121: 122, listed the
    // same way.
    // The same exact-branch-counter report test retains all 122 (123).
    // The same eleven https://github.com/rrnewton/hermit/pull/3686
    // --max-log-bytes tests as test.cli retain all 123 prior identities:
    // 123 + 11 = 134, listed with this node's arguments. None of them starts
    // an in-guest LiteInst guest (the one LiteInst case is refused before
    // launch), so the exact-name filterset is unchanged: 165 - 31 = 134.
    // The same perf-unavailable --max-log-bytes test as test.cli retains all
    // 134 (135, listed with this node's arguments); it starts no LiteInst
    // guest, so the filterset is unchanged: 166 - 31 = 135.
    // The host twin selects the same three exit-dependency tests: 138.
    // The host twin selects the same two probes: 140.
    // The same fifteen cli.rs capped-run diagnostic tests as test.cli retain
    // all 140 (155, listed with this node's arguments). The sixteenth,
    // liteinst_in_guest_max_log_bytes_exits_promptly_when_stderr_is_a_full_pipe,
    // starts an in-guest LiteInst guest, so it joins the exact-name
    // filterset: 187 - 32 = 155.
    // The same five capped-run diagnostic tests as test.cli retain all 155
    // (160, listed with this node's arguments); none starts a LiteInst guest,
    // so the filterset is unchanged: 192 - 32 = 160. The PMU case is skipped
    // by exact name here as in test.cli.
    // The same --print-verify-logs read-error test as test.cli retains all
    // 160 (161, listed with this node's arguments); it starts no LiteInst
    // guest, so the filterset is unchanged: 193 - 32 = 161.
    // The same two fail-closed stderr-writer tests as test.cli retain all 161
    // (163, listed with this node's arguments); neither starts a LiteInst
    // guest, so the filterset is unchanged: 195 - 32 = 163.
    // The same four stderr-writer tests as test.cli retain all 163 (167,
    // listed with this node's arguments); none starts a LiteInst guest, so
    // the filterset is unchanged: 199 - 32 = 167.
    // The same never-returning stat test as test.cli retains all 167 (168,
    // listed with this node's arguments); it starts no LiteInst guest, so the
    // filterset is unchanged: 200 - 32 = 168.
    // The same interrupted file-type query test as test.cli retains all 168
    // (169, listed with this node's arguments); it starts no LiteInst guest,
    // so the filterset is unchanged: 201 - 32 = 169.
    // The descriptor-alias and close_range port tests start in-guest LiteInst
    // guests, so both join the filterset: 203 - 34 = 169.
    // Those five cases (the two FUSE tests, the userfaultfd test and the two
    // probes) start the in-guest LiteInst runtime: four failed on CPUID
    // faulting in GitHub portable run
    // https://github.com/rrnewton/hermit/actions/runs/37543769782, and the
    // FUSE plain-file case passed there only by returning early without a
    // FUSE mount. They join the exact-name filterset, which test.cli keeps
    // running: 203 - 39 = 164. Listed with the build.workspace_on_host
    // features and this node's arguments: 169 before, 164 after, and the
    // removed set is exactly those five.
    // The two LiteInst refusal tests added to test.cli start no guest, so the
    // filterset is unchanged: 205 - 39 = 166, listed the same way.
    // The same four unreadable-page and DBT backend-failure tests start no
    // in-guest LiteInst guest, so the filterset is unchanged: 209 - 39 = 170.
    // The host twin selects the unobserved-digest verify test too: 171.
    // The user-address-limit errno test starts an in-guest LiteInst guest, so
    // it joins the exact-name filterset: 171, listed the same way.
    // The two happens-before tests (inert turn count, ordering edge) start no
    // LiteInst guest, so the filterset is unchanged and the host twin selects
    // both too: 173.
    // The rt_sigsuspend SIGCHLD wake test starts no LiteInst guest either and
    // needs no PMU (it disables the timeslice): 174, listed.
    // So does the shared-mask rewrite test: 175, listed.
    // liteinst_in_guest_refuses_a_guest_socket_at_the_forwarding_number is not
    // in the exclusion filterset, so the host twin selects it too: 175.
    // The seven happens-before tests start no LiteInst guest: 183.
    // And the shared-stack mask test: 176, listed.
    // And the effective exit-signal test: 185, listed.
    // liteinst_in_guest_refuses_a_guest_socket_at_the_forwarding_number starts
    // an in-guest LiteInst guest (CPUID faulting), so it joins the hosted
    // CPUID-faulting exact-name filterset: 185 - 1 = 184.
    // a_child_exit_is_notified_once_with_linuxs_siginfo: 185.
    // And the two run_config_ tests: 187.
    // The seven happens-before fd-anchor tests start no LiteInst guest: 194.
    // The four in-guest-trap cli tests start in-guest LiteInst guests, so they
    // join the exact-name filterset: 187 + 4 - 4 = 187.
    // And the DBT syscall-numbering test: 188.
    // The happens-before unusable-spec refusal test starts no LiteInst guest: 196.
    // And the two inherited-seccomp tests: 197.
    // The four in-guest heap tests (heap at main, fork child, admitted SIGALRM,
    // preloaded libc wrappers) start in-guest LiteInst guests, so they join the
    // exact-name filterset: 195 + 4 - 4 = 195.
    // And the two DBT guest-environment tests: 198 + 2 = 200 (listed).
    // The three self-signal-death tests start in-guest LiteInst guests, so they
    // join the exact-name filterset: 200 + 3 - 3 = 200.
    // And the two killed-vfork-parent CLI tests: 202.
    // And the LiteInst runtime's fingerprint refusal test, which runs no guest
    // under Hermit and needs no CPUID faulting: 203, measured the same way. The
    // config-fingerprint guest test is excluded with the CPUID-faulting cases.
    // The DBT network namespace collision regression: 203, listed.
    // And the six relative-anchor CLI tests: 210.
    // And the skid-retry predicate unit test, which runs no guest: 204.
    // The FUTEX_WAKE count test starts no LiteInst guest: 212.
    // And the two child-wait CLI tests: 214.
    // Nor does the real-time timer expiry refusal test start a LiteInst guest:
    // 215.
    // Nor does the multithreaded process-directed signal test: 216.
    // Nor does the process-group signal test: 217.
    // And the nine posthook-anchor CLI tests: 226.
    // Nor do the four FUTEX_REQUEUE/FUTEX_WAKE_OP, FUTEX_LOCK_PI, futex
    // key/access and file-backed WAKE_OP tests: 230.
    // Nor do the three HERMIT_VERIFY_TMPDIR tests, which start ptrace guests or
    // none: 233.
    // Nor does the unsequentialized group-kill test: 234.
    // Nor does the futex re-wait test: 235.
    // The same six version-2 happens-before cases as test.cli need the PMU
    // preemption timer, which the hosted runner lacks; each was refused with
    // HERMIT_HB_HOLD_WITHOUT_PREEMPTION in hosted portable run
    // https://github.com/rrnewton/hermit/actions/runs/38110887257. They are
    // skipped by exact name here and run in privileged-test.pmu_cli_cases:
    // 235 - 6 = 229, listed with this node's arguments.
    // Nor does the unreadable futex word test: 230.
    // Nor does the pidfd multi-thread signal test: 231.
    // Nor do the three tests of signals to Hermit's own tasks: 234.
    // Nor do the two namespace-only kill(0) and terminal tests: 236.
    // Nor does the pending real-time timer expiration test: 237.
    // Nor do the cross-process SIGKILL parity and record-refusal tests: 239.
    // Nor do the four own-process signal CLI tests: 243.
    ("test.cli_on_host", 243),
    ("test.hermit_modes_on_host", 21),
    ("privileged-only-test.pmu_buck_chaos_cases", 6),
    // run_kvm_futex_wait_on_an_unreadable_word_matches_linux
    // (https://github.com/rrnewton/hermit/issues/4030) retains all 55: 56.
    ("privileged-only-test.cli_kvm", 56),
    ("privileged-only-test.pmu_buck_chaos_cases_on_host", 6),
    // And the KVM unreadable futex word test: 56.
    ("privileged-only-test.cli_kvm_on_host", 56),
    ("test.app_strict_verify_on_host", 8),
    ("test.arbitrary_binaries_on_host", 4),
    ("test.command_strict_verify_on_host", 9),
    // The host node carries the identical tests_misc selection.
    // And the same futex wake-order guest test: 89.
    // And the same procfs inode guest tests: 106.
    // And the same twelve descriptor-entry identity tests: 118.
    // And the same eleven child-subreaper guest tests: 129.
    // And the same five raw-clone process tests: 134.
    ("test.detcore_misc_on_host", 134),
    ("test.detcore_parallel_on_host", 5),
    // The host twin selects the same announcement-order test
    // (https://github.com/rrnewton/hermit/issues/3463).
    // io_buffers::event_tests::
    //   backend_runtime_bootstrap_syscall_is_handled_but_not_charged_to_guest_time
    // and io_buffers::event_tests::
    //   backend_runtime_bootstrap_window_charges_time_reads_and_caps_uncharged_syscalls
    // (https://github.com/rrnewton/hermit/pull/3430) retain all 850 prior
    // identities.
    // The host node carries the identical library/binary selection.
    // The host twin carries test.detcore_time's PMU-free filter: 2 of 31
    // (https://github.com/rrnewton/hermit/issues/3663).
    ("test.detcore_time_on_host", 2),
    // The two bootstrap-turn scheduler-time tests listed for test.detcore_unit
    // (https://github.com/rrnewton/hermit/issues/3517) retain all 876 prior
    // identities.
    // The three ephemeral host seed mount tests listed for test.detcore_unit
    // retain all 878 prior identities: 878 + 3 = 881.
    // The host twin selects the same 23 directory-stream tests
    // (https://github.com/rrnewton/hermit/pull/3226): 881 + 23 = 904.
    // The same 27 https://github.com/rrnewton/hermit/pull/3266 tests as
    // test.detcore_unit retain all 904 prior identities: measured 931.
    // The same sixteen https://github.com/rrnewton/hermit/pull/3778 tests
    // retain all 931: 931 + 16 = 947.
    // The same scheduler backoff and backend-user-address-limit tests retain
    // all 947: 947 + 2 = 949.
    // The host twin selects the same two inode-identity tests (951) and the
    // host-input observation test (952).
    // The same two forwarded-record tests retain all 952: 952 + 2 = 954.
    // The same three DETLOG forwarding-policy tests retain all 954: 957.
    // And the rebound-path test (958), the six mount-ID tests (964), and the
    // select-set test (965), and the two forwarded-record ordering tests (967).
    // And the three untraced-code tests (970).
    // The host twin selects the same
    // https://github.com/rrnewton/hermit/pull/3686 logdiff test: 970 + 1 = 971.
    // The same four signal-control tests retain all 971: 975.
    // And the host-independent /proc/modules test (976).
    // The host twin selects the same eleven exit-hold tests: 987.
    // The host twin selects the same six exit_dependencies tests: 993.
    // The host twin selects the same test: 994.
    // And the RNG digest exact-read test (995).
    // And the three write-only digest-read tests (998).
    // And the poll-family nfds extent test (999).
    // And the inode request-ordinal test (1000).
    // And the mtime-update inode test (1001).
    // The host twin selects the same test: 999.
    // The host twin selects both tests: 1000.
    // And the descriptor-alias interning test (1002).
    // And the same 66 https://github.com/rrnewton/hermit/pull/3361 signal tests
    // listed for test.detcore_unit (1041, measured).
    // The host twin selects the same tests, re-listed together: 1068.
    // And the seven SIGCHLD Phase A scheduler tests listed for
    // test.detcore_unit, re-listed with the host twin's selection: 1075.
    // And an_exit_that_names_another_process_installs_no_hold, re-listed: 1076.
    // The host twin selects the four unobserved-digest tests too: 1080.
    // The host twin selects the two logdiff tests too: 1082.
    // And backend_exit_reports_take_the_path_the_backend_capabilities_select,
    // re-listed: 1083.
    // The host twin loses the same deleted time test: 1082.
    // The host twin also selects the same procfs user-address-limit ordering
    // test and the two renamed iovecs tests: 1082 + 1 = 1083.
    // The host twin selects the same five fd.rs tests, re-listed: 1088.
    // And the same unidentified-stop SIGCHLD refusal test: 1088 + 1 = 1089.
    // The host twin selects the same background-signal test, re-listed: 1090.
    // The host twin selects the same seven B5a tests, re-listed: 1096.
    // And the same thirteen SIGALRM ledger tests, re-listed: 1110.
    // And the same five SIGALRM ledger control tests, re-listed: 1123.
    // The host twin selects the same four instruction_trap tests: 1127.
    // And the same fifteen fatal_core::tests: 1142.
    // And the same seven signal phase 1 table tests, re-listed: 1134.
    // And the schedstats sanitizer test: 1150.
    // And the rt_sigsuspend request signal test: 1151.
    // And the same four fatal_core::tests: 1155.
    // And the rt_sigsuspend mask-copy test: 1152.
    // And the same six detlog forwarding tests: 1161.
    // And the two signal phase 1 step I3 tests: 1152.
    // And the three happens-before parked-thread scheduler tests: 1167.
    // And the rt_sigsuspend scratch-alias test: 1157, listed.
    // And the two effective exit-signal tests: 1170, listed.
    // And the rt_sigsuspend cross-task signal test: 1169, listed.
    // And the directory-listing test: 1171.
    // And the two signal phase 1 step I4 tests: 1173.
    // And the ten child-exit SIGCHLD tests: 1184.
    // And the same two refused-turn-charge tests: 1186.
    // And the two happens-before syscall-occurrence scheduler tests: 1188.
    // And the same fourteen rejoin and accept-in-turn tests: 1202.
    // And the in-guest-trap site-patching check test: 1187.
    // And the same ten tests of the rt_sigsuspend-mask subset of
    // https://github.com/rrnewton/hermit/pull/3224: 1213, listed with the
    // host twin's arguments.
    // And the device-probe error description test: 1214.
    // And the same four futex wake-order tests: 1218.
    // And the same futex-waker requeue test: 1219.
    // And the eleven procfs inode tests: 1230.
    // And the three tests of naming procfs entries through the guest: 1233.
    // And the two tests of where procfs entries are named: 1235.
    // And the three tests of exited tasks and outside procfs files: 1238.
    // And the errno test: 1239.
    // And the mount table close test: 1240.
    // And the two tests of mount points in the guest's frame: 1242.
    // And the five tests of moved mount points and injected calls: 1247.
    // Less the test of the removed descriptor guess: 1246.
    // And the four tests of interrupted mount table reads: 1250.
    // And the eleven poll-deadline tests: 1261.
    // And the same eleven pause/nanosleep cross-task wake tests: 1241.
    // And the two /proc/cgroups tests: 1275.
    // And the eight vfork-parent barrier tests: 1283.
    // The same ten SIGALRM sleep boundary/resource tests retain all 1283: 1293.
    // And the eight relative-anchor scheduler tests: 1301.
    // And the three vfork barrier valve tests: 1304.
    // And the two FUTEX_WAKE count tests: 1306.
    // And the fifteen child-subreaper tests: 1321.
    // And the valve's consumer and bound tests: 1323.
    // And the same unknown-sleeping-mask test: 1324.
    // And the four child-wait deadlock tests: 1328.
    // And a_committed_sigchld_ends_a_gated_wait_unless_a_host_timed_source_can_post_it and a_precise_futex_wait_holds_a_committed_signal_only_if_a_host_timed_source_can_post_it (https://github.com/rrnewton/hermit/issues/4005): 1330.
    // And the four posthook-anchor tests: 1334.
    // And the five FUTEX_REQUEUE/FUTEX_WAKE_OP tests and the DONTUNMAP alias
    // test: 1340.
    // And the interrupted shared-key probe test: 1341.
    // And the terminal-deadlock hook test: 1342.
    // And the same descriptor rediscovery history test: 1343.
    // And a_background_call_with_a_temporary_mask_holds_the_schedule_until_the_kernel_installs_it (https://github.com/rrnewton/hermit/issues/3993): 1344.
    // And the three background-entry proof tests: 1347.
    // And the futex timeout clamp test: 1348.
    // And the Hermit thread-group test: 1349.
    // And the eleven real-time expiration fate tests: 1360.
    // And the chaos yield-epoch tests: 1384.
    // And the yielder harvest-gate unit test: 1385.
    // And the 29 SIGKILL-retirement tests: 1414.
    // And the rt_sigsuspend SIGKILL-arm test: 1415.
    // And the 14 own-process signal tests: 1429.
    ("test.detcore_unit_on_host", 1429),
    // Host variants select the same proc regressions and retain prior identities.
    // The host twin also selects the two clock_passthrough tests
    // (https://github.com/rrnewton/hermit/issues/1176).
    // It also selects the four verify-claim wording tests.
    // The host twin also selects the six reopened_pipe_progress tests
    // (https://github.com/rrnewton/hermit/pull/3534).
    // The host twin also selects the two dispatch_stats tests
    // (https://github.com/rrnewton/hermit/pull/3522).
    // The host twin also selects the two first_seen_mtime tests
    // (https://github.com/rrnewton/hermit/issues/3639).
    // The host twin also selects the two utimensat_mtime tests
    // (https://github.com/rrnewton/hermit/issues/3565).
    // The host twin also selects the five host-input-change verify tests (200).
    // And the two --passthru-opt host-input verify tests and the
    // private-page rebinding test (203).
    // And the four private-page guard tests (207).
    // The host twin also selects the 68 external_signal_interrupt tests
    // (https://github.com/rrnewton/hermit/pull/3361) (275, measured).
    // And the six verify signal-report tests (281).
    // And the blocking-stderr signal-report test (282).
    // And the agreed-guest-stdout signal-report test (283).
    // And the three stdout-file tests (286).
    // And the minimal-environment test (287, measured).
    // The host twin skips the same 25 PMU-timer cases: 262.
    // And the same eight fatal_core_capture tests: 270.
    // And the same scheduler-turn-cost clock-exhaustion test: 271.
    // And the robust-futex exec owner-death test: 272.
    // And the robust-futex exec no-wake test: 273.
    // And the run-mode rt_sigtimedwait SIGCHLD test: 274.
    // And the queued-signal rt_sigtimedwait test: 275.
    // And the large-pselect6 mutable-mask test: 276.
    // And the large-pselect6 tight-stack test: 277.
    // And the four targeted-chaos sched_yield tests: 281.
    // And the ppoll tight-stack test: 282.
    // And the stop-signal SIGHUP poll test: 283.
    ("test.hermit_integration_on_host", 283),
    // The host twin selects the same 4 GiB iced decode regression
    // (https://github.com/rrnewton/hermit/issues/3462), and the two fbcode
    // version-format tests (https://github.com/rrnewton/hermit/pull/3511),
    // the verify-claim wording test, and the top-level help and replay-hint
    // wording tests.
    // The host twin selects the same https://github.com/rrnewton/hermit/pull/3544 tests.
    // The host twin selects the same https://github.com/rrnewton/hermit/pull/3580 tests.
    // The host twin selects the same https://github.com/rrnewton/hermit/issues/3537 tests.
    // The host twin selects the same https://github.com/rrnewton/hermit/pull/3598 test,
    // and the two Cargo version-format tests
    // (https://github.com/rrnewton/hermit/pull/3547).
    // The host twin carries the same dispatch-record change (+2, -1;
    // https://github.com/rrnewton/hermit/pull/3522).
    // The host node carries the identical selection.
    // The host twin selects the same https://github.com/rrnewton/hermit/pull/3603 test.
    // The host twin selects the same eighteen
    // https://github.com/rrnewton/hermit/pull/3635 tests.
    // The host twin selects the same 562b7dd7a635 test.
    // The host twin selects the same
    // https://github.com/rrnewton/hermit/issues/1845 test.
    // The host twin selects the same https://github.com/rrnewton/hermit/pull/3219 test.
    // The host twin selects the same four
    // https://github.com/rrnewton/hermit/issues/3765 dbt_detconfig_tests,
    // the same six https://github.com/rrnewton/hermit/pull/3778 tests, and
    // drops the same eight https://github.com/rrnewton/hermit/issues/3520
    // tests: 822 + 6 - 8 = 820.
    // The host twin selects the same thirteen host_input_change tests (833).
    // The host twin selects the same four backend-capability controls:
    // 833 + 4 = 837.
    // The same forwarded-record bound test retains all 837: 837 + 1 = 838.
    // The same SaBRe DETLOG forwarding-policy test retains all 838: 839.
    // And the two guest-rebinding host_input_change tests (841).
    // And the same two LiteInst DETLOG forwarding-policy tests (843).
    // The host twin selects the same six exact-branch-counter tests: 843 + 6 = 849.
    // And the same --passthru-opt coverage test (850).
    // And the same two exact-branch-counter report tests (861).
    // And the same private-page range test (862).
    // The host twin selects the same 40
    // https://github.com/rrnewton/hermit/pull/3686 --max-log-bytes tests:
    // 862 + 40 = 902.
    // The host twin selects the same seven watcher and admission tests: 909.
    // The host twin selects the same six watchdog tests: 915.
    // The host twin selects the same three latch and kernel-floor tests: 918.
    // The host twin selects the same two auto-reap tests: 920.
    // The host twin selects the same three tests: 923.
    // And the same two record-version tests (925).
    // The host twin selects the same tests: 926.
    // The host twin selects the same tests: 928.
    // And the same blocked-wait backend-contract test and two inherited-terminal
    // tests (905, measured).
    // The host twin selects the same tests, re-listed together: 936.
    // The host twin selects the same six signal-report tests: 942.
    // The host twin selects the three exit-watch tests too: 953.
    // And the signal-report pipe-page test: 954.
    // And the two bisect epoch tests: 956.
    // And the analyze epoch test: 957.
    // And the PMU-profile validation test: 958.
    // And the same fatal-core cleanup test: 959.
    // And the analyze network-trace epoch test: 960.
    // And the record timer-support test: 961.
    // And the DBT determinism-loss evidence test: 962.
    // And the analyze schedule-input refusal test: 963.
    // And the same record-version and accept copy-out tests: 969.
    // And the same two shutdown replay tests: 971.
    // And the same two accept stand-in tests: 973.
    // And the same seven accept stand-in and peer-address tests: 980.
    // And the scheduler-turn-cost CLI test: 981.
    // And the same sixteen run config tests: 997.
    // The same launch refusal test: 998.
    // And the same backgrounded-accept record-version test: 999.
    // And the nine in-guest-trap tests: 1006.
    // And the same version 2 frame-descriptor test: 998.
    // And the same two happens-before refusal tests: 1011.
    // And the same four inherited-seccomp tests: 1013.
    // And the same futex record-version test: 1016.
    // And the futex-wake CLI test: 1017.
    // And the vfork release-edge record-version test: 1018.
    // And the version-2 preemption refusal test: 1019.
    // And the child-subreaper record-version test: 1020.
    // And the same two signal-target metadata tests: 1022.
    // And the 0x129 record-version refusal test: 1023.
    // And the two run_failed tests: 1025.
    // And the raw-clone-process record-version test: 1026.
    // And the same four DBT config transport tests: 1029; two historical-test
    // renames preserve existing cases without changing this count.
    // And the futex re-wait record-epoch test: 1031.
    // And the futex-wait record-epoch test: 1032.
    // And the pidfd wake record-epoch test: 1033.
    // And the namespace-only group-kill unit test: 1034.
    // And the zombie-kill record-epoch test: 1035.
    // And metadata::tests::pre_epoll_pwait_recordings_are_refused_by_the_version_gate: 1036.
    // And the zombie-sigqueueinfo record-epoch test: 1037.
    // Three physical constructor-registration controls retain all prior cases.
    // And the backend timer-delivery test: 1041.
    // And the thread-id kill record-epoch test: 1042.
    // And metadata::tests::pre_yield_readmission_recordings_are_refused_by_the_version_gate
    // (Codex review of https://github.com/rrnewton/hermit/pull/4120): 1043.
    // And metadata::tests::pre_tight_stack_ppoll_recordings_are_refused_by_the_version_gate
    // (Claude review of https://github.com/rrnewton/hermit/pull/4127): 1044.
    // And the gone-sigqueueinfo record-epoch test: 1045.
    // And the SIGKILL-retirement record-version and replay-refusal tests: 1047.
    // And metadata::tests::pre_epoll_pwait2_recordings_are_refused_by_the_version_gate
    // (https://github.com/rrnewton/hermit/issues/4088): 1048.
    // And the orphaned-group record-epoch test: 1049.
    ("test.hermit_unit_on_host", 1049),
    ("test.ignored_syscall_regressions_on_host", 4),
    // The host node carries the identical selection.
    // test-harness no_retry_flag_turns_framework_retries_off retains all 763 prior identities.
    // The fold-2 resolver test retains all 764 prior identities.
    // The 13 Buck/RE harness tests (https://github.com/rrnewton/hermit/pull/3507)
    // retain all 765 prior identities.
    // The host twin selects the same https://github.com/rrnewton/hermit/pull/3544 tests.
    // The dagrun pin guard retains all 778 prior identities.
    // Its every-spelling companion retains all 779 prior identities.
    // The dispatch-record runner test retains all 783 prior identities
    // (https://github.com/rrnewton/hermit/pull/3522).
    // The selection includes the S12 parity tests above: 31 identities
    // added and one removed, whose test S12 rewrote under a new name, so 783
    // of its 784 prior identities are retained.
    // The six import-mode tests of https://github.com/rrnewton/hermit/pull/3542
    // retain all 813 prior identities (measured 819, as above), and the three
    // fold-3 run-type tests retain all 819.
    // The three fold-4 variant tests retain all 822 prior identities.
    // The three fold-5 replay-variant tests retain all 825 prior identities.
    // The six later cpu_evidence, glibc_compat and runner tests listed for
    // test.regular_crates retain all 828 prior identities, and the four
    // SKID-RETRY tests of https://github.com/rrnewton/hermit/issues/1845 listed
    // there retain all 834. The home-path DAG text test listed there retains
    // all 838, and the retained-PASS re-decision test listed there retains
    // all 839.
    // committed_buck_e2e_selection_replaces_22_nodes_with_18 retains all 840.
    // The two detcore-model seed mount tests listed for test.regular_crates
    // retain all 841 prior identities.
    // The detcore-model named-seed test listed for test.regular_crates retains
    // all 843 prior identities.
    // The three sync-cells tests of 32053b6d listed for test.regular_crates
    // bring the measured count to 847, and the optional-cell sync test of
    // 6166181d8f listed there to 848, and the three tests of 76521affca and
    // 9fd0c01a39 listed there to 851.
    // The invocation-cgroup CPU tests listed for test.regular_crates add 33
    // identities and remove one, so 850 of the 851 prior identities are
    // retained (+32). The spawn-failure test listed there retains all 883.
    // The two process-group scan tests listed there retain all 884.
    // The three parity import tests listed there retain all 886, and the
    // retained-command-line test listed there retains all 889. The
    // main-ancestor floor admission test listed there retains all 890. The
    // seven stat re-read tests listed there retain all 891, and the
    // declared-exit pressure history test listed there retains all 898. The
    // focused run-type reproducer test listed there retains all 899, and the
    // two replay-retry tests listed there retain all 900. The rr fold listed
    // there removes three tests, rewrites them under new names and adds three,
    // so 899 of the 902 prior identities are retained (+3).
    // The two timed-out-match stress_series tests listed there: 905 + 2 = 907.
    // The 16 https://github.com/rrnewton/hermit/pull/3777 tests listed there
    // retain all 907, and the ten https://github.com/rrnewton/hermit/pull/3778
    // tests listed there retain all 923: 923 + 10 = 933. The seven post-facto
    // network replay fix tests listed there retain all 933: 933 + 7 = 940.
    // The four tests of the fixes to the post-facto review of
    // https://github.com/rrnewton/hermit/pull/3791 listed there retain all
    // 940: 940 + 4 = 944.
    // The three host-input-change retry tests listed there (947), and the
    // inode-RPC fingerprint test (948). The three hermit-test-workdir bind
    // tests listed there retain all 948: 951.
    // The fourteen backend-capability controls listed there retain all
    // 951: 951 + 14 = 965.
    // The delegated write-back test listed there retains all 965: 966.
    // The legacy-form host-input test listed there retains all 966: 967.
    // The same detcore-sabre forwarding-policy test retains all 967: 968.
    // The same pinned-workspace-compile ordering test retains all 968: 969.
    // The three exact-branch-counter tests listed there retain all 969: 972.
    // The two network trace codec tests listed there retain all 972: 974.
    // The row-recording exact-branch-counter test listed there retains all 974: 975.
    // The report exact-branch-counter test listed there retains all 975: 976.
    // The same detcore-sabre fingerprint test retains all 976: 977.
    // The two shared_dequeue_timers tests listed there retain all 977: 979.
    // The path-included validate_plan test listed there retains all 979: 980.
    // The chaos determinism-failure test listed there retains all 980: 981.
    // The same detcore-model legacy-form test retains all 981: 982.
    // The ineligible-execution-path series test listed there retains all 982: 983.
    // The hosted script unit-test twin test listed there retains all 983: 984.
    // The two detcore-dbt handler-error tests listed there retain all 984: 986.
    // The two framework-retry ledger tests listed there retain all 986: 988.
    // The two detcore-model legacy-form tests of
    // https://github.com/rrnewton/hermit/pull/3361 listed there retain all 988: 990.
    // The prepared-input field-naming test of 2a43eb81 listed there retains all 990: 991.
    // The Buck cells wall-bound test listed there retains all 991: 992.
    // The parity-population identity changes listed for test.regular_crates
    // keep the count at 991, its not-sampled test listed there makes 992, and
    // the two review-fix tests listed there make 994.
    // The compare_io_buffers refusal test listed there retains all 995: 996.
    // The host twin selects the same detcore-model legacy-form test: 997.
    // And the same fork-window cgroup test: 998.
    // It selects the detcore-model checkpoint-filter test too: 999.
    // And the detcore-dbt determinism-loss record test: 1000.
    // And the same in_guest_detlog_forward_policy legacy-form test: 1001.
    // And the same committed-selection preparation test: 1002.
    // And the two detcore-model replaying tests listed there: 1004.
    // And the same test-free declaration test: 1005.
    // And the same six scheduler-turn-cost tests: 1011.
    // The same three detcore-model happens-before tests: 1014.
    // And the same test-free declaration test: 1005; removed with it: 1004.
    // And the same two in-guest-trap tests: 1013.
    // And the same in-guest CPUID-faulting test: 1014.
    // And the same shared standalone population test: 1018.
    // Less the same seven retired-parity tests: 1011.
    // And the same four corpus additional-backend tests: 1015.
    // And the same three futex-wake config tests: 1018.
    // And the three detcore-model inherited_seccomp tests: 1021.
    // And the probed in-guest timeslice test: 1022.
    // And the same nine glibc_compat unwind tests: 1031.
    // And the config-fingerprint mismatch test: 1032.
    // The native per-physical-network namespace regression: 1023, listed.
    // And the five relative-anchor model tests: 1038.
    // And the duplicated-field model test: 1039.
    // And the guest permission-denied first-run-rejection test: 1040.
    // And the same allocator prerequisite mutation test: 1040.
    // And the same four retained cell-results tests: 1046.
    // And the two posthook-anchor model tests: 1048.
    // And the same four retained-census, transcoding, depth and key-order tests: 1052.
    // The DBT startup-authority refusal regression retains all 1048 prior identities.
    // And the same depth-boundary, repeated-key and extension-count tests: 1056.
    // And the run_failed runner test: 1049.
    // And the same six model/runtime config transport tests: 1064.
    // And the failed-match pressure evidence test: 1059.
    ("test.regular_crates_on_host", 1065),
    ("test.rr_suite_contract_on_host", 1),
    // The host twin also selects the two startup-order tests, and the three
    // SaBRe host-input tests (12).
    // And the same target-scoped DETLOG forwarding test (13), and the same
    // forked-child stderr test (14), and the same two forwarding-socket
    // tests (16), the same missing-records refusal test (17), and the same
    // guest-stderr-file test (18), and the same guest-socket test (19).
    // And the same exec-image forwarding test: 20.
    // And the same guest-RDTSC test: 21, and the same signal-handler and
    // early RDTSC tests: 23, and the same re-entrant RDTSC test: 24, and the
    // same stack-limit and early limit-change tests: 26, and the same marker
    // limit-read test: 27.
    ("test.sabre_examples_on_host", 27),
];

pub(super) fn structured_result_producer_kind(tag: &str) -> Option<StructuredResultProducerKind> {
    for (tags, kind) in [
        (
            NEXTEST_RESULT_PRODUCERS,
            StructuredResultProducerKind::Nextest,
        ),
        (
            TEST_HARNESS_RESULT_PRODUCERS,
            StructuredResultProducerKind::TestHarness,
        ),
        (
            ENVELOPE_RESULT_PRODUCERS,
            StructuredResultProducerKind::Envelope,
        ),
        (
            APPLICATION_RESULT_PRODUCERS,
            StructuredResultProducerKind::Applications,
        ),
    ] {
        if tags.contains(&tag) {
            return Some(kind);
        }
    }
    None
}

fn expected_nextest_count(tag: &str) -> Option<u64> {
    NEXTEST_EXPECTED_COUNTS
        .iter()
        .find_map(|(candidate, count)| (*candidate == tag).then_some(*count))
}

#[derive(Clone, Copy)]
struct ManifestSpec {
    lane: &'static str,
    category: &'static str,
    test: Option<&'static str>,
    mode: Option<&'static str>,
    backend: Option<&'static str>,
    /// The run type whose labelled cells the node selects with
    /// `test-harness run --label`; `None` selects the default run type
    /// (runner::DEFAULT_RUN_TYPE).
    label: Option<&'static str>,
}

impl ManifestSpec {
    fn materialize(self) -> DagManifest {
        DagManifest {
            lane: self.lane.into(),
            category: self.category.into(),
            test: self.test.map(Into::into),
            mode: self.mode.map(Into::into),
            backend: self.backend.map(Into::into),
        }
    }
}

#[derive(Clone, Copy)]
struct HintSpec {
    resources: &'static [(&'static str, i64)],
    est_duration_s: f64,
    rss_baseline_bytes: Option<i64>,
    hard_mem_max_bytes: Option<i64>,
    classification: StepClass,
    preferred_inner_jobs: Option<i64>,
    measured_effective_cores: Option<f64>,
    measured_cpu_utilization: Option<f64>,
}

impl HintSpec {
    fn materialize(self) -> ResourceHint {
        ResourceHint {
            resources: self
                .resources
                .iter()
                .map(|(name, demand)| ((*name).into(), *demand))
                .collect(),
            est_duration_s: self.est_duration_s,
            rss_baseline_bytes: self.rss_baseline_bytes,
            rss_baseline_inner_jobs: None,
            hard_mem_max_bytes: self.hard_mem_max_bytes,
            classification: self.classification,
            preferred_inner_jobs: self.preferred_inner_jobs,
            measured_effective_cores: self.measured_effective_cores,
            measured_cpu_utilization: self.measured_cpu_utilization,
        }
    }
}

#[derive(Clone, Copy)]
struct StaticStepSpec {
    group: &'static str,
    job: &'static str,
    desc: &'static str,
    description: &'static str,
    labels: &'static [&'static str],
    cmd: &'static str,
    cmdtype: CmdType,
    manifest: Option<ManifestSpec>,
    integration_test_binaries: Option<&'static [&'static str]>,
    deps: &'static [&'static str],
    env: &'static [(&'static str, &'static str)],
    hint: HintSpec,
    networkonly: bool,
    engine_only: bool,
    timeout: i64,
    cpu_timeout: i64,
    jobs_flag: Option<&'static str>,
    jobs_env: Option<&'static str>,
}

impl StaticStepSpec {
    fn materialize(self) -> Step {
        let tag = format!("{}.{}", self.group, self.job);
        let producer = structured_result_producer_kind(&tag);
        let mut env = self
            .env
            .iter()
            .map(|(name, value)| ((*name).into(), (*value).into()))
            .collect::<BTreeMap<String, String>>();
        if let Some(expected) = expected_nextest_count(&tag) {
            assert_eq!(
                producer,
                Some(StructuredResultProducerKind::Nextest),
                "{tag} has an expected Nextest count but is not a declared Nextest producer"
            );
            assert!(
                env.insert("NEXTEST_EXPECTED_EXECUTED".into(), expected.to_string())
                    .is_none(),
                "{tag} declares NEXTEST_EXPECTED_EXECUTED twice"
            );
        }
        if producer == Some(StructuredResultProducerKind::Nextest)
            || self.cmd.contains("nextest-binaries.rs executable ")
        {
            let selection = crate::nextest_build_selections::for_step(&tag)
                .unwrap_or_else(|| panic!("{tag} has no declared Cargo build selection"));
            assert!(
                env.insert(
                    crate::nextest_binaries::SELECTION_ENV.into(),
                    serde_json::to_string(selection).expect("string list is serializable"),
                )
                .is_none()
            );
            assert!(
                env.insert(crate::nextest_binaries::REQUIRED_ENV.into(), "1".into())
                    .is_none()
            );
        }
        Step {
            group: self.group.into(),
            job: self.job.into(),
            desc: self.desc.into(),
            description: self.description.into(),
            labels: self.labels.iter().map(|value| (*value).into()).collect(),
            cmd: self.cmd.into(),
            cmdtype: self.cmdtype,
            manifest: self.manifest.map(ManifestSpec::materialize),
            integration_test_binaries: self
                .integration_test_binaries
                .map(|values| values.iter().map(|value| (*value).into()).collect()),
            result_manifests: Some(
                producer
                    .map(|kind| {
                        vec![ResultManifest::StructuredTestResults(kind.declaration(
                            tag.clone(),
                            self.manifest.as_ref().map(|manifest| manifest.category),
                        ))]
                    })
                    .unwrap_or_default(),
            ),
            deps: self.deps.iter().map(|value| (*value).into()).collect(),
            env,
            hint: self.hint.materialize(),
            networkonly: self.networkonly,
            engine_only: self.engine_only,
            delegated_children: false,
            timeout: self.timeout,
            cpu_timeout: self.cpu_timeout,
            jobs_flag: self.jobs_flag.map(Into::into),
            jobs_env: self.jobs_env.map(Into::into),
            skip_reason: None,
            write_domains: None,
            write_domain_guarantee: None,
            explains: Vec::new(),
            fail_fast_family: PMU_MEMORY_FAILURE_FAMILY_MEMBERS
                .contains(&tag.as_str())
                .then(|| PMU_MEMORY_FAILURE_FAMILY.into()),
        }
    }
}

/// The run type a static manifest bucket node selects by label
/// ([`ManifestSpec::label`]), or `None` for the default run type. A node's
/// hosted twin (`<tag>_on_host`) selects what the node selects.
pub(super) fn manifest_run_type(tag: &str) -> Option<&'static str> {
    let tag = tag.strip_suffix("_on_host").unwrap_or(tag);
    STATIC_STEPS.iter().find_map(|spec| {
        (tag.split_once('.') == Some((spec.group, spec.job)))
            .then_some(spec.manifest)
            .flatten()
            .and_then(|manifest| manifest.label)
    })
}

/// Every run type some static manifest bucket node selects by label.
pub(super) fn manifest_run_types() -> std::collections::BTreeSet<&'static str> {
    STATIC_STEPS
        .iter()
        .filter_map(|spec| spec.manifest.and_then(|manifest| manifest.label))
        .collect()
}

pub(super) fn config() -> DagConfig {
    let mut steps = STATIC_STEPS
        .iter()
        .copied()
        .map(StaticStepSpec::materialize)
        .collect::<Vec<_>>();
    for (parent, job, description, command) in [
        (
            "cli",
            "isolated_dbt_workdir",
            "Verify sequential and concurrent physical DBT working directories, a bound DBT run in the adapter's own user namespace, and retain the existing blocked-input failure control. The exact selected tests require the pinned-root marker, prepared CLI executable, canonical INFO and IO-buffer comparison, and unchanged per-test CPU/wall policy. Retained files additionally inherit a 64 MiB per-file limit; a cap hit remains failure.",
            concat!(
                "export PATH=\"$PWD/ci/rust-script-bin:$PATH\"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT=\"$PWD/target/ci/rust-scripts\"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ",
                "test \"${HERMIT_E2E_EMPTY_WORKDIR:-}\" = /test && ",
                "prlimit --fsize=67108864:67108864 -- ./ci/run-with-reverie-dbt-budget.sh ./ci/run-nextest-counted.sh ",
                "${CI:+--profile ci} -p hermit --features third-party-backends --test cli -j 1 ",
                "-E 'test(=run_dbt_verifies_fresh_physical_workdirs) | test(=run_dbt_binds_in_a_user_namespace_of_its_own) | test(=run_dbt_strict_returns_with_blocked_stdin_source)' -- --include-ignored"
            ),
        ),
        (
            "regular_crates",
            "isolated_detcore_workdir",
            "Execute the marked in-process testutils control through its existing prepared regular-crates selection. Require exactly one terminal test covering two generic Tool runs and two Detcore strict-log repetitions, with the original CPU/wall policy and a 64 MiB inherited per-file limit.",
            concat!(
                "export PATH=\"$PWD/ci/rust-script-bin:$PATH\"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT=\"$PWD/target/ci/rust-scripts\"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ",
                "test \"${HERMIT_E2E_EMPTY_WORKDIR:-}\" = /test && ",
                "prlimit --fsize=67108864:67108864 -- ./ci/run-with-reverie-dbt-budget.sh ./ci/run-nextest-counted.sh ",
                "${CI:+--profile ci} --workspace --exclude hermit-detcore --exclude hermit --exclude hermetic_infra_hermit_flaky-tests ",
                "-E 'package(=detcore-testutils) & test(=tests::isolated_workdir_reaches_the_in_process_guest_when_requested)'"
            ),
        ),
    ] {
        let parent = *STATIC_STEPS
            .iter()
            .find(|step| step.group == "test" && step.job == parent)
            .unwrap();
        steps.push(
            StaticStepSpec {
                job,
                desc: description,
                description,
                labels: &["full", "portable"],
                cmd: command,
                hint: HintSpec {
                    preferred_inner_jobs: Some(1),
                    ..parent.hint
                },
                jobs_flag: Some(""),
                jobs_env: Some("NEXTEST_TEST_THREADS"),
                ..parent
            }
            .materialize(),
        );
    }
    let gate = *STATIC_STEPS
        .iter()
        .find(|step| step.group == "gate" && step.job == "manifest")
        .expect("the ordinary manifest gate is authored");
    assert_eq!(
        TOOL_SELF_TEST_NODES
            .iter()
            .map(|node| node.name)
            .collect::<Vec<_>>(),
        crate::validation_dag::TOOL_SELF_TESTS
            .iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>(),
        "every tool self-test needs exactly one measured node budget"
    );
    for node in TOOL_SELF_TEST_NODES {
        assert_eq!(
            node.cmd,
            format!(
                "{RUST_SCRIPT_ENVIRONMENT}target/debug/test-harness selftest {}",
                node.name
            ),
            "selftest.{} must run exactly its own self-test",
            node.name
        );
        steps.push(
            StaticStepSpec {
                group: crate::validation_dag::TOOL_SELF_TEST_GROUP,
                job: node.name,
                desc: node.desc,
                description: node.description,
                cmd: node.cmd,
                deps: &["gate.manifest"],
                env: &[],
                hint: HintSpec {
                    rss_baseline_bytes: Some(node.rss_baseline_bytes),
                    hard_mem_max_bytes: Some(node.memory_bytes),
                    preferred_inner_jobs: Some(node.inner_jobs),
                    ..gate.hint
                },
                timeout: node.timeout,
                cpu_timeout: node.cpu_timeout,
                ..gate
            }
            .materialize(),
        );
    }
    DagConfig {
        description: "Hermit validation superset; select quick, portable, hosted-portable, full, super, privileged, or hosted-privileged by step label".into(),
        default_step_timeout: 600,
        resource_caps: BTreeMap::from([("manifest_guest".into(), 8), ("integration_test_binaries.cli".into(), 1), ("integration_test_binaries.hermit_modes".into(), 1)]),
        steps,
        ..DagConfig::default()
    }
}

pub(super) const RUST_SCRIPT_ENVIRONMENT: &str = r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; "########;

/// The measured budget and width of one `selftest.<name>` node. Every other
/// field is the ordinary manifest gate's: the same labels, the admitted width
/// carried through CARGO_BUILD_JOBS, and no appended -j argument.
struct ToolSelfTestNode {
    name: &'static str,
    desc: &'static str,
    cmd: &'static str,
    description: &'static str,
    timeout: i64,
    cpu_timeout: i64,
    memory_bytes: i64,
    /// The declared working set, which the plan view prints and profile
    /// estimates start from; the hard bound above is what dagrun enforces.
    rss_baseline_bytes: i64,
    /// The node's preferred_inner_jobs, which dagrun also carries through
    /// CARGO_BUILD_JOBS.
    inner_jobs: i64,
}

// Each cap below is at least 1.5 times the largest measured sample named in
// its description.
const TOOL_SELF_TEST_NODES: &[ToolSelfTestNode] = &[
    ToolSelfTestNode {
        name: "scorecard",
        desc: "Compatibility scorecard regression-tier self-test and tracked-output check",
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; target/debug/test-harness selftest scorecard"########,
        description: r########"TOOL SELF-TEST NODE, REGRESSION TIER 2026-09-29 (https://github.com/rrnewton/hermit/issues/3381): runs `ci/compat-envelope/scorecard.rs self-test-and-check` through `test-harness selftest scorecard`, which is the scorecard's in-process brackets followed by the tracked-output check. The brackets create no scratch Git repository, run none of the scorecard's own commands and read no ledger; they read only this checkout's commits and the checked-in fixture under ci/compat-envelope/testdata/series-snapshot, with 57 git commands per run (the undivided self-test ran 3182). The brackets that do build scratch repositories (the command-line, retained-history and series-worktree brackets) run in selftest.scorecard_commands. This node is a leaf: a failure makes the validation red and no node depends on it. It keeps the gate's admitted two-core width (preferred_inner_jobs=2 carried through CARGO_BUILD_JOBS, no appended -j). NO CARGO: the tracked-output check derives the manifest-plan rows with the `hermit-manifest-plan` binary that setup.manifest_plan built beside test-harness; test-harness passes it through HERMIT_MANIFEST_PLAN_BIN (ToolSelfTest.manifest_plan_helper), and the hosted checks job unpacks both from one tarball. So this node runs no Cargo command: with a `cargo` first on PATH that fails when called and an empty CARGO_TARGET_DIR, it passed three times and `cargo` was never called. Before this it ran `cargo build -p hermit-manifest-plan`. The hosted tarball carries no Cargo fingerprints, so that build was cold there: 48.30 s wall and 89.53 CPU seconds at two jobs and load average 93, three times this node's 30-second CPU cap. Locally it waited on Cargo's target/debug lock while a concurrent debug build held it (in the full local validation of Hermit 84da7b816939, build.workspace held it from 379.0 to 997.9 seconds after the start, and the undivided selftest.scorecard started at 383.0 seconds). If the helper beside test-harness is missing, test-harness fails the node instead of building one. The Cargo-built path is still exercised by selftest.scorecard_commands and by the scorecard.compatibility nodes. Measured 2026-09-29 on the development host recorded in docs/TESTING_ENVIRONMENTS.md, "Named measurement hosts": in a small clone of Hermit dc395f8ae295 the node's command took about 2.8 CPU seconds (2.79-3.00 over three runs at load average 59 with the host's telemetry git wrapper first on PATH, 2.01-2.23 with the unwrapped git). In a retained validation checkout of the same commit, a worktree of the shared development clone, it took 4.19-4.91 CPU seconds over three runs at load average 65 with GIT_NO_LAZY_FETCH set, the guard the object-store-independence bracket now puts on its absent-commit probe, and 4.96-6.58 CPU seconds without it. That clone had an 8,531-line configuration and 24,818 refs, which nearly doubled the CPU of each of the node's 62 wrapped git calls (47 against 25 ms), and a promisor remote, which made the unguarded probe for the deliberately absent commit 5ca1ab1e0000000000000000000000000000dead fetch four times before failing: about 1 CPU second, and 13.31 s wall in one run. In two full local validations of dc395f8ae295 beside release builds at load average 50-200, before the guard, the node recorded 7.19 CPU seconds in one and 15.65 in the other, which exceeded the 15-second cap then in place: the CPU varies with host load as well as the wall. Earlier samples with the telemetry git wrapper first on PATH: `self-test` alone took 1.95-6.90 s wall and 1.95-3.82 CPU seconds over sixteen runs at load average 57-163, and 8.40 and 13.32 s wall with 2.59 and 2.74 CPU seconds while a workspace `cargo doc` ran beside it. Before the split the undivided command took 1183.84 s wall and 914.53 CPU seconds at load average 215-233. SYSTEM CPU IN LOCAL VALIDATIONS, measured 2026-10-04 and 2026-10-05 UTC on the same host: 52 retained local validation runs from 2026-09-30 to 2026-10-05 UTC ran this node under a 15-CPU-second cap. Its user CPU stayed between 2.08 and 4.06 seconds while its system CPU ranged from 0.68 to 13.46 seconds. 48 runs passed at 2.87-10.37 CPU seconds. Three finished the work and were counted over the cap: 15.09, 15.24 and 15.65 CPU seconds at Hermit 8503b4fc, 66378ba1 and dc395f8a, of which 11.02, 12.89 and 12.57 seconds were system CPU. The fourth, at Hermit c1312a56, was stopped at 15.29 CPU seconds after the brackets had finished and before the tracked-output check had; the brackets are about 77 percent of the node's CPU (4.25-4.70 of 5.53-6.14 CPU seconds in three standalone runs), so that run would have taken at most about 20 CPU seconds. The extra system CPU is not work this node adds. Across the 52 runs its system CPU correlates 0.84-0.94 with the system CPU of the light checks that start within the same three seconds (check.dagrun_naming 0.94, e2e.audit_compile_c_programs 0.93, check.portability_paths and check.script_sigpipe 0.91, check.shard_coverage 0.89, selftest.dbt_budget 0.84), and with neither the host load average (0.18) nor host CPU, memory or I/O pressure (0.04-0.14). In the 66378ba1 run those checks and check.run_node_args spent 1.7 to 5.5 times their median system CPU (this node 5.2 times), while every one of them kept its user CPU within 1.21 times its median. That run already used the unwrapped git: the validation driver's git substitution was applied. A second full local validation of 66378ba1 40 minutes later, also with the unwrapped git and with identical node output, passed this node at 10.30 CPU seconds (2.47 user, 7.82 system) at load average 249-256, against 15.24 (2.35 user, 12.89 system) at load average 176-199 in the failing run; the six checks that started within three seconds before this node in both runs (check.dagrun_naming, e2e.audit_compile_c_programs, check.portability_paths, check.script_sigpipe, check.shard_coverage and check.run_node_args) each spent 1.44 to 1.75 times as much system CPU in the failing run as in the passing one (this node 1.65 times), with user CPU within 6 percent. Standalone at 66378ba1, three runs each at load average 172-190, counted by a cgroup as dagrun counts this node, the command took 2.18-2.90 CPU seconds with the unwrapped git and 5.25-9.15 with the telemetry git wrapper first on PATH; /usr/bin/time reported only 3.99-5.07 for the wrapped runs because it misses the wrapper's detached telemetry logger, which the cgroup charges. With selftest.scorecard_commands, selftest.pressure_test, selftest.validate_rs and scripts/run-script-tests.sh running beside it at load average 252-272, four completed runs took 2.93-4.52 CPU seconds; two more in that set failed after waiting 30 seconds for the scorecard write lock. The telemetry git wrapper's system CPU also varies with host state (the gate recorded 32 to 374 system seconds for unchanged work, 2026-09-22 to 2026-09-28). CAPS: 30 CPU seconds is 1.5 times the largest validation sample, the stopped c1312a56 run's estimated 20 seconds, and 1.9 times the largest counted one (15.65 s). The cap was 15, 1.5 times the 10-second goal of https://github.com/rrnewton/hermit/issues/3381; that goal still describes this node's own work, the user CPU and the unwrapped standalone samples above, but not the system CPU that the validation's concurrent start adds to every node. The wall cap is 120 seconds: up to 30 seconds waiting for the scorecard write lock, which the check takes, plus 90 seconds, 6.8 times the largest standalone wall sample (13.32 s) and 3.7 times the largest validation wall above (24.43 s, at 66378ba1), because the wall of the same work varied 6.8-fold with host load. Memory keeps the 5-GiB bound the seven audits shared in the gate."########,
        timeout: 120,
        cpu_timeout: 30,
        memory_bytes: 5368709120,
        rss_baseline_bytes: 5368709120,
        inner_jobs: 2,
    },
    ToolSelfTestNode {
        name: "scorecard_commands",
        desc: "Compatibility scorecard commands-tier self-test, when its paths change",
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; target/debug/test-harness selftest scorecard_commands"########,
        description: r########"TOOL SELF-TEST NODE, COMMANDS TIER 2026-09-29 (https://github.com/rrnewton/hermit/issues/3381): runs `ci/compat-envelope/scorecard.rs self-test-commands` through `test-harness selftest scorecard_commands`: the regression-tier brackets of selftest.scorecard again, then the brackets that run the scorecard's own commands against a scratch clone of this checkout, a scratch ledger repository and a scratch reverie repository (the command-line, retained-history and series-worktree brackets, including the empty-result command, the replacement-ref guard, the dirty-worktree refusal and the missing-tree refusal). It needs the pinned ledger corpus through DEV_HERMIT_TEST_LEDGER_ROOT. WHEN IT RUNS: test-harness decides before starting it (ci/manifest-plan/src/self_test_selection.rs). It runs when any path changed since the merge base with origin/main, committed or not, is under one of its known inputs (SCORECARD_INPUTS in ci/manifest-plan/src/validation_dag.rs: the directories ci/compat-envelope/, ci/manifest-plan/, detcore-model/, ci/rust-script-bin/ and tests/e2e/manifests/, and the paths agent-utils (a submodule, whose commit moving counts), .gitmodules, Cargo.toml, Cargo.lock, rust-toolchain.toml, scripts/lib/rust_script_prelude.rs, ci/prepare-rust-scripts.sh, ci/prepare-scorecard-self-test-corpus.sh and ci/expected-e2e-plan.json; a deleted or renamed path counts under its old and its new name); it always runs when HEAD is contained in main (refs/heads/main or any refs/remotes/*/main); it also runs when the change set cannot be resolved (a shallow clone, no origin/main, an empty change set, any git failure) and when HERMIT_SELFTEST_SELECTION=all. Otherwise it prints one line containing "NOT RUN by file selection:" that names the merge base and the changed paths, and passes; that line is the node's summary. This node is a leaf: a failure makes the validation red and no node depends on it. PARTS: the tier runs as four child processes (`self-test-commands --parts 4` is the default). Each child builds the same scratch fixtures and runs its share of the 42 commands segments, assigned by their measured times; every segment runs in exactly one child. The child that runs a segment restores the fixture paths it lists (every fixture path except the Git repositories' internals, of which only HEAD, packed-refs and refs/ count) and fails if the segment changed the clone's Git status, any fixture repository's index, worktree list or configuration, or the clone's ignored files, none of which a restore returns; reflogs, object stores and state outside the fixtures (the plan caches, the shared Cargo target directory) are not compared. The parent fails unless every child passes and all report the same segment table, the same fixture state before each segment and the same final fixture state; that shows the children built the same fixtures, and is not a second check of a segment's restore, since each segment runs in one child. Each child's output is copied line by line as it arrives, after `[part K/N]`, and the parent waits for every child before it returns. Measured 2026-10-06 on the development host recorded in docs/TESTING_ENVIRONMENTS.md, "Named measurement hosts", at Hermit 6a029e95 plus this change, with the prepared scorecard binary, each run in a systemd scope, alternating with main's single-process tier: four parts under a 6-core quota and a 14 GiB bound took 77.68-78.90 s wall and 396.5-403.1 CPU seconds in four runs at load average 14-47 (one more run failed in 12 s because a concurrent cargo test in the same checkout swapped the shared helper binary, which the tier refuses); without the Git-state check above they took 72.45-81.85 s and 363.6-402.3 CPU seconds in six runs at load average 14-36, and 95.74 s and 521.1 CPU seconds (307.2 of them system) in one run at load average 73; main's tier under its 2-core quota and 5 GiB bound took 207.10-209.36 s and 274.3-276.3 CPU seconds in five runs. A 4-core quota gave 91.26-93.84 s, so the node asks for 6 cores. The extra CPU is the fixture setup each child repeats, mostly system CPU. MEMORY: each child holds the tier's own working set, about 0.9-1.2 GB resident (the decoded pinned ledger corpus, the derived catalogue and the in-process commands' data; main's single process holds 1.3 GB plus 0.67 GB for its observe-results child, and the corpus and derived fixture state are freed once the fixtures are written), so four children peaked at 5.96-6.38 GB anonymous and 7.94-9.71 GB cgroup memory.peak (page cache included; GB here is 10^9 bytes); before the corpus was freed, six runs (under 4- and 6-core quotas) peaked at 6.95-7.43 GB anonymous and 9.40-9.94 GB memory.peak. The declared working set is 10 GiB, above every memory.peak measured, and the hard bound is 14 GiB, 1.51 times the largest (9.94 GB, 9.26 GiB). Under the old 5 GiB bound four children hit memory.max 56,776 times and ran 109 s, and eight thrashed (682 s of system CPU). The 1800-second wall comes from the single-process tier's largest sample, 1183.84 s wall and 914.53 CPU seconds at load average 215-233, inside the undivided self-test with the telemetry git wrapper first on PATH; it is 1.52 times that sample. The four parts were not measured under that load. At equal load they use 1.46 times the single process's CPU (402.3 against 275.8 CPU seconds), the fixture setup that each child repeats, which projects that sample to 1334 CPU seconds; the 2100 CPU seconds are 1.57 times that projection and 5.2 times the largest four-part sample at low load. The cost is process creation (system CPU) for the scratch repositories' git commands; removing those repositories is the remaining step recorded in https://github.com/rrnewton/hermit/issues/3381. It does not take the prepared helper that selftest.scorecard uses: it refuses HERMIT_MANIFEST_PLAN_BIN, because its command-line brackets check the helper that Cargo builds in the shared target directory. KNOWN WALL RISK, NOT MEASURED: it runs `cargo build` and `cargo run -p hermit-manifest-plan` against the checkout's target directory, and forces CARGO_TARGET_DIR=<checkout>/target for its result commands, so it waits on Cargo's build-directory lock while a concurrent product build holds it. In the full local validation of Hermit 84da7b816939, build.workspace held that directory for 618.8 seconds (379.0 to 997.9 s after the start); that window plus the single-process tier's 480.80-second sample (2026-09-29, at Hermit 84da7b81, load average 158 falling to 89) is 1099.6 seconds, inside the 1800-second wall."########,
        timeout: 1800,
        cpu_timeout: 2100,
        memory_bytes: 15032385536,
        rss_baseline_bytes: 10737418240,
        inner_jobs: 6,
    },
    ToolSelfTestNode {
        name: "pressure_test",
        desc: "Pressure-test tool self-test",
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; target/debug/test-harness selftest pressure_test"########,
        description: r########"TOOL SELF-TEST NODE 2026-09-29: this self-test used to run inside gate.manifest, which every product node waits on; the local validation of Hermit 98a621f42935d585e1306262b11d64488dd51614 was killed there by the gate's 900-second CPU cap. It now runs as this leaf node through `test-harness selftest pressure_test`: a failure still makes the validation red, and no node depends on it. It keeps the gate's admitted two-core width (preferred_inner_jobs=2 carried through CARGO_BUILD_JOBS, no appended -j). Measurements below were taken 2026-09-29 between 20:39Z and 21:05Z on the measurement host recorded for gate.manifest in docs/TESTING_ENVIRONMENTS.md ("Named measurement hosts") at Hermit 98a621f4 plus this change, at load average 215-233, with the telemetry git wrapper first on PATH and the other self-tests running at the same time; they are the worst samples available, not typical ones. `ci/compat-envelope/pressure-test.rs self-test` took 505.20 s wall and 414.87 CPU seconds (165.42 user, 249.45 system) run directly, and 332.56 s wall and 235.83 CPU seconds (61.82 user, 174.01 system, 98468 KiB largest process RSS) through test-harness. Across 39 retained gate logs this audit's wall peaked at 165.2 s; isolated controls at 81ca8822 consumed 62.5-70.7 CPU seconds. The 900-second wall is 1.78 times the largest measured 505.20 s; the 900 CPU seconds are 2.17 times the largest measured 414.87. Memory keeps the gate's 5-GiB bound."########,
        timeout: 900,
        cpu_timeout: 900,
        memory_bytes: 5368709120,
        rss_baseline_bytes: 5368709120,
        inner_jobs: 2,
    },
    ToolSelfTestNode {
        name: "validate_rs",
        desc: "Validation driver self-test",
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; target/debug/test-harness selftest validate_rs"########,
        description: r########"TOOL SELF-TEST NODE 2026-09-29: this self-test used to run inside gate.manifest, which every product node waits on; the local validation of Hermit 98a621f42935d585e1306262b11d64488dd51614 was killed there by the gate's 900-second CPU cap. It now runs as this leaf node through `test-harness selftest validate_rs`: a failure still makes the validation red, and no node depends on it. It keeps the gate's admitted two-core width (preferred_inner_jobs=2 carried through CARGO_BUILD_JOBS, no appended -j). Measurements below were taken 2026-09-29 between 20:39Z and 21:05Z on the measurement host recorded for gate.manifest in docs/TESTING_ENVIRONMENTS.md ("Named measurement hosts") at Hermit 98a621f4 plus this change, at load average 215-233, with the telemetry git wrapper first on PATH and the other self-tests running at the same time; they are the worst samples available, not typical ones. `scripts/validate.rs --self-test` passed in 295.67 s wall and 186.07 CPU seconds (11.26 user, 174.81 system, 70828 KiB largest process RSS) through test-harness. An earlier direct run that stopped at the submodule-fixture comparison, because the source tree had uncommitted edits, still consumed 207.54 CPU seconds in 239.53 s wall. Across 39 retained gate logs this audit's wall peaked at 155.2 s. The 600-second wall is 2.03 times the largest measured 295.67 s; the 600 CPU seconds are 2.89 times the largest measured 207.54. Memory keeps the gate's 5-GiB bound."########,
        timeout: 600,
        cpu_timeout: 600,
        memory_bytes: 5368709120,
        rss_baseline_bytes: 5368709120,
        inner_jobs: 2,
    },
    ToolSelfTestNode {
        name: "manifest_cli",
        desc: "Manifest CLI self-test",
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; target/debug/test-harness selftest manifest_cli"########,
        description: r########"TOOL SELF-TEST NODE 2026-09-29: this self-test used to run inside gate.manifest, which every product node waits on; the local validation of Hermit 98a621f42935d585e1306262b11d64488dd51614 was killed there by the gate's 900-second CPU cap. It now runs as this leaf node through `test-harness selftest manifest_cli`: a failure still makes the validation red, and no node depends on it. It keeps the gate's admitted two-core width (preferred_inner_jobs=2 carried through CARGO_BUILD_JOBS, no appended -j). Measurements below were taken 2026-09-29 between 20:39Z and 21:05Z on the measurement host recorded for gate.manifest in docs/TESTING_ENVIRONMENTS.md ("Named measurement hosts") at Hermit 98a621f4 plus this change, at load average 215-233, with the telemetry git wrapper first on PATH and the other self-tests running at the same time; they are the worst samples available, not typical ones. `tests/manifest-cli.rs self-test` took 0.41 s wall and 0.39-0.41 CPU seconds (5128 KiB largest process RSS); across 39 retained gate logs it peaked at 0.9 s wall. The 120-second wall and 60 CPU seconds bound a hang, not the work; they are far above 1.5 times every sample. 2 GiB of memory is likewise far above the measured RSS."########,
        timeout: 120,
        cpu_timeout: 60,
        memory_bytes: 2147483648,
        rss_baseline_bytes: 2147483648,
        inner_jobs: 2,
    },
    ToolSelfTestNode {
        name: "dbt_budget",
        desc: "DBT budget wrapper end-to-end self-test",
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; target/debug/test-harness selftest dbt_budget"########,
        description: r########"TOOL SELF-TEST NODE 2026-09-29: this self-test used to run inside gate.manifest, which every product node waits on; the local validation of Hermit 98a621f42935d585e1306262b11d64488dd51614 was killed there by the gate's 900-second CPU cap. It now runs as this leaf node through `test-harness selftest dbt_budget`: a failure still makes the validation red, and no node depends on it. It keeps the gate's admitted two-core width (preferred_inner_jobs=2 carried through CARGO_BUILD_JOBS, no appended -j). Measurements below were taken 2026-09-29 between 20:39Z and 21:05Z on the measurement host recorded for gate.manifest in docs/TESTING_ENVIRONMENTS.md ("Named measurement hosts") at Hermit 98a621f4 plus this change, at load average 215-233, with the telemetry git wrapper first on PATH and the other self-tests running at the same time; they are the worst samples available, not typical ones. `ci/run-with-reverie-dbt-budget-test.sh` took 13.89 s wall and 16.94 CPU seconds run directly, and 18.78 s wall and 18.32 CPU seconds (180796 KiB largest process RSS) through test-harness; across 39 retained gate logs it peaked at 30.8 s wall. The 120-second wall is 3.9 times the largest observed 30.8 s; the 60 CPU seconds are 3.3 times the largest measured 18.32; 2 GiB is 11 times the measured RSS."########,
        timeout: 120,
        cpu_timeout: 60,
        memory_bytes: 2147483648,
        rss_baseline_bytes: 2147483648,
        inner_jobs: 2,
    },
];

const STATIC_STEPS: &[StaticStepSpec] = &[
    StaticStepSpec {
        group: r########"pre"########,
        job: r########"submodules"########,
        desc: r########"Verify repository submodules without initializing or repairing them"########,
        description: r########"Checks that every gitlink HEAD records (third-party/rr, agent-utils and reverie, recursively) is declared in .gitmodules, is its own populated repository at exactly the recorded commit, and has a clean worktree; ci/verify-submodules.sh first self-tests its refusals (missing, non-repository, wrong HEAD, dirty, conflicted, symlinked, undeclared, unsafe path) on scratch repositories, and never initializes, updates or registers a submodule. Every build and test reads these checkouts, so this keeps the validation on the sources the commit records. A reverie checkout left at an older commit is refused with "submodule HEAD differs from recorded gitlink"; a stray untracked file inside agent-utils is refused as a dirty worktree."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
            r########"quick"########,
            r########"super"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/verify-submodules.sh --self-test && ./ci/verify-submodules.sh"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(2147483648),
            hard_mem_max_bytes: Some(2147483648),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 900,
        cpu_timeout: 300,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"pre"########,
        job: r########"reverie_pin"########,
        desc: r########"Reverie pin consistency"########,
        description: r########"Runs ci/run-reverie-pin-check.sh. scripts/check-reverie-pin.rs requires every Reverie revision in tracked Cargo.toml and Cargo.lock files, the LiteInst cache keys, the DBT budget bindings and hermit-cli/BUCK to name one commit, and refuses a pin that is not an ancestor of Reverie main or that moves backwards from the pin on origin/main (an unreachable remote blocks rather than passes); scripts/check-git-pin-uniformity.rs then refuses any git dependency or submodule gitlink recorded at two revisions. Build and test nodes depend on it so no run compiles Reverie from mixed commits. Resolving a Cargo.lock conflict to an older Reverie prints "REVERIE PIN REGRESSION - BLOCKED"; moving the reverie gitlink without the Cargo revisions breaks uniformity."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
            r########"quick"########,
            r########"super"########,
        ],
        cmd: crate::validation_dag::PIN_GATE_COMMAND,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"pre.submodules"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(2147483648),
            hard_mem_max_bytes: Some(2147483648),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 900,
        cpu_timeout: 300,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"setup"########,
        job: r########"manifest_plan"########,
        desc: r########"Build the manifest-plan binaries the metadata validation runs"########,
        description: r########"Builds the hermit-manifest-plan binaries (test-harness, generate-validation-dag, generate-test-footprints and the other manifest tools) in the dev profile. On the host, gate.manifest runs them; the generated twin setup.manifest_plan_in_pinned_root builds the same binaries for the E2E bucket and manifest-guest nodes, which run inside the pinned root. Neither copy can stand in for the other: test-harness resolves the repository from its compile-time CARGO_MANIFEST_DIR, which is the checkout path on the host and /src inside the pinned root. Width 8 since 2026-09-30: at the former one-core cap it took 101 s wall for 100 CPU-s at d44bbbb79acd, while a cold build at 8 cores took 26 s wall, 102 CPU-s and a 2.53-GiB peak, hence the 3-GiB baseline and 4-GiB cap. The 300-second wall cap (https://github.com/rrnewton/hermit/issues/3381) was set after a 187-second timeout of the pinned-root twin under load at one core."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
            r########"quick"########,
            r########"super"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; cargo build -p hermit-manifest-plan --bins"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"build.rust_scripts"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 60.0,
            rss_baseline_bytes: Some(3221225472),
            hard_mem_max_bytes: Some(4294967296),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: Some(8),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 300,
        cpu_timeout: 7200,
        jobs_flag: Some(""),
        jobs_env: Some(r########"CARGO_BUILD_JOBS"########),
    },
    StaticStepSpec {
        group: r########"gate"########,
        job: r########"manifest"########,
        desc: r########"Centralized test manifest and inventory"########,
        description: MANIFEST_GATE_DESCRIPTION,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
            r########"quick"########,
            r########"super"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; target/debug/test-harness validate"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"setup.manifest_plan"########],
        env: &[(
            r########"HERMIT_VALIDATE_AUDIT_JOBS"########,
            r########"1"########,
        )],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(5368709120),
            hard_mem_max_bytes: Some(5368709120),
            classification: StepClass::Light,
            preferred_inner_jobs: Some(MANIFEST_GATE_INNER_JOBS),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 900,
        cpu_timeout: MANIFEST_GATE_CPU_SECONDS,
        jobs_flag: Some(r########""########),
        jobs_env: Some(r########"CARGO_BUILD_JOBS"########),
    },
    StaticStepSpec {
        group: r########"build"########,
        job: r########"rust_scripts"########,
        desc: r########"Build every tracked rust-script before graph consumers run"########,
        description: RUST_SCRIPT_PRODUCER_DESCRIPTION,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
            r########"quick"########,
            r########"super"########,
        ],
        cmd: RUST_SCRIPT_PRODUCER_COMMAND,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"pre.reverie_pin"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 190.0,
            rss_baseline_bytes: Some(RUST_SCRIPT_PRODUCER_RSS_BASELINE_BYTES),
            hard_mem_max_bytes: Some(RUST_SCRIPT_PRODUCER_HARD_MEM_MAX_BYTES),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: Some(RUST_SCRIPT_PRODUCER_INNER_JOBS),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: RUST_SCRIPT_PRODUCER_WALL_SECONDS,
        cpu_timeout: 7200,
        jobs_flag: Some(r########""########),
        jobs_env: Some(r########"CARGO_BUILD_JOBS"########),
    },
    StaticStepSpec {
        group: r########"check"########,
        job: r########"skill_discovery"########,
        desc: r########"Verify Claude and stock Codex discover the same safe product skills"########,
        description: r########"Makes Claude and stock Codex discover the same product skills from one copy. It compiles and runs scripts/check-skill-discovery.rs, which requires each skill to exist exactly once under .claude/skills and to resolve to that same file through .llms/skills and through one .agents/skills symlink per skill, requires CLAUDE.md to link to AGENTS.md, and requires each SKILL.md to open with frontmatter whose name equals its directory and a quoted description. The packaged set must equal the checker's list, and no parent-workspace coordinator role may appear among the product skills. A skill added without its .agents/skills link, a copied instead of linked SKILL.md, or a frontmatter name that differs from its directory breaks it."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; mkdir -p target/ci && RUSTUP_TOOLCHAIN=stable rustc --edition=2021 scripts/check-skill-discovery.rs -o target/ci/check-skill-discovery && target/ci/check-skill-discovery"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"build.rust_scripts"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 3.0,
            rss_baseline_bytes: Some(67108864),
            hard_mem_max_bytes: Some(536870912),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 60,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"check"########,
        job: r########"exit_status_class"########,
        desc: r########"Refuse a bare exit-status integer in a test, for the four values that changed meaning"########,
        description: r########"hermit#2558 gave hermit its own failure exit code 125, so tests asserting 1 for a failure hermit CHOSE are now wrong -- and which ones cannot be found by running the suite, because 85 of 133 hermit-cli test targets are run by no node. This node is STATIC for exactly that reason: it reads source, so it sees the unrun targets that no execution ever will. It does not judge correctness, which is undecidable here -- stress_suite.rs:342 and flock_exclusion.rs:245 are the same expression and mean opposite things. It refuses a BARE integer, i.e. a site that never says which channel produced the number, for the four contested values 1/125/126/127. Ratchet at 17; it may only go down."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./scripts/check-exit-status-class.rs --gate"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"build.rust_scripts"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 3.0,
            rss_baseline_bytes: Some(67108864),
            hard_mem_max_bytes: Some(536870912),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 60,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"check"########,
        job: r########"dbt_runtime_abi"########,
        desc: r########"Hermit supplies every reverie_dbt_runtime_* callback the DBT client declares"########,
        description: r########"The DBT client declares and calls every reverie_dbt_runtime_* callback unconditionally, but their upstream definitions sit behind reverie-dbt's prototype-runtime feature, which detcore-dbt disables by design so Hermit can supply Detcore's runtime instead. The two lists are joined by hand and nothing compared them: cargo cannot, because the client is C and the link happens when DynamoRIO loads it. MEASURED 2026-08-20: test.dbt_parity had been red nine days on one missing symbol, and advancing the Reverie pin took the gap from one to five while every build stayed green. This node compares the two dynamic symbol tables and names the missing callbacks, which DynamoRIO's own '<ERROR: using undefined symbol!>' never does. test.dbt_parity was retired in slice S13 of https://github.com/rrnewton/hermit/issues/3301 and its 28 cases became DBT verify cells of the c-programs and system-utils manifests, so e2e.manifest_c_programs, e2e.manifest_system_utils and privileged-e2e.manifest_c_programs depend on this node. THOSE DEPENDENCIES ARE AN ORDERING FIX, NOT A DATA DEPENDENCY: nothing this node produces is consumed; the check runs first so a missing callback is named before eager-exit can cancel it. The cost is that a failure here skips those nodes' cells on every backend for that run, which is already red with the cause named. The e2e _on_host twins do not depend on it: the hosted-portable workflow runs each of them as its own job through ci/run-node.sh, which omits dependencies outside the job's selection, while this node runs in the separate dbt-runtime-abi release-shard job, which an E2E failure cannot cancel; ci/check-shard-coverage.sh therefore refuses an E2E edge to a node that no earlier hosted job supplies. The privileged-only-e2e nodes do not depend on it because this node is not in the privileged or hosted-privileged profiles."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./scripts/check-dbt-runtime-abi.rs"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"build.e2e_artifact"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 3.0,
            rss_baseline_bytes: Some(67108864),
            hard_mem_max_bytes: Some(536870912),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 60,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"check"########,
        job: r########"backend_abstraction"########,
        desc: r########"Detcore backend-abstraction check"########,
        description: r########"Keeps detcore independent of any concrete Reverie backend, so backends are chosen only in hermit-cli. scripts/check-detcore-backend-abstraction.sh collects every reverie-* crate a workspace manifest names (plus a reverie-e9patch sentinel), excluding reverie-core, and refuses any of them in detcore/Cargo.toml's runtime or build dependency tables and, through the syn-based scripts/detcore-backend-source.rs, any extern crate, use, path or macro reference in detcore/src. Before trusting a pass it plants each prohibited crate into scratch copies of detcore and requires the lint to name it, and requires a copy that mentions the crates only in strings and comments to pass. reverie-ptrace added to detcore's [dependencies], or `use reverie_ptrace::` in detcore/src, is refused."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./scripts/check-detcore-backend-abstraction.sh"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"build.rust_scripts"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 20.0,
            rss_baseline_bytes: Some(268435456),
            hard_mem_max_bytes: Some(1073741824),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 120,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"check"########,
        job: r########"backend_parity_mutation"########,
        desc: r########"Identity fixtures in tests/c are non-vacuous (native mutation harness)"########,
        description: r########"Proves every identity fixture in tests/c that includes parity_probe.h would FAIL if a backend got its value wrong: for each declared field, planting a mutation must diverge the fixture's (exit,stdout); a field that changes nothing is flagged VACUOUS. The harness refuses to run unless its registry equals the set of tests/c sources that include parity_probe.h, and prints every fixture it examined. Native mode (cc only, no hermit build) so it guards the whole growing family cheaply on every PR. The fixtures and this harness moved from tests/backend-parity when the backend-parity-c bucket was folded into c-programs (https://github.com/rrnewton/hermit/issues/3301); the job id is kept so shard maps and history keep naming the same node. Real cross-backend parity for those fixtures is carried by their c-programs verify cells, which e2e.manifest_c_programs and its _on_host twin select: c-programs/ioctl-fionread on ptrace and LiteInst, and c-programs/rlimit-identity and c-programs/sched-getaffinity-identity on ptrace, KVM and LiteInst. None of the three has a CI-selected DBT cell: ioctl-fionread disables DBT, and rlimit-identity and sched-getaffinity-identity enable a DBT verify cell with ci.dbt=false (infrastructure-error, RUN1598 at Hermit cd976f804de7568e34dc731b859dd7a5a1c1c0b9: DBT cannot isolate the required /test workdir). The retired test.dbt_parity matrix never compiled them either; its 28 cases now run as DBT verify cells in the c-programs and system-utils manifests (https://github.com/rrnewton/hermit/issues/3301, slice S13)."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; python3 tests/c/fixture_mutation.py"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"build.rust_scripts"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 5.0,
            rss_baseline_bytes: Some(134217728),
            hard_mem_max_bytes: Some(536870912),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 120,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"check"########,
        job: r########"e9patch_corpus"########,
        desc: r########"e9patch corpus driver contracts (typed build-info and engagement readers)"########,
        description: r########"tests/e9patch/e9patch_corpus.py ratchets e9patch preprocessing parity against golden ptrace, but it needs e9tool and a Hermit built with the e9patch cargo feature, and CI has neither, so no validation lane runs its guests. This node runs tests/e9patch/test_e9patch_corpus.py, which checks the driver without running Hermit or compiling a guest: the build-info reader follows the boolean features.e9patch of a schema-1 `hermit version --json` record and refuses a record without it by field name; the engagement reader follows the producer's candidate, mapped and B0 site counts and refuses a record that lacks one; corpus commands use the caller's private host --tmp root rather than host /tmp; and `e9patch_corpus.py --check` finds exactly the corpus guest sources. The six reader checks ran in check.backend_parity_suites, inside tests/backend-parity/test_verify_tier_evidence.py, until the corpus moved out of tests/backend-parity in slice S13 of https://github.com/rrnewton/hermit/issues/3301. The --tmp check came from tests/backend-parity/test_parallel_validate_paths.py, which no node ran."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; python3 tests/e9patch/test_e9patch_corpus.py"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"build.rust_scripts"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 2.0,
            rss_baseline_bytes: Some(134217728),
            hard_mem_max_bytes: Some(536870912),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 60,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"check"########,
        job: r########"portability_paths"########,
        desc: r########"Reject developer-specific paths in build/run files"########,
        description: r########"Runs scripts/check-portable-paths.sh, which first self-tests that its scanner accepts ${HOME}-relative paths and rejects literal user homes and host names, then scans every tracked script, Rust, Python, TOML and YAML file, Makefile, everything under ci/ and .github/, and every executable file (skipping vendored, scratch and generated trees) for a path under a user home directory or the macOS Users root that names a login, the owner's login name, or a development-host name. Such literals make a script or test work only on the machine where it was written, and break hosted runners and other checkouts. A ci/ script that hard-codes a user's CARGO_HOME is reported with its file and line."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./scripts/check-portable-paths.sh"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"build.rust_scripts"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 2.0,
            rss_baseline_bytes: Some(67108864),
            hard_mem_max_bytes: Some(536870912),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 60,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"check"########,
        job: r########"shard_coverage"########,
        desc: r########"Every portable DAG node is assigned to exactly one fan-out job"########,
        description: r########"ci/check-shard-coverage.sh compares the portable label in ci/dag/validate.json to ci/portable-shards.json and refuses when a node is assigned to no job, to more than one, or when the map names a node the DAG does not have. The script is not new and was not broken: it was added 2026-08-02 with the fan-out workflow and fails closed correctly. What was missing is a reader. Its only caller was .github/workflows/ci-portable.yml, which builds the hosted matrix from the shard map, and hosted CI has not run since 2026-08-11 -- so the guard reported to nobody while TWO nodes drifted out of the map behind it: check.dbt_runtime_abi, corrected per-node on 2026-08-21 by 3b6cd503cc, and check.exit_status_class, corrected in this change. A guard whose only scheduler is dormant is indistinguishable from no guard, which is why this node runs it in the lane that actually executes: scripts/validate.rs selects the portable label from validate.json directly, so a node listed here runs on every local validate. THE UNASSIGNED NODE IS NOT MERELY UNRUN EVERYWHERE. Local validate reads the DAG, so check.exit_status_class HAS been running and failing there (measured: 6 of 6 recent local validates ran it, 0 passed) while never running on the hosted lane. This node is itself listed in preflight_nodes, because a guard against unassigned nodes that is itself unassigned is the same defect wearing the fix's clothes."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/check-shard-coverage.sh"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"build.rust_scripts"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 2.0,
            rss_baseline_bytes: Some(67108864),
            hard_mem_max_bytes: Some(1073741824),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"check"########,
        job: r########"dagrun_naming"########,
        desc: r########"Keep hermit's DAG-runner naming in agreement with the pinned agent-utils"########,
        description: r########"agent-utils renamed its DAG runner to dagrun: the crate, the package, the executables under common/bin, py/bin and rs/bin, the per-checkout profile store directory, and every runner-read environment variable (which took a DAGRUN_ prefix). scripts/check-dagrun-naming.sh is the authoritative record of the retired spellings and is the one file exempt from this scan, so the exact old names live there rather than here. The rename arrives through a SUBMODULE PIN BUMP, so no hermit build target fails when a hermit-side reference goes stale -- which is exactly how main broke at 4b9a56bfc2: cargo build, test and clippy all stayed green while ci/run-dag.sh and ci/run-node.sh could not locate the runner at all and NO DAG NODE COULD EXECUTE. This node holds the retired spellings at zero in forward-looking files, and separately asserts that the Python and Rust JOBS_ENV_ENV constants match each other and Hermit's export. That second check is not decoration: it is how a node's declared build width reaches Cargo, and if any of the three names diverge, one or both engines silently fall back to the ambient 8 -- the width fix would revert itself in silence. Demonstrated failing on planted occurrences of both classes, and exiting 2 (not 1) when the submodule is unpopulated so a checkout problem is never misread as a naming problem."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./scripts/check-dagrun-naming.sh"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"build.rust_scripts"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 2.0,
            rss_baseline_bytes: Some(67108864),
            hard_mem_max_bytes: Some(536870912),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 60,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"check"########,
        job: r########"run_node_args"########,
        desc: r########"Guard ci/run-node.sh's argument contract (refusals + single-node edit)"########,
        description: r########"Until 2026-08-25 ci/run-node.sh read two positional arguments and IGNORED the rest, so `ci/run-node.sh portable test.detcore_unit -E 'test(=cpuid_leaf_count)'` ran the whole 534-test node and printed PASS with the filter silently dropped -- a full-node green read as a one-test green. This node runs ci/run-node-args-test.sh, which proves every trailing replacement form refuses, a real long hosted-portable selection reaches the committed graph without rewriting it, old privileged public IDs map to their committed hosted nodes, and unknown privileged IDs refuse by name. It executes no DAG node and needs no build artifacts because RUN_NODE_PRINT_ONLY stops after committed selection, which is why it sits in preflight."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-node-args-test.sh"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"build.rust_scripts"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 1.0,
            rss_baseline_bytes: Some(67108864),
            hard_mem_max_bytes: Some(536870912),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 120,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"check"########,
        job: r########"check_outcome_consumers"########,
        desc: r########"Run the check-outcome consumer regression and the merge-gate policy lint"########,
        description: r########"These two scripts guard the merge-gate status classifier, and until 2026-08-23 NO DAG NODE RAN EITHER OF THEM. PR #2363's test plan cited check.script_sigpipe as their coverage; that node is a SIGPIPE and rust-script compile guard and both of these are plain bash, so it never executed them and the coverage claim was false. A guard nothing runs is indistinguishable from a guard that passes, which is the defect class the classifier change itself was fixing. Measured 2026-08-23: with the classifier mutated to report a failing check as PASSED, this node exits 1 on `mismatch: completed/failure expected=FAILED python=PASSED shell=PASSED`, while the DAG without this node stays green because nothing runs either script."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/check-outcome-consumers-node.sh"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"build.rust_scripts"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 5.0,
            rss_baseline_bytes: Some(67108864),
            hard_mem_max_bytes: Some(536870912),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 120,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"check"########,
        job: r########"lint_checks"########,
        desc: r########"Run `make lint-checks`: every checker in the lint target except the cargo passes and the rust-script unit tests"########,
        description: r########"Runs `make lint-checks` through ci/lint-checks-node.sh: every repository checker in the Makefile's lint-checks recipe, including shellcheck, `git diff --check`, submodule and Reverie-pin policy, nested lockfiles, merge-gate and workflow-trigger policy, checker scheduling, and the tool self-tests listed there. The node points at the target rather than at scripts, so a checker added to the recipe is gated without a DAG edit, and scripts/check-checker-scheduling.rs refuses a checker that neither the recipe nor a DAG command reaches. The two cargo passes run as lint.rustfmt and lint.clippy, the rust-script unit tests as check.script_unit_tests, and the parent-only canonical-adapter accept arm as check.canonical_adapter_accept. Exit 75 means a checker could not be evaluated from this checkout, for example uninitialized submodules, and is a no-result, never a pass. Each checker is its own make target, and the node runs `make -jN -k -Otarget lint-checks` with N from HERMIT_LINT_CHECK_JOBS, which dagrun sets to this node's 8-core width: -k reports every failing checker by target name, and -Otarget prints each checker's output whole so a NO-RESULT-CASE marker stays at column 0. Until 2026-10-04 the checkers ran as serial lines of one recipe under a one-core cap, so the node's wall time was the sum of their CPU time: 366 seconds in the full validation of 20d49d84. Run alone, scripts/check-checker-scheduling.rs took 141 seconds before its prefilter and 2 after. On one tree, the per-target checkers took 312 seconds of wall time with one job and 72 to 75 with eight. Measured again at 32e2d4bb3e on 2026-10-05 on the lint-checks host in docs/TESTING_ENVIRONMENTS.md, Named measurement hosts, the node's anonymous memory peaked at 1.46 GB with eight jobs and 4.18 GB with 44, one per checker; the cgroup's memory.peak, which also counts page cache, read 1.8 to 3.7 GB with eight. So the 2 GiB baseline covers the eight-job working set and the 4 GiB bound leaves room for reclaimable cache. More jobs did not shorten the node: 66 to 73 seconds with 8, 16 or 44, because scripts/test-authority-obtained-once.sh alone takes 67 to 71 seconds. The 2400-second wall bound is about four times the slowest serial run observed under load."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/lint-checks-node.sh"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"build.rust_scripts"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 120.0,
            rss_baseline_bytes: Some(2147483648),
            hard_mem_max_bytes: Some(4294967296),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: Some(8),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 2400,
        cpu_timeout: 7200,
        jobs_flag: Some(""),
        jobs_env: Some("HERMIT_LINT_CHECK_JOBS"),
    },
    StaticStepSpec {
        group: r########"check"########,
        job: r########"script_unit_tests"########,
        desc: r########"Run the unit tests carried by every test-bearing rust-script entrypoint"########,
        description: r########"Runs scripts/run-script-tests.sh: the #[cfg(test)] unit tests of every tracked rust-script entrypoint that has them (discovered from the tree, not listed), using the test harnesses build.rust_scripts compiled, so no harness is compiled here; some tests still start Cargo builds of their own, described below. The harnesses are independent processes; up to HERMIT_SCRIPT_TEST_JOBS of them run at once, which dagrun sets to this node's 8-core width, and each harness's stdout and stderr are printed whole, on their own streams, as soon as it finishes. Until 2026-09-30 they ran serially inside check.lint_checks under that node's one-core CPU cap, where they took about 890 of its 1,194 seconds at d44bbbb79acd. ci/compat-envelope/scorecard.rs runs as four processes, each given a disjoint share of the harness's own test list by exact name and required to report exactly that share as run and the rest as filtered out: 49 of its 165 tests hold one process-wide lock for the environment and working directory they change, so in one process they ran one at a time (47 of them took 304 seconds serially at a25b3fcd3805) and bounded this node at about 300 seconds. With the shards and the scripts/validate.rs Nextest fixture's wrapper build at eight Cargo jobs, the whole run took 219, 226 and 219 seconds against 399, 428 and 402 unsharded, under an 8-core quota on 2026-10-06 with this change applied to a25b3fcd3805, and used about 1,500 CPU-seconds, so this width, not one harness, now bounds it. Measured again on 2026-10-06 at 9b495ceb3f on a clean checkout with build.rust_scripts run first, three runs under the same quota and the 8 GiB bound on the host named for this node in docs/TESTING_ENVIRONMENTS.md, Named measurement hosts, with every suite passing: anonymous memory peaked at 4.87, 5.05 and 4.80 GiB and stayed above 4 GiB for 58, 54 and 58 seconds of runs lasting 219, 217 and 221 seconds; the cgroup's memory.peak, which also counts page cache, read 6.50, 6.53 and 6.63 GiB, and the kernel reclaimed nothing at the bound. At the anonymous peak the ci/compat-envelope/pressure-test.rs harness held 1.6 to 2.5 GiB, the four scorecard shards 1.0 to 1.2 GiB between them, and the Cargo builds that scripts/validate.rs tests start in temporary checkouts 0.8 to 1.4 GiB (sums of per-process resident sizes, which count shared pages more than once). An earlier run whose rust-script cache and hermit-manifest-plan debug build were stale peaked at 5.60 GiB: the scorecard harness rebuilt hermit-manifest-plan in the checkout (one rustc at 1.2 GiB) while a validate.rs test compiled scripts/validate.rs (1.0 GiB). The seven full validations after the shards landed, through 0e176d54bd, recorded memory.peak of 5.69 to 6.26 GiB for this node in 172 to 185 seconds, against 3.61 to 3.69 GiB in 295 to 300 seconds in the five unsharded runs just before; those later runs also carry the eight-job wrapper build, so the two changes are not separated by that data. So the 6.5 GiB baseline covers every anonymous peak measured and every memory.peak a full validation recorded. The bound is 10 GiB since 2026-10-08, about 50% over the highest memory.peak measured (6.63 GiB): at 8 GiB the node sat at its cap with the kernel reclaiming cache (memory.events max=5487 in a full validation of 608ef0de), and two uncapped runs at c715720e under a 16 GiB box measured memory.peak 5.35 and 3.25 GiB. Building the Nextest fixture's CPU wrapper at four Cargo jobs instead of eight, measured in alternation with the three runs above, did not lower the peak (4.73, 4.86 and 4.85 GiB; the wrapper build is not running at the peak) and shortened the time above 4 GiB to 33 to 40 seconds, but every run took longer, 225 to 226 seconds. Running fewer harnesses at once would not keep the speedup either: across the three runs the pressure-test harness ran for 180 to 183 seconds, scripts/validate.rs for 188 to 192, and three of the four scorecard shards for 186 to 209, so holding back any of them lengthens the node."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./scripts/run-script-tests.sh"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"build.rust_scripts"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 180.0,
            rss_baseline_bytes: Some(6979321856),
            hard_mem_max_bytes: Some(10737418240),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: Some(8),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: SCRIPT_UNIT_TESTS_WALL_SECONDS,
        cpu_timeout: 7200,
        jobs_flag: Some(""),
        jobs_env: Some("HERMIT_SCRIPT_TEST_JOBS"),
    },
    StaticStepSpec {
        group: r########"check"########,
        job: r########"canonical_adapter_accept"########,
        desc: r########"Canonical ledger adapter contract, accept arm, against the real dev-hermit parent adapter (needs the parent: local full lane only)"########,
        description: r########"The accept arm of the canonical ledger adapter contract drives the REAL dev-hermit parent adapter (ci-hub/ledger/validate_rows.py in the dev-hermit PARENT repository; this repository has no ci-hub/ directory), so it can be evaluated only from a checkout nested under that parent. That is a host capability in the same sense as /dev/kvm, and this node is labelled `full` ONLY because local `scripts/validate.rs full` is the one lane whose checkouts sit under the parent: the hosted-portable and portable lanes check out bare Hermit on GitHub runners, and the privileged, hosted-privileged and super lanes use actions/checkout into the runner workspace. Until 2026-09-29 the arm ran inside check.lint_checks, which is also labelled hosted-portable, so every hosted run of that node could only report NO RESULT (exit 75) for it and the hosted workflow was red with every checker passing: https://github.com/rrnewton/hermit/actions/runs/36550265580 and https://github.com/rrnewton/hermit/actions/runs/36532203200. `make lint-checks` now runs the stop-path file with --exclude-canonical-adapter-accept-arm and prints that the arm is NOT COVERED by that run and that this node covers it; that exclude mode fails unless this node exists, is the only command that runs the arm, carries exactly the `full` label, and invokes the script directly. The command is a direct python3 invocation, not a make recipe, because make collapses a recipe's exit 75 into its own error and would turn a missing parent into a failure. Outcomes: 0 when the real adapter accepted one production-shaped schema-5 write into its stop-test spool and left the retired raw shadow and the published ledger untouched; 75 (no_result, with a NO-RESULT-CASE line) when no parent adapter is on any ancestor directory; any other nonzero is a failure. The arm runs `scripts/validate.rs full` once in stop-test mode with an early exit, through the prebuilt rust-script shim, so it depends on build.rust_scripts like check.lint_checks."########,
        labels: &[r########"full"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; python3 ./scripts/test_validate_stop_paths.py --canonical-adapter-accept-arm-only"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"build.rust_scripts"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 30.0,
            rss_baseline_bytes: Some(268435456),
            hard_mem_max_bytes: Some(2147483648),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 1200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"check"########,
        job: r########"script_sigpipe"########,
        desc: r########"Standalone scripts exit cleanly on downstream SIGPIPE, and every rust-script has a prepared binary"########,
        description: r########"Compilation moved to build.rust_scripts on 2026-08-31. This node retains the independent SIGPIPE behavior check and audits that every tracked rust-script entrypoint keeps the freshness-preserving shebang, initializes the shared prelude, and has one matching executable in the producer manifest. Under the graph it uses --check and cannot compile; a standalone invocation builds first so the checker remains independently runnable."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./scripts/check-script-sigpipe.sh"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"build.rust_scripts"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 35.0,
            rss_baseline_bytes: Some(314572800),
            hard_mem_max_bytes: Some(1073741824),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 300,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"lint"########,
        job: r########"rustfmt"########,
        desc: r########"Rustfmt (cargo fmt --all -- --check)"########,
        description: r########"Runs `cargo fmt --all -- --check` over the root workspace (agent-utils is excluded) with the toolchain rust-toolchain.toml pins, which the nightly-only rustfmt.toml options require: std, external and crate import groups, one import per item, and formatted code in doc comments. It prints the would-be diff and exits nonzero instead of rewriting, which keeps formatting changes out of unrelated diffs. A brace import such as `use std::{fs, path::Path};`, or a std import placed after an external-crate import, is reported."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; cargo fmt --all -- --check"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"build.rust_scripts"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 20.0,
            rss_baseline_bytes: Some(268435456),
            hard_mem_max_bytes: Some(1073741824),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 120,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"setup"########,
        job: r########"nextest"########,
        desc: r########"cargo-nextest available"########,
        description: r########"Makes cargo-nextest available: it probes `cargo nextest show-config version` and installs cargo-nextest 0.9.100 with `cargo install --locked` only when the probe fails, so an already installed nextest of any version is accepted. Only nodes that list it among their needs rely on it: hosted-portable nodes, and through its quick-super copy, super nodes. In the full and portable profiles none does, although many of their nodes run cargo nextest inside the pinned root without depending on this node. A host without nextest where the install cannot complete (no crates.io access, a compile error, or more than its 600 seconds) turns it red."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
            r########"quick"########,
            r########"super"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; cargo nextest show-config version >/dev/null 2>&1 || if command -v with-proxy >/dev/null 2>&1; then with-proxy cargo install cargo-nextest --locked --version 0.9.100; else cargo install cargo-nextest --locked --version 0.9.100; fi"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"build.rust_scripts"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 30.0,
            rss_baseline_bytes: Some(536870912),
            hard_mem_max_bytes: Some(2147483648),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"build"########,
        job: r########"workspace"########,
        desc: r########"Prepare DBT native resources, then build every workspace target, the backend plugins and the one Hermit binary in the validate profile"########,
        description: r########"The one Cargo compilation of Hermit in the full and portable validations, in the [profile.validate] profile: release optimisation with debug assertions and overflow checks on, so cells run an optimised Hermit with detcore's debug_assert invariants and the determinism-log hash lines --verify compares. It first cleans and builds detcore-dbt alone, so reverie-dbt's DynamoRIO cache exists before hermit-install stages the DBT client, DynamoRIO, SaBRe, e9patch and the LiteInst runtime into target/install_pkg. It then builds the whole workspace, all targets, with the union of the features every prepared Nextest selection names, and `nextest-binaries.rs prepare` lists every test executable from that one build and records each selection's subset, so no selection recompiles anything or relinks target/validate/hermit. Until 2026-09-30 sixteen per-selection Cargo invocations each re-resolved features and relinked Hermit, about 620 of this node's 830 seconds at d44bbbb79acd. preferred_inner_jobs=32 is kept from the cold measurement at hermit@846baeca."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; cargo clean --profile validate -p reverie-dbt -p detcore-sabre && ./ci/run-with-reverie-dbt-budget.sh cargo build --locked --profile validate -p detcore-dbt && ./ci/run-with-reverie-dbt-budget.sh cargo build --locked --profile validate --workspace --all-targets --features hermit/kvm-execution-tests,hermit/kvm-native-test-support,hermit/third-party-backends && ./ci/nextest-binaries.rs prepare full"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[
            r########"gate.manifest"########,
            r########"setup.nextest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 130.0,
            rss_baseline_bytes: Some(11663998976),
            hard_mem_max_bytes: Some(68719476736),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: Some(32),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: WORKSPACE_WALL_SECONDS,
        cpu_timeout: 7200,
        jobs_flag: Some(r########""########),
        jobs_env: Some(r########"CARGO_BUILD_JOBS"########),
    },
    StaticStepSpec {
        group: r########"build"########,
        job: r########"buck_release_artifact"########,
        desc: r########"Prepare the opt-in Buck release artifact"########,
        description: r########"Cargo mode is the default and this node is an inert, explicit branch. Buck mode runs only on the network-capable host, builds the feature-complete release target through the caller-bound public DotSlash launcher, reconciles the retained Buck event/stdout evidence, checks typed provenance and features, then publishes one content-addressed binary under ignored/. The network-disabled pinned-root producer depends on this node and may only install those exact verified bytes; no Buck failure falls back to Cargo."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; case "${HERMIT_VALIDATE_RELEASE_BUILD_MODE:-cargo}" in cargo) : ;; buck) test -n "${HERMIT_VALIDATE_BUCK_DOTSLASH:-}" || { echo 'Buck release mode has no explicit DotSlash launcher' >&2; exit 2; }; ./scripts/build-buck-release.rs --validate-dag-build --dotslash "$HERMIT_VALIDATE_BUCK_DOTSLASH" ;; *) echo "unknown Hermit release build mode: ${HERMIT_VALIDATE_RELEASE_BUILD_MODE}" >&2; exit 2 ;; esac"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"gate.manifest"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 185.0,
            rss_baseline_bytes: Some(9006452736),
            hard_mem_max_bytes: Some(68719476736),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: Some(32),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 1800,
        cpu_timeout: 7200,
        jobs_flag: Some(r########""########),
        jobs_env: Some(r########"CARGO_BUILD_JOBS"########),
    },
    StaticStepSpec {
        group: r########"build"########,
        job: r########"e2e_artifact"########,
        desc: r########"Publish the one Hermit binary at target/ci/hermit and as a verified content-addressed bundle with its backend resources"########,
        description: r########"This is the publication barrier between the one Hermit producer and every consumer. Cargo-default mode takes target/validate/hermit, the validate-profile binary (release optimisation with debug assertions and overflow checks), after `nextest-binaries.rs assert full` has proved without compiling that it is byte-for-byte the runtime file every prepared test selection recorded, i.e. the CARGO_BIN_EXE_hermit the nextest consumers run; the explicit Buck mode installs the reconciled Buck release candidate through build-buck-release.rs --validate-dag-install and takes its target/ci/hermit-strict copy. Either way the selected bytes are installed at target/ci/hermit, the ONE path every direct consumer names (compatibility cells, test.cli, test.sabre_examples, test.envelope_levels), with libdetcore_sabre.so copied beside it and hashed before and after the copy; resources resolve through target/install_pkg. The same bytes are then published as a unique content-addressed directory plus pointer: the publisher hashes the binary before and after copying, snapshots target/install_pkg with symlinks dereferenced, verifies the required DBT/SaBRe/LiteInst/e9patch resources and the complete resource hash manifest, and atomically updates the pointer that the E2E manifest runs and ci/run-with-hermit-e2e-artifact.sh consumers read. The validate ledger records the choice as release_builder and e2e_payload, and a Buck row is neither a Cargo cache hit nor a receipt. Cargo does not hash-suffix binary outputs, so each preparation selection that links the Hermit bin relinks target/validate/hermit; the preparation record hashes it after the last selection, and this node publishes exactly those bytes instead of rebuilding (a rebuild here recompiled Hermit and hit the 120-second cap in the first one-build validation, and would have diverged from the recorded runtime file). Cargo mode removes any Buck hermit-runtime closure left in target/install_pkg, as build.runtime_release did before it was merged into this node on 2026-09-30."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; case "${HERMIT_VALIDATE_RELEASE_BUILD_MODE:-cargo}" in cargo) ./ci/nextest-binaries.rs assert full && rm -rf target/install_pkg/rsrcs/hermit-runtime && hermit_payload=target/validate/hermit ;; buck) ./scripts/build-buck-release.rs --validate-dag-install && hermit_payload=target/ci/hermit-strict ;; *) echo "unknown Hermit release build mode: ${HERMIT_VALIDATE_RELEASE_BUILD_MODE}" >&2; exit 2 ;; esac && mkdir -p target/ci && install -m 755 "$hermit_payload" target/ci/hermit && sabre_source=target/install_pkg/rsrcs/libdetcore_sabre.so && sabre_before=$(sha256sum "$sabre_source" | cut -d' ' -f1) && install -m 755 "$sabre_source" target/ci/libdetcore_sabre.so && sabre_after=$(sha256sum "$sabre_source" | cut -d' ' -f1) && sabre_copy=$(sha256sum target/ci/libdetcore_sabre.so | cut -d' ' -f1) && test "$sabre_before" = "$sabre_after" && test "$sabre_before" = "$sabre_copy" && sha256sum target/ci/hermit target/ci/libdetcore_sabre.so target/install_pkg/rsrcs/sabre && cat target/install_pkg/rsrcs/sabre.revision && ./ci/publish-hermit-e2e-artifact.sh target/ci/hermit target/ci/hermit-e2e-artifacts target/ci/hermit-e2e-artifact.path target/install_pkg"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[
            r########"build.buck_release_artifact"########,
            r########"build.workspace"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 20.0,
            rss_baseline_bytes: Some(536870912),
            hard_mem_max_bytes: Some(2147483648),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 120,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"build"########,
        job: r########"host_hermit_link"########,
        desc: r########"Link the host target/ci/hermit and artifact pointer to the one Hermit the pinned root published"########,
        description: r########"The strict compatibility corpus (e2e.manifest_compat) exercises programs installed on the host, which the pinned image does not carry, so it and its fixtures run on the host. It runs the one validate-profile Hermit that build.e2e_artifact_in_pinned_root published, not a second, host-built copy. This node points the host's target/ci/hermit at those exact bytes (ignored/hermetic/split/target is the pinned root's /src/target) and proves the binary runs here. It does because the binary's loader and libraries are the host nix store paths that ci/hermetic/build-image.sh built the image from; if nix garbage collection has removed them, the node fails and names the missing store paths. It also writes the host artifact pointer target/ci/hermit-e2e-artifact.path, naming the same content-addressed bundle by its host path, and verifies it with ci/verify-hermit-e2e-artifact.sh, so the corpus bucket reaches the binary and its resources through the same verified wrapper as every other manifest bucket."########,
        labels: &[r########"full"########, r########"portable"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; hermit=ignored/hermetic/split/target/ci/hermit && test -f "$hermit" && test -x "$hermit" && mkdir -p target/ci && ln -sfnT "../../$hermit" target/ci/hermit && if ! target/ci/hermit --version; then echo "build.host_hermit_link: $hermit does not run on this host. Its loader and libraries are the host nix store paths the pinned image was built from: $(readelf -l "$hermit" | grep -o '/nix/store/[^]]*'). Rebuild the image with ci/hermetic/build-image.sh to restore that closure." >&2; exit 1; fi && pointer=ignored/hermetic/split/target/ci/hermit-e2e-artifact.path && bundle=$(cat "$pointer") && case "$bundle" in /src/target/*) ;; *) echo "build.host_hermit_link: $pointer names $bundle, which is not under the pinned root's /src/target" >&2; exit 1;; esac && printf '%s\n' "$PWD/ignored/hermetic/split/target/${bundle#/src/target/}" > target/ci/hermit-e2e-artifact.path && ./ci/verify-hermit-e2e-artifact.sh target/ci/hermit-e2e-artifact.path >/dev/null"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"build.e2e_artifact"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 1.0,
            rss_baseline_bytes: Some(67108864),
            hard_mem_max_bytes: Some(536870912),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 60,
        cpu_timeout: 60,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"build"########,
        job: r########"manifest_guests"########,
        desc: r########"Prepare every CI-enabled portable manifest guest"########,
        description: r########"The harness used to prepare every distinct guest in one serial loop. On 2026-09-03 at Hermit a58e9ecf, three paired runs of the 158-test c-programs bucket measured --jobs 1 at 10.04/9.64/9.75s wall and 8.34/8.17/8.06s CPU, versus --jobs 16 at 1.29/1.29/1.29s wall and 8.12/8.31/8.15s CPU. Width 32 saved only another 0.15s in one sweep while doubling requested capacity, so 16 is the measured useful width. Host load averages were 22.6-25.9 (1m), 34.4-35.1 (5m), and 41.7-42.2 (15m); paired order and stable CPU cost limit that contamination. The implementation keeps one output directory per test and prints results in manifest order after all workers finish."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; target/debug/test-harness build --lane portable --ci-only --allow-empty"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"gate.manifest"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 90.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(3221225472),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: Some(16),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: Some(r########"--jobs"########),
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"e2e"########,
        job: r########"manifest_applications"########,
        desc: r########"Portable manifest bucket: applications"########,
        description: r########"Runs the applications bucket's CI cells: workloads that exec chains of real tools (a cc and ar build-and-link, a git commit-and-archive script with pinned identities) and one that polls the wall clock (examples/timed-progress-bar.py, also on KVM). They show that a process tree of stock tools, and a clock-polling loop, give the same canonical log and stdout in both strict runs. Expect it to go red on a divergence anywhere in the cc or git process tree, or when the progress bar overruns its budget on either backend."########,
        labels: &[r########"full"########, r########"portable"########],
        cmd: r########"./ci/hermetic/run-in-pinned-root.sh --src . --out ignored/hermetic/split --src-rw --cargo-home ignored/hermetic/split/cargo --env CARGO_BUILD_JOBS --env DAGRUN_STEP_STARTED_MONOTONIC_NS --env DAGRUN_TEST_COUNTS_PATH --env E2E_BUILD_ROOT --env E2E_KERNEL_VERSION --env E2E_MACHINE_SHORTNAME --env E2E_RESULT_ROOT --env E2E_RUN_ID --env HERMIT_E2E_EMPTY_WORKDIR --env HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT --env L4_REPS --env PR_NUMBER --env SUPER_REPETITIONS --env THIRD_PARTY_BUILD_JOBS --env VALIDATE_VERBOSITY -- bash -c '/src/ci/hermetic/assert-no-network.sh && /src/ci/hermetic/assert-build-dependencies.sh && exec bash -c "$1"' bash 'export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category applications --ci-only --allow-empty --prebuilt --results "$E2E_RESULT_ROOT/portable/manifest_applications/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_applications/junit.xml"'"########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"portable"########,
            category: r########"applications"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact_in_pinned_root"########,
            r########"build.manifest_guests_in_pinned_root"########,
            r########"build.rust_scripts_in_pinned_root"########,
            r########"check.dbt_runtime_abi"########,
            r########"gate.manifest"########,
            r########"setup.pinned_root_fetch"########,
        ],
        env: &[(
            r########"HERMIT_E2E_EMPTY_WORKDIR"########,
            r########"/test"########,
        )],
        hint: HintSpec {
            resources: &[(r########"manifest_guest"########, 1)],
            est_duration_s: 120.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(3221225472),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"e2e"########,
        job: r########"audit_compile_c_programs"########,
        desc: r########"Compile every c-programs guest, ci=false cells included"########,
        description: r########"Compiles every guest the c-programs manifest declares, including guests whose cells are all ci=false and so never reach an e2e node. The node compiled only the backend-parity-c guests until that bucket was folded into c-programs (https://github.com/rrnewton/hermit/issues/3301); it now covers the whole merged bucket, so a folded guest that stops compiling still fails here even when none of its cells is selected."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; target/debug/test-harness audit-compile --category c-programs"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"gate.manifest"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 20.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(3221225472),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 300,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"e2e"########,
        job: r########"manifest_bin_c"########,
        desc: r########"Portable manifest bucket: bin-c"########,
        description: r########"Verifies tests/bin/posix_timer_test.c on ptrace, SaBRe and LiteInst: a CLOCK_MONOTONIC POSIX timer must deliver SIGALRM inside a bounded window while the program sleeps toward an absolute deadline, so a missed timer signal fails promptly instead of hanging. This carries the timer-delivery check to the two binary-rewriting backends. A guest exit of 1 (signal missing or outside the window) or a canonical divergence is a failure, and the SaBRe cell also requires eligible execution-path evidence."########,
        labels: &[r########"full"########, r########"portable"########],
        cmd: r########"./ci/hermetic/run-in-pinned-root.sh --src . --out ignored/hermetic/split --src-rw --cargo-home ignored/hermetic/split/cargo --env CARGO_BUILD_JOBS --env DAGRUN_STEP_STARTED_MONOTONIC_NS --env DAGRUN_TEST_COUNTS_PATH --env E2E_BUILD_ROOT --env E2E_KERNEL_VERSION --env E2E_MACHINE_SHORTNAME --env E2E_RESULT_ROOT --env E2E_RUN_ID --env HERMIT_E2E_EMPTY_WORKDIR --env HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT --env L4_REPS --env PR_NUMBER --env SUPER_REPETITIONS --env THIRD_PARTY_BUILD_JOBS --env VALIDATE_VERBOSITY -- bash -c '/src/ci/hermetic/assert-no-network.sh && /src/ci/hermetic/assert-build-dependencies.sh && exec bash -c "$1"' bash 'export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category bin-c --ci-only --allow-empty --prebuilt --results "$E2E_RESULT_ROOT/portable/manifest_bin_c/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_bin_c/junit.xml"'"########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"portable"########,
            category: r########"bin-c"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact_in_pinned_root"########,
            r########"build.manifest_guests_in_pinned_root"########,
            r########"build.rust_scripts_in_pinned_root"########,
            r########"check.dbt_runtime_abi"########,
            r########"gate.manifest"########,
            r########"setup.pinned_root_fetch"########,
        ],
        env: &[(
            r########"HERMIT_E2E_EMPTY_WORKDIR"########,
            r########"/test"########,
        )],
        hint: HintSpec {
            resources: &[(r########"manifest_guest"########, 1)],
            est_duration_s: 5.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(3221225472),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"e2e"########,
        job: r########"manifest_c_programs"########,
        desc: r########"Portable manifest bucket: c-programs"########,
        description: r########"FOLD 2026-09-28 (https://github.com/rrnewton/hermit/issues/3301): the backend-parity-c bucket was folded into c-programs, so this node now also runs the 276 portable cells that e2e.manifest_backend_parity_c ran (713 portable cells in all), and that node is gone. Validation run validate-coord-s15-fdd8f55a8e7c (Hermit fdd8f55a8e7c, 2026-09-28) measured the two nodes at 128.62 s and 75.76 s of wall time (about 204 s together), 844 s and 510 s of CPU time (1354 s together, under the unchanged 7200 s CPU bound), and peaks of 3862880256 and 3859349504 bytes at width 8. The wall bound rises from 600 s to 900 s and the estimate from 300 s to 480 s, the sum of the two former estimates; memory bounds are unchanged because peak memory follows the worker width, which stays 8, not the number of cells. The selector no longer passes --allow-empty: a c-programs node that selects no cells now fails instead of passing having run nothing. WORKER WIDTH measured 2026-08-23: recent 20-way validation runs rotated an identical empty early-Run1 no_result across unrelated ptrace cells, while each affected cell passed in another run. A focused run at eight workers completed all 127 selected strict rows in 264.3s with 127 canonical matches and no no_result, leaving a measured 335.7s margin to the unchanged 600s hang bound. This node reserves all 8 manifest_guest slots and passes the same width to the harness; ordinary Hermit gates may overlap now that their unsupported exclusive resource is removed. Tradeoff: blocking validation still does not exercise the former 20-way manifest pressure; every cell and the strict comparator remain enabled and unchanged. MEMORY measured 2026-08-25 at Hermit 16f70d9994 with the complete current-main artifact and width 8 under ambient load 40-107: five uncapped cgroup peaks were 2855145472-2894295040 bytes; three stricter 4-GiB-cap repetitions completed all 128 cells with peaks up to 3025047552 bytes and no cgroup kill. The 4-GiB baseline rounds above the observed high-water mark; the 6-GiB hard cap preserves nearly 3 GiB of runaway headroom without reserving the former unmeasured 32 GiB."########,
        labels: &[r########"full"########, r########"portable"########],
        cmd: r########"./ci/hermetic/run-in-pinned-root.sh --src . --out ignored/hermetic/split --src-rw --cargo-home ignored/hermetic/split/cargo --env CARGO_BUILD_JOBS --env DAGRUN_STEP_STARTED_MONOTONIC_NS --env DAGRUN_TEST_COUNTS_PATH --env E2E_BUILD_ROOT --env E2E_KERNEL_VERSION --env E2E_MACHINE_SHORTNAME --env E2E_RESULT_ROOT --env E2E_RUN_ID --env HERMIT_E2E_EMPTY_WORKDIR --env HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT --env L4_REPS --env PR_NUMBER --env SUPER_REPETITIONS --env THIRD_PARTY_BUILD_JOBS --env VALIDATE_VERBOSITY -- bash -c '/src/ci/hermetic/assert-no-network.sh && /src/ci/hermetic/assert-build-dependencies.sh && exec bash -c "$1"' bash 'export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category c-programs --ci-only --prebuilt --results "$E2E_RESULT_ROOT/portable/manifest_c_programs/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_c_programs/junit.xml"'"########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"portable"########,
            category: r########"c-programs"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact_in_pinned_root"########,
            r########"build.manifest_guests_in_pinned_root"########,
            r########"build.rust_scripts_in_pinned_root"########,
            r########"check.dbt_runtime_abi"########,
            r########"gate.manifest"########,
            r########"setup.pinned_root_fetch"########,
        ],
        env: &[(
            r########"HERMIT_E2E_EMPTY_WORKDIR"########,
            r########"/test"########,
        )],
        hint: HintSpec {
            resources: &[(r########"manifest_guest"########, 8)],
            est_duration_s: 480.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(6442450944),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: Some(8),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 900,
        cpu_timeout: 7200,
        jobs_flag: Some(r########"--jobs"########),
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"e2e"########,
        job: r########"manifest_chaos_c"########,
        desc: r########"Portable manifest bucket: chaos-c"########,
        description: r########"Runs tests/chaos/lock_granularity.c, whose second thread computes from a value it read under the lock and that the first thread may overwrite before the result is stored, printing Fail: the ptrace chaos cell sweeps seeds, each reproduced under its own --verify, and requires both a passing and a failing seed, while the verify cell requires the default schedule to pass identically in both runs. As a race demonstrator it fails when chaos stops exposing the race, when a seed does not reproduce itself, or when the default schedule diverges or fails."########,
        labels: &[r########"full"########, r########"portable"########],
        cmd: r########"./ci/hermetic/run-in-pinned-root.sh --src . --out ignored/hermetic/split --src-rw --cargo-home ignored/hermetic/split/cargo --env CARGO_BUILD_JOBS --env DAGRUN_STEP_STARTED_MONOTONIC_NS --env DAGRUN_TEST_COUNTS_PATH --env E2E_BUILD_ROOT --env E2E_KERNEL_VERSION --env E2E_MACHINE_SHORTNAME --env E2E_RESULT_ROOT --env E2E_RUN_ID --env HERMIT_E2E_EMPTY_WORKDIR --env HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT --env L4_REPS --env PR_NUMBER --env SUPER_REPETITIONS --env THIRD_PARTY_BUILD_JOBS --env VALIDATE_VERBOSITY -- bash -c '/src/ci/hermetic/assert-no-network.sh && /src/ci/hermetic/assert-build-dependencies.sh && exec bash -c "$1"' bash 'export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category chaos-c --ci-only --allow-empty --prebuilt --results "$E2E_RESULT_ROOT/portable/manifest_chaos_c/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_chaos_c/junit.xml"'"########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"portable"########,
            category: r########"chaos-c"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact_in_pinned_root"########,
            r########"build.manifest_guests_in_pinned_root"########,
            r########"build.rust_scripts_in_pinned_root"########,
            r########"gate.manifest"########,
            r########"setup.pinned_root_fetch"########,
        ],
        env: &[(
            r########"HERMIT_E2E_EMPTY_WORKDIR"########,
            r########"/test"########,
        )],
        hint: HintSpec {
            resources: &[(r########"manifest_guest"########, 1)],
            est_duration_s: 5.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(3221225472),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"e2e"########,
        job: r########"manifest_compat"########,
        desc: r########"Portable manifest bucket: compat, the strict compatibility corpus on the host"########,
        description: r########"The portable strict compatibility corpus (tests/e2e/manifests/compat.yaml): 189 programs installed on the validation host, each one verify cell on ptrace under the corpus's portable flags (--no-virtualize-cpuid, --max-timeslice=disabled, TMPDIR=/tmp, an empty tmpfs /test workdir) and Hermit's strict --verify comparison (--verify-strict: bitwise INFO logs and I/O buffers), so each passing cell is an L2 reference whose log the parity post-pass compares every candidate backend against. 63 rows also have a verify cell on each in-guest LiteInst backend, liteinst and in-guest-trap (the corpus's `additional` entries: 126 cells, six of them on the sabre-compat-only rows df-direct, lua-direct and perl-direct, whose ptrace cells this node does not run), each under --max-timeslice=disabled, which in-guest LiteInst requires, and the global default budget; they need CPUID faulting and the in-guest Detcore runtime the e2e artifact ships, and passed 10 of 10 verify runs each in a local survey on 2026-10-08 (20 of the 63 are rows that passed 10 of 10 in the second survey at the heap-fix tree, with the compat fixtures prepared). Until 2026-10-07 the corpus used the stripped --verify comparison, below L2; measured at Hermit 0d25b6f7, all 189 cells passed --verify-strict in 4 of 4 full runs. It runs on the host, not in the pinned root, because 31 of the programs are not in the pinned image; the Hermit is the pinned root's validate-profile binary, verified through the host artifact pointer build.host_hermit_link publishes. compatprep.fixtures prepares the files the rows read under $VALIDATE_RUN_STATE/strict-compat. Every row is an ordinary cell whose failure fails the node; the five former diagnostic rows (df, ranlib, top, zstd, zstd-roundtrip) passed strict 4 of 4 at Hermit 0d25b6f7. Until 2026-10-01 these programs were 189 separate compat.<program> nodes; in the validation of Hermit 76980bac they took 92.8 s of wall and 60.3 CPU seconds in total, the slowest 3.0 s."########,
        labels: &[r########"full"########, r########"portable"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category compat --ci-only --prebuilt --diagnostic-results --results "$E2E_RESULT_ROOT/portable/manifest_compat/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_compat/junit.xml""########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"portable"########,
            category: r########"compat"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.host_hermit_link"########,
            r########"build.rust_scripts"########,
            r########"compatprep.fixtures"########,
            r########"gate.manifest"########,
        ],
        env: &[(
            r########"HERMIT_E2E_EMPTY_WORKDIR"########,
            r########"/test"########,
        )],
        hint: HintSpec {
            resources: &[(r########"manifest_guest"########, 8)],
            est_duration_s: 120.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(6442450944),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: Some(8),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 1800,
        jobs_flag: Some(r########"--jobs"########),
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"portablecompat"########,
        job: r########"manifest_compat"########,
        desc: r########"The strict compatibility corpus against this lane's release Hermit"########,
        description: r########"The portable-strict-compat-only run type: the same compat.yaml bucket and selector as e2e.manifest_compat, run on the host against the release Hermit that compatprep.hermit_release_in_pinned_root builds, which the pinned root writes under ignored/hermetic/split/target (HERMIT_BIN=ignored/hermetic/split/target/release/hermit, resolved under the checkout; the harness probes the binary before any cell, so one that cannot run on the host fails every cell rather than passing) and the fixtures portablecompatprep.fixtures writes under $VALIDATE_RUN_STATE/strict-compat, so a corpus-only check needs one release build instead of the full profile's workspace build and test barrier. Each row is one ptrace verify cell under the corpus's portable flags and strict comparison (--verify-strict). The node omits the corpus's liteinst and in-guest-trap cells with --exclude-backend: its release build (cargo build --release -p hermit --features third-party-backends) does not build the in-guest Detcore runtime those backends load (libdetcore_liteinst.so, from the separate detcore-liteinst package), so every such cell would end backend-unavailable; e2e.manifest_compat runs them in the full and portable profiles, whose e2e artifact ships the runtime. A program that diverges or fails under the release Hermit turns it red; until 2026-10-01 these rows were 189 separate portablecompat.<program> nodes."########,
        labels: &[r########"portable-strict-compat-only"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; target/debug/test-harness run --lane portable --category compat --ci-only --prebuilt --diagnostic-results --exclude-backend liteinst --exclude-backend in-guest-trap --results "$E2E_RESULT_ROOT/portable/manifest_compat/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_compat/junit.xml""########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"portable"########,
            category: r########"compat"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.rust_scripts"########,
            r########"gate.manifest"########,
            r########"portablecompatprep.fixtures"########,
        ],
        env: &[
            (
                r########"HERMIT_BIN"########,
                r########"ignored/hermetic/split/target/release/hermit"########,
            ),
            (
                r########"HERMIT_E2E_EMPTY_WORKDIR"########,
                r########"/test"########,
            ),
        ],
        hint: HintSpec {
            resources: &[(r########"manifest_guest"########, 8)],
            est_duration_s: 120.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(6442450944),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: Some(8),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 1800,
        jobs_flag: Some(r########"--jobs"########),
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"rrcompat"########,
        job: r########"manifest_compat"########,
        desc: r########"The compatibility corpus recorded and replayed under strict Hermit"########,
        description: r########"The rr-compat-only run type: `test-harness run --label rr-compat-only` over tests/e2e/manifests/compat.yaml, so it runs exactly the cells labelled with that run type and nothing the full validation runs. Those are the replay cells of the rows' own tests compat/<row>, from the corpus's `replay` section: one ptrace replay cell for each row but the lua, perl and df -direct twins and netlink-sock-diag, 215 in all: the 139 programs the retired rr lane listed as passing (ci/compat/corpus-rr.json and RR_PASSING_LABELS until 2026-10-02) and, since 2026-10-09, 76 of the 77 rows it never listed, each running `hermit record start --strict --verify` under the stricter --verify-strict comparison, which records the program, replays the recording and compares the two, with the old lane's 60-second wall bound per program. It runs the validation's one Hermit build, the e2e artifact build.host_hermit_link links on the host, against the fixtures rrcompatprep.fixtures writes. The old lane ran `record start --verify --verify-strict`; the harness adds what every manifest replay cell has: --strict, --log info, --base-env=minimal, and a tmpfs /test working directory. On 2026-10-02 only 51 passed, on the old lane's own command too, because the other 88 stopped recording with exit status 122 at their first clock read (https://github.com/rrnewton/hermit/issues/3519); record mode now records clock reads, and on 2026-10-08 at 27eab6dc only make and node of those 88 still failed, because their replay readmitted a different backgrounded thread than the recording (https://github.com/rrnewton/hermit/issues/3934); the rejoin log of https://github.com/rrnewton/hermit/pull/3908 fixed that, and the node gated all 139. A three-run survey on 2026-10-09 gave the other 77 rows replay cells: 73 recorded and replayed at canonical bitwise parity in 3 of 3 runs and joined, and timeout, flex and lsof stayed `unselected` under their issues; flex joined once its replay stall, a pipe write captured as container output, was fixed (https://github.com/rrnewton/hermit/issues/3964), timeout once its false rt_sigsuspend deadlock in record was fixed (https://github.com/rrnewton/hermit/issues/3963), and lsof stays. netlink-sock-diag is excepted: its own check asserts Detcore's run-mode sock_diag sanitizer, which record/replay disables by design, and its replay matched the recording bitwise (https://github.com/rrnewton/hermit/issues/3965). The node now gates 214 cells: any of them that diverges, crashes or exceeds its budget turns it red. Until 2026-10-02 these rows were 139 separate rrcompat.<program> nodes running a separately built release Hermit, and until 2026-10-04 139 separate compat/rr-<row> tests duplicating the rows' own tests. Like the old lane's command, each replay cell runs without the corpus's verify settings: no added Hermit flags or comparator, no TMPDIR=/tmp, and one attempt. The node's 600-second wall bound is the 200 seconds `scripts/validate.rs --self-test` requires of it as outer headroom (two modelled attempts of the largest cell, 60 seconds scaled by the representative 1.5x multiplier, each with 10 seconds of termination grace: 2 x (90 + 10)), plus 400 seconds for the rest of the bucket; the 139 cells took 34 seconds of wall time at up to 8 workers on 2026-10-08 (149 seconds of summed cell time and 72 CPU seconds for the 137 that passed then; the slowest, java, 26.8 seconds; with all 139 passing at 7be21e8f on 2026-10-09, 139.2 seconds of summed cell time and 77.8 CPU seconds, the slowest, java, 33.4 seconds), against 56.7 seconds on 2026-10-02 for the 51 programs that passed then, run one after another through the old command. The two attempts are that conservative model only; the harness never retries a replay cell, so a recording that fails to replay fails its one attempt, as it did on the old lane."########,
        labels: &[r########"rr-compat-only"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category compat --label rr-compat-only --ci-only --prebuilt --diagnostic-results --results "$E2E_RESULT_ROOT/portable/manifest_compat/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_compat/junit.xml""########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"portable"########,
            category: r########"compat"########,
            test: None,
            mode: None,
            backend: None,
            label: Some(r########"rr-compat-only"########),
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.host_hermit_link"########,
            r########"build.rust_scripts"########,
            r########"gate.manifest"########,
            r########"pre.reverie_pin"########,
            r########"rrcompatprep.fixtures"########,
            r########"setup.manifest_plan"########,
        ],
        env: &[(
            r########"HERMIT_E2E_EMPTY_WORKDIR"########,
            r########"/test"########,
        )],
        hint: HintSpec {
            resources: &[(r########"manifest_guest"########, 8)],
            est_duration_s: 30.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(6442450944),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: Some(8),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 1800,
        jobs_flag: Some(r########"--jobs"########),
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"sabrecompat"########,
        job: r########"manifest_compat"########,
        desc: r########"The compatibility corpus on SaBRe"########,
        description: r########"The sabre-compat-only run type: `test-harness run --label sabre-compat-only` over tests/e2e/manifests/compat.yaml, so it runs exactly the cells labelled with that run type and nothing the full validation runs. Those are each corpus row's SaBRe verify cell (the `focused` section: every row but lsof, netlink-route, netlink-sock-diag and shell-build) and both cells of the 30 rows only this run type has (23 programs the SaBRe corpus had and the strict corpus lacks; lua-direct, perl-direct and df-direct, which keep that corpus's single-image argv for programs whose strict rows run under a bash wrapper; and rustc, javac, java and node, whose ptrace cells get 600 seconds). It runs the validation's one Hermit build, the e2e artifact build.host_hermit_link links on the host, whose install tree carries the SaBRe loader and libdetcore_sabre.so, against the fixtures sabrecompatprep.fixtures writes; each cell runs Hermit's strict --verify (--verify-strict) once and must also pass the harness's SaBRe execution-path contract (an RPC from the in-guest tool, zero ptrace-fallback and zero trusted shared-object system-call sites). A cell measured red stays enabled with `ci: false` and its failure class's issue (https://github.com/rrnewton/hermit/issues/3486 and the issues it names), so `--ci-only` selects only the cells that pass. The guest's own dynamic loader loads libdetcore_sabre.so against the guest's libc, so the artifact's publication and verification refuse a plugin that records a library search path, needs a library outside glibc, or requires a glibc symbol version newer than 2.34 (https://github.com/rrnewton/hermit/issues/3652). A program that diverges, crashes or leaves SaBRe's measured path turns it red. Until 2026-10-01 these rows were 212 separate sabrecompat.<program> nodes that checked only Hermit's exit status, so a program whose children ran outside SaBRe passed, and on a fresh checkout their release Hermit had no SaBRe loader at all. The node's 2120-second wall bound is the 1820 seconds `scripts/validate.rs --self-test` requires of it as outer headroom, plus 300 seconds for the rest of the bucket, so a hung heavy cell is reported as that cell's timeout rather than ending the node. The self-test models every manifest cell as two attempts with 10 seconds of termination grace each, and its representative 1.5x wall multiplier turns the largest selected cells (rustc and node on ptrace, 600 seconds each) into 900-second windows: 2 x (900 + 10). The two attempts are that conservative model only; compat cells carry the manifest's no_retry_reason, so the harness never retries them."########,
        labels: &[r########"sabre-compat-only"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category compat --label sabre-compat-only --ci-only --prebuilt --diagnostic-results --results "$E2E_RESULT_ROOT/portable/manifest_compat/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_compat/junit.xml""########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"portable"########,
            category: r########"compat"########,
            test: None,
            mode: None,
            backend: None,
            label: Some(r########"sabre-compat-only"########),
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.host_hermit_link"########,
            r########"build.rust_scripts"########,
            r########"gate.manifest"########,
            r########"pre.reverie_pin"########,
            r########"sabrecompatprep.fixtures"########,
            r########"setup.manifest_plan"########,
        ],
        env: &[(
            r########"HERMIT_E2E_EMPTY_WORKDIR"########,
            r########"/test"########,
        )],
        hint: HintSpec {
            resources: &[(r########"manifest_guest"########, 8)],
            est_duration_s: 120.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(6442450944),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: Some(8),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 2120,
        cpu_timeout: 1800,
        jobs_flag: Some(r########"--jobs"########),
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"strictcompat"########,
        job: r########"manifest_compat"########,
        desc: r########"The compatibility corpus under strict Hermit without relaxations"########,
        description: r########"The strict-compat-only run type: `test-harness run --label strict-compat-only` over tests/e2e/manifests/compat.yaml, so it runs exactly the cells labelled with that run type and nothing the full validation runs. Those are the corpus's `variants` cells compat/strict-<row>: one ptrace verify cell for each of the 193 rows of ci/compat/corpus-strict.json, with that file's guest argv (112 byte-identical; the other 81 name the same prepare_real_compat_fixtures.sh output, or for shell-build and top their scratch directory, under $VALIDATE_RUN_STATE/strict-compat instead of strictcompat), and since 2026-10-10 one for each of the 26 sabre-compat-only corpus rows the variant had excluded (timeout through msgunfmt, and the lua, perl and df -direct twins), with compat.yaml's own argv, under --strict and Hermit's strict --verify (--verify-strict) with none of the corpus's relaxations, so CPUID virtualization and the preemption timeslice stay on, and the old lane's 60-second wall bound per program. It runs the validation's one Hermit build, the e2e artifact build.host_hermit_link links on the host, against the fixtures strictcompatprep.fixtures writes. The harness adds what every manifest verify cell has and the old lane did not: --log info, --base-env=minimal, and a tmpfs /test working directory. A cell measured red stays enabled with `ci: false` and its failure class's issue, so `--ci-only` selects only the cells that pass (none is today: java and javac were, until https://github.com/rrnewton/hermit/pull/3996 normalized the /proc/cgroups read that made them diverge); any other program that diverges, crashes or exceeds its budget turns the node red. Until 2026-10-02 these rows were 193 separate strictcompat.<program> nodes running a separately built release Hermit. The node's 600-second wall bound is the 200 seconds `scripts/validate.rs --self-test` requires of it as outer headroom (two modelled attempts of the largest cell, 60 seconds scaled by the representative 1.5x multiplier, each with 10 seconds of termination grace: 2 x (90 + 10)), plus 400 seconds for the rest of the bucket; the 193 cells measured 123 seconds of summed wall time and 84 CPU seconds on 2026-10-02. The two attempts are that conservative model only; compat cells carry the manifest's no_retry_reason, so the harness never retries them."########,
        labels: &[r########"strict-compat-only"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category compat --label strict-compat-only --ci-only --prebuilt --diagnostic-results --results "$E2E_RESULT_ROOT/portable/manifest_compat/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_compat/junit.xml""########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"portable"########,
            category: r########"compat"########,
            test: None,
            mode: None,
            backend: None,
            label: Some(r########"strict-compat-only"########),
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.host_hermit_link"########,
            r########"build.rust_scripts"########,
            r########"gate.manifest"########,
            r########"pre.reverie_pin"########,
            r########"strictcompatprep.fixtures"########,
            r########"setup.manifest_plan"########,
        ],
        env: &[(
            r########"HERMIT_E2E_EMPTY_WORKDIR"########,
            r########"/test"########,
        )],
        hint: HintSpec {
            resources: &[(r########"manifest_guest"########, 8)],
            est_duration_s: 120.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(6442450944),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: Some(8),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 1800,
        jobs_flag: Some(r########"--jobs"########),
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"e2e"########,
        job: r########"manifest_data_handling"########,
        desc: r########"Portable manifest bucket: data-handling"########,
        description: r########"Runs data-processing shell workloads under verify on ptrace, and jq also on KVM: a tar round trip, a sort, uniq and awk pipeline, jq transforming JSON that calls now(), sqlite3 printing random(), randomblob and the current time, a multithreaded zstd compression, and dd moving data in partial blocks through a pipe. They require clock and PRNG reads inside stock tools to repeat, a multithreaded compressor to emit identical frames, and partial pipe reads and writes to give the same results. Divergence, a wrong dd byte count, a zstd round-trip digest mismatch, or a budget overrun counts as failure."########,
        labels: &[r########"full"########, r########"portable"########],
        cmd: r########"./ci/hermetic/run-in-pinned-root.sh --src . --out ignored/hermetic/split --src-rw --cargo-home ignored/hermetic/split/cargo --env CARGO_BUILD_JOBS --env DAGRUN_STEP_STARTED_MONOTONIC_NS --env DAGRUN_TEST_COUNTS_PATH --env E2E_BUILD_ROOT --env E2E_KERNEL_VERSION --env E2E_MACHINE_SHORTNAME --env E2E_RESULT_ROOT --env E2E_RUN_ID --env HERMIT_E2E_EMPTY_WORKDIR --env HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT --env L4_REPS --env PR_NUMBER --env SUPER_REPETITIONS --env THIRD_PARTY_BUILD_JOBS --env VALIDATE_VERBOSITY -- bash -c '/src/ci/hermetic/assert-no-network.sh && /src/ci/hermetic/assert-build-dependencies.sh && exec bash -c "$1"' bash 'export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category data-handling --ci-only --allow-empty --prebuilt --results "$E2E_RESULT_ROOT/portable/manifest_data_handling/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_data_handling/junit.xml"'"########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"portable"########,
            category: r########"data-handling"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact_in_pinned_root"########,
            r########"build.manifest_guests_in_pinned_root"########,
            r########"build.rust_scripts_in_pinned_root"########,
            r########"gate.manifest"########,
            r########"setup.pinned_root_fetch"########,
        ],
        env: &[(
            r########"HERMIT_E2E_EMPTY_WORKDIR"########,
            r########"/test"########,
        )],
        hint: HintSpec {
            resources: &[(r########"manifest_guest"########, 1)],
            est_duration_s: 90.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(3221225472),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"e2e"########,
        job: r########"manifest_debugger_c"########,
        desc: r########"Portable manifest bucket: debugger-c"########,
        description: r########"Verifies tests/debugger/guests/debuggee.c, which prints its virtualized pid and a computed result, on ptrace, KVM, SaBRe and LiteInst. The debugger integration tests set breakpoints in this guest and rely on that pid being stable across runs, so this checks that the guest itself is deterministic on every backend that runs it; two runs that disagree on the pid or the canonical log, or a nonzero exit, are failures on any of the four."########,
        labels: &[r########"full"########, r########"portable"########],
        cmd: r########"./ci/hermetic/run-in-pinned-root.sh --src . --out ignored/hermetic/split --src-rw --cargo-home ignored/hermetic/split/cargo --env CARGO_BUILD_JOBS --env DAGRUN_STEP_STARTED_MONOTONIC_NS --env DAGRUN_TEST_COUNTS_PATH --env E2E_BUILD_ROOT --env E2E_KERNEL_VERSION --env E2E_MACHINE_SHORTNAME --env E2E_RESULT_ROOT --env E2E_RUN_ID --env HERMIT_E2E_EMPTY_WORKDIR --env HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT --env L4_REPS --env PR_NUMBER --env SUPER_REPETITIONS --env THIRD_PARTY_BUILD_JOBS --env VALIDATE_VERBOSITY -- bash -c '/src/ci/hermetic/assert-no-network.sh && /src/ci/hermetic/assert-build-dependencies.sh && exec bash -c "$1"' bash 'export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category debugger-c --ci-only --allow-empty --prebuilt --results "$E2E_RESULT_ROOT/portable/manifest_debugger_c/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_debugger_c/junit.xml"'"########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"portable"########,
            category: r########"debugger-c"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact_in_pinned_root"########,
            r########"build.manifest_guests_in_pinned_root"########,
            r########"build.rust_scripts_in_pinned_root"########,
            r########"check.dbt_runtime_abi"########,
            r########"gate.manifest"########,
            r########"setup.pinned_root_fetch"########,
        ],
        env: &[(
            r########"HERMIT_E2E_EMPTY_WORKDIR"########,
            r########"/test"########,
        )],
        hint: HintSpec {
            resources: &[(r########"manifest_guest"########, 1)],
            est_duration_s: 5.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(3221225472),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"e2e"########,
        job: r########"manifest_determinism_stress_c"########,
        desc: r########"Portable manifest bucket: determinism-stress-c"########,
        description: r########"Runs the C concurrency guests one program per test, so a divergence names one program rather than a shell chain: a fork tree, a pipe chain and pipe prefill (both also on KVM), thread stress, shared mmap across fork, a lock-free compare-and-swap counter, pid and tid identity across threads, procfs, wait and exec, a condition-variable producer-consumer queue, cross-thread signal order, and thread_contention in contention mode. thread_contention also runs, in epoll mode, as a ptrace chaos cell with distinct-outcome and entropy floors. Any verify mismatch, or the chaos cell missing a floor or failing to reproduce a seed, fails the bucket."########,
        labels: &[r########"full"########, r########"portable"########],
        cmd: r########"./ci/hermetic/run-in-pinned-root.sh --src . --out ignored/hermetic/split --src-rw --cargo-home ignored/hermetic/split/cargo --env CARGO_BUILD_JOBS --env DAGRUN_STEP_STARTED_MONOTONIC_NS --env DAGRUN_TEST_COUNTS_PATH --env E2E_BUILD_ROOT --env E2E_KERNEL_VERSION --env E2E_MACHINE_SHORTNAME --env E2E_RESULT_ROOT --env E2E_RUN_ID --env HERMIT_E2E_EMPTY_WORKDIR --env HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT --env L4_REPS --env PR_NUMBER --env SUPER_REPETITIONS --env THIRD_PARTY_BUILD_JOBS --env VALIDATE_VERBOSITY -- bash -c '/src/ci/hermetic/assert-no-network.sh && /src/ci/hermetic/assert-build-dependencies.sh && exec bash -c "$1"' bash 'export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category determinism-stress-c --ci-only --allow-empty --prebuilt --results "$E2E_RESULT_ROOT/portable/manifest_determinism_stress_c/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_determinism_stress_c/junit.xml"'"########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"portable"########,
            category: r########"determinism-stress-c"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact_in_pinned_root"########,
            r########"build.manifest_guests_in_pinned_root"########,
            r########"build.rust_scripts_in_pinned_root"########,
            r########"check.dbt_runtime_abi"########,
            r########"gate.manifest"########,
            r########"setup.pinned_root_fetch"########,
        ],
        env: &[(
            r########"HERMIT_E2E_EMPTY_WORKDIR"########,
            r########"/test"########,
        )],
        hint: HintSpec {
            resources: &[(r########"manifest_guest"########, 1)],
            est_duration_s: 5.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(3221225472),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"e2e"########,
        job: r########"manifest_determinism_stress"########,
        desc: r########"Portable manifest bucket: determinism-stress"########,
        description: r########"Runs concurrency workloads: two bash writers interleaving output (examples/race.sh, on ptrace and KVM), thread contention, epoll and shared-mmap stress inside one guest, a fork tree followed by a pipe chain, and the tests/chaos order-violation race under the default schedule. A ptrace chaos cell runs tests/chaos/thread_interleaving.c, sweeping seeds over four workers whose append order can permute, and requires every seed to pass while meeting distinct-order and normalized-entropy floors, so a partial loss of schedule diversity is caught, not only a total collapse. A verify divergence in any of these programs, or the chaos cell missing a floor or not reproducing a seed, makes it red."########,
        labels: &[r########"full"########, r########"portable"########],
        cmd: r########"./ci/hermetic/run-in-pinned-root.sh --src . --out ignored/hermetic/split --src-rw --cargo-home ignored/hermetic/split/cargo --env CARGO_BUILD_JOBS --env DAGRUN_STEP_STARTED_MONOTONIC_NS --env DAGRUN_TEST_COUNTS_PATH --env E2E_BUILD_ROOT --env E2E_KERNEL_VERSION --env E2E_MACHINE_SHORTNAME --env E2E_RESULT_ROOT --env E2E_RUN_ID --env HERMIT_E2E_EMPTY_WORKDIR --env HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT --env L4_REPS --env PR_NUMBER --env SUPER_REPETITIONS --env THIRD_PARTY_BUILD_JOBS --env VALIDATE_VERBOSITY -- bash -c '/src/ci/hermetic/assert-no-network.sh && /src/ci/hermetic/assert-build-dependencies.sh && exec bash -c "$1"' bash 'export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category determinism-stress --ci-only --allow-empty --prebuilt --results "$E2E_RESULT_ROOT/portable/manifest_determinism_stress/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_determinism_stress/junit.xml"'"########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"portable"########,
            category: r########"determinism-stress"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact_in_pinned_root"########,
            r########"build.manifest_guests_in_pinned_root"########,
            r########"build.rust_scripts_in_pinned_root"########,
            r########"gate.manifest"########,
            r########"setup.pinned_root_fetch"########,
        ],
        env: &[(
            r########"HERMIT_E2E_EMPTY_WORKDIR"########,
            r########"/test"########,
        )],
        hint: HintSpec {
            resources: &[(r########"manifest_guest"########, 1)],
            est_duration_s: 150.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(3221225472),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"e2e"########,
        job: r########"manifest_language_runtimes"########,
        desc: r########"Portable manifest bucket: language-runtimes"########,
        description: r########"Verifies programs that read entropy, hash seeds or clocks through language runtimes: Bash $RANDOM and a loop, pipe and time script, Python random numbers, hash seeds, dict order and subprocess timing, Perl rand, hash order and subprocess timing, awk, Lua, Ruby and Tcl random numbers, m4 temporary names, a C++ program reading std::random_device and std::chrono, Rust HashMap iteration, and Node.js JIT output with Math.random, on ptrace and most also on KVM (examples/rand.py also on LiteInst). It checks that Hermit's virtualization of those sources holds across independent runtime implementations, not one library path; the native-variance controls are not CI-selected, so it does not itself show the output would differ without Hermit. A runtime reaching a source Hermit does not virtualize shows as a stdout or canonical-log divergence."########,
        labels: &[r########"full"########, r########"portable"########],
        cmd: r########"./ci/hermetic/run-in-pinned-root.sh --src . --out ignored/hermetic/split --src-rw --cargo-home ignored/hermetic/split/cargo --env CARGO_BUILD_JOBS --env DAGRUN_STEP_STARTED_MONOTONIC_NS --env DAGRUN_TEST_COUNTS_PATH --env E2E_BUILD_ROOT --env E2E_KERNEL_VERSION --env E2E_MACHINE_SHORTNAME --env E2E_RESULT_ROOT --env E2E_RUN_ID --env HERMIT_E2E_EMPTY_WORKDIR --env HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT --env L4_REPS --env PR_NUMBER --env SUPER_REPETITIONS --env THIRD_PARTY_BUILD_JOBS --env VALIDATE_VERBOSITY -- bash -c '/src/ci/hermetic/assert-no-network.sh && /src/ci/hermetic/assert-build-dependencies.sh && exec bash -c "$1"' bash 'export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category language-runtimes --ci-only --allow-empty --prebuilt --results "$E2E_RESULT_ROOT/portable/manifest_language_runtimes/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_language_runtimes/junit.xml"'"########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"portable"########,
            category: r########"language-runtimes"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact_in_pinned_root"########,
            r########"build.manifest_guests_in_pinned_root"########,
            r########"build.rust_scripts_in_pinned_root"########,
            r########"check.dbt_runtime_abi"########,
            r########"gate.manifest"########,
            r########"setup.pinned_root_fetch"########,
        ],
        env: &[(
            r########"HERMIT_E2E_EMPTY_WORKDIR"########,
            r########"/test"########,
        )],
        hint: HintSpec {
            resources: &[(r########"manifest_guest"########, 1)],
            est_duration_s: 120.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(3221225472),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"e2e"########,
        job: r########"manifest_shared_futex_c"########,
        desc: r########"Portable manifest bucket: shared-futex-c"########,
        description: r########"Verifies tests/shared-futex-verify/qemu_hello.c on ptrace, a guest that prints a marker with its virtualized pid and exits 7; the manifest declares that exit, so the cell passes only on a matched comparison whose guest status and Hermit exit status are both exactly 7. It pins exact propagation of a nonzero guest exit; the same program is the userspace payload tests/qemu-boot/strict_l2_userspace_test.sh expects to return 7. A divergence, a zero exit, or any other status is refused."########,
        labels: &[r########"full"########, r########"portable"########],
        cmd: r########"./ci/hermetic/run-in-pinned-root.sh --src . --out ignored/hermetic/split --src-rw --cargo-home ignored/hermetic/split/cargo --env CARGO_BUILD_JOBS --env DAGRUN_STEP_STARTED_MONOTONIC_NS --env DAGRUN_TEST_COUNTS_PATH --env E2E_BUILD_ROOT --env E2E_KERNEL_VERSION --env E2E_MACHINE_SHORTNAME --env E2E_RESULT_ROOT --env E2E_RUN_ID --env HERMIT_E2E_EMPTY_WORKDIR --env HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT --env L4_REPS --env PR_NUMBER --env SUPER_REPETITIONS --env THIRD_PARTY_BUILD_JOBS --env VALIDATE_VERBOSITY -- bash -c '/src/ci/hermetic/assert-no-network.sh && /src/ci/hermetic/assert-build-dependencies.sh && exec bash -c "$1"' bash 'export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category shared-futex-c --ci-only --allow-empty --prebuilt --results "$E2E_RESULT_ROOT/portable/manifest_shared_futex_c/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_shared_futex_c/junit.xml"'"########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"portable"########,
            category: r########"shared-futex-c"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact_in_pinned_root"########,
            r########"build.manifest_guests_in_pinned_root"########,
            r########"build.rust_scripts_in_pinned_root"########,
            r########"check.dbt_runtime_abi"########,
            r########"gate.manifest"########,
            r########"setup.pinned_root_fetch"########,
        ],
        env: &[(
            r########"HERMIT_E2E_EMPTY_WORKDIR"########,
            r########"/test"########,
        )],
        hint: HintSpec {
            resources: &[(r########"manifest_guest"########, 1)],
            est_duration_s: 5.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(3221225472),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"e2e"########,
        job: r########"manifest_system_utils"########,
        desc: r########"Portable manifest bucket: system-utils"########,
        description: r########"WORKER-WIDTH DERIVATION 2026-08-24: a current-main sweep of the 28 ptrace cells found 0 failures at --jobs 1 and 2, then rotating failures at every wider sampled setting: 2 at 8, 2 at 16, 6 at 28, and 3 at 64; system-utils/mktemp-name failed in the 28- and 64-wide buckets and passed alone at width 1. The counts are intentionally not treated as monotonic or stable; the durable fact is that widening manufactures contention-sensitive reds while the production width of 1 is clean. Keep --jobs 1 explicit until the cells are made concurrency-safe; system-utils/harness-width-contract reads the exact parsed worker capacity and fails if this control changes without a new measurement."########,
        labels: &[r########"full"########, r########"portable"########],
        cmd: r########"./ci/hermetic/run-in-pinned-root.sh --src . --out ignored/hermetic/split --src-rw --cargo-home ignored/hermetic/split/cargo --env CARGO_BUILD_JOBS --env DAGRUN_STEP_STARTED_MONOTONIC_NS --env DAGRUN_TEST_COUNTS_PATH --env E2E_BUILD_ROOT --env E2E_KERNEL_VERSION --env E2E_MACHINE_SHORTNAME --env E2E_RESULT_ROOT --env E2E_RUN_ID --env HERMIT_E2E_EMPTY_WORKDIR --env HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT --env L4_REPS --env PR_NUMBER --env SUPER_REPETITIONS --env THIRD_PARTY_BUILD_JOBS --env VALIDATE_VERBOSITY -- bash -c '/src/ci/hermetic/assert-no-network.sh && /src/ci/hermetic/assert-build-dependencies.sh && exec bash -c "$1"' bash 'export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category system-utils --ci-only --allow-empty --prebuilt --jobs 1 --results "$E2E_RESULT_ROOT/portable/manifest_system_utils/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_system_utils/junit.xml"'"########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"portable"########,
            category: r########"system-utils"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact_in_pinned_root"########,
            r########"build.manifest_guests_in_pinned_root"########,
            r########"build.rust_scripts_in_pinned_root"########,
            r########"check.dbt_runtime_abi"########,
            r########"gate.manifest"########,
            r########"setup.pinned_root_fetch"########,
        ],
        env: &[(
            r########"HERMIT_E2E_EMPTY_WORKDIR"########,
            r########"/test"########,
        )],
        hint: HintSpec {
            resources: &[(r########"manifest_guest"########, 1)],
            est_duration_s: 120.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(3221225472),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: Some(1),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: Some(r########""########),
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"e2e"########,
        job: r########"manifest_util_c"########,
        desc: r########"Portable manifest bucket: util-c"########,
        description: r########"Verifies tests/util/pmu_skid.c on ptrace as a guest: its child calls PTRACE_TRACEME, which Detcore refuses with a deterministic EPERM, so the program exits with its dedicated refused status 3, which the manifest declares. It guards Hermit's refusal of guest ptrace: guest ptrace succeeding (the program goes on to open perf counters), the refusal arriving with a different errno (a different exit status), or the two runs diverging all fail it."########,
        labels: &[r########"full"########, r########"portable"########],
        cmd: r########"./ci/hermetic/run-in-pinned-root.sh --src . --out ignored/hermetic/split --src-rw --cargo-home ignored/hermetic/split/cargo --env CARGO_BUILD_JOBS --env DAGRUN_STEP_STARTED_MONOTONIC_NS --env DAGRUN_TEST_COUNTS_PATH --env E2E_BUILD_ROOT --env E2E_KERNEL_VERSION --env E2E_MACHINE_SHORTNAME --env E2E_RESULT_ROOT --env E2E_RUN_ID --env HERMIT_E2E_EMPTY_WORKDIR --env HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT --env L4_REPS --env PR_NUMBER --env SUPER_REPETITIONS --env THIRD_PARTY_BUILD_JOBS --env VALIDATE_VERBOSITY -- bash -c '/src/ci/hermetic/assert-no-network.sh && /src/ci/hermetic/assert-build-dependencies.sh && exec bash -c "$1"' bash 'export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category util-c --ci-only --allow-empty --prebuilt --results "$E2E_RESULT_ROOT/portable/manifest_util_c/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_util_c/junit.xml"'"########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"portable"########,
            category: r########"util-c"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact_in_pinned_root"########,
            r########"build.manifest_guests_in_pinned_root"########,
            r########"build.rust_scripts_in_pinned_root"########,
            r########"check.dbt_runtime_abi"########,
            r########"gate.manifest"########,
            r########"setup.pinned_root_fetch"########,
        ],
        env: &[(
            r########"HERMIT_E2E_EMPTY_WORKDIR"########,
            r########"/test"########,
        )],
        hint: HintSpec {
            resources: &[(r########"manifest_guest"########, 1)],
            est_duration_s: 5.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(3221225472),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"test"########,
        job: r########"applications_e2e"########,
        desc: r########"Run application end-to-end strict verification"########,
        description: r########"Runs tests/e2e/lib/applications/run_all.sh with the published Hermit artifact: an on-disk SQLite workload, a deeper SQLite workload and a make-driven CMake build. Each must first produce different output in two native runs (timestamps, random nonces, a urandom build stamp), proving there is nondeterminism to remove, then pass `hermit run --strict --no-virtualize-cpuid --max-timeslice=disabled --verify --verify-strict` with bitwise_parity true and nonzero compared log messages; the SQLite workloads also pin their query-output digest. A divergence (DIVERGED: bitwise_parity=False), a comparison of zero log messages, or a changed query result is a failure."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install ./tests/e2e/lib/applications/run_all.sh"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"build.e2e_artifact"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 30.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(3221225472),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 300,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"lint"########,
        job: r########"clippy"########,
        desc: r########"Clippy (cargo clippy --workspace --all-targets --all-features -D warnings)"########,
        description: r########"FEATURE SCOPE 2026-08-25 (task workspace-clippy-gate-never-sees-the-dbt-feature): --all-features added. Without it this gate ran hermit-cli's default feature set, so the DBT, SaBRe, e9patch and third-party-backends paths were never linted. --all-features deliberately picks up future features without another manual widening. MEMORY RECALIBRATED 2026-08-25 (task remeasure_the_fourteen_stale): five current exact-command cgroup samples peaked at 279248896 bytes after the feature widening; the larger width-8 historical calibration of 4.0 GiB governs. The 4-GiB baseline preserves that floor and the 6-GiB hard cap adds 2 GiB of headroom. CPU WIDTH remains pinned at 8 by CARGO_BUILD_JOBS and preferred_inner_jobs; jobs_flag stays empty because appending '-j 8' would reach rustc. See ai_docs/dag-memory-caps-recalibration-20260825.md. PROFILE 2026-09-30: this node stays in Cargo's dev profile and must not take --profile validate. Measured in the pinned root on the one-build target, `cargo clippy --profile validate --workspace --all-targets --all-features` reran hermit-install's build script (its release-derived PROFILE and the widened --all-features feature set make a new build-script unit), which deletes and recreates target/install_pkg while test nodes read it; the dev-profile check leaves it untouched (build script exits for PROFILE=debug) and cost 45 s wall, 379 CPU-s at CARGO_BUILD_JOBS=8."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-reverie-dbt-budget.sh cargo clippy --workspace --all-targets --all-features -- -D warnings"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"build.e2e_artifact"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 300.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(6442450944),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: Some(8),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 1200,
        cpu_timeout: 7200,
        jobs_flag: Some(r########""########),
        jobs_env: Some(r########"CARGO_BUILD_JOBS"########),
    },
    StaticStepSpec {
        group: r########"doc"########,
        job: r########"doctests"########,
        desc: r########"Test workspace documentation (cargo test --workspace --features third-party-backends --doc)"########,
        description: r########"MEM-CAP DERIVATION 2026-08-04 (task memory-caps-must-scale-with-job-count): pinned_jobs=8 via the scheduler-controlled CARGO_BUILD_JOBS channel (was unset -> cargo defaulted the build phase to nproc). rss_baseline=measured_peak@j8=208MiB; method=cgroup-RECORDED (systemd MemoryPeak == cgroup memory.peak, NOT sampled); target_state=warm/production-representative (depends on build.workspace); hermit@0f891e43. headroom=+1.5GiB additive -> hard_mem_max_bytes=2.0GiB (was 5GiB). Artifact: experiments/dag-mem-caps-pinned-jobs_20260804/results.csv. CPU-WIDTH DERIVATION 2026-08-10 (task p0_dynamorio_build_cache): preferred_inner_jobs=8 is delivered through the CARGO_BUILD_JOBS jobs_env channel. See doc.rustdoc for the shared derivation; jobs_flag is empty so the declaration sizes the cgroup box only and never edits argv. PROFILE 2026-09-30: stays in Cargo's dev profile, like lint.clippy and doc.rustdoc. In the first one-build validation that ran it with --profile validate, the doctest build after the nextest preparation was not fresh: it recompiled reverie-dbt, hermit-install (whose build script restages target/install_pkg), detcore-dbt and hermit in target/validate while prepared test nodes ran, and six of them refused with 'prepared executable, package identity, or runtime file changed'. In the dev profile hermit-install's build script exits immediately and nothing under target/validate is written. MEM-CAP RE-DERIVATION 2026-09-30: the 2-GiB cap above was derived on a WARM host target that a debug workspace build had filled. In the pinned root no node builds dev-profile libraries first (clippy and rustdoc produce metadata only), so this node compiles the workspace libraries cold; with the 2-GiB cap it was OOM-killed after 23 s (4 oom_kill events) in the validation of hermit 63a043595. The cold dev-profile clippy at the same CARGO_BUILD_JOBS=8 completes under its 6-GiB cap, so rss_baseline is 4 GiB and hard_mem_max_bytes 8 GiB until a cold cgroup-recorded peak is measured."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-reverie-dbt-budget.sh cargo test --workspace --features third-party-backends --doc"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"build.e2e_artifact"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 180.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: Some(8),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 900,
        cpu_timeout: 7200,
        jobs_flag: Some(r########""########),
        jobs_env: Some(r########"CARGO_BUILD_JOBS"########),
    },
    StaticStepSpec {
        group: r########"doc"########,
        job: r########"rustdoc"########,
        desc: r########"Documentation (RUSTDOCFLAGS=-D warnings cargo doc --workspace --no-deps --all-features)"########,
        description: r########"MEM-CAP RE-DERIVATION 2026-10-06 (doc.rustdoc OOM-killed on hermit main): rss_baseline 1505439744 -> 4359524352 and hard_mem_max_bytes 3221225472 (3.0 GiB) -> 7516192768 (7.0 GiB). WHY: the 2026-08-25 figure below was measured WARM (only target/doc removed), but the pinned-root node starts from a cold or nearly cold dev-profile target under ignored/hermetic/split/target, so before documenting 15 crates it compiles 34-54 units (build scripts and their dependencies) and checks about 190 crates. Cold, it presses on the 3 GiB cap. EVIDENCE: the Cargo-runner validate of hermit 0a2e0c24 (buck-cargo-repro 20261006T034408Z) was killed at 101.8 s with 13 oom_kill events, peak 3221229568, memory.events max 42600; the cbef0ac8 ops-tick validate was killed at 105.0 s with 9 oom_kill events, peak 3221225472, max 42100. The Buck-runner validate of the same 0a2e0c24 head PASSED in 57.4 s, but its peak_bytes equalled the cap exactly and memory.events max was 9608: it survived by page reclaim at the cap, not by needing less memory, so pass or fail at 3 GiB was a race. METHOD: hermit cbef0ac85172, 2026-10-06 04:40-04:49Z, on the host recorded for node doc.rustdoc under "Named measurement hosts" in docs/TESTING_ENVIRONMENTS.md. The exact pinned-root doc.rustdoc cmd from ci/dag/validate.json, run with CARGO_BUILD_JOBS=8 inside an UNCAPPED transient user scope (MemoryAccounting=yes), recording the scope's cgroup memory.peak; run after the split fetch phase and build.rust_scripts_in_pinned_root, as in validate. Cold = target/debug, target/doc, .rustdoc_fingerprint.json and .rustc_info.json removed so only target/ci remains (254-269 units built). n=6 cold runs, all exit 0 with zero memory.events max/oom: 4247703552, 4359524352 and 4048121856 bytes with the container seeing all 316 host CPUs (taskset affinity is not inherited into the container), then 3890401280, 3997745152 and 4209971200 bytes with AllowedCPUs=240-247 on the scope, so nproc=8 as under the validate runner. The highest, 4359524352 (4.06 GiB), is recorded as rss_baseline. In 0.5 s samples, anon memory (not reclaimable without swap) peaked at 2.47-2.66 GiB; at each run's highest memory.current sample, file cache was 1.06-1.28 GB and everything else (kernel and other) under 4%. Reclaimable file cache is why a 3 GiB cap sometimes survives. Cold wall clock was 60.2-61.8 s with 8 CPUs (47.2-54.1 s when the container saw 316 CPUs). CONFIRMATION on this change's base, hermit 04b389291e0d (reverie pin d2257c06), 2026-10-06 05:07-05:11Z, same method with AllowedCPUs=240-247: three more cold runs peaked at 3643789312, 3324948480 and 3324235776 bytes (3.39, 3.10 and 3.10 GiB) in 58.7-60.7 s, all exit 0, all above the old 3 GiB cap and below the cbef0ac maximum. Warm control (0 units compiled, 15 crates documented): 1929154560 bytes (316 CPUs) and 1497948160 bytes (8 CPUs), the latter reproducing the 08-25 figure and confirming that figure was the warm case. HEADROOM: 7516192768 / 4359524352 = 1.72x the highest cold peak, above the 1.5x (50% headroom) floor. est_duration_s (150) unchanged: the cold 8-CPU 59-62 s fits inside it. MEM RE-DERIVATION 2026-08-25 (task re_derive_doc_rustdoc): rss_baseline 1181116006 -> 1505439744. The previous figure PREDATED the --all-features/-D warnings change and I flagged it stale when I made that change; this replaces it rather than leaving the caveat standing. method=cgroup-RECORDED (systemd-run --user -p MemoryAccounting=yes, MemoryPeak == cgroup memory.peak), pinned_jobs=8 via the CARGO_BUILD_JOBS=8 prefix, target_state=warm with `rm -rf target/doc` first so rustdoc actually reruns. n=2 complete runs, both result=success: 1505439744 and 1491546112 bytes (1.402 and 1.389 GiB); the larger is recorded. A THIRD, COLDER, INCOMPLETE run peaked HIGHER at 1913589760 (1.78 GiB) before it was killed, so a cold-cache run costs more than this warm figure and 1.78 GiB is a lower bound for that case -- the existing 3 GiB hard cap still clears it, which is why hard_mem_max_bytes is UNCHANGED. ⚠️ est_duration_s DELIBERATELY NOT UPDATED. Warm wall clock here was 12s against the recorded 150, but this box reports 316 effective CPUs (see the run-with-reverie-dbt-budget.sh banner), so its wall clock is not transferable to a CI runner. CPU time consumed was 33.4s, which is the portable number if anyone wants to re-derive duration properly. Memory peak transfers; wall clock on a 316-CPU host does not. ENFORCEMENT 2026-08-25 (task doc_rustdoc_cannot_fail): RUSTDOCFLAGS=-D warnings added. Until now this node ran cargo doc with no RUSTDOCFLAGS, so it EXITED 0 WHILE EMITTING WARNINGS -- it rendered documentation and could not fail on documentation defects. Measured against the real pre-fix source at 89288ed6ad^: the old command exits 0 with 3 warnings; with -D warnings it exits 101 and names both offending items and lines. --all-features added alongside, and HONESTLY IT IS PRECAUTIONARY RATHER THAN A FIX: measured today it changes nothing for rustdoc -- 13 crates and 920 HTML pages either way, zero delta -- because the feature-gated items in hermit-cli are #[doc(hidden)] re-exports and detcore-dbt is a workspace member documented regardless. It is here so that a DOCUMENTED item placed behind a feature gate later cannot become invisible, which is the mechanism that hid clippy findings until e602aaa991. Do not read it as closing a live gap. .github/workflows/docs.yml deliberately NOT changed: it PUBLISHES to gh-pages rather than gating, and a publisher that refuses to publish over a doc warning is a worse trade. MEM/CPU DERIVATION 2026-08-10 (tasks memory-caps-must-scale-with-job-count, fix_doc_rustdoc_dynamorio and p0_dynamorio_build_cache): pinned_jobs=8 via both CARGO_BUILD_JOBS=8 and preferred_inner_jobs=8. THIS NODE CARRIES THE SHARED DERIVATION for every step whose preferred width is delivered through the CARGO_BUILD_JOBS jobs_env channel; target/debug/test-harness enforces the pairing as a rule over all such steps. The explicit preferred width is load-bearing: safe-ci's declarations-first default (agent-utils DEFAULT_SMALL_CPU_COUNT=1) otherwise boxes this eight-worker cold DynamoRIO build to cpu.max=1 core while the DBT ratchet derives 132s from eight effective workers. ROOT CAUSE, measured: hermit ab4a8c08 advanced the agent-utils gitlink a6f4232 ('make the SMALL default cap OPT-IN, default OFF') -> 1d893fbc, which contains ada564d ('activate small undeclared-step caps'); every head that failed pins the activated runner and both heads that passed pin a6f4232. LIVE CGROUP READ with the runner built at the pinned SHA, each step reporting its own /proc/self/cgroup -> cpu.max: an undeclared step gets '100000 100000' (1 core) and a preferred_inner_jobs=8 step gets '800000 100000' (8 cores). A/B ON THE IDENTICAL DynamoRIO cmake build --parallel 8 (316-core validation host, load avg 46): unboxed 15.37s wall / 102.51 CPU-s at 666% CPU, versus CPUQuota=100% 112.23s wall / 112.01 CPU-s at 99% CPU; cmake configure is a further 3.65s serial. 112s at one core is already 85% of the 132s budget for the build alone, and sharing that core with the node's concurrent rustdoc/cargo work produced the observed 185.51/187.79/188.06/286.70s. CONTROL: build.workspace and build.runtime_release were the only steps that already declared a width (32) and were the only cpu-bound steps that did not regress (91s -> 88s), while undeclared test.detcore_unit went 34s -> 103s. The 132s ratchet is UNCHANGED and stays fully live: 15.4s observed against 132s once eight cores are actually delivered. jobs_flag is empty so the declaration sizes the cgroup box only; CARGO_BUILD_JOBS already conveys the width to Cargo, and appending '-j 8' would edit argv (harmless here, but it would reach libtest or rustc on three sibling steps). rss_baseline=measured_peak@j8=1.1GiB; method=cgroup-RECORDED (systemd MemoryPeak == cgroup memory.peak, NOT sampled); target_state=warm Rust artifacts but a distinct cargo-doc reverie-dbt build-script OUT_DIR can still cold-build DynamoRIO; hermit@21b22c2b. headroom=+1.9GiB additive -> hard_mem_max_bytes=3.0GiB. Artifact: experiments/dag-mem-caps-pinned-jobs_20260804/results.csv PROFILE 2026-09-30: stays in Cargo's dev profile for the same reason as lint.clippy: `cargo doc --profile validate --all-features` measurably reran hermit-install's build script and recreated target/install_pkg; the dev-profile run took 33 s wall, 142 CPU-s at CARGO_BUILD_JOBS=8."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; RUSTDOCFLAGS='-D warnings' ./ci/run-with-reverie-dbt-budget.sh cargo doc --workspace --no-deps --all-features"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"build.e2e_artifact"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 150.0,
            rss_baseline_bytes: Some(4359524352),
            hard_mem_max_bytes: Some(7516192768),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: Some(8),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 900,
        cpu_timeout: 7200,
        jobs_flag: Some(r########""########),
        jobs_env: Some(r########"CARGO_BUILD_JOBS"########),
    },
    StaticStepSpec {
        group: r########"test"########,
        job: r########"regular_crates"########,
        desc: r########"Test regular workspace crates (nextest, excludes hermit-detcore/hermit/flaky)"########,
        description: r########"MEM-CAP DERIVATION 2026-08-04 (task memory-caps-must-scale-with-job-count): pinned_jobs=8 via the scheduler-controlled CARGO_BUILD_JOBS channel (build phase). rss_baseline=measured_peak@j8=2.0GiB; method=cgroup-RECORDED (systemd MemoryPeak == cgroup memory.peak, NOT sampled); target_state=warm/production-representative (depends on build.workspace); hermit@0f891e43. headroom=+1.5GiB additive -> hard_mem_max_bytes=3.5GiB (was 6GiB). Artifact: experiments/dag-mem-caps-pinned-jobs_20260804/results.csv. CPU-WIDTH DERIVATION 2026-08-31 (task remove-the-hermit-guest-serialisation-and-expose-what-actually-breaks): preferred_inner_jobs=8 is the enforced cgroup width and jobs_flag=-j passes the same admitted width to nextest. Without the flag, nextest used all 316 visible CPUs inside an 8-core cgroup; after removing hermit_guest serialization, three hard-wall-clock runner fixtures launched together and stretched from their 1-2s contracts to 8.427-8.460s. Solo and three-way reruns of the already-built test binary passed in 1.03/2.21/1.05s. Coupling nextest to the admitted width removes the oversubscription without widening any timeout or changing the selected tests."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; CARGO_BUILD_JOBS=8 ./ci/run-with-reverie-dbt-budget.sh ./ci/run-nextest-counted.sh ${CI:+--profile ci} --workspace --exclude hermit-detcore --exclude hermit --exclude hermetic_infra_hermit_flaky-tests"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact"########,
            r########"setup.nextest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 200.0,
            rss_baseline_bytes: Some(2147483648),
            hard_mem_max_bytes: Some(3758096384),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: Some(8),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 900,
        cpu_timeout: 7200,
        jobs_flag: Some(r########"-j"########),
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"test"########,
        job: r########"hermit_unit"########,
        desc: r########"Test Hermit unit and binary targets (-p hermit --lib --bins)"########,
        description: r########"MEMORY RECALIBRATED 2026-08-25 (task remeasure_the_fourteen_stale): five current exact-command cgroup samples peaked at 1130741760 bytes; the larger width-8 historical calibration of 4.6 GiB governs. The 5-GiB scheduling baseline rounds above that floor and the 7-GiB hard cap adds 2 GiB of headroom. CPU WIDTH remains pinned at 8 for the build phase while nextest execution remains -j1. See ai_docs/dag-memory-caps-recalibration-20260825.md. TEST CONCURRENCY -j1 EXPLAINED 2026-08-26 (task goal-5-build-the-validate-parallel-scaling-model-1-to-316-threads): the -j1 is DELIBERATE, not a default. It preserves a --test-threads=1 introduced 2026-07-27 in 6044c2d39a and carried to nextest -j1 on 2026-08-10 in 3dc3a08a79, whose message says 'Preserve serial execution where Hermit tests require it'. The sibling test.detcore_unit (-p hermit-detcore --lib --bins) has never carried it, so the choice is selective. Hermit's INTEGRATION tests get the same serialization by a different route -- .config/nextest.toml caps the test-group hermit-serialized at max-threads 1 over filter package(=hermit) & kind(=test) & !binary(=cli) (the cli binary runs at the width the DAG gives test.cli; see that node); --lib and --bins are NOT kind(=test), which is why this node needs the flag instead. That file states the reason above the group: the hermit test binaries 'use process-local mutexes to serialize Hermit executions', and several tests here do execute Hermit (for example run::detects_symlink_resolution_through_implicit_mounts and the verify::tests::* family). MEASURED COST OF THE PIN: 1.93x. Two replicates each at a6b0c37648df on the host recorded for this measurement in docs/TESTING_ENVIRONMENTS.md under Named measurement hosts, -j1 gave 27.0s and 27.3s against -j16 at 14.4s and 13.7s. WARNING FOR WHOEVER MEASURES THIS NEXT -- TWO DISTINCT KNOBS, DO NOT PUT THEM ON ONE AXIS. CARGO_BUILD_JOBS=8 bounds BUILD width; nextest -j bounds TEST concurrency. The 1.93x is attributable to -j alone; the wrapper's build width was unchanged throughout. Plotting a build width and a test concurrency on one axis produces a smooth curve that means nothing. Those figures are WARM-REPEAT times, not node times: this node takes 57.6s in a full validate. The SPEEDUP transfers, the absolute seconds do not. NOT ESTABLISHED: whether the serialization is still necessary. No commit or comment names which lib/bin test requires it. Two -j16 runs returned rc=0 and that is NOT evidence of safety, because a race that only appears under concurrency is exactly what two green runs cannot rule out. Raising this needs someone to confirm Hermit's lib and bin targets are free of the process-local mutex constraint; until then the 1.93x is not claimable. KVM EXECUTION TEST SKIPPED HERE 2026-09-30: the full profile's unified Nextest build compiles the hermit library tests with kvm-execution-tests, which adds kvm_execution_tests::initialized_vm_setup_failures_consume_detcore_state_without_further_guest_execution to this binary; --skip kvm_execution_tests:: keeps this node's population exactly what it ran before, and privileged-test.cli_kvm still runs that test and requires it by name."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-reverie-dbt-budget.sh ./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit --features third-party-backends,kvm-native-test-support --lib --bins -j 1 -- --skip ptrace_completion::tests::real_random_ --skip kvm_execution_tests::"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact"########,
            r########"setup.nextest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 120.0,
            rss_baseline_bytes: Some(5368709120),
            hard_mem_max_bytes: Some(7516192768),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: Some(8),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 900,
        cpu_timeout: 7200,
        jobs_flag: Some(r########""########),
        jobs_env: Some(r########"CARGO_BUILD_JOBS"########),
    },
    StaticStepSpec {
        group: r########"test"########,
        job: r########"detcore_unit"########,
        desc: r########"Test Detcore unit and binary targets (-p hermit-detcore --lib --bins)"########,
        description: r########"MEM-CAP DERIVATION 2026-08-04 (task memory-caps-must-scale-with-job-count): pinned_jobs=8 via the scheduler-controlled CARGO_BUILD_JOBS channel (was unset -> build phase defaulted to nproc). rss_baseline=measured_peak@j8=1.8GiB; method=cgroup-RECORDED (systemd MemoryPeak == cgroup memory.peak, NOT sampled); target_state=warm/production-representative (depends on build.workspace); hermit@0f891e43. headroom=+1.7GiB additive -> hard_mem_max_bytes=3.5GiB (was 5GiB). NOTE: cpu_timeout intentionally NOT set here or on the sibling test.detcore_misc node -- detcore_misc currently livelocks under load (reverie#355 is the established fix; a cpu_timeout derived from pre-355 hang behaviour would be wrong). cpu_timeout derivation is out of scope for this memory-cap change. Artifact: experiments/dag-mem-caps-pinned-jobs_20260804/results.csv. CPU-WIDTH DERIVATION 2026-08-10 (task p0_dynamorio_build_cache): preferred_inner_jobs=8 is delivered through the CARGO_BUILD_JOBS jobs_env channel. This node is the cleanest measured case of the one-core default: 34s at agent-utils a6f4232 (caps OFF) versus 103s at d2fdfc83 (caps ON), a 3.0x regression against a flat build.runtime_release control that already declared its width. See doc.rustdoc for the shared derivation; jobs_flag is empty so the declaration sizes the cgroup box only and never edits argv."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit-detcore --lib --bins"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact"########,
            r########"setup.nextest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 120.0,
            rss_baseline_bytes: Some(1932735283),
            hard_mem_max_bytes: Some(3758096384),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: Some(8),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 900,
        cpu_timeout: 7200,
        jobs_flag: Some(r########""########),
        jobs_env: Some(r########"CARGO_BUILD_JOBS"########),
    },
    StaticStepSpec {
        group: r########"test"########,
        job: r########"detcore_misc"########,
        desc: r########"Detcore non-CPUID miscellaneous cases (tests_misc, skips RDRAND host probes)"########,
        description: r########"MEMORY RECALIBRATED 2026-08-25 (task remeasure_the_fourteen_stale): five current exact-command cgroup samples peaked at 66965504 bytes; the larger 1697730560-byte historical completed peak governs. The 2-GiB baseline rounds above that floor and the 4-GiB hard cap adds 2 GiB of headroom. See ai_docs/dag-memory-caps-recalibration-20260825.md."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit-detcore --test tests_misc -j 1 -- --skip has_rdrand_without_detcore --skip rdrand_rdseed_is_masked --skip ordinary_clone_child_starts_before_parent_resumes --skip ordinary_clone_parent_mode_can_resume_before_child --skip network_syscalls_are_deterministic_across_five_runs"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact"########,
            r########"setup.nextest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 90.0,
            rss_baseline_bytes: Some(2147483648),
            hard_mem_max_bytes: Some(4294967296),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 720,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"test"########,
        job: r########"detcore_parallel"########,
        desc: r########"Detcore non-PMU parallel cases (tests_parallelism, --skip detcore, -j4)"########,
        description: r########"MEMORY RECALIBRATED 2026-08-25 (task remeasure_the_fourteen_stale): five current exact-command cgroup samples peaked at 411189248 bytes; the larger 1605021696-byte historical completed peak governs. The 2-GiB baseline rounds above that floor and the 4-GiB hard cap adds 2 GiB of headroom. See ai_docs/dag-memory-caps-recalibration-20260825.md."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit-detcore --test tests_parallelism -j 4 -- --skip detcore"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact"########,
            r########"setup.nextest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 120.0,
            rss_baseline_bytes: Some(2147483648),
            hard_mem_max_bytes: Some(4294967296),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 720,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"test"########,
        job: r########"detcore_time"########,
        desc: r########"Detcore time integration cases that need no PMU (tests_time, serial execution)"########,
        description: r########"Runs, selected by exact name, the two tests_time cases that construct a Config with max_timeslice disabled: proc_stat_btime_is_fixed_for_a_fractional_epoch and target_timeslice_yields_at_syscall_boundaries_without_pmu. The other 36 cases enable max_timeslice and therefore exercise ptrace's perf_event_open-backed RCB clock/timer: 28 inherit the 200000000 default through ..Default::default(), the four tod_gettimeofday_delta variants use the testutils BOTTOM, MIDDLE, and TOP configs (5000000) or the default, max_timeslice_preempts_cpu_bound_code_without_rcb_logical_time sets 1000000, and rdtsc_inside_a_spinlock_does_not_end_a_target_timeslice sets 2000000, and a_futex_wake_hands_a_contended_mutex_to_the_waiter and a_futex_wake_handoff_is_deterministic set 5000000. Without a PMU each of the 36 fails with Perf support required; the hosted runner has none, and the hosted twin failed exactly those 29 of 31 there (https://github.com/rrnewton/hermit/actions/runs/37143469197, https://github.com/rrnewton/hermit/issues/3663). They run in privileged-test.pmu_detcore_time_cases, whose filter is the exact complement of this one, so the full label still runs all 38 cases once each; a case added to tests_time later is selected by that PMU node, whose expected count then fails until the new case is placed deliberately. MEASURED 2026-10-03 at hermit c3f7b2bcfa00 on the measurement host recorded for test.detcore_time in docs/TESTING_ENVIRONMENTS.md ("Named measurement hosts"): five runs of this selection with cargo nextest run -p hermit-detcore --test tests_time -j 1 and this filter, invoked directly rather than through run-nextest-counted.sh, each ran 2 tests with 29 filtered out, in 1.09-1.20 s of wall time (0.56-0.72 s of nextest time), the largest process peaking at 96-106 MB RSS. The hint keeps the whole-target budget measured 2026-09-29 at hermit 905903e0a7ed, which bounds this subset from above: five serial runs of all 28 then-existing cases, each in its own systemd-run --user --scope unit, peaked at 111661056-115081216 bytes of cgroup memory.peak and took 5.32-6.12 s of wall time, and a sixth used 7.12 CPU-seconds. The 256-MiB scheduling baseline is the next power of two at or above the 115081216-byte maximum plus 20%, the 1-GiB hard cap supplies headroom, and est_duration_s rounds the maximum wall time up to 7 seconds."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit-detcore --test tests_time -j 1 -E 'test(=proc_stat_btime_is_fixed_for_a_fractional_epoch) | test(=target_timeslice_yields_at_syscall_boundaries_without_pmu)'"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact"########,
            r########"setup.nextest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 7.0,
            rss_baseline_bytes: Some(268435456),
            hard_mem_max_bytes: Some(1073741824),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 720,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"test"########,
        job: r########"hermit_integration"########,
        desc: r########"Portable Hermit integration targets (batched compile, serial test binaries)"########,
        description: r########"MEMORY RECALIBRATED 2026-08-25 (task remeasure_the_fourteen_stale): all five current full-population runs were red for existing test failures, so their partial peaks are censored and do not lower the estimate. The last successful historical peak was 5351632896 bytes and an older 4-GiB run OOMed. The 6-GiB baseline rounds above the successful floor and the 8-GiB hard cap preserves 2 GiB more headroom. See ai_docs/dag-memory-caps-recalibration-20260825.md. REGISTRATION 2026-10-05 (https://github.com/rrnewton/hermit/issues/3146): external_signal_interrupt runs here on the ptrace backend. A signal that Linux would deliver must end the tests/c/external_signal_interrupt.c guest's blocked futex, poll, epoll, select or child wait, and an ignored, blocked or default-ignored signal must leave the wait running to its deadline. test.liteinst_strict, where earlier rounds of https://github.com/rrnewton/hermit/pull/3361 registered the binary, no longer exists, and the binary's LiteInst cells are restoration targets (https://github.com/rrnewton/hermit/issues/3745). PMU CASES MOVED 2026-10-07: 25 external_signal_interrupt ptrace cases assert guest-observed virtual-time bounds (a 300 ms timed wait must end within 300-350 virtual ms) or watchdog progress that hold only with the PMU preemption timer. GitHub-hosted runners have no PMU, so hermit falls back to --max-timeslice=disabled, and in https://github.com/rrnewton/hermit/actions/runs/37631051557 exactly these 25 failed in test.hermit_integration_on_host (300 ms waits took 454-895 virtual ms). Forcing --max-timeslice=disabled locally fails the same 25 of 69 and no other; with the timer all 69 pass. This node and its hosted twin skip them by exact name and test.pmu_integration_cases runs them after privileged-pmu.preemption has shown the PMU works; the two filters are exact complements over this selection, so the full label runs each case once."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install ./ci/run-with-reverie-dbt-budget.sh ./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit --features third-party-backends --test aio_nr_determinism --test arch_status_determinism --test chaos_sched_yield_progress --test chaos_stress_pmu_detection --test child_time_rpc --test chown_virtual_root_identity --test cli_owned_lifecycle --test clock_determinism --test clock_discipline_determinism --test clock_passthrough --test container_init_deadline --test cpufreq_avg_determinism --test dispatch_stats --test epoll_determinism --test epoll_pwait_zero_timeout_progress --test external_signal_interrupt --test fatal_core_capture --test file_nr_determinism --test first_seen_mtime --test fp_reduction_determinism --test futex2_refusal --test hashseed_determinism --test inode_nr_determinism --test kernel_keyring --test key_users_determinism --test mmap_determinism --test node_vmstat_determinism --test numa_maps_determinism --test perf_event_refusal --test pidfd_creation --test process_isolation_refusals --test proc_fdinfo_determinism --test proc_locks_determinism --test procfs_determinism --test procfs_positioned_determinism --test pty_nr_determinism --test python_stdlib --test reopened_pipe_progress --test robust_futex_owner_death --test run_evidence --test self_sched_determinism --test self_schedstat_determinism --test signal_determinism --test smaps_determinism --test smaps_rollup_determinism --test softnet_stat_determinism --test sockstat_determinism --test swaps_determinism --test thp_stats_determinism --test utimensat_mtime --test verification_report_cli --test verification_report_consumers --test verify_claim_names_its_limit --test writev_determinism --test zero_copy_pipe_fallback -j 1 -E 'not (test(=ptrace_a_handlers_first_restart_syscall_resumes_the_interrupted_timed_futex_wait) | test(=ptrace_child_waits_restart_under_sa_restart) | test(=ptrace_futex_wait_is_ended_by_a_caught_signal_pending_with_a_default_ignored_sigchld) | test(=ptrace_futex_wait_is_ended_by_a_signal_caught_after_it_parked) | test(=ptrace_futex_wait_is_not_ended_by_a_signal_ignored_after_it_parked) | test(=ptrace_futex_wait_of_the_thread_that_forked_the_child_takes_its_sigchld) | test(=ptrace_other_waits_take_a_discarded_default_stop_as_on_linux) | test(=ptrace_poll_and_epoll_are_ended_by_the_sigchld_of_an_exiting_child) | test(=ptrace_polling_futex_wait_holds_a_sigchld_caught_after_it_parked_until_it_returns) | test(=ptrace_polling_futex_wait_is_ended_by_the_sigchld_of_an_exiting_child) | test(=ptrace_polling_timed_futex_wait_keeps_its_deadline_when_a_runnable_sibling_takes_the_sigchld) | test(=ptrace_polling_untimed_futex_wait_keeps_waiting_when_a_runnable_sibling_takes_the_sigchld) | test(=ptrace_precise_futex_wait_is_ended_by_a_sigchld_caught_after_it_parked) | test(=ptrace_precise_futex_wait_is_ended_by_the_sigchld_of_an_exiting_child) | test(=ptrace_precise_futex_wait_leaves_its_childs_sigchld_to_a_running_sibling) | test(=ptrace_precise_timed_futex_wait_keeps_its_deadline_when_a_runnable_sibling_takes_the_sigchld) | test(=ptrace_precise_timed_futex_wait_leaves_its_childs_sigchld_to_a_running_sibling) | test(=ptrace_precise_untimed_futex_wait_keeps_waiting_when_a_runnable_sibling_takes_the_sigchld) | test(=ptrace_readiness_waits_are_not_ended_by_non_interrupting_process_signals) | test(=ptrace_readiness_waits_are_not_ended_by_non_interrupting_thread_signals) | test(=ptrace_selects_are_ended_by_a_caught_sigchld_from_an_exiting_child) | test(=ptrace_selects_are_not_ended_by_non_interrupting_process_signals) | test(=ptrace_selects_are_not_ended_by_non_interrupting_thread_signals) | test(=ptrace_timed_futex_wait_and_poll_keep_their_deadline_through_a_discarded_default_stop) | test(=ptrace_timed_futex_wait_is_not_ended_by_non_interrupting_signals))'"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: Some(&[
            r########"aio_nr_determinism"########,
            r########"arch_status_determinism"########,
            r########"chaos_sched_yield_progress"########,
            r########"chaos_stress_pmu_detection"########,
            r########"child_time_rpc"########,
            r########"chown_virtual_root_identity"########,
            r########"cli_owned_lifecycle"########,
            r########"clock_determinism"########,
            r########"clock_discipline_determinism"########,
            r########"clock_passthrough"########,
            r########"container_init_deadline"########,
            r########"cpufreq_avg_determinism"########,
            r########"dispatch_stats"########,
            r########"epoll_determinism"########,
            r########"epoll_pwait_zero_timeout_progress"########,
            r########"external_signal_interrupt"########,
            r########"fatal_core_capture"########,
            r########"file_nr_determinism"########,
            r########"first_seen_mtime"########,
            r########"fp_reduction_determinism"########,
            r########"futex2_refusal"########,
            r########"hashseed_determinism"########,
            r########"inode_nr_determinism"########,
            r########"kernel_keyring"########,
            r########"key_users_determinism"########,
            r########"mmap_determinism"########,
            r########"node_vmstat_determinism"########,
            r########"numa_maps_determinism"########,
            r########"perf_event_refusal"########,
            r########"pidfd_creation"########,
            r########"process_isolation_refusals"########,
            r########"proc_fdinfo_determinism"########,
            r########"proc_locks_determinism"########,
            r########"procfs_determinism"########,
            r########"procfs_positioned_determinism"########,
            r########"pty_nr_determinism"########,
            r########"python_stdlib"########,
            r########"reopened_pipe_progress"########,
            r########"robust_futex_owner_death"########,
            r########"run_evidence"########,
            r########"self_sched_determinism"########,
            r########"self_schedstat_determinism"########,
            r########"signal_determinism"########,
            r########"smaps_determinism"########,
            r########"smaps_rollup_determinism"########,
            r########"sockstat_determinism"########,
            r########"softnet_stat_determinism"########,
            r########"swaps_determinism"########,
            r########"thp_stats_determinism"########,
            r########"utimensat_mtime"########,
            r########"verification_report_cli"########,
            r########"verification_report_consumers"########,
            r########"verify_claim_names_its_limit"########,
            r########"writev_determinism"########,
            r########"zero_copy_pipe_fallback"########,
        ]),
        deps: &[
            r########"build.e2e_artifact"########,
            r########"setup.nextest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 300.0,
            rss_baseline_bytes: Some(6442450944),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 1200,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"test"########,
        job: r########"arbitrary_binaries"########,
        desc: r########"Portable arbitrary-binary cases"########,
        description: r########"MEMORY RECALIBRATED 2026-08-25 (task remeasure_the_fourteen_stale): five current exact-command cgroup samples peaked at 255152128 bytes; the larger 1936408576-byte historical completed peak governs. The 2-GiB baseline rounds above that floor and the 4-GiB hard cap adds 2 GiB of headroom. See ai_docs/dag-memory-caps-recalibration-20260825.md."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-reverie-dbt-budget.sh ./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit --features third-party-backends --test arbitrary_binaries -j 1 -- --skip record_replay_stable_arbitrary_binaries"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: Some(&[r########"arbitrary_binaries"########]),
        deps: &[
            r########"build.e2e_artifact"########,
            r########"setup.nextest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 120.0,
            rss_baseline_bytes: Some(2147483648),
            hard_mem_max_bytes: Some(4294967296),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 900,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"test"########,
        job: r########"record_replay"########,
        desc: r########"Record/replay integration tests"########,
        description: r########"Runs every test in hermit-cli/tests/record_replay.rs, all 140 of its Nextest identities, on one thread in the pinned root. Most tests run `hermit record start --verify` (some with --strict --verify-strict and a --verify-json verdict, some as separate record and `replay --autopilot` invocations) under a `timeout --kill-after=5s 45s` wrapper with --record-timeout=30, against the C and Rust workloads that build.workspace_in_pinned_root prepares and the prepared Nextest runner names in HERMIT_PREPARED_RECORD_WORKLOADS, and against host programs such as sh, bash, find, sqlite3 and coreutils pipelines; they require exit 0 and "Success: replay matched recording.", and the strict ones require verified and bitwise_parity in the verdict. They protect record/replay of file-system side effects (including a standalone replay of a mkdir and of AT_FDCWD- and descriptor-relative mkdirat calls beneath host directories that only existed at record time), fork and exec chains, pipes (including a blocking read after the guest clears O_NONBLOCK with FIONBIO or F_SETFL) and socketpairs (a blocking read, recv or recvmsg through the original descriptor, a dup and a forked child's inherited descriptor must wait for its writer, also after the guest clears O_NONBLOCK with FIONBIO or F_SETFL, with a pipe as the control), loopback TCP accept and accept4 (a guest that accepts its own connections must print the same echoed bytes, full and truncated peer address, close-on-exec flag, SOCK_NONBLOCK and EAGAIN and EINVAL failures under record and under a replay that serves both calls from the log, and every replay of one recording of a guest that accepts on one thread, on its own listener or on one received over SCM_RIGHTS, while another opens and closes descriptors must end the same way, either with the recorded output or with replay's named refusal of the accept's descriptor slot (https://github.com/rrnewton/hermit/issues/3880); accept4 with address buffers and *addrlen next to unmapped, read-only, write-only or inaccessible pages, or on a guest stack with little room below it, must return the result, length, copied prefix and untouched tail Linux 7.1 gives, writing the length before the address as Linux 7.1 and later do (7.0 and earlier copy the address first; record uses the 7.1 order on every host), and accept must read *addrlen after it has waited for a connection; a socketpair shutdown must reach a live recvmmsg in replay; and record must refuse sendmmsg and recvmmsg on an accepted connection by name), poll, select, ppoll and pselect copy-outs, futexes, clocks and RDTSC; record timeouts that kill the guest and its descendants without committing a partial recording; replay bootstrap from the recorded executable and interpreter; refusal of an unsupported syscall by name in both phases; and replay output that cannot create a host file through /proc/<pid>/root or truncate stdout through /dev/fd. Eight record_workloads unit tests check how those workloads are built and handed over, including that a consumer refuses a prepared workload whose bytes, path or alias set changed after preparation, and that test processes outside the prepared runner share one content-keyed standalone population instead of building one each (https://github.com/rrnewton/hermit/issues/3946). record_replay_matrix is also what super.record_replay_matrix_diagnostic runs alone. All fourteen network_trace tests run here: one checks the TCP fixture natively against its loopback controller; one runs the fixture's poll and ppoll on a readable pipe, each with nfds's high word set and its low word 1, natively and under --record-networking, and requires both runs to print one ready entry for each call, because Linux reads nfds as an unsigned int and record reads the pollfd array before it knows whether the call names a channel; three record and replay it with 4-KiB socket buffers that fill, one checking that a nonblocking send replays EAGAIN at the same offset, one that a blocking send does not stall a sibling thread, and one that replay refuses a blocking send where the recording holds a refused nonblocking send and that record refuses another send on the connection while a blocking send waits; three more check what happens around that wait: one records a client that prints whether the thread that released the peer had run and its monotonic clock before and after the send, replays it offline in the same configuration and requires the same stdout byte for byte and the same sequence of network-send retry lines in the INFO log, one requires record to refuse, with exit 122, a waiting send whose descriptor number another thread replaced with dup2 while a dup kept the original open, and requires that the replacement connection received no byte, and one sends from a shared file mapping whose unsent half a forked child rewrites during the wait and requires that the recorded outputs equal what the peer received and that the recording replays at L2; and three record the fixture's client with --record-networking against that controller and replay it with --replay-networking after the controller is gone, under four schedules at L2, checking that record and replay refuse socket operations outside the model and that replay refuses changed outbound bytes; one records the client waiting on a connection and a pipe with select, pselect6 and glibc's select and pselect, each result checked against Linux's, and checking that poll and ppoll with more entries than RLIMIT_NOFILE fail with EINVAL though the array names the connection, and replays it offline under two schedules, requiring the same stdout, an L2 verdict and an unchanged trace; one records the pinned root's unmodified curl (nixpkgs' curl.bin, added to the image for https://github.com/rrnewton/hermit/issues/3786) fetching from a one-shot loopback HTTP server under --strict with --record-networking, replays it with --replay-networking after the server has exited, and requires the same stdout, a matched `hermit log-diff` of the two INFO logs that compared messages on both sides, and an L2 verdict from a --verify-strict replay; the last checks, without running Hermit, that the stdout comparison the backpressure tests share is exact, apart from the observe mode's own lines, which it removes byte for byte. Record/replay does not enable PMU preemption, so the node needs no performance counters. A replay that diverges from its recording, a recording or replay that hangs until its timeout, a prepared workload that fails that check, or a run that executes other than 140 tests fails the node. record_node_eventfd_epoll_sequence, which records and replays `node -e console.log(42)` under --backend=ptrace --strict --verify --verify-strict, was skipped until the rejoin log of https://github.com/rrnewton/hermit/pull/3908: at hermit d1744e9fc07e its replay rescheduled the backgrounded DetPid(5) where the recording committed DetPid(3) at scheduler turn 144 (https://github.com/rrnewton/hermit/issues/3782). MEASURED 2026-10-05 in the pinned root at 2ab6a514458d (https://github.com/rrnewton/hermit/pull/3795 before its est_duration_s and this paragraph were updated, six tests fewer than now; the mkdir, mkdirat and pipe-clear-nonblock tests added since took 2.9 s, 2.5 s and 2.6 s wall in a debug-build nextest run outside the pinned root on 2026-10-06, and the curl test, unskipped since, took 3.2 s there, with 4.7 CPU-s for the whole `cargo nextest run` of that one test, cargo included), by a full validate run (validate-netreplay-rework-2ab6a514458d-1791233575988436941-3055230-bbd98825, off the record) at load average 14: 103 passed, 2 filtered, in 76.7 s wall and 70.6 CPU-s (cgroup cpu.usage_usec; 22.6 user, 48.0 system), with a cgroup memory peak of 152 MiB and 1.8 s of throttling. The 103 tests' own cgroups sum to 64.8 CPU-s. The six tcp_backpressure tests took 9.7, 8.1, 8.1, 6.8, 4.7 and 2.3 CPU-s (the shared-mapping, nonblocking, blocking, refusal, waited-send and replaced-descriptor tests; wall within 0.1 s of each), each under the 22 CPU-s per-test cap; no other test took more than 1.7 CPU-s. The previous measurement, at a67d9d031189 before the three wait tests were added, was 100 passed in 59.5 s wall and 53.2 CPU-s. Earlier runs of the debug-profile build outside the pinned root took 211 to 290 s wall and up to 285 CPU-s; the validate-profile Hermit and prepared workloads account for the difference, so those numbers no longer set the budget. est_duration_s is the pinned-root wall. The 1-GiB baseline rounds the cgroup peak up and the 3-GiB hard cap adds 2 GiB. The 300-second wall timeout is 1.5 times the wall rounded up to the next 300-second bucket, and the 300-second CPU cap applies the same rule to the CPU time."########,
        labels: &[r########"full"########, r########"portable"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit --features third-party-backends --test record_replay -j 1"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: Some(&[r########"record_replay"########]),
        deps: &[
            r########"build.e2e_artifact"########,
            r########"setup.nextest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 76.7,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(3221225472),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 300,
        cpu_timeout: 300,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"build"########,
        job: r########"liteinst_allocator_fixtures"########,
        desc: r########"Qualify both real diagnostic preload leaves and separate frozen allocator/stack guests"########,
        description: r########"M1 diagnostic producer only: exact initialized Reverie Git pin, full locked nested package metadata, current Cargo artifact selection, constructor/local-shim/private call-path and portability qualification for the genuine standalone release leaf and Detcore validate leaf, plus the unchanged M1 C workload and a separate frozen M2 stack guest/receipt. Both builds require allocator-fixture solely for exports. Unique targets/bundle do not change normal resources or prepared test executables. Shared 900-second deadline, 64-MiB command stream caps and process-group cleanup supplement the outer DAG CPU/memory/cgroup backstop. Initial conservative width 4, 4-GiB accounting and 8-GiB hard cap reuse the bounded focused baseline envelope (380.7 seconds, 4.03-GiB peak); two fresh diagnostic trees are not yet a calibrated cost. No guest or test executes in this node."########,
        labels: &[r########"full"########, r########"portable"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/build-liteinst-allocator-fixtures.rs"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[
            r########"pre.reverie_pin"########,
            r########"build.rust_scripts"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 650.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: Some(4),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 900,
        cpu_timeout: 3600,
        jobs_flag: Some(""),
        jobs_env: Some("CARGO_BUILD_JOBS"),
    },
    StaticStepSpec {
        group: r########"test"########,
        job: r########"liteinst_allocator"########,
        desc: r########"Require scope-free Rust allocation ownership in both actual preload leaves"########,
        description: r########"Eight Nextest identities: two genuine leaf matrices, five shared artifact qualifier controls and the real Gitless source-copy factory regression. Each completed leaf matrix requires fixed-input quiet/work pairs S,Z,G,O,R,E,A, actual constructor/launcher bootstrap, six allocation-route positive controls, zero scope depths, private membership and unchanged literal guest/next addresses, breaks and bytes. Only stdin differs. Missing producer/exports/placement or a failed pair fails; raw captures retain actual execution separately from selection. The standalone uses Reverie's public configure_command Strace API; Detcore uses the one prepared Hermit with in-guest-trap, strict settings, fixed epoch and disabled timeslice. libc internals, stacks/TLS/protection and RV's caller/guard controls are separate nonclaims. Existing per-test backstop remains unchanged."########,
        labels: &[r########"full"########, r########"portable"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; export HERMIT_M1_ALLOCATOR_FIXTURE_BUNDLE="$PWD/target/ci/m1-allocator-fixtures.path"; REVERIE_LITEINST_PRELOAD=$(./ci/build-liteinst-allocator-fixtures.rs --print-standalone) && export REVERIE_LITEINST_PRELOAD && ./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit --features third-party-backends --test liteinst_allocator -j 1"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: Some(&[r########"liteinst_allocator"########]),
        deps: &[
            r########"build.e2e_artifact"########,
            r########"build.liteinst_allocator_fixtures"########,
            r########"pre.reverie_pin"########,
            r########"setup.nextest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 60.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(4294967296),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 900,
        cpu_timeout: 900,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"test"########,
        job: r########"liteinst_stacks"########,
        desc: r########"Require literal fixed placement of actual LiteInst signal and callback stacks"########,
        description: r########"Fourteen required identities: the original four genuine leaf rows, five artifact qualifier controls and two shared stack-observation controls, plus one actual Detcore constructor-placement row and two constructor-oracle refusal controls. Standalone native Strace SITE1 ALT1 must report the real fixed guarded altstack and absent continuation; ALT0 gets no stack credit; inert loading must reserve nothing. Genuine Detcore trap/site0 ALT1 must reach one actual callback at the exact marker IP/TID/RSP, with one completion and fixed guarded 8-MiB capacity. The additional constructor fixture executes the unchanged M2 workload and requires real bootstrap samples disjoint from both later stack extents including guards. Separate source-bound receipts, unchanged M1/M2 comparators, current constructor-bearing leaf qualification and independent clean H/RV source guards are mandatory. No consumer compilation, warm artifact search, caller-byte equality, full-entry/TLS or interior-write-protection claim. Raw observations remain separate from selection."########,
        labels: &[r########"full"########, r########"portable"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; export HERMIT_M2_STACK_FIXTURE_BUNDLE="$PWD/target/ci/m2-stack-fixtures.path"; export HERMIT_COLD_STACK_FIXTURE_BUNDLE="$PWD/target/ci/cold-stack-fixtures.path"; REVERIE_LITEINST_PRELOAD=$(./ci/build-liteinst-allocator-fixtures.rs --print-stack-standalone) && export REVERIE_LITEINST_PRELOAD && ./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit --features third-party-backends --test liteinst_stacks -j 1"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: Some(&[r########"liteinst_stacks"########]),
        deps: &[
            r########"build.e2e_artifact"########,
            r########"build.liteinst_allocator_fixtures"########,
            r########"pre.reverie_pin"########,
            r########"setup.nextest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 60.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(4294967296),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 900,
        cpu_timeout: 900,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"test"########,
        job: r########"cli"########,
        desc: r########"Portable CLI cases (skips KVM/DBT-backend-only cases)"########,
        description: r########"Five exact DBT product failures are excluded from the portable baseline under #2791 and retain source TODOs naming their individual defects; the LiteInst tests remain active. They include real programs (coreutils, sqlite3, Python, jq, perl and the tests/c guests) run under in-guest LiteInst with --max-timeslice=disabled and without --verify (liteinst_in_guest_programs::, formerly the liteinst_advanced binary); those load the libdetcore_liteinst.so that the workspace build writes beside target/validate/hermit, so a missing runtime fails them with Hermit's refusal naming `cargo build -p detcore-liteinst`. MEMORY RECALIBRATED 2026-08-25 (task remeasure_the_fourteen_stale): five current exact-command cgroup samples peaked at 339279872 bytes; the larger 4068401152-byte historical completed peak governs. The 4-GiB baseline rounds above that floor and the 6-GiB hard cap adds 2 GiB of headroom. See ai_docs/dag-memory-caps-recalibration-20260825.md. PARALLELISM 2026-10-06: the node runs at the width the DAG assigns (preferred 4) through NEXTEST_TEST_THREADS, and .config/nextest.toml keeps the cli binary out of the hermit-serialized group. Nextest runs every test in its own process, so the process-local HERMIT_RUN_LOCK never serialized two tests; until this change the group, and this node's -j 1, kept two cli tests from running at once. Every fixture that the cli binary builds under CARGO_TARGET_TMPDIR, including those in the shared tests/common modules it includes, now goes in a directory named for the test process (process_build_root), so two tests running at once never write, or run, the same file. Measured 2026-10-06 with 15 alternating off-the-record runs, five per width: every run passed 166 of 166 with no retried, flaky, timed-out or leaked test; nextest time was 113.7-114.2 s at 1 thread, 59.0-59.1 s at 2 and 33.2-33.5 s at 4; node CPU stayed at 45.7-47.4 CPU-s at every width; the step's memory.peak was 171-183 MB at 1 thread, 227-267 MB at 2 and 269-276 MB at 4, about 35 MB more per added thread and far below the 4-GiB memory hint, which still comes from width-1 calibration. Like every other node whose width channel is NEXTEST_TEST_THREADS, the width is the DAG allocator's: preferred 4, capped by the run's CPU budget, and wider only if a measured speedup curve for this step justifies it; the longest case (about 27 s) bounds what a wider setting could gain. test.cli_on_host, the hosted twin, takes its width the same way. The other nodes that run the cli binary keep one thread with -j 1; for test.isolated_dbt_workdir the explicit -j 1 overrides the width it is assigned."########,
        labels: &[r########"full"########, r########"portable"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; HERMIT_LITEINST_TEST_BINARY=$PWD/target/ci/hermit ./ci/run-with-reverie-dbt-budget.sh ./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit --features third-party-backends --test cli -- --skip run_kvm_ --skip backend_accepted_in_global_position --skip run_dbt_aggregates_unsupported_syscalls_and_strict_rejects_them --skip run_dbt_strict_returns_with_blocked_stdin_source --skip run_dbt_verifies_pipe_backpressure --skip run_dbt_keeps_diagnostics_out_of_guest_stderr --skip run_dbt_recovers_after_failed_exec --skip run_dbt_fails_closed_by_default_and_opt_out_aggregates_unsupported_syscalls --skip run_dbt_verifies_queued_self_signals --skip run_dbt_verifies_self_prlimit --skip run_dbt_verifies_shell_process_lifecycle --skip run_dbt_verifies_simple_env_shebang --skip run_liteinst_rejects_non_fork_clone --skip run_liteinst_handles_inherited_ignored_sigchld --skip run_liteinst_verifies_forked_guest --skip run_liteinst_verifies_raw_fork_guest --skip skid_overshoot_and_guest_failure_have_different_exit_codes --skip run_ptrace_nonleader_exec_preserves_identity_and_time --skip run_ptrace_nonleader_exec_preserves_preemption --skip run_ptrace_nonleader_exec_displaces_runnable_leader --skip run_ptrace_nonleader_exec_refuses_preemption_artifacts --skip run_chaos_preemption_replay_reuses_the_recorded_epoch --skip max_log_bytes_preemption_stacktrace_exits_promptly_when_stderr_is_a_full_pipe --skip poll_timeout_returns_on_time_while_another_thread_spins --skip happens_before_relative_anchor_holds_the_first_call_after_its_base --skip happens_before_relative_anchor_state_is_not_inherited_by_a_new_thread --skip happens_before_unfired_relative_anchor_is_refused_with_its_base --skip happens_before_hold_budget_ends_a_hold_a_spinner_never_releases --skip happens_before_posthook_anchor_on_an_interrupted_call_is_refused_by_name --skip happens_before_hold_in_a_vfork_child_is_refused_by_name"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: Some(&[r########"cli"########]),
        deps: &[
            r########"build.e2e_artifact"########,
            r########"setup.nextest"########,
        ],
        // A hermit-dap missing from the build must fail the hermit_dap_*
        // cases here rather than let them skip
        // (https://github.com/rrnewton/hermit/issues/3419). The pinned
        // hermetic image ships GDB 17.2 at /bin/gdb, so the hermit-dap
        // end-to-end cases must drive it here rather than skip. The hosted
        // twin does not set HERMIT_REQUIRE_DAP_GDB: its distribution GDB is
        // not the tested 17.2, and there the cases skip loudly when GDB is
        // absent or refused by managed replay.
        env: &[
            (r########"HERMIT_REQUIRE_DAP"########, r########"1"########),
            (
                r########"HERMIT_REQUIRE_DAP_GDB"########,
                r########"1"########,
            ),
        ],
        hint: HintSpec {
            resources: &[(r########"integration_test_binaries.cli"########, 1)],
            est_duration_s: 150.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(6442450944),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: Some(4),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 900,
        cpu_timeout: 7200,
        jobs_flag: Some(""),
        jobs_env: Some("NEXTEST_TEST_THREADS"),
    },
    StaticStepSpec {
        group: r########"test"########,
        job: r########"cli_on_host"########,
        desc: r########"Portable CLI cases (skips KVM/DBT-backend-only cases)"########,
        description: r########"Five exact DBT product failures are excluded from the portable baseline under #2791 and retain source TODOs naming their individual defects; the LiteInst tests remain active. They include real programs (coreutils, sqlite3, Python, jq, perl and the tests/c guests) run under in-guest LiteInst with --max-timeslice=disabled and without --verify (liteinst_in_guest_programs::, formerly the liteinst_advanced binary); those load the libdetcore_liteinst.so that the workspace build writes beside target/validate/hermit, so a missing runtime fails them with Hermit's refusal naming `cargo build -p detcore-liteinst`. The 40 of them that start the in-guest LiteInst runtime (33 liteinst_in_guest_programs:: cases, the five liteinst_in_guest_verify_ cases, liteinst_backend_stats_report_the_guests_own_dispatch_paths and run_liteinst_finds_the_runtime_staged_as_an_installed_resource) need CPUID faulting, a host capability like the PMU: the GitHub-hosted runner has none, and in portable run 37383177918 each of the first 25 refused there with detcore-liteinst: initialization failed: CPUID faulting is unavailable: No such device (the 26th failed earlier, on the runtime the prebuilt tree did not yet ship). In portable run https://github.com/rrnewton/hermit/actions/runs/37543769782 four later cases (liteinst_in_guest_exit_reaping_matches_ptrace, liteinst_in_guest_unscheduled_deaths_complete_and_refuse_verification, liteinst_in_guest_serves_its_own_userfaultfd_and_exits_registered and liteinst_in_guest_refuses_a_guest_that_opens_dev_fuse) failed with the same error; liteinst_in_guest_reads_a_plain_file_on_a_fuse_filesystem starts the same runtime and was reported passed there only because the runner has no FUSE mount, so it returned before launching anything. This hosted twin excludes them with an exact-name filterset (-E 'not (test(=...) | ...)'), never a substring --skip, and test.cli keeps running all of them in the pinned root; the LiteInst refusal, fixture-staging and command-construction cases, which launch no real runtime, stay here. MEMORY RECALIBRATED 2026-08-25 (task remeasure_the_fourteen_stale): five current exact-command cgroup samples peaked at 339279872 bytes; the larger 4068401152-byte historical completed peak governs. The 4-GiB baseline rounds above that floor and the 6-GiB hard cap adds 2 GiB of headroom. See ai_docs/dag-memory-caps-recalibration-20260825.md."########,
        labels: &[r########"hosted-portable"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; HERMIT_LITEINST_TEST_BINARY=$PWD/target/ci/hermit ./ci/run-with-reverie-dbt-budget.sh ./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit --features third-party-backends --test cli -E 'not (test(=liteinst_backend_stats_report_the_guests_own_dispatch_paths) | test(=liteinst_in_guest_programs::in_guest_trap_dispatch_record_reports_no_patched_sites) | test(=liteinst_in_guest_programs::in_guest_trap_refuses_a_guest_library_that_turns_site_patching_back_on) | test(=liteinst_in_guest_programs::in_guest_trap_sets_site_patching_off_and_refuses_a_caller_who_turns_it_on) | test(=liteinst_in_guest_programs::in_guest_trap_sigalrm_scalar_identity_matches_ptrace) | test(=liteinst_in_guest_programs::in_guest_trap_verifies_a_program_with_no_site_patched) | test(=liteinst_in_guest_programs::liteinst_in_guest_a_root_that_aborts_ends_with_its_own_status) | test(=liteinst_in_guest_programs::liteinst_in_guest_abnormal_exit_after_registration_does_not_hang) | test(=liteinst_in_guest_programs::liteinst_in_guest_admitted_sigalrm_leaves_the_guest_heap_as_the_program_made_it) | test(=liteinst_in_guest_programs::liteinst_in_guest_an_undeliverable_sigalrm_is_a_loss_and_stops_the_guest) | test(=liteinst_in_guest_programs::liteinst_in_guest_checks_and_hides_the_config_fingerprint) | test(=liteinst_in_guest_programs::liteinst_in_guest_close_range_releases_ports_like_ptrace) | test(=liteinst_in_guest_programs::liteinst_in_guest_cpuid_in_a_late_loaded_library_runs) | test(=liteinst_in_guest_programs::liteinst_in_guest_alarm_outside_emulated_sleep_keeps_its_loss) | test(=liteinst_in_guest_programs::liteinst_in_guest_default_fatal_alarms_end_emulated_sleeps_and_verify) | test(=liteinst_in_guest_programs::liteinst_in_guest_noninterrupting_alarms_preserve_sleep_and_pending_state) | test(=liteinst_in_guest_programs::liteinst_in_guest_detcore_micro_suite) | test(=liteinst_in_guest_programs::liteinst_in_guest_dispatch_record_reports_patched_sites) | test(=liteinst_in_guest_programs::liteinst_in_guest_dup_aliases_share_one_cursor_after_fork) | test(=liteinst_in_guest_programs::liteinst_in_guest_encoding_and_digest_utilities) | test(=liteinst_in_guest_programs::liteinst_in_guest_exit_reaping_matches_ptrace) | test(=liteinst_in_guest_programs::liteinst_in_guest_file_and_text_utilities) | test(=liteinst_in_guest_programs::liteinst_in_guest_fork_child_heap_is_the_parents) | test(=liteinst_in_guest_programs::liteinst_in_guest_fork_runs_without_hanging) | test(=liteinst_in_guest_programs::liteinst_in_guest_formatting_and_sequence_utilities) | test(=liteinst_in_guest_programs::liteinst_in_guest_heap_growth_avoids_trampoline_mappings) | test(=liteinst_in_guest_programs::liteinst_in_guest_identity_utilities) | test(=liteinst_in_guest_programs::liteinst_in_guest_max_log_bytes_exits_promptly_when_stderr_is_a_full_pipe) | test(=liteinst_in_guest_programs::liteinst_in_guest_path_and_language_utilities) | test(=liteinst_in_guest_programs::liteinst_in_guest_python_entropy) | test(=liteinst_in_guest_programs::liteinst_in_guest_python_random_example) | test(=liteinst_in_guest_programs::liteinst_in_guest_reads_a_plain_file_on_a_fuse_filesystem) | test(=liteinst_in_guest_programs::liteinst_in_guest_refuses_a_guest_that_opens_dev_fuse) | test(=liteinst_in_guest_programs::liteinst_in_guest_round2_arithmetic_and_predicate_utilities) | test(=liteinst_in_guest_programs::liteinst_in_guest_round2_encoding_and_comparison_utilities) | test(=liteinst_in_guest_programs::liteinst_in_guest_round2_representation_and_path_utilities) | test(=liteinst_in_guest_programs::liteinst_in_guest_round3_encoding_and_compression_utilities) | test(=liteinst_in_guest_programs::liteinst_in_guest_round3_portable_system_utilities) | test(=liteinst_in_guest_programs::liteinst_in_guest_round3_stdin_filter_utilities) | test(=liteinst_in_guest_programs::liteinst_in_guest_runtime_bootstrap_is_not_charged_to_host_identity_uptime) | test(=liteinst_in_guest_programs::liteinst_in_guest_runtime_calls_none_of_the_converted_libc_wrappers) | test(=liteinst_in_guest_programs::liteinst_in_guest_runtime_leaves_the_guest_heap_as_the_program_made_it) | test(=liteinst_in_guest_programs::liteinst_in_guest_runtime_owned_self_signals_refuse_verification) | test(=liteinst_in_guest_programs::liteinst_in_guest_self_signal_deaths_are_scheduled_and_verify) | test(=liteinst_in_guest_programs::liteinst_in_guest_semantic_file_and_sqlite_utilities) | test(=liteinst_in_guest_programs::liteinst_in_guest_semantic_text_utilities) | test(=liteinst_in_guest_programs::liteinst_in_guest_serves_its_own_userfaultfd_and_exits_registered) | test(=liteinst_in_guest_programs::liteinst_in_guest_shell_and_entropy_consumer) | test(=liteinst_in_guest_programs::liteinst_in_guest_sigalrm_committed_at_an_instruction_trap_runs_before_exit) | test(=liteinst_in_guest_programs::liteinst_in_guest_sigalrm_handler_is_virtual_and_published) | test(=liteinst_in_guest_programs::liteinst_in_guest_sigalrm_handler_runs_at_syscall_completion) | test(=liteinst_in_guest_programs::liteinst_in_guest_sigalrm_pending_at_handler_return_runs_before_the_next_syscall) | test(=liteinst_in_guest_programs::liteinst_in_guest_tool_directory_reads_stay_out_of_the_guest_heap) | test(=liteinst_in_guest_programs::liteinst_in_guest_unscheduled_deaths_complete_and_refuse_verification) | test(=liteinst_in_guest_programs::liteinst_in_guest_user_address_limit_queries_keep_the_guest_errno) | test(=liteinst_in_guest_programs::liteinst_in_guest_virtual_identity_and_time) | test(=liteinst_in_guest_programs::liteinst_sigalrm_scalar_identity_keeps_handler_admission_controls) | test(=liteinst_in_guest_refuses_a_guest_socket_at_the_forwarding_number) | test(=liteinst_in_guest_verify_compares_the_records_the_guest_forwards) | test(=liteinst_in_guest_verify_forwards_records_by_the_cli_filters_per_target_answer) | test(=liteinst_in_guest_verify_log_keeps_records_in_ptraces_order) | test(=liteinst_in_guest_verify_survives_a_guest_stderr_without_a_reader) | test(=liteinst_in_guest_verify_with_records_past_the_log_bound_is_no_result) | test(=run_liteinst_finds_the_runtime_staged_as_an_installed_resource))' -- --skip run_kvm_ --skip backend_accepted_in_global_position --skip run_dbt_aggregates_unsupported_syscalls_and_strict_rejects_them --skip run_dbt_strict_returns_with_blocked_stdin_source --skip run_dbt_verifies_pipe_backpressure --skip run_dbt_keeps_diagnostics_out_of_guest_stderr --skip run_dbt_recovers_after_failed_exec --skip run_dbt_fails_closed_by_default_and_opt_out_aggregates_unsupported_syscalls --skip run_dbt_verifies_queued_self_signals --skip run_dbt_verifies_self_prlimit --skip run_dbt_verifies_shell_process_lifecycle --skip run_dbt_verifies_simple_env_shebang --skip run_liteinst_rejects_non_fork_clone --skip run_liteinst_handles_inherited_ignored_sigchld --skip run_liteinst_verifies_forked_guest --skip run_liteinst_verifies_raw_fork_guest --skip skid_overshoot_and_guest_failure_have_different_exit_codes --skip run_ptrace_nonleader_exec_preserves_identity_and_time --skip run_ptrace_nonleader_exec_preserves_preemption --skip run_ptrace_nonleader_exec_displaces_runnable_leader --skip run_ptrace_nonleader_exec_refuses_preemption_artifacts --skip run_chaos_preemption_replay_reuses_the_recorded_epoch --skip max_log_bytes_preemption_stacktrace_exits_promptly_when_stderr_is_a_full_pipe --skip poll_timeout_returns_on_time_while_another_thread_spins --skip happens_before_relative_anchor_holds_the_first_call_after_its_base --skip happens_before_relative_anchor_state_is_not_inherited_by_a_new_thread --skip happens_before_unfired_relative_anchor_is_refused_with_its_base --skip happens_before_hold_budget_ends_a_hold_a_spinner_never_releases --skip happens_before_posthook_anchor_on_an_interrupted_call_is_refused_by_name --skip happens_before_hold_in_a_vfork_child_is_refused_by_name"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: Some(&[r########"cli"########]),
        deps: &[
            r########"build.e2e_artifact"########,
            r########"setup.nextest"########,
        ],
        // A hermit-dap missing from the build must fail the hermit_dap_*
        // cases here rather than let them skip
        // (https://github.com/rrnewton/hermit/issues/3419).
        env: &[(r########"HERMIT_REQUIRE_DAP"########, r########"1"########)],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 150.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(6442450944),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: Some(4),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 900,
        cpu_timeout: 7200,
        jobs_flag: Some(""),
        jobs_env: Some("NEXTEST_TEST_THREADS"),
    },
    StaticStepSpec {
        group: r########"test"########,
        job: r########"sabre_examples"########,
        desc: r########"SaBRe non-racy examples: per-backend time determinism and non-time ptrace parity"########,
        description: r########"Holds the SaBRe backend to ptrace's results on non-racy examples: hermit-cli/tests/sabre_examples.rs runs one test at a time, its count pinned by NEXTEST_EXPECTED_EXECUTED, against an explicitly configured SaBRe loader, so a missing loader, libdetcore_sabre.so or revision file fails rather than skips. examples/devrand.sh, a root-PID shell probe and a glibc getrandom caller must give identical status, stdout and stderr under ptrace and --backend sabre; examples/date.sh and a clock-progress guest must repeat across three runs on each backend; and every SaBRe guest must pass `--verify --verify-strict` with bitwise_parity true. A guest that points its stderr at a pipe and forks a child that only exits must, at --log=warn, read nothing from that pipe on either backend (at --log=info the plugin's DETLOG forwarding still writes there, and that run is only checked for repeatability). Under --verify the plugin sends its DETLOG records on a socket reverie-sabre keeps from the guest, so the run log must match ptrace's through the post-exec AT_RANDOM record, and a guest that dup2s onto every descriptor from 1024 to 1039 must still run to the end with its records in the log. SaBRe letting getrandom return host entropy, a controller line leaking into guest stderr, the plugin's fingerprint warning reaching a forked child's guest stderr, the in-guest records appended after the run instead of interleaved, a run whose forwarded records went missing (a guest that closes the socket before the plugin adopts it) being compared instead of refused, the plugin writing its records into a file the guest made its own stderr after the socket it was given could not be adopted, or the plugin adopting a socket of the guest's own that guest code put at the passed number before the plugin ran, is caught."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; HERMIT_SABRE_TEST_BINARY=$PWD/target/ci/hermit HERMIT_SABRE_BINARY=$PWD/target/install_pkg/rsrcs/sabre ./ci/run-with-reverie-dbt-budget.sh ./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit --features third-party-backends --test sabre_examples -j 1"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: Some(&[r########"sabre_examples"########]),
        deps: &[
            r########"build.e2e_artifact"########,
            r########"setup.nextest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 90.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(3221225472),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 900,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"test"########,
        job: r########"hermit_modes"########,
        desc: r########"Portable Hermit mode cases (skips default_ and chaos_buck_)"########,
        description: r########"MEMORY RECALIBRATED 2026-08-25 (task remeasure_the_fourteen_stale): five current exact-command cgroup samples peaked at 286810112 bytes; the larger 2038853632-byte historical completed peak governs. The 2-GiB baseline rounds above that floor and the 4-GiB hard cap adds 2 GiB of headroom. See ai_docs/dag-memory-caps-recalibration-20260825.md."########,
        labels: &[r########"full"########, r########"portable"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-reverie-dbt-budget.sh ./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit --features third-party-backends --test hermit_modes -j 1 -- --skip default_ --skip chaos_buck_ --skip hello_race_chaos_verify"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: Some(&[r########"hermit_modes"########]),
        deps: &[
            r########"build.e2e_artifact"########,
            r########"setup.nextest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[(r########"integration_test_binaries.hermit_modes"########, 1)],
            est_duration_s: 150.0,
            rss_baseline_bytes: Some(2147483648),
            hard_mem_max_bytes: Some(4294967296),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 900,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"test"########,
        job: r########"hermit_modes_on_host"########,
        desc: r########"Portable Hermit mode cases (skips default_ and chaos_buck_)"########,
        description: r########"MEMORY RECALIBRATED 2026-08-25 (task remeasure_the_fourteen_stale): five current exact-command cgroup samples peaked at 286810112 bytes; the larger 2038853632-byte historical completed peak governs. The 2-GiB baseline rounds above that floor and the 4-GiB hard cap adds 2 GiB of headroom. See ai_docs/dag-memory-caps-recalibration-20260825.md."########,
        labels: &[r########"hosted-portable"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-reverie-dbt-budget.sh ./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit --features third-party-backends --test hermit_modes -j 1 -- --skip default_ --skip chaos_buck_ --skip hello_race_chaos_verify"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: Some(&[r########"hermit_modes"########]),
        deps: &[
            r########"build.e2e_artifact"########,
            r########"setup.nextest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 150.0,
            rss_baseline_bytes: Some(2147483648),
            hard_mem_max_bytes: Some(4294967296),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 900,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"test"########,
        job: r########"app_strict_verify"########,
        desc: r########"Portable application strict verification (skips java/javac and mountinfo-dependent Go cases)"########,
        description: r########"MEMORY RECALIBRATED 2026-08-25 (task remeasure_the_fourteen_stale): five successful current exact-command cgroup samples peaked at 1502470144 bytes, plus one failed/censored attempt; the larger 2838315008-byte historical completed peak governs, while prior 4-GiB pressure argues against a tight hard ceiling. The 3-GiB baseline rounds above the floor and the 6-GiB hard cap preserves 3 GiB of adverse-load headroom. See ai_docs/dag-memory-caps-recalibration-20260825.md."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-reverie-dbt-budget.sh ./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit --features third-party-backends --test app_strict_verify -j 1 -- --ignored --skip java_ --skip javac_ --skip go_hello_is_deterministic_under_strict_verify --skip go_goroutines_are_deterministic_under_strict_verify"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: Some(&[r########"app_strict_verify"########]),
        deps: &[
            r########"build.e2e_artifact"########,
            r########"setup.nextest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 180.0,
            rss_baseline_bytes: Some(3221225472),
            hard_mem_max_bytes: Some(6442450944),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 900,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"test"########,
        job: r########"command_strict_verify"########,
        desc: r########"Portable command strict verification (skips mountinfo-dependent kernel pseudofile case)"########,
        description: r########"MEMORY RECALIBRATED 2026-08-25 (task remeasure_the_fourteen_stale): five current exact-command cgroup samples peaked at 239931392 bytes; the larger 1948848128-byte historical completed peak governs. The 2-GiB baseline rounds above that floor and the 4-GiB hard cap adds 2 GiB of headroom. See ai_docs/dag-memory-caps-recalibration-20260825.md."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-reverie-dbt-budget.sh ./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit --features third-party-backends --test command_strict_verify -j 1 -- --ignored --skip kernel_pseudofile_commands_are_deterministic_under_strict_verify"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: Some(&[r########"command_strict_verify"########]),
        deps: &[
            r########"build.e2e_artifact"########,
            r########"setup.nextest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 180.0,
            rss_baseline_bytes: Some(2147483648),
            hard_mem_max_bytes: Some(4294967296),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 900,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"test"########,
        job: r########"ignored_syscall_regressions"########,
        desc: r########"Portable ignored syscall regressions (epoll_determinism, rcx_canonicalization)"########,
        description: r########"MEMORY RECALIBRATED 2026-08-25 (task remeasure_the_fourteen_stale): five current exact-command cgroup samples peaked at 268689408 bytes; the larger 1774698496-byte historical completed peak governs. The 2-GiB baseline rounds above that floor and the 4-GiB hard cap adds 2 GiB of headroom. See ai_docs/dag-memory-caps-recalibration-20260825.md."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-reverie-dbt-budget.sh ./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit --features third-party-backends --test epoll_determinism --test rcx_canonicalization -j 1 -- --ignored"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: Some(&[
            r########"epoll_determinism"########,
            r########"rcx_canonicalization"########,
        ]),
        deps: &[
            r########"build.e2e_artifact"########,
            r########"setup.nextest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 90.0,
            rss_baseline_bytes: Some(2147483648),
            hard_mem_max_bytes: Some(4294967296),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 720,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"test"########,
        job: r########"rr_suite_contract"########,
        desc: r########"rr suite source contract (scratch dirs fresh and cleaned)"########,
        description: r########"MEM-CAP DERIVATION 2026-08-04 (follow-on to task memory-caps-must-scale-with-job-count / #1583, which capped the OTHER compile-bearing nodes but MISSED this one): pinned_jobs=8 via the scheduler-controlled CARGO_BUILD_JOBS channel (was unset -> the test binary's build phase defaulted to nproc, and under the full portable profile that cc1plus fan-out blew the old 2.0GiB cap -> OOM; the job-pin is the actual fix). rss_baseline=measured_peak@j8=3.5GiB; method=cgroup-RECORDED (dagrun step_profiles peak_bytes == cgroup memory.peak, NOT sampled); target_state=warm/production-representative (depends on build.workspace); hermit@b384187e. CAVEAT: peak_bytes is cap-influenced because compile page-cache is reclaimable (a negative-control run at a 512MiB cap PASSED via reclaim, oom_kills=0) -> the true non-reclaimable working set at j8 is well under this figure; the cap is conservative headroom, the pin is the fix. headroom=+1.5GiB additive -> hard_mem_max_bytes=5.0GiB (was 2.0GiB, which was too tight once the node was left unpinned). classification light->cpu-bound (compile-bearing). CPU-WIDTH DERIVATION 2026-08-10 (task p0_dynamorio_build_cache): preferred_inner_jobs=8 is delivered through the CARGO_BUILD_JOBS jobs_env channel. See doc.rustdoc for the shared derivation; jobs_flag is empty because appending '-j 8' here would land after '--' and reach libtest, which rejects it."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-reverie-dbt-budget.sh ./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit --features third-party-backends --test rr_suite -j 1 rr_scratch_directories_are_fresh_and_cleaned -- --exact"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: Some(&[r########"rr_suite"########]),
        deps: &[
            r########"build.e2e_artifact"########,
            r########"setup.nextest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 30.0,
            rss_baseline_bytes: Some(3758096384),
            hard_mem_max_bytes: Some(5368709120),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: Some(8),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 720,
        cpu_timeout: 7200,
        jobs_flag: Some(r########""########),
        jobs_env: Some(r########"CARGO_BUILD_JOBS"########),
    },
    StaticStepSpec {
        group: r########"test"########,
        job: r########"envelope_levels"########,
        desc: r########"Portable working-envelope levels (L1-L4 over true/echo/date; implemented by scripts/validate.rs)"########,
        description: r########"MEMORY RECALIBRATED 2026-08-25 (task remeasure_the_fourteen_stale): five current exact-command cgroup samples peaked at 28729344 bytes and the historical completed maximum is 61661184 bytes. The 128-MiB baseline is above both the p90-plus-20-percent estimate and that floor; the 1-GiB hard cap leaves 896 MiB of runaway headroom. See ai_docs/dag-memory-caps-recalibration-20260825.md."########,
        labels: &[
            r########"full"########,
            r########"hosted-portable"########,
            r########"portable"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; set -e; HERMIT=target/ci/hermit; ARGS='run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled'; REPS=${L4_REPS:-20}; EXECUTED=0; CURRENT_TEST=''; RESULTS=(); publish_counts() { local status=$? count_status; trap - EXIT; set +e; if ((status != 0)) && [[ -n $CURRENT_TEST ]]; then RESULTS+=("envelope/$CURRENT_TEST" fail 1); fi; ./ci/write-structured-test-counts.sh "$EXECUTED" 0 "${RESULTS[@]}"; count_status=$?; if ((status == 0 && count_status != 0)); then status=$count_status; fi; exit "$status"; }; trap publish_counts EXIT; run_probe() { local id="$1"; local c="$2"; CURRENT_TEST="$id"; EXECUTED=$((EXECUTED + 1)); printf '##TEST-START %s\n' "$id" >&2; timeout 30s $HERMIT $ARGS --strict -- $c </dev/null >&2; timeout 30s $HERMIT $ARGS --strict --verify -- $c </dev/null >&2; timeout 30s $HERMIT $ARGS --strict --verify --detlog-heap --detlog-stack -- $c </dev/null >&2; local i; for ((i=0;i<REPS;i++)); do timeout 30s $HERMIT $ARGS --strict --verify -- $c </dev/null >&2; done; printf '##TEST-END %s PASS\n' "$id" >&2; RESULTS+=("envelope/$id" pass 1); CURRENT_TEST=''; }; run_probe true '/bin/true'; run_probe echo '/bin/echo hermit-envelope'; run_probe date '/bin/date -u +%Y'"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"build.e2e_artifact"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 120.0,
            rss_baseline_bytes: Some(134217728),
            hard_mem_max_bytes: Some(1073741824),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 900,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"privileged-build"########,
        job: r########"privileged_tests"########,
        desc: r########"Build Hermit and the focused test binaries used by the privileged lane"########,
        description: r########"MEM-CAP DERIVATION 2026-08-04 (follow-on to task memory-caps-must-scale-with-job-count / #1583, which MISSED this privileged-lane build node): pinned_jobs=8 literal via CARGO_BUILD_JOBS=8 on all three Cargo commands (was unset -> cargo defaulted build phases to nproc; that cc1plus fan-out is the OOM class the pin bounds; the job-pin is the actual fix). The final command prebuilds the exact cli and hermit_modes integration binaries consumed by the privileged test nodes, so their wall budgets measure test execution instead of Cargo compilation or target-lock waiting. After the bin build, the content-addressed publisher verifies source type/mode/size, hashes before and after copying, verifies the published hash and atomically updates the pointer before any later Cargo invocation can relink target/debug/hermit. This privileged lane deliberately publishes a binary-only artifact; unlike portable DBT/SaBRe/LiteInst cells, it does not consume install_pkg resources. Cap KEPT at the existing generous 8.0GiB: a warm/incremental measurement read peak_bytes~1.64GiB @j8, but that reused build.workspace artifacts and UNDER-estimates a cold from-scratch privileged build, so the 8.0GiB headroom is retained rather than tightened on an unreliable warm figure. rss_baseline=5.0GiB (scheduling reservation). hermit@b384187e."########,
        labels: &[r########"full"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/verify-hermit-e2e-artifact.sh target/ci/hermit-e2e-artifact.path >/dev/null && ./ci/nextest-binaries.rs assert privileged && tests_misc="$(./ci/nextest-binaries.rs executable hermit-detcore tests_misc)" || exit 1"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact"########,
            r########"gate.manifest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[
                (r########"integration_test_binaries.cli"########, 1),
                (r########"integration_test_binaries.hermit_modes"########, 1),
            ],
            est_duration_s: 60.0,
            rss_baseline_bytes: Some(5368709120),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 120,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"privileged-cpuid"########,
        job: r########"faulting"########,
        desc: r########"CPUID-faulting smoke: Detcore masks host RDRAND/RDSEED feature bits"########,
        description: r########"REQUIRES A MACHINE FACILITY, DECLARED 2026-08-12 (hermit#2135, hermit#2148, hermit#2205). rdrand_rdseed_is_masked can only observe Detcore masking host feature bits if the kernel can trap the guest's CPUID, which needs arch_prctl(ARCH_SET_CPUID, 0) to succeed. Where it cannot, this node used to FAIL in 0.11 s with exit 101 and an empty detail block -- indistinguishable from a broken build -- and its eager-exit aborted the twelve other in-flight nodes and filtered twenty-seven more, so a machine that runs 31 of 33 nodes in 3m22s produced no receipt at all. `requires_host_capability` moves that judgement OUT of the node, to an out-of-band probe run during plan construction, and makes the outcome a third recorded state: host-inapplicable, which is neither a pass nor a failure and is written to the ledger as a typed intentional skip. THIS DOES NOT WEAKEN THE TEST. Where the capability is present the node runs unchanged and its assertions keep full force; the probe fails closed toward running, so a probe error or an unexpected errno still runs it. The capability name is checked against the closed vocabulary in scripts/lib/validate_plan.rs::HostCapability, and an unknown name refuses the whole run."########,
        labels: &[r########"full"########, r########"cpuid-faulting"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; tests_misc="$(./ci/nextest-binaries.rs executable hermit-detcore tests_misc)" || exit 1; timeout 30 "$tests_misc" rdrand_rdseed_is_masked --exact"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"privileged-build.privileged_tests"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 15.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(17179869184),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 40,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"privileged-pmu"########,
        job: r########"preemption"########,
        desc: r########"PMU smoke: retired-conditional-branch overflow delivery and skid measurement"########,
        description: r########"Compiles tests/util/pmu_skid.c in the pinned root and runs it for 16 overflow periods of 100000 retired conditional branches on a traced child pinned to one CPU, requiring each overflow to stop the child, mid-loop, with the counter's SIGUSR1; it prints the measured skid but does not bound it. privileged-test.pmu_buck_chaos_cases, pmu_ptrace_completion_cases, pmu_cli_cases and pmu_detcore_time_cases depend on it, so a host whose PMU cannot interrupt a traced thread fails here in seconds with the cause rather than deep inside those suites. Refusal of perf_event_open or PTRACE_TRACEME, a CPU that is neither Intel nor AMD, a descheduled counter, or no overflow signal within 20 seconds turns it red."########,
        labels: &[r########"full"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; cc -O2 -Wall -Wextra -Werror tests/util/pmu_skid.c -o target/ci-pmu-skid && timeout 20 target/ci-pmu-skid --iterations 16 --period 100000"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"privileged-build.privileged_tests"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 5.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(17179869184),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 30,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"privileged-test"########,
        job: r########"pmu_buck_chaos_cases"########,
        desc: r########"Run the six measured-passing Buck chaos cases under PMU preemption"########,
        description: r########"The six enabled cases passed direct measurement on the privileged host. chaos_buck_nanosleep_parallel remains ignored because an interrupted nanosleep reports EINTR; chaos_buck_mem_race remains ignored because it produced no verdict within 300 seconds. Both remain visible in super.pmu_buck_chaos_cases. Depending on pmu.preemption keeps this coverage in the existing privileged PMU partition without changing quick or portable-only."########,
        labels: &[r########"full"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; set -uo pipefail; status=0; ./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit --features third-party-backends --test hermit_modes -j 1 -E 'test(/^(chaos_buck_getpid|chaos_buck_uname|chaos_buck_sysinfo|chaos_buck_wait_on_child|chaos_buck_clone|chaos_buck_hello_alarm)$/)' || status=$?; exit "$status""########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: Some(&[r########"hermit_modes"########]),
        deps: &[
            r########"privileged-build.privileged_tests"########,
            r########"privileged-pmu.preemption"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[(r########"integration_test_binaries.hermit_modes"########, 1)],
            est_duration_s: 45.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(17179869184),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 120,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"privileged-test"########,
        job: r########"pmu_ptrace_completion_cases"########,
        desc: r########"Run the five real-random ptrace completion cases that arm the reverie PMU timer"########,
        description: r########"The five ptrace_completion::tests::real_random_ cases run a real guest under Reverie ptrace with the default timeslice, which arms the PMU timer; without a PMU the run is refused with Perf support required. The hosted runner has no PMU (https://github.com/rrnewton/hermit/actions/runs/36499357369), so test.hermit_unit and its hosted twin skip exactly these five and this node runs them in full, after privileged-pmu.preemption has shown the PMU works. Measured on the development host recorded in docs/TESTING_ENVIRONMENTS.md, "Named measurement hosts": the five pass in 1.94 s wall with a 33 MB peak RSS; the hint keeps the pmu_buck_chaos_cases budget shape."########,
        labels: &[r########"full"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; set -uo pipefail; status=0; ./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit --features third-party-backends,kvm-native-test-support --lib --bins -j 1 -E 'test(/^ptrace_completion::tests::real_random_/)' || status=$?; exit "$status""########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[
            r########"privileged-build.privileged_tests"########,
            r########"privileged-pmu.preemption"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 30.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 120,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"privileged-test"########,
        job: r########"pmu_cli_cases"########,
        desc: r########"Run the CLI cases whose subject is the PMU"########,
        description: r########"skid_overshoot_and_guest_failure_have_different_exit_codes asserts that the skid injector induces a PMU overshoot, so the PMU is its subject; on a host without one it fails with skid injector did not induce an overshoot. The hosted runner has no PMU (https://github.com/rrnewton/hermit/actions/runs/36499357369), so test.cli and test.cli_on_host skip exactly this case and this node runs it in full. The four ptrace nonleader-exec cases run_ptrace_nonleader_exec_{preserves_identity_and_time,preserves_preemption,displaces_runnable_leader,refuses_preemption_artifacts} assert the actual PMU counter, the PMU timer and PMU preemption across exec; with perf_event_open blocked (EACCES) they fail with worker clock accounted before exec or actual PMU preemption, or the displaced leader never yields and hits its 54 s wall limit, while run_ptrace_nonleader_exec_exit_only passes and stays in test.cli (https://github.com/rrnewton/hermit/pull/3268). They are skipped by exact name in test.cli and test.cli_on_host and run here, after privileged-pmu.preemption has shown the PMU works. run_chaos_preemption_replay_reuses_the_recorded_epoch (https://github.com/rrnewton/hermit/issues/3413) records a --chaos run, requires its PMU-timer preemption points to be present, and replays them without --epoch; without a PMU the recording has no preemption points and the case fails its own premise check, so it is skipped the same way and runs here. poll_timeout_returns_on_time_while_another_thread_spins requires a 50 ms ppoll to return within 100 ms of virtual time, five times, while another thread spins with no syscalls; only the PMU timer (--max-timeslice) preempts that spinner, so without a PMU the case proves nothing and is skipped the same way (https://github.com/rrnewton/hermit/issues/3952). max_log_bytes_preemption_stacktrace_exits_promptly_when_stderr_is_a_full_pipe prints the guest's stack at every PMU timer preemption under --chaos --preemption-stacktrace and requires a 448K-capped run whose stderr is a full unread pipe to end with 123 within its 45 s bound; without a PMU there is no preemption and it fails its own premise check, so it is skipped the same way and runs here. It consumes the shared cli binary published by privileged-build.privileged_tests and therefore holds the integration_test_binaries.cli token like the other consumers. Measured on the development host recorded in docs/TESTING_ENVIRONMENTS.md, "Named measurement hosts": 0.44 s for the skid case alone, 23.2-30.7 s of nextest time for the five cases the node had on 2026-09-29 over 20 runs at one-minute load averages of 63-226, and 40.6-41.6 s for all seven cases together over 5 runs at one-minute load averages of 286-322 on 2026-10-06, so the wall limit is 300 s. poll_timeout_returns_on_time_while_another_thread_spins runs Hermit again, at most five times in all, only when a run exits 122 with the typed skid-overshoot refusal line HERMIT_POLICY_REFUSAL class=policy-refusal cause=skid-overshoot count=N and nothing else (https://github.com/rrnewton/hermit/issues/1845, https://github.com/rrnewton/reverie/issues/990), and prints a SKID-RETRY line for every retried attempt; NEXTEST_SUCCESS_OUTPUT=final prints each case's captured output even when it passes, so those lines reach this node's log rather than being discarded with a passing case's output. One attempt of that case cost 8.2-10.7 CPU-s and 8.4-10.9 s of wall time in 13 local validation runs on 2026-10-09, so five attempts need up to about 54 CPU-s and 55 s; HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER=4 raises each case's CPU budget from 22 to 88 CPU-s and HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER=2 its wall bound from 57 to 114 s, since at the defaults a third attempt would be killed by the CPU budget. The retrying case is bounded by its 114 s wall bound, and with the other seven cases (about 30 s together in the 2026-10-06 measurement) the node stays below its 300 s limit. Six happens-before cases, happens_before_relative_anchor_holds_the_first_call_after_its_base, happens_before_relative_anchor_state_is_not_inherited_by_a_new_thread, happens_before_unfired_relative_anchor_is_refused_with_its_base, happens_before_hold_budget_ends_a_hold_a_spinner_never_releases, happens_before_posthook_anchor_on_an_interrupted_call_is_refused_by_name and happens_before_hold_in_a_vfork_child_is_refused_by_name, load version-2 specs, which bound every hold by max_hold_ns and need the PMU preemption timer to charge it. Without perf counters Hermit turns the timer off and then refuses each run with HERMIT_HB_HOLD_WITHOUT_PREEMPTION before the guest starts, so none of them reaches what it asserts; all six failed that way in hosted portable run https://github.com/rrnewton/hermit/actions/runs/38110887257. The PMU is not their subject, but they cannot run without it, so test.cli and test.cli_on_host skip them by exact name and this node runs them. happens_before_version_2_without_perf_counters_is_refused_before_stdin, which denies perf_event_open itself to check that refusal, stays in both. Measured 2026-10-11 on the measurement host recorded for this node in docs/TESTING_ENVIRONMENTS.md, at one-minute load averages of 138-140: the six alone took 9.0 s of nextest time at -j 1 (happens_before_hold_budget_ends_a_hold_a_spinner_never_releases 7.4 s, each other case under 0.5 s), and this node's command with all fourteen cases took 114.4 s of nextest time (the max_log_bytes preemption case 48.9 s, the poll spinner 20.2 s), so the node stays below its 300 s limit."########,
        labels: &[r########"full"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; set -uo pipefail; status=0; NEXTEST_SUCCESS_OUTPUT=final ./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit --features third-party-backends --test cli -j 1 -E 'test(=skid_overshoot_and_guest_failure_have_different_exit_codes) | test(=run_ptrace_nonleader_exec_preserves_identity_and_time) | test(=run_ptrace_nonleader_exec_preserves_preemption) | test(=run_ptrace_nonleader_exec_displaces_runnable_leader) | test(=run_ptrace_nonleader_exec_refuses_preemption_artifacts) | test(=run_chaos_preemption_replay_reuses_the_recorded_epoch) | test(=max_log_bytes_preemption_stacktrace_exits_promptly_when_stderr_is_a_full_pipe) | test(=poll_timeout_returns_on_time_while_another_thread_spins) | test(=happens_before_relative_anchor_holds_the_first_call_after_its_base) | test(=happens_before_relative_anchor_state_is_not_inherited_by_a_new_thread) | test(=happens_before_unfired_relative_anchor_is_refused_with_its_base) | test(=happens_before_hold_budget_ends_a_hold_a_spinner_never_releases) | test(=happens_before_posthook_anchor_on_an_interrupted_call_is_refused_by_name) | test(=happens_before_hold_in_a_vfork_child_is_refused_by_name)' || status=$?; exit "$status""########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: Some(&[r########"cli"########]),
        deps: &[
            r########"privileged-build.privileged_tests"########,
            r########"privileged-pmu.preemption"########,
        ],
        env: &[
            (
                r########"HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER"########,
                r########"4"########,
            ),
            (
                r########"HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER"########,
                r########"2"########,
            ),
        ],
        hint: HintSpec {
            resources: &[(r########"integration_test_binaries.cli"########, 1)],
            est_duration_s: 30.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 300,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"privileged-test"########,
        job: r########"pmu_detcore_time_cases"########,
        desc: r########"Run the 36 Detcore time cases that need the PMU (tests_time, serial execution)"########,
        description: r########"The 36 tests_time cases that construct a Config with max_timeslice enabled and therefore exercise ptrace's perf_event_open-backed RCB clock/timer: 28 inherit the 200000000 default through ..Default::default(), the four tod_gettimeofday_delta variants use the testutils BOTTOM, MIDDLE, and TOP configs (5000000) or the default, max_timeslice_preempts_cpu_bound_code_without_rcb_logical_time sets 1000000, rdtsc_inside_a_spinlock_does_not_end_a_target_timeslice sets 2000000, and a_futex_wake_hands_a_contended_mutex_to_the_waiter and a_futex_wake_handoff_is_deterministic set 5000000. Without a PMU each fails with Perf support required. The hosted runner has none, and test.detcore_time_on_host failed exactly these 29 of 31 there (https://github.com/rrnewton/hermit/actions/runs/37143469197, https://github.com/rrnewton/hermit/issues/3663), so they moved here, after privileged-pmu.preemption has shown the PMU works. The filter is the exact complement of test.detcore_time's, which keeps the two cases that disable max_timeslice, so the full label still runs all 38 cases once each, and a case added to tests_time later is selected here until it is placed deliberately. tod_gettimeofday_faulting_tz_pkey_write_disabled_leaves_tv_unchanged also needs memory protection keys: on a host whose CPU flags lack pku or ospke, pkey_alloc returns -1 and the test fails rather than skips. MEASURED 2026-10-03 at hermit c3f7b2bcfa00 on the measurement host recorded for privileged-test.pmu_detcore_time_cases in docs/TESTING_ENVIRONMENTS.md ("Named measurement hosts"): five runs of this selection with cargo nextest run -p hermit-detcore --test tests_time -j 1 and this filter, invoked directly rather than through run-nextest-counted.sh, each ran 29 tests with 2 filtered out, in 4.72-5.90 s of wall time (4.15-5.00 s of nextest time), the largest process peaking at 68-76 MB RSS. The hint keeps test.detcore_time's measured whole-target budget (256-MiB scheduling baseline, 1-GiB hard cap, 7 s estimate, 720 s wall limit), which these runs fit inside."########,
        labels: &[r########"full"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; set -uo pipefail; status=0; ./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit-detcore --test tests_time -j 1 -E 'not (test(=proc_stat_btime_is_fixed_for_a_fractional_epoch) | test(=target_timeslice_yields_at_syscall_boundaries_without_pmu))' || status=$?; exit "$status""########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[
            r########"privileged-build.privileged_tests"########,
            r########"privileged-pmu.preemption"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 7.0,
            rss_baseline_bytes: Some(268435456),
            hard_mem_max_bytes: Some(1073741824),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 720,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"test"########,
        job: r########"pmu_integration_cases"########,
        desc: r########"Run the 25 external_signal_interrupt ptrace cases that need the PMU timer"########,
        description: r########"The 25 external_signal_interrupt ptrace cases whose assertions hold only with the PMU preemption timer: each bounds a guest-observed virtual time (a 300 ms timed wait must end within 300-350 virtual ms, a wait ended by a signal within its slack) or requires a spinning guest thread to make progress before a watchdog. GitHub-hosted runners have no PMU, so hermit continues with --max-timeslice=disabled; in https://github.com/rrnewton/hermit/actions/runs/37631051557 at hermit ccbe5ae8 test.hermit_integration_on_host failed exactly these 25, with 300 ms waits taking 454-895 virtual ms. Forcing --max-timeslice=disabled into the cells locally fails the same 25 of the binary's 69 and no other; with the timer all 69 pass. The overshoot without the timer is a product defect of its own, not a test defect, and these assertions keep full force here. test.hermit_integration and its hosted twin skip the 25 by exact name; this node runs the same prepared selection with the complementary filter, so the full label runs each case once, after privileged-pmu.preemption has shown the PMU works. Like the privileged-test PMU nodes it carries only the full label: local scripts/validate.rs full runs it, .github/workflows/ci-privileged.yml does not. It is in the test group, not privileged-test, because it executes the 54 hermit-cli integration binaries of test.hermit_integration and declares them (ci/audit-test-binary-registration.py requires the declaration to match the executed targets); a privileged-* consumer of those binaries would make each one a fused shared binary of privileged-build.privileged_tests, which this node does not use. Memory hints are test.hermit_integration's, whose selection this is. Measured on the measurement host recorded in docs/TESTING_ENVIRONMENTS.md, 2026-10-07: cargo nextest run --profile ci with this selection and filter, -j 1, ran 25 of 25 passing in 76.3-94.2 s of nextest time over three runs at one-minute load averages of 21-31, so the estimate is 80 s and the wall limit 600 s."########,
        labels: &[r########"full"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install ./ci/run-with-reverie-dbt-budget.sh ./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit --features third-party-backends --test aio_nr_determinism --test arch_status_determinism --test chaos_sched_yield_progress --test chaos_stress_pmu_detection --test child_time_rpc --test chown_virtual_root_identity --test cli_owned_lifecycle --test clock_determinism --test clock_discipline_determinism --test clock_passthrough --test container_init_deadline --test cpufreq_avg_determinism --test dispatch_stats --test epoll_determinism --test epoll_pwait_zero_timeout_progress --test external_signal_interrupt --test fatal_core_capture --test file_nr_determinism --test first_seen_mtime --test fp_reduction_determinism --test futex2_refusal --test hashseed_determinism --test inode_nr_determinism --test kernel_keyring --test key_users_determinism --test mmap_determinism --test node_vmstat_determinism --test numa_maps_determinism --test perf_event_refusal --test pidfd_creation --test process_isolation_refusals --test proc_fdinfo_determinism --test proc_locks_determinism --test procfs_determinism --test procfs_positioned_determinism --test pty_nr_determinism --test python_stdlib --test reopened_pipe_progress --test robust_futex_owner_death --test run_evidence --test self_sched_determinism --test self_schedstat_determinism --test signal_determinism --test smaps_determinism --test smaps_rollup_determinism --test softnet_stat_determinism --test sockstat_determinism --test swaps_determinism --test thp_stats_determinism --test utimensat_mtime --test verification_report_cli --test verification_report_consumers --test verify_claim_names_its_limit --test writev_determinism --test zero_copy_pipe_fallback -j 1 -E 'test(=ptrace_a_handlers_first_restart_syscall_resumes_the_interrupted_timed_futex_wait) | test(=ptrace_child_waits_restart_under_sa_restart) | test(=ptrace_futex_wait_is_ended_by_a_caught_signal_pending_with_a_default_ignored_sigchld) | test(=ptrace_futex_wait_is_ended_by_a_signal_caught_after_it_parked) | test(=ptrace_futex_wait_is_not_ended_by_a_signal_ignored_after_it_parked) | test(=ptrace_futex_wait_of_the_thread_that_forked_the_child_takes_its_sigchld) | test(=ptrace_other_waits_take_a_discarded_default_stop_as_on_linux) | test(=ptrace_poll_and_epoll_are_ended_by_the_sigchld_of_an_exiting_child) | test(=ptrace_polling_futex_wait_holds_a_sigchld_caught_after_it_parked_until_it_returns) | test(=ptrace_polling_futex_wait_is_ended_by_the_sigchld_of_an_exiting_child) | test(=ptrace_polling_timed_futex_wait_keeps_its_deadline_when_a_runnable_sibling_takes_the_sigchld) | test(=ptrace_polling_untimed_futex_wait_keeps_waiting_when_a_runnable_sibling_takes_the_sigchld) | test(=ptrace_precise_futex_wait_is_ended_by_a_sigchld_caught_after_it_parked) | test(=ptrace_precise_futex_wait_is_ended_by_the_sigchld_of_an_exiting_child) | test(=ptrace_precise_futex_wait_leaves_its_childs_sigchld_to_a_running_sibling) | test(=ptrace_precise_timed_futex_wait_keeps_its_deadline_when_a_runnable_sibling_takes_the_sigchld) | test(=ptrace_precise_timed_futex_wait_leaves_its_childs_sigchld_to_a_running_sibling) | test(=ptrace_precise_untimed_futex_wait_keeps_waiting_when_a_runnable_sibling_takes_the_sigchld) | test(=ptrace_readiness_waits_are_not_ended_by_non_interrupting_process_signals) | test(=ptrace_readiness_waits_are_not_ended_by_non_interrupting_thread_signals) | test(=ptrace_selects_are_ended_by_a_caught_sigchld_from_an_exiting_child) | test(=ptrace_selects_are_not_ended_by_non_interrupting_process_signals) | test(=ptrace_selects_are_not_ended_by_non_interrupting_thread_signals) | test(=ptrace_timed_futex_wait_and_poll_keep_their_deadline_through_a_discarded_default_stop) | test(=ptrace_timed_futex_wait_is_not_ended_by_non_interrupting_signals)'"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: Some(&[
            r########"aio_nr_determinism"########,
            r########"arch_status_determinism"########,
            r########"chaos_sched_yield_progress"########,
            r########"chaos_stress_pmu_detection"########,
            r########"child_time_rpc"########,
            r########"chown_virtual_root_identity"########,
            r########"cli_owned_lifecycle"########,
            r########"clock_determinism"########,
            r########"clock_discipline_determinism"########,
            r########"clock_passthrough"########,
            r########"container_init_deadline"########,
            r########"cpufreq_avg_determinism"########,
            r########"dispatch_stats"########,
            r########"epoll_determinism"########,
            r########"epoll_pwait_zero_timeout_progress"########,
            r########"external_signal_interrupt"########,
            r########"fatal_core_capture"########,
            r########"file_nr_determinism"########,
            r########"first_seen_mtime"########,
            r########"fp_reduction_determinism"########,
            r########"futex2_refusal"########,
            r########"hashseed_determinism"########,
            r########"inode_nr_determinism"########,
            r########"kernel_keyring"########,
            r########"key_users_determinism"########,
            r########"mmap_determinism"########,
            r########"node_vmstat_determinism"########,
            r########"numa_maps_determinism"########,
            r########"perf_event_refusal"########,
            r########"pidfd_creation"########,
            r########"process_isolation_refusals"########,
            r########"proc_fdinfo_determinism"########,
            r########"proc_locks_determinism"########,
            r########"procfs_determinism"########,
            r########"procfs_positioned_determinism"########,
            r########"pty_nr_determinism"########,
            r########"python_stdlib"########,
            r########"reopened_pipe_progress"########,
            r########"robust_futex_owner_death"########,
            r########"run_evidence"########,
            r########"self_sched_determinism"########,
            r########"self_schedstat_determinism"########,
            r########"signal_determinism"########,
            r########"smaps_determinism"########,
            r########"smaps_rollup_determinism"########,
            r########"sockstat_determinism"########,
            r########"softnet_stat_determinism"########,
            r########"swaps_determinism"########,
            r########"thp_stats_determinism"########,
            r########"utimensat_mtime"########,
            r########"verification_report_cli"########,
            r########"verification_report_consumers"########,
            r########"verify_claim_names_its_limit"########,
            r########"writev_determinism"########,
            r########"zero_copy_pipe_fallback"########,
        ]),
        deps: &[
            r########"build.e2e_artifact"########,
            r########"privileged-build.privileged_tests"########,
            r########"privileged-pmu.preemption"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 80.0,
            rss_baseline_bytes: Some(6442450944),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"privileged-build"########,
        job: r########"manifest_guests"########,
        desc: r########"Prepare every CI-enabled privileged manifest guest"########,
        description: r########"Prepares the privileged lane's CI test programs once each under $E2E_BUILD_ROOT/<test-id>: `test-harness build --lane privileged --ci-only` compiles tests/c/cpuid_probe.c with -Werror and runs the shell programs' --prepare steps (sysfs-sanitized-prefixes.sh runs its fixture self-test there). The privileged-e2e and privileged-only-e2e buckets run with --prebuilt and depend on the _in_pinned_root copy, which runs this command in the pinned root; the host copy has no dependents. A new compiler warning in cpuid_probe.c, or a failing --prepare self-test, stops it."########,
        labels: &[r########"full"########, r########"privileged"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; target/debug/test-harness build --lane privileged --ci-only --allow-empty"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"gate.manifest"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 75.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(3221225472),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: Some(3),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 120,
        cpu_timeout: 7200,
        jobs_flag: Some(r########"--jobs"########),
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"privileged-e2e"########,
        job: r########"manifest_applications"########,
        desc: r########"Privileged manifest bucket: applications"########,
        description: r########"The selected KVM cell has a measured 74 s wall backstop and may retry once. The shared 600 s E2E-class node bound remains a generous backup after per-machine scaling, while the typed inner CPU/wall policy identifies the actual stop."########,
        labels: &[r########"full"########],
        cmd: r########"./ci/hermetic/run-in-pinned-root.sh --src . --out ignored/hermetic/split --src-rw --cargo-home ignored/hermetic/split/cargo --env CARGO_BUILD_JOBS --env DAGRUN_STEP_STARTED_MONOTONIC_NS --env DAGRUN_TEST_COUNTS_PATH --env E2E_BUILD_ROOT --env E2E_KERNEL_VERSION --env E2E_MACHINE_SHORTNAME --env E2E_RESULT_ROOT --env E2E_RUN_ID --env HERMIT_E2E_EMPTY_WORKDIR --env HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT --env L4_REPS --env PR_NUMBER --env SUPER_REPETITIONS --env THIRD_PARTY_BUILD_JOBS --env VALIDATE_VERBOSITY -- bash -c '/src/ci/hermetic/assert-no-network.sh && /src/ci/hermetic/assert-build-dependencies.sh && exec bash -c "$1"' bash 'export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh target/debug/test-harness run --lane privileged --category applications --ci-only --allow-empty --prebuilt --results "$E2E_RESULT_ROOT/privileged/manifest_applications/results.jsonl" --junit "$E2E_RESULT_ROOT/privileged/manifest_applications/junit.xml"'"########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"privileged"########,
            category: r########"applications"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact_in_pinned_root"########,
            r########"build.rust_scripts_in_pinned_root"########,
            r########"gate.manifest"########,
            r########"privileged-build.manifest_guests_in_pinned_root"########,
            r########"privileged-build.privileged_tests"########,
            r########"setup.pinned_root_fetch"########,
        ],
        env: &[(
            r########"HERMIT_E2E_EMPTY_WORKDIR"########,
            r########"/test"########,
        )],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 90.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(17179869184),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"privileged-e2e"########,
        job: r########"manifest_c_programs"########,
        desc: r########"Privileged manifest bucket: c-programs"########,
        description: r########"The selected cells are the five cpuid-probe verify cells (dbt, in-guest-trap, kvm, liteinst, ptrace): the kvm and ptrace cells that the privileged backend-parity-c node ran before that bucket was folded into c-programs (https://github.com/rrnewton/hermit/issues/3301), the dbt cell that slice S13 of the same issue carried over from the retired DBT parity matrix, and the in-guest LiteInst cells (liteinst, and in-guest-trap with site patching off) that returned on the new architecture after the LiteInst reset (https://github.com/rrnewton/hermit/issues/3745), each run with the maximum timeslice disabled. The selector no longer passes --allow-empty, so selecting no cells fails. They use the 57 s wall backstop and may retry once. The shared 600 s E2E-class node bound remains a generous backup after per-machine scaling, while the typed inner CPU/wall policy identifies the actual stop."########,
        labels: &[r########"full"########],
        cmd: r########"./ci/hermetic/run-in-pinned-root.sh --src . --out ignored/hermetic/split --src-rw --cargo-home ignored/hermetic/split/cargo --env CARGO_BUILD_JOBS --env DAGRUN_STEP_STARTED_MONOTONIC_NS --env DAGRUN_TEST_COUNTS_PATH --env E2E_BUILD_ROOT --env E2E_KERNEL_VERSION --env E2E_MACHINE_SHORTNAME --env E2E_RESULT_ROOT --env E2E_RUN_ID --env HERMIT_E2E_EMPTY_WORKDIR --env HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT --env L4_REPS --env PR_NUMBER --env SUPER_REPETITIONS --env THIRD_PARTY_BUILD_JOBS --env VALIDATE_VERBOSITY -- bash -c '/src/ci/hermetic/assert-no-network.sh && /src/ci/hermetic/assert-build-dependencies.sh && exec bash -c "$1"' bash 'export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh target/debug/test-harness run --lane privileged --category c-programs --ci-only --prebuilt --results "$E2E_RESULT_ROOT/privileged/manifest_c_programs/results.jsonl" --junit "$E2E_RESULT_ROOT/privileged/manifest_c_programs/junit.xml"'"########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"privileged"########,
            category: r########"c-programs"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact_in_pinned_root"########,
            r########"build.rust_scripts_in_pinned_root"########,
            r########"check.dbt_runtime_abi"########,
            r########"gate.manifest"########,
            r########"privileged-build.manifest_guests_in_pinned_root"########,
            r########"privileged-build.privileged_tests"########,
            r########"setup.pinned_root_fetch"########,
        ],
        env: &[(
            r########"HERMIT_E2E_EMPTY_WORKDIR"########,
            r########"/test"########,
        )],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 5.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(17179869184),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"privileged-e2e"########,
        job: r########"manifest_system_utils"########,
        desc: r########"Privileged manifest bucket: system-utils"########,
        description: r########"HOST-REQUIREMENT ROUTING 2026-09-28: system-utils/sysfs-sanitized-prefixes is the only cell in this privileged bucket. Its guest reads one live leaf under each of eight host sysfs prefixes (block, hwmon, rtc, node, btrfs, irq, uevent, module) and refuses when a prefix has no readable leaf, so it needs a host that has that hardware and a mounted btrfs. GitHub-hosted run https://github.com/rrnewton/hermit/actions/runs/36485831200 failed it with "hwmon has no readable sanitized leaf". The manifest entry is therefore lane: privileged, which keeps it out of the hosted-portable selection while the full profile still runs it. Wall evidence from the ledger series of the development host recorded in docs/TESTING_ENVIRONMENTS.md, "Named measurement hosts" (2026-08..09): verify/ptrace 148 samples, per-run median 2096 ms, max 15438 ms; verify/kvm 29 samples, median 2316 ms, max 3467 ms. Memory hints match the portable system-utils bucket, whose 3 GiB hard cap already contained both cells at --jobs 1. The shared 600 s E2E-class node bound is the backstop."########,
        labels: &[r########"full"########],
        cmd: r########"./ci/hermetic/run-in-pinned-root.sh --src . --out ignored/hermetic/split --src-rw --cargo-home ignored/hermetic/split/cargo --env CARGO_BUILD_JOBS --env DAGRUN_STEP_STARTED_MONOTONIC_NS --env DAGRUN_TEST_COUNTS_PATH --env E2E_BUILD_ROOT --env E2E_KERNEL_VERSION --env E2E_MACHINE_SHORTNAME --env E2E_RESULT_ROOT --env E2E_RUN_ID --env HERMIT_E2E_EMPTY_WORKDIR --env HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT --env L4_REPS --env PR_NUMBER --env SUPER_REPETITIONS --env THIRD_PARTY_BUILD_JOBS --env VALIDATE_VERBOSITY -- bash -c '/src/ci/hermetic/assert-no-network.sh && /src/ci/hermetic/assert-build-dependencies.sh && exec bash -c "$1"' bash 'export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh target/debug/test-harness run --lane privileged --category system-utils --ci-only --allow-empty --prebuilt --jobs 1 --results "$E2E_RESULT_ROOT/privileged/manifest_system_utils/results.jsonl" --junit "$E2E_RESULT_ROOT/privileged/manifest_system_utils/junit.xml"'"########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"privileged"########,
            category: r########"system-utils"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact_in_pinned_root"########,
            r########"build.rust_scripts_in_pinned_root"########,
            r########"gate.manifest"########,
            r########"privileged-build.manifest_guests_in_pinned_root"########,
            r########"privileged-build.privileged_tests"########,
            r########"setup.pinned_root_fetch"########,
        ],
        env: &[(
            r########"HERMIT_E2E_EMPTY_WORKDIR"########,
            r########"/test"########,
        )],
        hint: HintSpec {
            resources: &[(r########"manifest_guest"########, 1)],
            est_duration_s: 30.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(3221225472),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: Some(1),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: Some(r########""########),
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"privileged-test"########,
        job: r########"cli_kvm"########,
        desc: r########"KVM CLI tests and initialized-VM setup cleanup"########,
        description: r########"Runs all 55 KVM-specific hermit CLI tests and the initialized-VM setup cleanup control after requiring /dev/kvm. The inventory gate requires all 55 distinct, nonignored CLI tests including the self-SIGKILL, synchronous-fault, root-exit reparenting, exec timer, nonleader-exec refusal and unreadable futex word regressions and exactly the named setup test. The portable lane continues to skip run_kvm_ because these tests self-guard without /dev/kvm and would otherwise report silent passes. KVM consumers may overlap: /dev/kvm supports multiple concurrent guests, and no repository or host constraint establishes it as an exclusive resource."########,
        labels: &[r########"full"########, r########"kvm"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; set -uo pipefail; if ! exec 9<>/dev/kvm; then printf 'test.cli_kvm: /dev/kvm could not be opened, so the selected run_kvm_ tests would self-guard and report silent passes. Refusing rather than reporting a green that measured nothing.\n' >&2; exit 1; fi; exec 9<&-; log=$(mktemp); trap 'rm -f "$log"' EXIT; if ! ./ci/nextest-binaries.rs list ${CI:+--profile ci} -p hermit --features third-party-backends,kvm-execution-tests --lib --test cli -E 'test(/^run_kvm_/) | test(=kvm_execution_tests::initialized_vm_setup_failures_consume_detcore_state_without_further_guest_execution)' --message-format json >"$log"; then exit 1; fi; if ! jq -e '[."rust-suites"[] | .testcases | to_entries[] | select(.value."filter-match".status == "matches")] as $selected | ($selected | length == 56) and ([$selected[].key] | unique | length == 56) and ($selected | all(.value.ignored == false)) and ([$selected[] | select(.key == "run_kvm_self_sigkill_from_nonleader_is_group_fatal")] | length == 1) and ([$selected[] | select(.key == "run_kvm_synchronous_root_segv_preserves_guest_exit")] | length == 1) and ([$selected[] | select(.key == "run_kvm_synchronous_orphan_segv_preserves_root_success")] | length == 1) and ([$selected[] | select(.key == "run_kvm_root_exit_reparents_live_child_and_grandchild")] | length == 1) and ([$selected[] | select(.key == "run_kvm_exec_deletes_posix_timers_and_preserves_itimer")] | length == 1) and ([$selected[] | select(.key == "run_kvm_nonleader_exec_is_policy_refusal")] | length == 1) and ([$selected[] | select(.key == "run_kvm_futex_wait_on_an_unreadable_word_matches_linux")] | length == 1) and ([$selected[] | select(.key | startswith("run_kvm_"))] | length == 55) and ([$selected[] | select(.key == "kvm_execution_tests::initialized_vm_setup_failures_consume_detcore_state_without_further_guest_execution")] | length == 1)' "$log" >/dev/null; then printf 'test.cli_kvm: expected exactly 55 distinct, nonignored run_kvm_ tests including self-SIGKILL, both synchronous-fault regressions, root-exit reparenting, exec timer lifetime, nonleader-exec refusal, the unreadable futex word, and the named setup test; the inventory changed. Update the tests and this gate together.\n' >&2; exit 1; fi; ./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit --features third-party-backends,kvm-execution-tests --lib --test cli -j 1 -E 'test(/^run_kvm_/) | test(=kvm_execution_tests::initialized_vm_setup_failures_consume_detcore_state_without_further_guest_execution)'; exit $?"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: Some(&[r########"cli"########]),
        deps: &[r########"privileged-build.privileged_tests"########],
        env: &[],
        hint: HintSpec {
            resources: &[(r########"integration_test_binaries.cli"########, 1)],
            est_duration_s: 60.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(17179869184),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 180,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"full-scorecard"########,
        job: r########"compatibility"########,
        desc: r########"Verify fresh per-cell results and print the compatibility table"########,
        description: r########"After every portable and privileged manifest bucket finishes, `ci/compat-envelope/scorecard.rs verify-results` requires one fresh row at git HEAD with a clean tree for every cell ci/expected-e2e-plan.json selects in those lanes. Each must be a PASS, except a cell the manifest declares diagnostic, whose FAIL is listed without failing the check; declared stripped-comparator passes are counted apart as below L2. All rows must come from one Hermit binary, and each cell's rows from one run. A cell no bucket wrote, a row from another commit, or rows from two Hermit builds fail it even when every bucket node passed; on success it prints the compatibility table."########,
        labels: &[r########"full"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/compat-envelope/scorecard.rs verify-results --results "$E2E_RESULT_ROOT" --lanes portable,privileged"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[
            r########"e2e.manifest_applications"########,
            r########"e2e.manifest_bin_c"########,
            r########"e2e.manifest_c_programs"########,
            r########"e2e.manifest_chaos_c"########,
            r########"e2e.manifest_compat"########,
            r########"e2e.manifest_data_handling"########,
            r########"e2e.manifest_debugger_c"########,
            r########"e2e.manifest_determinism_stress"########,
            r########"e2e.manifest_determinism_stress_c"########,
            r########"e2e.manifest_language_runtimes"########,
            r########"e2e.manifest_shared_futex_c"########,
            r########"e2e.manifest_system_utils"########,
            r########"e2e.manifest_util_c"########,
            r########"privileged-e2e.manifest_applications"########,
            r########"privileged-e2e.manifest_c_programs"########,
            r########"privileged-e2e.manifest_system_utils"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(1073741824),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 120,
        cpu_timeout: 120,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"scorecard"########,
        job: r########"compatibility"########,
        desc: r########"Verify fresh per-cell results and print the compatibility table"########,
        description: r########"After the thirteen portable e2e.manifest_* buckets finish, `ci/compat-envelope/scorecard.rs verify-results --lanes portable` requires one fresh row at git HEAD, from a clean tree, for every portable-lane regression cell ci/expected-e2e-plan.json selects. Each must be a PASS, except a cell the manifest declares diagnostic, whose FAIL is listed without failing the check; declared stripped-comparator passes are counted apart as below L2. All rows must come from one Hermit binary, and each cell's rows from one run. A cell no bucket wrote, a row from another commit, or rows from two Hermit builds fail it with "fresh result set refused: N missing, M non-passing" even when every bucket node passed; on success it prints the compatibility table. It is the portable-profile copy of full-scorecard.compatibility, which checks both lanes."########,
        labels: &[r########"portable"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/compat-envelope/scorecard.rs verify-results --results "$E2E_RESULT_ROOT" --lanes portable"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[
            r########"e2e.manifest_applications"########,
            r########"e2e.manifest_bin_c"########,
            r########"e2e.manifest_c_programs"########,
            r########"e2e.manifest_chaos_c"########,
            r########"e2e.manifest_compat"########,
            r########"e2e.manifest_data_handling"########,
            r########"e2e.manifest_debugger_c"########,
            r########"e2e.manifest_determinism_stress"########,
            r########"e2e.manifest_determinism_stress_c"########,
            r########"e2e.manifest_language_runtimes"########,
            r########"e2e.manifest_shared_futex_c"########,
            r########"e2e.manifest_system_utils"########,
            r########"e2e.manifest_util_c"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(1073741824),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 120,
        cpu_timeout: 120,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"quick"########,
        job: r########"build"########,
        desc: r########"Build workspace"########,
        description: r########"In the pinned root (one podman container pinned by digest, no runtime network, source at /src) it runs `cargo build --workspace --features third-party-backends` in the dev profile, then `./ci/nextest-binaries.rs prepare quick`, which builds and lists the test executables of the quick profile's one prepared Nextest selection (hermit-detcore --lib) and records their hashes, so quick.detcore_unit compiles nothing. The six other quick nodes depend on it; the three smoke runs execute the target/debug/hermit it produces. A compile error anywhere in the workspace, including code behind the third-party-backends feature, stops the quick profile here, as does a prepare step that cannot find an executable for that selection. WIDTH AND WALL: it runs at 8 CPUs (CpuBound, preferred_inner_jobs 8, passed to Cargo as CARGO_BUILD_JOBS rather than as a trailing -j, which `nextest-binaries.rs prepare` refuses as an extra argument). As a light node it would get dagrun's default one-CPU box, where the workspace build alone costs about 575 CPU-seconds. In the quick-profile validation of Hermit dcbf2768 on 2026-10-08, on the development host recorded in docs/TESTING_ENVIRONMENTS.md, "Named measurement hosts", it took 152 s. The 1200 s wall is 7.9 times that, and equals the wall of build.workspace_compile_in_pinned_root, whose 124 retained runs took 179-227 s. The wall was 3600 s, which made the quick profile refuse every node once preparation passed 600 s of ci-hub's 4200 s run budget (https://github.com/rrnewton/hermit/issues/3906)."########,
        labels: &[r########"quick"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; cargo build --workspace --features third-party-backends && ./ci/nextest-binaries.rs prepare quick"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[
            r########"gate.manifest"########,
            r########"setup.nextest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(17179869184),
            hard_mem_max_bytes: Some(17179869184),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: Some(8),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 1200,
        cpu_timeout: 7200,
        jobs_flag: Some(""),
        jobs_env: Some(r########"CARGO_BUILD_JOBS"########),
    },
    StaticStepSpec {
        group: r########"quick"########,
        job: r########"e2e_metadata"########,
        desc: r########"Portable E2E metadata"########,
        description: r########"Runs `target/debug/test-harness validate` on the host: byte-for-byte the command of quick-super-gate.manifest (and of gate.manifest), which runs the manifest-plan audits in process (DAG/manifest correspondence, budget ordering, determinism-stress evidence, CLI brackets, the expected-cell plan) and `generate-test-footprints --check` against ci/test-footprints.json. It runs after quick.build and without the gate's HERMIT_VALIDATE_AUDIT_JOBS=1, so the audits pick their own width. The node exists so the quick profile reports a manifest-metadata result among its product nodes, but its own dependency quick-super-gate.manifest already ran the same binary on the same tree. A stale ci/test-footprints.json, for instance, fails the gate first and leaves this node skipped."########,
        labels: &[r########"quick"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; target/debug/test-harness validate"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"quick.build"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(4294967296),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 1200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"quick"########,
        job: r########"e2e_verify"########,
        desc: r########"Portable ptrace E2E verification"########,
        description: r########"In the pinned root, with the shared /proc-locks runtime directory mounted, runs `target/debug/test-harness run --lane portable --mode verify --backend ptrace --ci-only --exclude-category compat`: every CI-selected portable-lane E2E cell, on the ptrace backend, in verify mode, except the compat category (359 result manifests in ci/dag/validate.json, across applications, C programs, system utilities and the other portable categories). It is the quick profile's end-to-end determinism coverage, run once from quick.build's tree before a full validation. A guest whose two runs differ in output or scheduler log, or that exits nonzero, records a failing row for that cell and turns the node red. The compat category is left to portablecompat.manifest_compat."########,
        labels: &[r########"quick"########],
        cmd: r########"./ci/hermetic/run-in-pinned-root.sh --src . --out ignored/hermetic/split --src-rw --cargo-home ignored/hermetic/split/cargo --env CARGO_BUILD_JOBS --env DAGRUN_STEP_STARTED_MONOTONIC_NS --env DAGRUN_TEST_COUNTS_PATH --env E2E_BUILD_ROOT --env E2E_KERNEL_VERSION --env E2E_MACHINE_SHORTNAME --env E2E_RESULT_ROOT --env E2E_RUN_ID --env HERMIT_E2E_EMPTY_WORKDIR --env HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT --env L4_REPS --env PR_NUMBER --env SUPER_REPETITIONS --env THIRD_PARTY_BUILD_JOBS --env VALIDATE_VERBOSITY -- bash -c '/src/ci/hermetic/assert-no-network.sh && /src/ci/hermetic/assert-build-dependencies.sh && exec bash -c "$1"' bash 'export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; target/debug/test-harness run --lane portable --mode verify --backend ptrace --ci-only --exclude-category compat'"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[
            r########"build.rust_scripts_in_pinned_root"########,
            r########"quick.build"########,
            r########"setup.pinned_root_fetch"########,
        ],
        env: &[(
            r########"HERMIT_E2E_EMPTY_WORKDIR"########,
            r########"/test"########,
        )],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 1800,
        cpu_timeout: 3600,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"quick"########,
        job: r########"detcore_unit"########,
        desc: r########"Detcore core unit tests"########,
        description: r########"In the pinned root, runs `./ci/run-nextest-counted.sh -p hermit-detcore --lib` with HERMIT_PREPARED_NEXTEST_REQUIRED=1, so the unit tests of the Detcore library run from the executables quick.build prepared rather than through a fresh `cargo nextest` build, and their pass/fail counts are written as schema-2 structured test results. Detcore holds the deterministic scheduler and syscall handling, so this is the quick profile's fast check of that logic. Unlike test.detcore_unit in the full profile, it selects only --lib (not --bins) and pins no NEXTEST_EXPECTED_EXECUTED count. A failing assertion in a Detcore unit test, or a prepared executable whose hash no longer matches quick.build's record, turns it red."########,
        labels: &[r########"quick"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-nextest-counted.sh -p hermit-detcore --lib"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[
            r########"quick.build"########,
            r########"setup.nextest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 1800,
        cpu_timeout: 3600,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"quick"########,
        job: r########"run_smoke"########,
        desc: r########"Hermit run smoke test"########,
        description: r########"In the pinned root, runs `timeout 30s target/debug/hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --mount=type=tmpfs,target=/test --workdir=/test -- /bin/echo hermit-validation-smoke` with quick.build's debug Hermit and requires its stdout to equal exactly "hermit-validation-smoke". It is the quickest proof that the freshly built binary can start a container, trace a guest on the default ptrace backend and pass its output through, before the slower quick nodes are trusted. A Hermit that panics at startup, hangs past 30 seconds, or adds or loses a byte of guest stdout fails the `test "$out" = hermit-validation-smoke` comparison or the run itself."########,
        labels: &[r########"quick"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; out=$(timeout 30s target/debug/hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --mount=type=tmpfs,target=/test --workdir=/test -- /bin/echo hermit-validation-smoke) && test "$out" = hermit-validation-smoke"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"quick.build"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(4294967296),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 120,
        cpu_timeout: 240,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"quick"########,
        job: r########"verify_smoke"########,
        desc: r########"Hermit verify-mode smoke test"########,
        description: r########"In the pinned root, runs `timeout 30s target/debug/hermit run --verify` with the same flags as quick.run_smoke (minimal base environment, no CPUID virtualization, timeslice preemption disabled, a tmpfs at /test as working directory) on `/bin/echo hermit-validation-smoke`. --verify runs the guest twice and compares stdout/stderr and the scheduler-step log, exiting nonzero if either run fails or they differ; the node checks only that exit status, not the text. It is the cheapest check that the debug build's determinism comparison works end to end before quick.e2e_verify runs hundreds of cells. A divergence between the two runs, a crash in either, or a run longer than 30 seconds turns it red."########,
        labels: &[r########"quick"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; timeout 30s target/debug/hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --mount=type=tmpfs,target=/test --workdir=/test --verify -- /bin/echo hermit-validation-smoke"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"quick.build"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(4294967296),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 120,
        cpu_timeout: 240,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"quick"########,
        job: r########"record_replay_smoke"########,
        desc: r########"Hermit record/replay smoke test"########,
        description: r########"In the pinned root, runs `timeout 30s target/debug/hermit record start --base-env=minimal --mount=type=tmpfs,target=/test --workdir=/test --verify -- /bin/echo hermit-validation-smoke`: it records the guest with quick.build's debug Hermit and, because of --verify, immediately replays the recording and deletes it if the replay succeeds. Unlike the two other smoke nodes it keeps CPUID virtualization and the default timeslice. It is the quick profile's only record/replay check, so a change that breaks the recorder or the replayer is caught before a full validation. A replay that fails or diverges from the recording, or a record phase that hangs past 30 seconds, gives a nonzero exit and turns it red."########,
        labels: &[r########"quick"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; timeout 30s target/debug/hermit record start --base-env=minimal --mount=type=tmpfs,target=/test --workdir=/test --verify -- /bin/echo hermit-validation-smoke"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"quick.build"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(4294967296),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 180,
        cpu_timeout: 360,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"super"########,
        job: r########"build_workspace"########,
        desc: r########"Build workspace"########,
        description: r########"On the host, runs `cargo build --workspace --features third-party-backends` in the dev profile and then `./ci/nextest-binaries.rs prepare super`, which builds and lists the test executables of every prepared Nextest selection the super profile's diagnostic nodes name (15 selections, as both 2026-10-08 runs printed) and records their hashes, so none of those nodes compiles. Its 28 dependents are those super.* Nextest diagnostics, super.build_pinned_leveldb_super_fixture, and the KVM and DBT stress nodes and their availability probes, which run the target/debug/hermit it produces. A compile error in any workspace crate, or a super node whose selection lacks HERMIT_PREPARED_NEXTEST_REQUIRED=1 (prepare refuses it), stops most of the super profile here. WIDTH, WALL AND MEMORY: it runs at 8 CPUs (CpuBound, preferred_inner_jobs 8, passed to Cargo as CARGO_BUILD_JOBS rather than as a trailing -j, which `nextest-binaries.rs prepare` refuses). As a light node it got dagrun's default one-CPU box, where its 1022-1305 CPU-seconds cannot fit a 600 s wall (https://github.com/rrnewton/hermit/issues/3911). In the super-profile validation of Hermit 77a3f25a on 2026-10-08, on the development host recorded in docs/TESTING_ENVIRONMENTS.md, "Named measurement hosts", it took 316 s and 1022 CPU-seconds while held at its old 16 GiB memory cap (27341 memory.max events) and CPU-throttled for 309 s. One cold run of the same command at 8 jobs in its own uncapped systemd scope took 433 s, 1305 CPU-seconds and a 19.84 GiB memory.peak (page cache included). The bounds are 1200 s wall (2.8 times 433 s), 7200 CPU-seconds (5.5 times 1305) and a 32 GiB hard memory cap (1.6 times 19.84 GiB) over a 20 GiB working set."########,
        labels: &[r########"super"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; cargo build --workspace --features third-party-backends && ./ci/nextest-binaries.rs prepare super"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[
            r########"gate.manifest"########,
            r########"setup.nextest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(21474836480),
            hard_mem_max_bytes: Some(34359738368),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: Some(8),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 1200,
        cpu_timeout: 7200,
        jobs_flag: Some(""),
        jobs_env: Some(r########"CARGO_BUILD_JOBS"########),
    },
    StaticStepSpec {
        group: r########"super"########,
        job: r########"build_release_hermit"########,
        desc: r########"Build release Hermit"########,
        description: r########"Runs `cargo build --release -p hermit --features third-party-backends` on the host, producing the target/release/hermit that the super profile's product probes use: super-compatprep.fixtures and the four compat.* rows behind it (rustc, javac, java, node), and the three ptrace stress nodes (strict verify of /bin/echo, a bash pipeline, and record/replay). It is the same core command as compatprep.hermit_release and qemu.hermit_release in the focused profiles; super needs its own copy because those nodes are not in the super plan. A compile error that appears only with optimisations or only in the hermit package's release build stops all of those dependents. WIDTH AND WALL: it runs at 8 CPUs (CpuBound, preferred_inner_jobs 8, passed to Cargo as CARGO_BUILD_JOBS), the width of compatprep.hermit_release, which runs the same command. As a light node it got dagrun's default one-CPU box, where the build's ~800 CPU-seconds exceed its 600 s wall (https://github.com/rrnewton/hermit/issues/3911). In the super-profile validation of Hermit 77a3f25a on 2026-10-08, on the development host recorded in docs/TESTING_ENVIRONMENTS.md, "Named measurement hosts", it took 171 s, 801 CPU-seconds and a 3.4 GiB peak. The bounds are 600 s wall (3.5 times 171 s) and 2400 CPU-seconds (3.0 times 801)."########,
        labels: &[r########"super"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; cargo build --release -p hermit --features third-party-backends"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"gate.manifest"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(17179869184),
            hard_mem_max_bytes: Some(17179869184),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: Some(8),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 2400,
        jobs_flag: Some(""),
        jobs_env: Some(r########"CARGO_BUILD_JOBS"########),
    },
    StaticStepSpec {
        group: r########"super"########,
        job: r########"relaxed_hermit_flag_matrix"########,
        desc: r########"Relaxed Hermit flag matrix"########,
        description: r########"Runs the ignored test meaningful_flag_combinations_run_without_crashing (hermit-cli/tests/relaxed_flag_matrix.rs) through ci/run-nextest-counted.sh on one thread, writing a TSV report to target/relaxed-flag-matrix/results.tsv. It runs 189 cases: 60 ptrace-backend configurations (`--backend=ptrace run --base-env=minimal --max-timeslice=disabled` combined with --no-sequentialize-threads, --no-deterministic-io, --no-virtualize-time, --no-virtualize-metadata, --no-virtualize-cpuid and --verify) plus three passthrough ones (--strace-only, --strace-only --verify, --namespace-only), each against /bin/true, /bin/echo and a compiled threaded probe (tests/c/relaxed_flag_matrix.c), with a 30-second timeout per case. It guards against unusual flag combinations crashing or hanging Hermit; a nondeterministic --verify verdict is recorded in the report, not treated as a failure. A case fails on a panic, segmentation fault or stack overflow message, on timeout exit 124, or when a direct run omits the guest marker. A failure is blocking for the super validation."########,
        labels: &[r########"super"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; env HERMIT_FLAG_MATRIX_REPORT=$PWD/target/relaxed-flag-matrix/results.tsv $PWD/ci/run-nextest-counted.sh -p hermit --features third-party-backends --test relaxed_flag_matrix meaningful_flag_combinations_run_without_crashing -j 1 --no-capture -- --exact --ignored"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"super.build_workspace"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 1800,
        cpu_timeout: 3600,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"super"########,
        job: r########"weekly_pmu_parallel_memory_diagnostic_mem_race_bottom_detcore"########,
        desc: r########"Weekly PMU parallel memory diagnostic: mem_race::bottom_detcore"########,
        description: r########"Runs the Detcore test mem_race::bottom_detcore from the hermit-detcore tests_parallelism binary (detcore/tests/parallelism/mod.rs) through ci/run-nextest-counted.sh on one thread. It runs in-process under Reverie's ptrace backend: two threads each claim 10,000,000 slots of a shared 20,000,000-element array through an atomic counter and write their own tag, and the test counts thread switch points. docs/TESTING_ENVIRONMENTS.md lists mem_race as needing PMU (retired-conditional-branch) counters. This variant uses BOTTOM_CFG: thread sequentialization, deterministic I/O and time, metadata and CPUID virtualization all off, max timeslice 5,000,000. Threads run in parallel, so the test only requires each repetition to exit successfully; a guest crash or non-zero exit under ptrace fails it. The four variants run as a chain (bottom, default, middle, top) in one fail-fast family, so a failure cancels the rest of the chain; nothing else depends on them. A failure is blocking for the super validation."########,
        labels: &[r########"super"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/ci/run-nextest-counted.sh -p hermit-detcore --test tests_parallelism mem_race::bottom_detcore -j 1 -- --exact"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"super.build_workspace"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 1200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"super"########,
        job: r########"weekly_pmu_parallel_memory_diagnostic_mem_race_default_detcore"########,
        desc: r########"Weekly PMU parallel memory diagnostic: mem_race::default_detcore"########,
        description: r########"Runs the Detcore test mem_race::default_detcore from the hermit-detcore tests_parallelism binary (detcore/tests/parallelism/mod.rs) through ci/run-nextest-counted.sh on one thread. It runs in-process under Reverie's ptrace backend: two threads each claim 10,000,000 slots of a shared 20,000,000-element array through an atomic counter and write their own tag, and the test counts thread switch points. docs/TESTING_ENVIRONMENTS.md lists mem_race as needing PMU (retired-conditional-branch) counters. This variant uses Detcore's default configuration: thread sequentialization off, default max timeslice 200,000,000. Threads run in parallel, so the test only requires each repetition to exit successfully; a guest crash or non-zero exit under ptrace fails it. The four variants run as a chain (bottom, default, middle, top) in one fail-fast family, so a failure cancels the rest of the chain; nothing else depends on them. A failure is blocking for the super validation."########,
        labels: &[r########"super"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/ci/run-nextest-counted.sh -p hermit-detcore --test tests_parallelism mem_race::default_detcore -j 1 -- --exact"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[
            r########"super.weekly_pmu_parallel_memory_diagnostic_mem_race_bottom_detcore"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 1200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"super"########,
        job: r########"weekly_pmu_parallel_memory_diagnostic_mem_race_middle_detcore"########,
        desc: r########"Weekly PMU parallel memory diagnostic: mem_race::middle_detcore"########,
        description: r########"Runs the Detcore test mem_race::middle_detcore from the hermit-detcore tests_parallelism binary (detcore/tests/parallelism/mod.rs) through ci/run-nextest-counted.sh on one thread. It runs in-process under Reverie's ptrace backend: two threads each claim 10,000,000 slots of a shared 20,000,000-element array through an atomic counter and write their own tag, and the test counts thread switch points. docs/TESTING_ENVIRONMENTS.md lists mem_race as needing PMU (retired-conditional-branch) counters. This variant uses MIDDLE_CFG: time, metadata and CPUID virtualization and deterministic I/O on, thread sequentialization off, max timeslice 5,000,000. Threads run in parallel, so the test only requires each repetition to exit successfully; a guest crash or non-zero exit under ptrace fails it. The four variants run as a chain (bottom, default, middle, top) in one fail-fast family, so a failure cancels the rest of the chain; nothing else depends on them. A failure is blocking for the super validation."########,
        labels: &[r########"super"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/ci/run-nextest-counted.sh -p hermit-detcore --test tests_parallelism mem_race::middle_detcore -j 1 -- --exact"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[
            r########"super.weekly_pmu_parallel_memory_diagnostic_mem_race_default_detcore"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 1200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"super"########,
        job: r########"weekly_pmu_parallel_memory_diagnostic_mem_race_top_detcore"########,
        desc: r########"Weekly PMU parallel memory diagnostic: mem_race::top_detcore"########,
        description: r########"Runs the Detcore test mem_race::top_detcore from the hermit-detcore tests_parallelism binary (detcore/tests/parallelism/mod.rs) through ci/run-nextest-counted.sh on one thread. It runs in-process under Reverie's ptrace backend: two threads each claim 10,000,000 slots of a shared 20,000,000-element array through an atomic counter and write their own tag, and the test counts thread switch points. docs/TESTING_ENVIRONMENTS.md lists mem_race as needing PMU (retired-conditional-branch) counters. This variant uses TOP_CFG: thread sequentialization and deterministic I/O on, max timeslice 5,000,000. Threads are sequentialized and preempted by the branch-counter timer: the test asserts more than 10 switch points ("Expecting deterministic preemptions when using RCB timers") and compares output across two repetitions, so missing preemptions or a differing repetition fails it. The four variants run as a chain (bottom, default, middle, top) in one fail-fast family, so a failure cancels the rest of the chain; nothing else depends on them. A failure is blocking for the super validation."########,
        labels: &[r########"super"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/ci/run-nextest-counted.sh -p hermit-detcore --test tests_parallelism mem_race::top_detcore -j 1 -- --exact"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[
            r########"super.weekly_pmu_parallel_memory_diagnostic_mem_race_middle_detcore"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 1200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"super"########,
        job: r########"pselect_signal_interruption_diagnostic"########,
        desc: r########"Pselect signal-interruption diagnostic"########,
        description: r########"Runs the whole pselect6_simulation integration binary (one test, pselect6_preserves_kernel_abi_and_unblocks_scheduler) on one thread. The test compiles tests/c/pselect6_simulation.c and runs it twice on the default ptrace backend under a 30-second timeout: `hermit --log=trace run --strict --base-env=minimal`, which must print pselect6-simulation-ok and log "Retry #1 for syscall due to result Ok(0): pselect6(", then `hermit --log=info run --strict --verify --base-env=minimal`, which must print "Determinism verified". The guest checks that Hermit keeps pselect6's kernel ABI: EINVAL and EFAULT ordering, timeout writeback, SIGUSR1 from a helper thread interrupting pselect6 with EINTR and a partly decremented timeout, and a delayed pipe writer waking an infinite wait. If the strict scheduler stopped polling a blocked pselect6 by retry, the trace line is missing; if the wait never returns, the timeout kills the run. A failure is blocking for the super validation."########,
        labels: &[r########"super"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/ci/run-nextest-counted.sh -p hermit --features third-party-backends --test pselect6_simulation -j 1 --"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"super.build_workspace"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 300,
        cpu_timeout: 600,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"super"########,
        job: r########"record_replay_matrix_diagnostic"########,
        desc: r########"Record/replay matrix diagnostic"########,
        description: r########"Runs record_replay_matrix from hermit-cli/tests/record_replay.rs on one thread. For each of ten baseline workloads (c_getpid, c_ioctl_fioclex, c_ioctl_siocethtool, c_recvmsg_scm_rights_mmap, c_ppoll_readv, c_uname, c_sysinfo, c_wait_on_child, c_nanosleep_parallel, rs_clock_gettime), taken from the prepared workload set named by HERMIT_PREPARED_RECORD_WORKLOADS, it runs `hermit record start --verify --record-timeout=30 --data-dir=<tmp> -- <workload>` under a 45-second timeout and requires exit 0 and "Success: replay matched recording." It protects record/replay for basic syscalls, ioctls, SCM_RIGHTS descriptor passing, ppoll, child waits and clock reads; the test notes that record/replay does not enable PMU preemption, so it needs no performance counters. A workload whose replay diverges from its recording, or a recording that hangs until the timeout, fails the test. A failure is blocking for the super validation."########,
        labels: &[r########"super"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/ci/run-nextest-counted.sh -p hermit --features third-party-backends --test record_replay record_replay_matrix -j 1 -- --exact"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"super.build_workspace"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 300,
        cpu_timeout: 600,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"super"########,
        job: r########"managed_jvm_strict_verify_diagnostics"########,
        desc: r########"Managed JVM strict-verify diagnostics"########,
        description: r########"Runs the ignored tests in hermit-cli/tests/app_strict_verify.rs whose names contain java, on one thread with HERMIT_APP_VERIFY_TIMEOUT=20s: six JVM workloads (java_hello, java_threads, java_thread_counter, java_gc_stress, java_jit_hot_loop, java_hashmap_string) and javac_is_l1_deterministic_under_strict. Each JVM test compiles its class with the host javac and runs the JVM under `hermit --log=info run --strict --verify --no-virtualize-cpuid` with -XX:+UseSerialGC, -XX:ActiveProcessorCount=1 and, except for the JIT test, -Xint, keeping PMU-driven preemption on because the JVM livelocks with --max-timeslice=disabled; it requires exit 0 and "Determinism verified". The javac test compiles Hello.java twice under --strict and byte-compares the class files. The portable test.app_strict_verify node skips these cases, so JVM determinism under strict mode is checked here. A JVM run longer than 20 seconds, or differing class files, fails. A failure is blocking for the super validation."########,
        labels: &[r########"super"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; env HERMIT_APP_VERIFY_TIMEOUT=20s RUST_BACKTRACE=1 $PWD/ci/run-nextest-counted.sh -p hermit --features third-party-backends --test app_strict_verify java -j 1 --no-capture -- --ignored"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"super.build_workspace"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 300,
        cpu_timeout: 600,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"super"########,
        job: r########"post_fork_scheduling_diagnostics"########,
        desc: r########"Post-fork scheduling diagnostics"########,
        description: r########"Runs the two hermit-detcore tests_misc tests matching ordinary_clone_ (detcore/tests/misc/mod.rs) on one thread: ordinary_clone_child_starts_before_parent_resumes and ordinary_clone_parent_mode_can_resume_before_child. Both run in-process through the Detcore test harness with thread sequentialization on and no PMU timer (max timeslice unset), and compare two repetitions. They pin the scheduler's policy after an ordinary clone: in child-first mode the child must have set its flag before the parent resumes, and in parent-first mode the flag must still be unset when the parent resumes. Because no preemption timer is used, they do not need performance counters. A scheduler change that let the parent run first in child-first mode would fail the first test's assertion. A failure is blocking for the super validation."########,
        labels: &[r########"super"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/ci/run-nextest-counted.sh -p hermit-detcore --test tests_misc ordinary_clone_ -j 1 --"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"super.build_workspace"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 180,
        cpu_timeout: 360,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"super"########,
        job: r########"network_syscall_determinism_diagnostic"########,
        desc: r########"Network syscall determinism diagnostic"########,
        description: r########"Runs network_syscalls_are_deterministic_across_five_runs from the hermit-detcore tests_misc binary (detcore/tests/misc/mod.rs:1916), in-process through the Detcore test harness with thread sequentialization and deterministic I/O on and no PMU timer, comparing five repetitions. The guest creates a socket, which must be fd 3, does a socketpair round trip and an AF_UNIX listen/connect/accept exchange, binds a TCP listener on 127.0.0.42 port 0, which must receive port 32768, and connects two TCP clients behind a barrier, printing descriptors and accept order. It protects deterministic descriptor numbering, ephemeral port assignment and accept ordering for socket syscalls. If the listener received a host-chosen port, the port-32768 assertion fails; if accept order varied, the cross-run comparison fails. A failure is blocking for the super validation."########,
        labels: &[r########"super"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/ci/run-nextest-counted.sh -p hermit-detcore --test tests_misc network_syscalls_are_deterministic_across_five_runs -j 1 -- --exact"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"super.build_workspace"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 180,
        cpu_timeout: 360,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"super"########,
        job: r########"ipc_determinism_diagnostic"########,
        desc: r########"IPC determinism diagnostic"########,
        description: r########"Runs ipc_patterns_are_deterministic_across_five_runs from hermit-cli/tests/ipc_determinism.rs. It compiles tests/c/ipc_determinism.c and runs each of five patterns (pipe-order, pipe-capacity, socketpair, eventfd, epoll) five times with `hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled` on the default ptrace backend, without --strict, each under a 15-second timeout. Each pattern's stdout must start with its name and be identical across the five runs, and pipe-capacity must print exactly pipe-capacity:8192:a5, because Hermit must not expose the host's pipe-page pressure. It protects repeatable ordering and capacity results for pipes, socketpairs, eventfd and epoll. A pipe capacity that reflected host state, or epoll readiness reported in a different order on one run, fails the test. A failure is blocking for the super validation."########,
        labels: &[r########"super"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/ci/run-nextest-counted.sh -p hermit --features third-party-backends --test ipc_determinism ipc_patterns_are_deterministic_across_five_runs -j 1 -- --exact"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"super.build_workspace"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 180,
        cpu_timeout: 360,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"super"########,
        job: r########"random_source_determinism_diagnostic"########,
        desc: r########"Random-source determinism diagnostic"########,
        description: r########"Runs random_sources_repeat_across_runs_and_change_with_seed from hermit-cli/tests/random_determinism.rs. It compiles tests/c/random_sources.c, which reads randomness through sources including getrandom (libc and raw syscall) and checks flag validation and fault handling, and runs it five times with `hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --rng-seed=17` on the default ptrace backend. The five outputs must be non-empty and identical, and a run with --rng-seed=18 must print something different. It protects seeded randomness: the same seed gives the same bytes, and the seed has an effect. If a random source leaked host entropy, the five runs would differ; if the seed were ignored, the seed-18 output would match and the test fails. A failure is blocking for the super validation."########,
        labels: &[r########"super"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/ci/run-nextest-counted.sh -p hermit --features third-party-backends --test random_determinism random_sources_repeat_across_runs_and_change_with_seed -j 1 -- --exact"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"super.build_workspace"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 180,
        cpu_timeout: 360,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"super"########,
        job: r########"threaded_integration_matrix_diagnostic"########,
        desc: r########"Threaded integration matrix diagnostic"########,
        description: r########"Runs the whole integration_matrix binary (one test, integration_matrix, in hermit-cli/tests/integration_matrix.rs). Each case runs twice as `hermit --log=off run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --bind=<fixture>:/tmp/integration-matrix -- <program>` on the default ptrace backend, and a run still going after 30 seconds has its process group killed. Required cases are echo, ls and cat; optional cases, skipped when the program is absent, are sqlite3, python3, a four-worker node script, an eight-thread Java jar and git --version; nginx -t is expected to fail. A pass needs both runs to exit 0, print the expected marker (for example SHARED_FUTEX_NODE_OK workers=4) and give identical exit status, stdout and stderr. It checks that common multi-threaded host programs run repeatably under Hermit. A second node run with different stderr, or nginx -t succeeding, fails the matrix. A failure is blocking for the super validation."########,
        labels: &[r########"super"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/ci/run-nextest-counted.sh -p hermit --features third-party-backends --test integration_matrix -j 1 --"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"super.build_workspace"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 300,
        cpu_timeout: 600,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"super"########,
        job: r########"chaos_hello_race_verification_diagnostic"########,
        desc: r########"Chaos hello-race verification diagnostic"########,
        description: r########"Runs hello_race_chaos_verify from hermit-cli/tests/hermit_modes.rs. It compiles flaky-tests/hello_race.rs, a deliberately racy two-thread program, and runs `hermit run --verify --verify-allow=both --chaos --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --env=HERMIT_MODE=chaos` on it with the default ptrace backend. --verify-allow=both accepts any guest exit status, so the property under test is that the chaos-mode run repeats itself: stderr must contain "Success: deterministic." and Hermit must exit with a status code, not a signal. It protects reproducibility of chaos scheduling without PMU preemption. If chaos scheduling drew on host timing so the two verify runs took different interleavings, Hermit would report nondeterminism and the test fails with "chaos verification for hello_race failed". A failure is blocking for the super validation."########,
        labels: &[r########"super"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/ci/run-nextest-counted.sh -p hermit --features third-party-backends --test hermit_modes hello_race_chaos_verify -j 1 -- --exact"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"super.build_workspace"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 300,
        cpu_timeout: 600,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"super"########,
        job: r########"dbt_pipe_backpressure_diagnostic"########,
        desc: r########"DBT pipe backpressure diagnostic"########,
        description: r########"Runs run_dbt_verifies_pipe_backpressure from hermit-cli/tests/cli.rs, with Hermit built with third-party-backends so the DBT backend is present. The test runs `hermit --backend dbt run --verify -- /bin/bash -c '{ printf "%4096s" x; <100000-iteration loop>; printf "%1371s" y; } | wc -c'`, which must exit 0, print 5467 and report ":: Success: deterministic. Determinism verified." The shell writes 4096 bytes, spins, then writes 1371 more, so wc must read a split write across a blocking pipe. Review comments tie it to a host-inherited O_NONBLOCK regression (https://github.com/rrnewton/hermit/issues/598) and partial-read semantics (https://github.com/rrnewton/hermit/issues/689). If the guest's pipe behaved as non-blocking or lost part of the split write, wc would print a different count or the verify would fail. A failure is blocking for the super validation."########,
        labels: &[r########"super"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/ci/run-nextest-counted.sh -p hermit --features third-party-backends --test cli run_dbt_verifies_pipe_backpressure -j 1 -- --exact"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"super.build_workspace"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 300,
        cpu_timeout: 600,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"super"########,
        job: r########"dbt_failed_exec_recovery_diagnostic"########,
        desc: r########"DBT failed-exec recovery diagnostic"########,
        description: r########"Runs run_dbt_recovers_after_failed_exec from hermit-cli/tests/cli.rs, with the DBT backend compiled in through third-party-backends. The test compiles tests/c/dbt_exec_failure.c and runs it with `hermit --backend dbt run --strict --verify`. The guest calls execve on /definitely/missing, requires it to fail with ENOENT, sleeps 1 ms with nanosleep and prints "recovered after failed exec". The run must exit 0, print exactly that line and report ":: Success: deterministic. Determinism verified." It protects the DBT backend's handling of a failed exec: the guest must keep running in its original image after the kernel refuses the exec, and the run must stay deterministic. If the backend tore down or lost track of the process when execve returned an error, the guest would exit non-zero or print nothing, and the test fails. A failure is blocking for the super validation."########,
        labels: &[r########"super"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/ci/run-nextest-counted.sh -p hermit --features third-party-backends --test cli run_dbt_recovers_after_failed_exec -j 1 -- --exact"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"super.build_workspace"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 180,
        cpu_timeout: 360,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"super"########,
        job: r########"dbt_unsupported_syscall_aggregation_diagnostic"########,
        desc: r########"DBT unsupported-syscall aggregation diagnostic"########,
        description: r########"Runs the cli integration binary with the exact filter run_dbt_aggregates_unsupported_syscalls_and_strict_rejects_them. No test of that name exists in hermit-cli/tests/cli.rs at fe6670ebf: commit 1f15510a6 (2026-08-12, "Fail closed across run, record, and replay") renamed it run_dbt_fails_closed_by_default_and_opt_out_aggregates_unsupported_syscalls, so the node selects no test. The renamed test checks that the DBT backend refuses the unsupported restart_syscall by default and under --strict, naming it in stderr, and that --allow-unsupported-syscalls prints one aggregated warning across fork and exec children. Its comment says the portable test.cli node skips it until DBT aggregation defect https://github.com/rrnewton/hermit/issues/2804 is fixed (https://github.com/rrnewton/hermit/issues/2791). With nextest's default handling of an empty selection the run exits non-zero; the filter needs the new name. A failure is blocking for the super validation."########,
        labels: &[r########"super"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/ci/run-nextest-counted.sh -p hermit --features third-party-backends --test cli run_dbt_aggregates_unsupported_syscalls_and_strict_rejects_them -j 1 -- --exact"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"super.build_workspace"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 180,
        cpu_timeout: 360,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"super"########,
        job: r########"dbt_strict_blocked_stdin_teardown_diagnostic"########,
        desc: r########"DBT strict blocked-stdin teardown diagnostic"########,
        description: r########"Runs run_dbt_strict_returns_with_blocked_stdin_source from hermit-cli/tests/cli.rs, with the DBT backend compiled in. The test starts `sleep 30` with its stdout piped and passes that pipe as stdin to `hermit --backend dbt run --strict -- dbt_unsupported_syscall` (compiled from tests/c/dbt_unsupported_syscall.c, which calls the unsupported restart_syscall), wrapped in `timeout --kill-after 2s 10s`. Hermit must not hit the timeout (exit 124), must fail, and must print "unsupported syscall". It protects strict-mode DBT teardown: when the guest is refused, Hermit must return even though its stdin source is still open and silent. A Hermit that waited for stdin to reach end-of-file before exiting would be killed after 10 seconds, and the test reports "strict DBT hung on stdin". A failure is blocking for the super validation."########,
        labels: &[r########"super"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/ci/run-nextest-counted.sh -p hermit --features third-party-backends --test cli run_dbt_strict_returns_with_blocked_stdin_source -j 1 -- --exact"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"super.build_workspace"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 120,
        cpu_timeout: 60,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"super"########,
        job: r########"dbt_guest_stderr_isolation_diagnostic"########,
        desc: r########"DBT guest-stderr isolation diagnostic"########,
        description: r########"Runs run_dbt_keeps_diagnostics_out_of_guest_stderr from hermit-cli/tests/cli.rs, with the DBT backend compiled in. First it runs `hermit --log INFO --backend dbt run --strict -- /bin/bash -c ...`, where the script captures the stderr of a static no-libc guest and must print isolated=guest-stderr; Hermit's own stderr must contain "INFO detcore" and "DETLOG [syscall]" but not the guest's text. Then, with HERMIT_LOG unset and set to a sentinel, it runs a log-environment guest with --strict --verify --verify-strict --keep-logs and --verify-json, requiring the exit status to agree with the JSON verdict, exactly two retained logs containing INFO detcore, and no guest output in them. It protects the separation between DBT controller diagnostics and the guest's fd 2 and HERMIT_LOG. If controller logging wrote into the guest's stderr, the captured text would not equal guest-stderr and the test fails. A failure is blocking for the super validation."########,
        labels: &[r########"super"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/ci/run-nextest-counted.sh -p hermit --features third-party-backends --test cli run_dbt_keeps_diagnostics_out_of_guest_stderr -j 1 -- --exact"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"super.build_workspace"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 120,
        cpu_timeout: 240,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"super"########,
        job: r########"weekly_relaxed_default_mode_cases"########,
        desc: r########"Weekly relaxed default-mode cases"########,
        description: r########"Runs every non-ignored test in hermit-cli/tests/hermit_modes.rs whose name contains default_, on one thread: default_mode_matrix, 44 default_<workload> tests generated by default_workload_tests!, and eight CLI checks (default_minimal_hello, default_lit_networking, default_exit_codes, default_virtualized_uname, default_cat_issue, default_bind_mounts, default_preserved_tmpfs, default_environment_selection). Most run a compiled C, Rust or shell workload with `hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --no-sequentialize-threads --no-deterministic-io` on the default ptrace backend and require exit 0; some also check output, such as uname -nr printing "hermetic-container.local 5.2.0" or exit codes 0, 1 and 42 propagating. It covers Hermit's relaxed default mode, where threads are not sequentialized. Two cases known to block (bind/connect race, clock total order) are ignored and not run. A workload that crashes or exits non-zero in relaxed mode fails its test. A failure is blocking for the super validation."########,
        labels: &[r########"super"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/ci/run-nextest-counted.sh -p hermit --features third-party-backends --test hermit_modes default_ -j 1 --"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"super.build_workspace"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 1200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"super"########,
        job: r########"weekly_portable_chaos_cases"########,
        desc: r########"Weekly portable chaos cases"########,
        description: r########"Runs the non-ignored tests of hermit-cli/tests/stress_suite.rs on one thread: chaos_finds_and_reproduces_order_violation and schedule_bisect_localizes_publish_ordering_race (the --skip of slow_cas_search_and_replay has no effect, as that test is ignored). Both use `hermit run --base-env=minimal --chaos --sched-heuristic=random --max-timeslice=disabled --no-virtualize-cpuid --seed=N` on the default ptrace backend with a 10-second timeout per run, so no PMU is needed. The first requires tests/chaos/order_violation.c to print "Hello world!" without chaos, seed 9 to be the first of seeds 0-15 that exposes the race, and seed 9 to reproduce "ERROR! global_str is null at use." twice. The second records preemptions for a passing and a failing publish-ordering seed, then requires `hermit bisect` to localize the divergence to two adjacent events with stack traces. If a scheduler change moved the first failing seed, the test fails with "documented seed no longer identifies the first failing schedule". A failure is blocking for the super validation."########,
        labels: &[r########"super"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/ci/run-nextest-counted.sh -p hermit --features third-party-backends --test stress_suite -j 1 -- --skip slow_cas_search_and_replay"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"super.build_workspace"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 1200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"super"########,
        job: r########"weekly_ignored_portable_chaos_cases"########,
        desc: r########"Weekly ignored portable chaos cases"########,
        description: r########"Runs only the ignored tests of hermit-cli/tests/stress_suite.rs, minus the PMU-dependent slow_cas_search_and_replay: targeted_chaos_finds_order_violation_at_least_as_often, fast_chaos_matrix and slow_race_matrix. All use chaos mode with --sched-heuristic=random and --max-timeslice=disabled on the default ptrace backend, 10 seconds per run. The targeted test requires --chaos-target-races to expose the order-violation race in at least as many of seeds 0-15 as uniform chaos, and to reproduce it. fast_chaos_matrix runs eight categories of tests/stress/concurrency.rs at 2, 4, 8 and 16 threads over 10 seeds: racy categories must be exposed (two of them only below 16 threads), and correct ones (mutex-correctness, rwlock-fairness, store-buffer) never. slow_race_matrix requires producer-consumer and condvar-lost-wakeup to be exposed at 16 threads within 100 seeds. A correct category reporting a failure, or a guest timeout, fails the test. A failure is blocking for the super validation."########,
        labels: &[r########"super"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/ci/run-nextest-counted.sh -p hermit --features third-party-backends --test stress_suite -j 1 -- --ignored --skip slow_cas_search_and_replay"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"super.build_workspace"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 1200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"super"########,
        job: r########"pmu_buck_chaos_cases"########,
        desc: r########"PMU Buck chaos cases"########,
        description: r########"Runs the hermit-cli/tests/hermit_modes.rs tests whose names contain chaos_buck_, with --ignored, which selects only ignored tests: chaos_buck_nanosleep_parallel and chaos_buck_mem_race, not the six non-ignored chaos_buck_ cases that privileged-test.pmu_buck_chaos_cases runs. Each runs `hermit run --verify --chaos --base-env=empty --max-timeslice=1000000 --env=HERMIT_MODE=chaos` on its compiled workload (nanosleep-par.c and the Rust mem_race workload) from the workload's build directory, so chaos preemption uses the PMU branch-counter timer, and requires exit 0. Both are ignored as known PMU chaos failures tracked in https://github.com/rrnewton/hermit/issues/2791: nanosleep_parallel fails after an interrupted nanosleep reports EINTR, and mem_race produced no verdict within 300 seconds. Unless those defects are fixed, this node is expected to fail, and that failure is blocking for the super validation."########,
        labels: &[r########"super"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/ci/run-nextest-counted.sh -p hermit --features third-party-backends --test hermit_modes chaos_buck_ -j 1 -- --ignored"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"super.build_workspace"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 1200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"super"########,
        job: r########"pmu_analyze_hello_race_stress_calibrated_skid"########,
        desc: r########"PMU analyze hello-race stress (calibrated skid)"########,
        description: r########"Calibrates the PMU skid margin on the host, then runs the ignored analyze_hello_race test. The shell command checks four positive-integer settings (ANALYZE_SKID_CALIBRATION_ITERATIONS, default 64; ANALYZE_SKID_CALIBRATION_PERIOD, 1000000; ANALYZE_SKID_MINIMUM_MARGIN, 20000; ANALYZE_SKID_CALIBRATION_TIMEOUT, 30 seconds), compiles tests/util/pmu_skid.c into target/ci-pmu-skid, runs it to read "Recommended margin: N RCB", and passes the larger of N and the floor as HERMIT_ANALYZE_SKID_MARGIN. analyze_hello_race (hermit-cli/tests/analyze.rs) then runs `hermit analyze --run-arg=--base-env=host --run-arg=--skid-margin=<margin> --analyze-seed=0 --search -- --chaos --summary --max-timeslice=400000 -- hello_race`, which must exit 0, print a guest stack trace and name flaky-tests/hello_race.rs:37. It checks that hermit analyze pinpoints the hello_race race with PMU-driven chaos preemption sized to this host's measured skid. A missing calibration line exits with an error before the test; a different pinpointed line fails the test. A failure is blocking for the super validation."########,
        labels: &[r########"super"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; set -u
iters=${ANALYZE_SKID_CALIBRATION_ITERATIONS:-64}
period=${ANALYZE_SKID_CALIBRATION_PERIOD:-1000000}
floor=${ANALYZE_SKID_MINIMUM_MARGIN:-20000}
cal_timeout=${ANALYZE_SKID_CALIBRATION_TIMEOUT:-30}
for name in iters period floor cal_timeout; do
    eval "v=\$$name"
    case "$v" in
        ''|*[!0-9]*|0*) echo "Analyze PMU calibration error: $name must be a positive integer, got $v" >&2; exit 2 ;;
    esac
done
bin=$PWD/target/ci-pmu-skid
mkdir -p "$(dirname "$bin")" || exit 1
cc -O2 -Wall -Wextra -Werror -std=gnu11 tests/util/pmu_skid.c -o "$bin" || {
    echo "Analyze PMU calibration error: failed to build tests/util/pmu_skid.c" >&2; exit 1; }
out=$(timeout "$cal_timeout" "$bin" --iterations "$iters" --period "$period" 2>&1) || {
    status=$?; printf 'Analyze PMU calibration failed (exit %s):\n%s\n' "$status" "$out" >&2; exit "$status"; }
printf '%s\n' "$out"
rec=$(printf '%s\n' "$out" | sed -n 's/^Recommended margin: \([0-9][0-9]*\) RCB.*/\1/p')
case "$rec" in
    ''|*[!0-9]*|0*) echo "Analyze PMU calibration error: output omitted a valid recommended margin" >&2; exit 1 ;;
esac
margin=$rec
if [ "$margin" -lt "$floor" ]; then margin=$floor; fi
printf 'Analyze PMU skid margin: calibrated=%s RCB, conservative floor=%s RCB, using=%s RCB\n' \
    "$rec" "$floor" "$margin"
HERMIT_ANALYZE_SKID_MARGIN=$margin ./ci/run-nextest-counted.sh -p hermit --features third-party-backends --test analyze -j 1 analyze_hello_race -- --exact --ignored"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: Some(&[r########"analyze"########]),
        deps: &[
            r########"setup.nextest"########,
            r########"super.build_workspace"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 1200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"super"########,
        job: r########"build_pinned_leveldb_super_fixture"########,
        desc: r########"Build pinned LevelDB super fixture"########,
        description: r########"Runs hermit-cli/tests/prepare_leveldb.sh target/hermit-leveldb-super target/hermit-leveldb-build-super. If both directories already hold a clean LevelDB checkout at the pinned revision 7ee830d02b623e8ffe0b95d59a74db1e58da04c5 and built c_test, env_posix_test and leveldb_tests, it reuses them. Otherwise it requires that neither path exists, clones https://github.com/google/leveldb with a blob-less filter, checks out the pinned revision, fetches the googletest submodule at depth 1, configures a Release CMake build with tests on and benchmarks off, and builds the three targets with LEVELDB_BUILD_JOBS (default 2) jobs. It produces the fixture that super.full_leveldb_strict_determinism depends on, and writes no test results. A clone failure, since it needs network access to github.com, or a partial directory left at either path ("source and build destinations must not already exist") fails it, and the dependent LevelDB run is skipped. A failure is blocking for the super validation."########,
        labels: &[r########"super"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./hermit-cli/tests/prepare_leveldb.sh $PWD/target/hermit-leveldb-super $PWD/target/hermit-leveldb-build-super"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"super.build_workspace"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(17179869184),
            hard_mem_max_bytes: Some(17179869184),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 1200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"super"########,
        job: r########"full_leveldb_strict_determinism"########,
        desc: r########"Full LevelDB strict determinism"########,
        description: r########"Runs the ignored full_leveldb_suite_is_deterministic_under_strict test from hermit-cli/tests/leveldb.rs, with HERMIT_LEVELDB_BUILD_DIR set to the fixture built by super.build_pinned_leveldb_super_fixture. The test runs the whole leveldb_tests GoogleTest binary, without a filter, as `hermit --log=info run --strict --verify --base-env=minimal -- leveldb_tests` on the default ptrace backend, and requires exit 0 and "Success: deterministic. Determinism verified." on stderr. It checks that a real storage library's complete suite, including what the ignore reason calls long concurrent and randomized stress cases, passes and repeats exactly under strict mode. If one of LevelDB's threaded tests printed different output in the second verify run, Hermit would report nondeterminism and the test fails; an unset build directory makes it panic. A failure is blocking for the super validation."########,
        labels: &[r########"super"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; env HERMIT_LEVELDB_BUILD_DIR=$PWD/target/hermit-leveldb-build-super $PWD/ci/run-nextest-counted.sh -p hermit --features third-party-backends --test leveldb full_leveldb_suite_is_deterministic_under_strict -j 1 -- --exact --ignored"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"super.build_pinned_leveldb_super_fixture"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 1200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"super"########,
        job: r########"sqlite_veryquick_strict_determinism"########,
        desc: r########"SQLite veryquick strict determinism"########,
        description: r########"Runs the ignored sqlite_veryquick_is_deterministic_under_strict_hermit test, which runs hermit-cli/tests/fixtures/sqlite-veryquick/run.sh with the test's Hermit binary. The script downloads sqlite-src-3510200.zip (SQLite 3.51.2) from sqlite.org and checks its SHA-256, applies root-userns.patch, builds the static testfixture, then runs `hermit --log off run --workdir=RUN_DIR --strict -- testfixture test/veryquick.test --verbose=0` twice. Each run must stall at the known lock4 point (the script watches for "Time: lock3.test " and stops a run whose output stops growing for 30 seconds) with exactly the 13 known pre-stall failures, and the two runs' normalized stdout and stderr must match. The test then requires "SQLite 3.51.2 veryquick: reproduced lock4 stall with 13 identical pre-stall failures." It tracks a reproducible known limitation, not a full pass. A run that finishes instead of stalling, or a different failure list, fails it. A failure is blocking for the super validation."########,
        labels: &[r########"super"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/ci/run-nextest-counted.sh -p hermit --features third-party-backends --test sqlite_veryquick sqlite_veryquick_is_deterministic_under_strict_hermit -j 1 -- --exact --ignored"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"super.build_workspace"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 1200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"privileged-only-build"########,
        job: r########"privileged_tests"########,
        desc: r########"Build Hermit and the focused test binaries used by the privileged lane"########,
        description: r########"MEM-CAP DERIVATION 2026-08-04 (follow-on to task memory-caps-must-scale-with-job-count / #1583, which MISSED this privileged-lane build node): pinned_jobs=8 literal via CARGO_BUILD_JOBS=8 on all three Cargo commands (was unset -> cargo defaulted build phases to nproc; that cc1plus fan-out is the OOM class the pin bounds; the job-pin is the actual fix). The final command prebuilds the exact cli and hermit_modes integration binaries consumed by the privileged test nodes, so their wall budgets measure test execution instead of Cargo compilation or target-lock waiting. After the bin build, the content-addressed publisher verifies source type/mode/size, hashes before and after copying, verifies the published hash and atomically updates the pointer before any later Cargo invocation can relink target/debug/hermit. This privileged lane deliberately publishes a binary-only artifact; unlike portable DBT/SaBRe/LiteInst cells, it does not consume install_pkg resources. Cap KEPT at the existing generous 8.0GiB: a warm/incremental measurement read peak_bytes~1.64GiB @j8, but that reused build.workspace artifacts and UNDER-estimates a cold from-scratch privileged build, so the 8.0GiB headroom is retained rather than tightened on an unreliable warm figure. rss_baseline=5.0GiB (scheduling reservation). hermit@b384187e."########,
        labels: &[r########"privileged"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; CARGO_BUILD_JOBS=8 cargo build -p hermit --features third-party-backends --bin hermit && ./ci/publish-hermit-e2e-artifact.sh target/debug/hermit target/ci/hermit-e2e-artifacts target/ci/hermit-e2e-artifact.path && ./ci/nextest-binaries.rs prepare privileged"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"gate.manifest"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 60.0,
            rss_baseline_bytes: Some(5368709120),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 120,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"privileged-only-cpuid"########,
        job: r########"faulting"########,
        desc: r########"CPUID-faulting smoke: Detcore masks host RDRAND/RDSEED feature bits"########,
        description: r########"REQUIRES A MACHINE FACILITY, DECLARED 2026-08-12 (hermit#2135, hermit#2148, hermit#2205). rdrand_rdseed_is_masked can only observe Detcore masking host feature bits if the kernel can trap the guest's CPUID, which needs arch_prctl(ARCH_SET_CPUID, 0) to succeed. Where it cannot, this node used to FAIL in 0.11 s with exit 101 and an empty detail block -- indistinguishable from a broken build -- and its eager-exit aborted the twelve other in-flight nodes and filtered twenty-seven more, so a machine that runs 31 of 33 nodes in 3m22s produced no receipt at all. `requires_host_capability` moves that judgement OUT of the node, to an out-of-band probe run during plan construction, and makes the outcome a third recorded state: host-inapplicable, which is neither a pass nor a failure and is written to the ledger as a typed intentional skip. THIS DOES NOT WEAKEN THE TEST. Where the capability is present the node runs unchanged and its assertions keep full force; the probe fails closed toward running, so a probe error or an unexpected errno still runs it. The capability name is checked against the closed vocabulary in scripts/lib/validate_plan.rs::HostCapability, and an unknown name refuses the whole run."########,
        labels: &[
            r########"privileged"########,
            r########"cpuid-faulting"########,
        ],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; tests_misc="$(./ci/nextest-binaries.rs executable hermit-detcore tests_misc)" || exit 1; status=0; timeout --kill-after=5s 30s "$tests_misc" rdrand_rdseed_is_masked --exact || status=$?; if [ "$status" -eq 124 ] || [ "$status" -eq 137 ]; then printf 'test hermit-detcore/tests_misc::rdrand_rdseed_is_masked exceeded 30 s (innermost exact test timeout: exit %s)\n' "$status" >&2; fi; exit "$status""########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"privileged-only-build.privileged_tests"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 15.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(17179869184),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 40,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"privileged-only-pmu"########,
        job: r########"preemption"########,
        desc: r########"PMU smoke: retired-conditional-branch overflow delivery and skid measurement"########,
        description: r########"Same command as privileged-pmu.preemption in the full profile, so its paragraph can be reused: it compiles tests/util/pmu_skid.c in the pinned root and runs it for 16 overflow periods of 100000 retired conditional branches on a traced child pinned to one CPU, requiring each overflow to stop the child, mid-loop, with the counter's SIGUSR1; it prints the measured skid but does not bound it. In the privileged profile only privileged-only-test.pmu_buck_chaos_cases depends on it, so a host whose PMU cannot interrupt a traced thread fails here in seconds rather than deep inside that suite. Refusal of perf_event_open or PTRACE_TRACEME, a CPU that is neither Intel nor AMD, a descheduled counter, or no overflow signal within 20 seconds turns it red."########,
        labels: &[r########"privileged"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; cc -O2 -Wall -Wextra -Werror tests/util/pmu_skid.c -o target/ci-pmu-skid && timeout 20 target/ci-pmu-skid --iterations 16 --period 100000"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"privileged-only-build.privileged_tests"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 5.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(17179869184),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 30,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"privileged-only-test"########,
        job: r########"pmu_buck_chaos_cases"########,
        desc: r########"Run the six measured-passing Buck chaos cases under PMU preemption"########,
        description: r########"The six enabled cases passed direct measurement on the privileged host. chaos_buck_nanosleep_parallel remains ignored because an interrupted nanosleep reports EINTR; chaos_buck_mem_race remains ignored because it produced no verdict within 300 seconds. Both remain visible in super.pmu_buck_chaos_cases. Depending on pmu.preemption keeps this coverage in the existing privileged PMU partition without changing quick or portable-only."########,
        labels: &[r########"privileged"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; set -uo pipefail; status=0; ./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit --features third-party-backends --test hermit_modes -j 1 -E 'test(/^(chaos_buck_getpid|chaos_buck_uname|chaos_buck_sysinfo|chaos_buck_wait_on_child|chaos_buck_clone|chaos_buck_hello_alarm)$/)' || status=$?; exit "$status""########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: Some(&[r########"hermit_modes"########]),
        deps: &[r########"privileged-only-pmu.preemption"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 45.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(17179869184),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 120,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"privileged-only-e2e"########,
        job: r########"manifest_applications"########,
        desc: r########"Privileged manifest bucket: applications"########,
        description: r########"The selected KVM cell has a measured 74 s wall backstop and may retry once. The shared 600 s E2E-class node bound remains a generous backup after per-machine scaling, while the typed inner CPU/wall policy identifies the actual stop."########,
        labels: &[r########"privileged"########],
        cmd: r########"./ci/hermetic/run-in-pinned-root.sh --src . --out ignored/hermetic/split --src-rw --cargo-home ignored/hermetic/split/cargo --env CARGO_BUILD_JOBS --env DAGRUN_STEP_STARTED_MONOTONIC_NS --env DAGRUN_TEST_COUNTS_PATH --env E2E_BUILD_ROOT --env E2E_KERNEL_VERSION --env E2E_MACHINE_SHORTNAME --env E2E_RESULT_ROOT --env E2E_RUN_ID --env HERMIT_E2E_EMPTY_WORKDIR --env HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT --env L4_REPS --env PR_NUMBER --env SUPER_REPETITIONS --env THIRD_PARTY_BUILD_JOBS --env VALIDATE_VERBOSITY -- bash -c '/src/ci/hermetic/assert-no-network.sh && /src/ci/hermetic/assert-build-dependencies.sh && exec bash -c "$1"' bash 'export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh target/debug/test-harness run --lane privileged --category applications --ci-only --allow-empty --prebuilt --results "$E2E_RESULT_ROOT/privileged/manifest_applications/results.jsonl" --junit "$E2E_RESULT_ROOT/privileged/manifest_applications/junit.xml"'"########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"privileged"########,
            category: r########"applications"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.rust_scripts_in_pinned_root"########,
            r########"gate.manifest"########,
            r########"privileged-build.manifest_guests_in_pinned_root"########,
            r########"privileged-only-build.privileged_tests_in_pinned_root"########,
            r########"setup.pinned_root_fetch"########,
        ],
        env: &[(
            r########"HERMIT_E2E_EMPTY_WORKDIR"########,
            r########"/test"########,
        )],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 90.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(17179869184),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"privileged-only-e2e"########,
        job: r########"manifest_c_programs"########,
        desc: r########"Privileged manifest bucket: c-programs"########,
        description: r########"The selected cells are the five cpuid-probe verify cells (dbt, in-guest-trap, kvm, liteinst, ptrace): the kvm and ptrace cells that the privileged backend-parity-c node ran before that bucket was folded into c-programs (https://github.com/rrnewton/hermit/issues/3301), the dbt cell that slice S13 of the same issue carried over from the retired DBT parity matrix, and the in-guest LiteInst cells (liteinst, and in-guest-trap with site patching off) that returned on the new architecture after the LiteInst reset (https://github.com/rrnewton/hermit/issues/3745), each run with the maximum timeslice disabled. The selector no longer passes --allow-empty, so selecting no cells fails. They use the 57 s wall backstop and may retry once. The shared 600 s E2E-class node bound remains a generous backup after per-machine scaling, while the typed inner CPU/wall policy identifies the actual stop."########,
        labels: &[r########"privileged"########],
        cmd: r########"./ci/hermetic/run-in-pinned-root.sh --src . --out ignored/hermetic/split --src-rw --cargo-home ignored/hermetic/split/cargo --env CARGO_BUILD_JOBS --env DAGRUN_STEP_STARTED_MONOTONIC_NS --env DAGRUN_TEST_COUNTS_PATH --env E2E_BUILD_ROOT --env E2E_KERNEL_VERSION --env E2E_MACHINE_SHORTNAME --env E2E_RESULT_ROOT --env E2E_RUN_ID --env HERMIT_E2E_EMPTY_WORKDIR --env HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT --env L4_REPS --env PR_NUMBER --env SUPER_REPETITIONS --env THIRD_PARTY_BUILD_JOBS --env VALIDATE_VERBOSITY -- bash -c '/src/ci/hermetic/assert-no-network.sh && /src/ci/hermetic/assert-build-dependencies.sh && exec bash -c "$1"' bash 'export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh target/debug/test-harness run --lane privileged --category c-programs --ci-only --prebuilt --results "$E2E_RESULT_ROOT/privileged/manifest_c_programs/results.jsonl" --junit "$E2E_RESULT_ROOT/privileged/manifest_c_programs/junit.xml"'"########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"privileged"########,
            category: r########"c-programs"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.rust_scripts_in_pinned_root"########,
            r########"gate.manifest"########,
            r########"privileged-build.manifest_guests_in_pinned_root"########,
            r########"privileged-only-build.privileged_tests_in_pinned_root"########,
            r########"setup.pinned_root_fetch"########,
        ],
        env: &[(
            r########"HERMIT_E2E_EMPTY_WORKDIR"########,
            r########"/test"########,
        )],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 5.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(17179869184),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"privileged-only-e2e"########,
        job: r########"manifest_system_utils"########,
        desc: r########"Privileged manifest bucket: system-utils"########,
        description: r########"HOST-REQUIREMENT ROUTING 2026-09-28: system-utils/sysfs-sanitized-prefixes is the only cell in this privileged bucket. Its guest reads one live leaf under each of eight host sysfs prefixes (block, hwmon, rtc, node, btrfs, irq, uevent, module) and refuses when a prefix has no readable leaf, so it needs a host that has that hardware and a mounted btrfs. GitHub-hosted run https://github.com/rrnewton/hermit/actions/runs/36485831200 failed it with "hwmon has no readable sanitized leaf". The manifest entry is therefore lane: privileged, which keeps it out of the hosted-portable selection while the full profile still runs it. Wall evidence from the ledger series of the development host recorded in docs/TESTING_ENVIRONMENTS.md, "Named measurement hosts" (2026-08..09): verify/ptrace 148 samples, per-run median 2096 ms, max 15438 ms; verify/kvm 29 samples, median 2316 ms, max 3467 ms. Memory hints match the portable system-utils bucket, whose 3 GiB hard cap already contained both cells at --jobs 1. The shared 600 s E2E-class node bound is the backstop."########,
        labels: &[r########"privileged"########],
        cmd: r########"./ci/hermetic/run-in-pinned-root.sh --src . --out ignored/hermetic/split --src-rw --cargo-home ignored/hermetic/split/cargo --env CARGO_BUILD_JOBS --env DAGRUN_STEP_STARTED_MONOTONIC_NS --env DAGRUN_TEST_COUNTS_PATH --env E2E_BUILD_ROOT --env E2E_KERNEL_VERSION --env E2E_MACHINE_SHORTNAME --env E2E_RESULT_ROOT --env E2E_RUN_ID --env HERMIT_E2E_EMPTY_WORKDIR --env HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT --env L4_REPS --env PR_NUMBER --env SUPER_REPETITIONS --env THIRD_PARTY_BUILD_JOBS --env VALIDATE_VERBOSITY -- bash -c '/src/ci/hermetic/assert-no-network.sh && /src/ci/hermetic/assert-build-dependencies.sh && exec bash -c "$1"' bash 'export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh target/debug/test-harness run --lane privileged --category system-utils --ci-only --allow-empty --prebuilt --jobs 1 --results "$E2E_RESULT_ROOT/privileged/manifest_system_utils/results.jsonl" --junit "$E2E_RESULT_ROOT/privileged/manifest_system_utils/junit.xml"'"########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"privileged"########,
            category: r########"system-utils"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.rust_scripts_in_pinned_root"########,
            r########"gate.manifest"########,
            r########"privileged-build.manifest_guests_in_pinned_root"########,
            r########"privileged-only-build.privileged_tests_in_pinned_root"########,
            r########"setup.pinned_root_fetch"########,
        ],
        env: &[(
            r########"HERMIT_E2E_EMPTY_WORKDIR"########,
            r########"/test"########,
        )],
        hint: HintSpec {
            resources: &[(r########"manifest_guest"########, 1)],
            est_duration_s: 30.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(3221225472),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: Some(1),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: Some(r########""########),
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"privileged-only-test"########,
        job: r########"cli_kvm"########,
        desc: r########"KVM CLI tests and initialized-VM setup cleanup"########,
        description: r########"Runs all 55 KVM-specific hermit CLI tests and the initialized-VM setup cleanup control after requiring /dev/kvm. The inventory gate requires all 55 distinct, nonignored CLI tests including the self-SIGKILL, synchronous-fault, root-exit reparenting, exec timer, nonleader-exec refusal and unreadable futex word regressions and exactly the named setup test. The portable lane continues to skip run_kvm_ because these tests self-guard without /dev/kvm and would otherwise report silent passes. KVM consumers may overlap: /dev/kvm supports multiple concurrent guests, and no repository or host constraint establishes it as an exclusive resource."########,
        labels: &[r########"privileged"########, r########"kvm"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; set -uo pipefail; if ! exec 9<>/dev/kvm; then printf 'test.cli_kvm: /dev/kvm could not be opened, so the selected run_kvm_ tests would self-guard and report silent passes. Refusing rather than reporting a green that measured nothing.\n' >&2; exit 1; fi; exec 9<&-; log=$(mktemp); trap 'rm -f "$log"' EXIT; if ! ./ci/nextest-binaries.rs list ${CI:+--profile ci} -p hermit --features third-party-backends,kvm-execution-tests --lib --test cli -E 'test(/^run_kvm_/) | test(=kvm_execution_tests::initialized_vm_setup_failures_consume_detcore_state_without_further_guest_execution)' --message-format json >"$log"; then exit 1; fi; if ! jq -e '[."rust-suites"[] | .testcases | to_entries[] | select(.value."filter-match".status == "matches")] as $selected | ($selected | length == 56) and ([$selected[].key] | unique | length == 56) and ($selected | all(.value.ignored == false)) and ([$selected[] | select(.key == "run_kvm_self_sigkill_from_nonleader_is_group_fatal")] | length == 1) and ([$selected[] | select(.key == "run_kvm_synchronous_root_segv_preserves_guest_exit")] | length == 1) and ([$selected[] | select(.key == "run_kvm_synchronous_orphan_segv_preserves_root_success")] | length == 1) and ([$selected[] | select(.key == "run_kvm_root_exit_reparents_live_child_and_grandchild")] | length == 1) and ([$selected[] | select(.key == "run_kvm_exec_deletes_posix_timers_and_preserves_itimer")] | length == 1) and ([$selected[] | select(.key == "run_kvm_nonleader_exec_is_policy_refusal")] | length == 1) and ([$selected[] | select(.key == "run_kvm_futex_wait_on_an_unreadable_word_matches_linux")] | length == 1) and ([$selected[] | select(.key | startswith("run_kvm_"))] | length == 55) and ([$selected[] | select(.key == "kvm_execution_tests::initialized_vm_setup_failures_consume_detcore_state_without_further_guest_execution")] | length == 1)' "$log" >/dev/null; then printf 'test.cli_kvm: expected exactly 55 distinct, nonignored run_kvm_ tests including self-SIGKILL, both synchronous-fault regressions, root-exit reparenting, exec timer lifetime, nonleader-exec refusal, the unreadable futex word, and the named setup test; the inventory changed. Update the tests and this gate together.\n' >&2; exit 1; fi; ./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit --features third-party-backends,kvm-execution-tests --lib --test cli -j 1 -E 'test(/^run_kvm_/) | test(=kvm_execution_tests::initialized_vm_setup_failures_consume_detcore_state_without_further_guest_execution)'; exit $?"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: Some(&[r########"cli"########]),
        deps: &[r########"privileged-only-build.privileged_tests"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 60.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(17179869184),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 180,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"privileged-scorecard"########,
        job: r########"compatibility"########,
        desc: r########"Verify fresh per-cell results and print the compatibility table"########,
        description: r########"After the three privileged-only-e2e buckets (applications, c_programs, system_utils) finish, `scorecard.rs verify-results --lanes privileged` requires one fresh row at git HEAD, from a clean tree, for every privileged-lane regression cell ci/expected-e2e-plan.json selects, with the same rules as full-scorecard.compatibility: each must PASS unless declared diagnostic, stripped-comparator passes are counted apart as below L2, all rows come from one Hermit binary and each cell's rows from one run. It is the privileged profile's only cross-bucket completeness check: a bucket node can pass while a cell it should have run left no row. A missing privileged cell, or a row left from an earlier commit, ends with "fresh result set refused" listing it."########,
        labels: &[r########"privileged"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/compat-envelope/scorecard.rs verify-results --results "$E2E_RESULT_ROOT" --lanes privileged"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[
            r########"privileged-only-e2e.manifest_applications"########,
            r########"privileged-only-e2e.manifest_c_programs"########,
            r########"privileged-only-e2e.manifest_system_utils"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(1073741824),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 120,
        cpu_timeout: 120,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"compatprep"########,
        job: r########"hermit_release"########,
        desc: r########"Release Hermit for compatibility"########,
        description: r########"Dedicated release build for the focused compatibility profiles that still run per-program probes (e9patch; the corpus-only profile builds its twin in the pinned root); it preserves their pre-cutover command, budgets, memory cap, and eight-job CPU allocation without selecting the broader full-profile runtime build. The sabre-compat-only, strict-compat-only and rr-compat-only profiles do not take it: their buckets run the validation's one build, the e2e artifact."########,
        labels: &[
            r########"portable-strict-compat-only"########,
            r########"e9patch-compat-only"########,
        ],
        cmd: r########"cargo build --release -p hermit --features third-party-backends"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"gate.manifest"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: None,
            hard_mem_max_bytes: Some(17179869184),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: Some(8),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 420,
        cpu_timeout: 840,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"qemu"########,
        job: r########"hermit_release"########,
        desc: r########"Release Hermit for QEMU L2"########,
        description: r########"Runs `cargo build --release -p hermit --features third-party-backends` on the host, after gate.manifest, as the only build of the qemu-l2-only focused profile. Its single dependent, qemu.strict_l2_boot, runs tests/qemu-boot/strict_l2_test.sh, which defaults to target/release/hermit and to a verification-report binary beside it. The command is the same as super.build_release_hermit and compatprep.hermit_release; the profile keeps its own copy so it can run without the full validation's builds. A release-mode compile error in the hermit package or a third-party backend stops the profile before any QEMU boot is attempted. WIDTH AND WALL: it runs at 8 CPUs (CpuBound, preferred_inner_jobs 8, passed to Cargo as CARGO_BUILD_JOBS), the width of compatprep.hermit_release, which runs the same command. At dagrun's default one CPU the build costs 747-753 CPU-seconds, more than its wall. In the qemu-l2-only validation of Hermit 7f661023 on 2026-10-08, on the development host recorded in docs/TESTING_ENVIRONMENTS.md, "Named measurement hosts", it took 166 s, and qemu.strict_l2_boot then passed in 373 s. The 600 s wall is 3.6 times 166 s. The wall was 3600 s, which made the qemu-l2-only profile refuse every node once preparation passed 600 s of ci-hub's 4200 s run budget (https://github.com/rrnewton/hermit/issues/3906)."########,
        labels: &[r########"qemu-l2-only"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; cargo build --release -p hermit --features third-party-backends"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"gate.manifest"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(17179869184),
            hard_mem_max_bytes: Some(17179869184),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: Some(8),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: Some(""),
        jobs_env: Some(r########"CARGO_BUILD_JOBS"########),
    },
    StaticStepSpec {
        group: r########"qemu"########,
        job: r########"strict_l2_boot"########,
        desc: r########"QEMU strict L2 boot (heavyweight)"########,
        description: r########"Runs tests/qemu-boot/strict_l2_test.sh: it compiles tests/shared-futex-verify/qemu_init.c as a static init, packs it into an initramfs, and boots the host kernel (/boot/vmlinuz) in qemu-system-x86_64 with TCG single-threaded, -icount shift=0,sleep=off, one CPU, no network and a VM clock. First `hermit run --strict` boots it once and requires the init's marker SHARED_FUTEX_QEMU_KERNEL_OK and none of the clock-failure messages (PIT calibration, skewed clocksource, unstable TSC); then `hermit run --strict --verify --verify-json` boots it again and `verification-report matched` must accept the report. It is the reproducible QEMU/Linux path under Hermit at full L2. A phase over 300 seconds, a missing marker, or "Marking TSC unstable" in the boot log fails it."########,
        labels: &[r########"qemu-l2-only"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./tests/qemu-boot/strict_l2_test.sh"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"qemu.hermit_release"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(17179869184),
            hard_mem_max_bytes: Some(17179869184),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 1500,
        cpu_timeout: 3000,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"envelope"########,
        job: r########"build"########,
        desc: r########"Build workspace for envelope measurement"########,
        description: r########"Runs `cargo build --workspace --features third-party-backends` in the debug profile on the host, with the prebuilt rust-script environment exported, to produce the target/debug/hermit that all 15 envelope probes invoke. It depends on gate.manifest and carries only the envelope-only label, so it runs only for `./scripts/validate.rs --envelope-only` or `--envelope-compare FILE`; scripts/progress-report.sh and the progress-rubric skill are the callers. Unlike the probes it stays blocking: validate.rs's own self-check refuses an envelope plan in which this build is nonblocking. A build failure is one of only two ways the measurement profile exits nonzero; the other is a baseline regression. Limits: 600 s wall at 8 CPUs (CpuBound, preferred_inner_jobs 8, passed to Cargo as CARGO_BUILD_JOBS), 7200 CPU-s, 16 GiB. At dagrun's default one CPU this build costs about 575 CPU-seconds, so it needs the width. In the envelope-only validation of Hermit 8e0ec926 on 2026-10-08, on the development host recorded in docs/TESTING_ENVIRONMENTS.md, "Named measurement hosts", it took 83 s; 600 s is 7.2 times that. The wall was 3600 s, which made the envelope profile refuse every node once preparation passed 600 s of ci-hub's 4200 s run budget (https://github.com/rrnewton/hermit/issues/3906). A compile error anywhere in the workspace, third-party backends included, fails this node and skips every probe, and each skipped probe scores 0 in envelope.json."########,
        labels: &[r########"envelope-only"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; cargo build --workspace --features third-party-backends"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"gate.manifest"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(17179869184),
            hard_mem_max_bytes: Some(17179869184),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: Some(8),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: Some(""),
        jobs_env: Some(r########"CARGO_BUILD_JOBS"########),
    },
    StaticStepSpec {
        group: r########"envelope"########,
        job: r########"true_l1"########,
        desc: r########"envelope true: L1 hermit run --strict"########,
        description: r########"Runs `$PWD/target/debug/hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --strict -- /bin/true </dev/null` once, on the default ptrace backend, with the debug Hermit that envelope.build compiled. This is the L1 entry of the working-envelope vector for the true probe. The node passes when Hermit exits 0 within 30 s wall time and 60 CPU-s, under a 4-GiB memory cap; it writes no structured test results, so the exit status is the whole verdict. It runs only under `./scripts/validate.rs --envelope-only` or `--envelope-compare FILE` (label envelope-only). That profile makes every probe node nonblocking: a failure only lowers l1_pass in envelope.json, and only `--envelope-compare` turns a count below the baseline into a failed run. A typical failure is strict mode refusing a syscall that /bin/true makes: Hermit exits nonzero and the probe scores 0."########,
        labels: &[r########"envelope-only"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/target/debug/hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --strict -- /bin/true </dev/null"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"envelope.build"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(4294967296),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 30,
        cpu_timeout: 60,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"envelope"########,
        job: r########"true_l2"########,
        desc: r########"envelope true: L2 --strict --verify"########,
        description: r########"Runs `$PWD/target/debug/hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --strict --verify -- /bin/true </dev/null` on the default ptrace backend: Hermit runs /bin/true twice under strict mode and compares the two runs' exit status, output and logs. The title says L2, but plain `--verify` selects the lossy Stripped comparator, which erases numbers, addresses, /tmp paths and timestamps before diffing. Without `--verify-strict`, `--verify-json` and a `bitwise_parity: true` check, this is not canonical L2 as AGENTS.md defines it, only stripped repeat parity. It passes on exit 0 within 30 s wall, 60 CPU-s and 4 GiB. The result is the exit status, with no structured results; the node is nonblocking and counted in l2_pass. envelope.true_l4 depends on it. A divergence prints "Failure: nondeterministic." and exits nonzero; a first run that exits nonzero stops with "First run errored during --verify, not continuing to a second.""########,
        labels: &[r########"envelope-only"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/target/debug/hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --strict --verify -- /bin/true </dev/null"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"envelope.build"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(4294967296),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 30,
        cpu_timeout: 60,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"envelope"########,
        job: r########"true_l3"########,
        desc: r########"envelope true: L3 --verify --detlog-heap --detlog-stack"########,
        description: r########"Runs the L2 command with heap and stack logging added: `$PWD/target/debug/hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --strict --verify --detlog-heap --detlog-stack -- /bin/true </dev/null` (the title omits --strict, but the command passes it). Detcore adds hash lines for the guest's heap and stack mappings to the log, so the two runs are also compared on memory contents; the stack hash covers argv and the environment. It inherits plain `--verify`, so the comparison is the lossy Stripped one: this is memory-hash parity under the stripped comparator, not canonical L3. It passes on exit 0 within 30 s wall, 60 CPU-s and 4 GiB. The node is nonblocking, counted in l3_pass of envelope.json, with no structured results. A heap or stack hash that differs between the two runs makes Hermit print "Failure: nondeterministic." and exit nonzero."########,
        labels: &[r########"envelope-only"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/target/debug/hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --strict --verify --detlog-heap --detlog-stack -- /bin/true </dev/null"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"envelope.build"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(4294967296),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 30,
        cpu_timeout: 60,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"envelope"########,
        job: r########"true_l4"########,
        desc: r########"envelope true: L4 = L2 stress x20 (no divergence)"########,
        description: r########"Repeats the L2 command 20 times: `i=0; while [ $i -lt 20 ]; do timeout 30s $PWD/target/debug/hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --strict --verify -- /bin/true </dev/null || exit 1; i=$((i+1)); done`, and stops at the first failure. AGENTS.md defines L4 as canonical L2/L3 repeated 20 times; this loop repeats the Stripped-comparator L2 command, so it measures stress at stripped parity only. It depends on envelope.true_l2, so when L2 fails it is skipped and scores 0 (this reproduces validate.sh's `p2 == 1` guard). Limits: 660 s wall, 1200 CPU-s, 4 GiB. The node is nonblocking and counted in l4_pass, with no structured results. The count of 20 is fixed in the committed graph: validate.rs refuses an L4_REPS override instead of applying it. One diverging or 30-second-timed-out repetition anywhere in the loop fails the node with exit 1."########,
        labels: &[r########"envelope-only"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; i=0; while [ $i -lt 20 ]; do timeout 30s $PWD/target/debug/hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --strict --verify -- /bin/true </dev/null || exit 1; i=$((i+1)); done"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"envelope.true_l2"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(4294967296),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 660,
        cpu_timeout: 1200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"envelope"########,
        job: r########"true_rr"########,
        desc: r########"envelope true: rr record/replay end-to-end"########,
        description: r########"Runs `$PWD/target/debug/hermit record start --verify -- /bin/true </dev/null`: Hermit records /bin/true, replays the recording immediately, and compares the two runs' output and logs with the Stripped comparator, because `--verify-strict` is not passed. Here 'rr' means Hermit's own record/replay, not the rr debugger. Recording does not use `run --strict`'s configuration: it reads the real host clock, does not make I/O deterministic, and uses the host base environment rather than minimal. So this measures replay fidelity, not agreement between independent runs. It passes on exit 0 within 30 s wall, 60 CPU-s and 4 GiB. The node is nonblocking and counted in rr_pass of envelope.json, with no structured results. A replay that departs from the recording prints "Recording output did not match replay output!" and exits nonzero."########,
        labels: &[r########"envelope-only"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/target/debug/hermit record start --verify -- /bin/true </dev/null"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"envelope.build"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(4294967296),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 30,
        cpu_timeout: 60,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"envelope"########,
        job: r########"echo_l1"########,
        desc: r########"envelope echo: L1 hermit run --strict"########,
        description: r########"Runs `$PWD/target/debug/hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --strict -- /bin/echo hermit-envelope </dev/null` once, on the default ptrace backend, with the debug Hermit that envelope.build compiled. This is the L1 entry of the working-envelope vector for the echo probe. The node passes when Hermit exits 0 within 30 s wall time and 60 CPU-s, under a 4-GiB memory cap; it writes no structured test results, so the exit status is the whole verdict. It runs only under `./scripts/validate.rs --envelope-only` or `--envelope-compare FILE` (label envelope-only). That profile makes every probe node nonblocking: a failure only lowers l1_pass in envelope.json, and only `--envelope-compare` turns a count below the baseline into a failed run. A typical failure is strict mode refusing a syscall that /bin/echo hermit-envelope makes: Hermit exits nonzero and the probe scores 0."########,
        labels: &[r########"envelope-only"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/target/debug/hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --strict -- /bin/echo hermit-envelope </dev/null"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"envelope.build"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(4294967296),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 30,
        cpu_timeout: 60,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"envelope"########,
        job: r########"echo_l2"########,
        desc: r########"envelope echo: L2 --strict --verify"########,
        description: r########"Runs `$PWD/target/debug/hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --strict --verify -- /bin/echo hermit-envelope </dev/null` on the default ptrace backend: Hermit runs /bin/echo hermit-envelope twice under strict mode and compares the two runs' exit status, output and logs. The title says L2, but plain `--verify` selects the lossy Stripped comparator, which erases numbers, addresses, /tmp paths and timestamps before diffing. Without `--verify-strict`, `--verify-json` and a `bitwise_parity: true` check, this is not canonical L2 as AGENTS.md defines it, only stripped repeat parity. It passes on exit 0 within 30 s wall, 60 CPU-s and 4 GiB. The result is the exit status, with no structured results; the node is nonblocking and counted in l2_pass. envelope.echo_l4 depends on it. A divergence prints "Failure: nondeterministic." and exits nonzero; a first run that exits nonzero stops with "First run errored during --verify, not continuing to a second.""########,
        labels: &[r########"envelope-only"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/target/debug/hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --strict --verify -- /bin/echo hermit-envelope </dev/null"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"envelope.build"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(4294967296),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 30,
        cpu_timeout: 60,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"envelope"########,
        job: r########"echo_l3"########,
        desc: r########"envelope echo: L3 --verify --detlog-heap --detlog-stack"########,
        description: r########"Runs the L2 command with heap and stack logging added: `$PWD/target/debug/hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --strict --verify --detlog-heap --detlog-stack -- /bin/echo hermit-envelope </dev/null` (the title omits --strict, but the command passes it). Detcore adds hash lines for the guest's heap and stack mappings to the log, so the two runs are also compared on memory contents; the stack hash covers argv and the environment. It inherits plain `--verify`, so the comparison is the lossy Stripped one: this is memory-hash parity under the stripped comparator, not canonical L3. It passes on exit 0 within 30 s wall, 60 CPU-s and 4 GiB. The node is nonblocking, counted in l3_pass of envelope.json, with no structured results. A heap or stack hash that differs between the two runs makes Hermit print "Failure: nondeterministic." and exit nonzero."########,
        labels: &[r########"envelope-only"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/target/debug/hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --strict --verify --detlog-heap --detlog-stack -- /bin/echo hermit-envelope </dev/null"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"envelope.build"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(4294967296),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 30,
        cpu_timeout: 60,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"envelope"########,
        job: r########"echo_l4"########,
        desc: r########"envelope echo: L4 = L2 stress x20 (no divergence)"########,
        description: r########"Repeats the L2 command 20 times: `i=0; while [ $i -lt 20 ]; do timeout 30s $PWD/target/debug/hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --strict --verify -- /bin/echo hermit-envelope </dev/null || exit 1; i=$((i+1)); done`, and stops at the first failure. AGENTS.md defines L4 as canonical L2/L3 repeated 20 times; this loop repeats the Stripped-comparator L2 command, so it measures stress at stripped parity only. It depends on envelope.echo_l2, so when L2 fails it is skipped and scores 0 (this reproduces validate.sh's `p2 == 1` guard). Limits: 660 s wall, 1200 CPU-s, 4 GiB. The node is nonblocking and counted in l4_pass, with no structured results. The count of 20 is fixed in the committed graph: validate.rs refuses an L4_REPS override instead of applying it. One diverging or 30-second-timed-out repetition anywhere in the loop fails the node with exit 1."########,
        labels: &[r########"envelope-only"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; i=0; while [ $i -lt 20 ]; do timeout 30s $PWD/target/debug/hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --strict --verify -- /bin/echo hermit-envelope </dev/null || exit 1; i=$((i+1)); done"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"envelope.echo_l2"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(4294967296),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 660,
        cpu_timeout: 1200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"envelope"########,
        job: r########"echo_rr"########,
        desc: r########"envelope echo: rr record/replay end-to-end"########,
        description: r########"Runs `$PWD/target/debug/hermit record start --verify -- /bin/echo hermit-envelope </dev/null`: Hermit records /bin/echo hermit-envelope, replays the recording immediately, and compares the two runs' output and logs with the Stripped comparator, because `--verify-strict` is not passed. Here 'rr' means Hermit's own record/replay, not the rr debugger. Recording does not use `run --strict`'s configuration: it reads the real host clock, does not make I/O deterministic, and uses the host base environment rather than minimal. So this measures replay fidelity, not agreement between independent runs. It passes on exit 0 within 30 s wall, 60 CPU-s and 4 GiB. The node is nonblocking and counted in rr_pass of envelope.json, with no structured results. A replay that departs from the recording prints "Recording output did not match replay output!" and exits nonzero."########,
        labels: &[r########"envelope-only"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/target/debug/hermit record start --verify -- /bin/echo hermit-envelope </dev/null"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"envelope.build"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(4294967296),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 30,
        cpu_timeout: 60,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"envelope"########,
        job: r########"date_l1"########,
        desc: r########"envelope date: L1 hermit run --strict"########,
        description: r########"Runs `$PWD/target/debug/hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --strict -- /bin/date -u +%Y </dev/null` once, on the default ptrace backend, with the debug Hermit that envelope.build compiled. This is the L1 entry of the working-envelope vector for the date probe. The node passes when Hermit exits 0 within 30 s wall time and 60 CPU-s, under a 4-GiB memory cap; it writes no structured test results, so the exit status is the whole verdict. It runs only under `./scripts/validate.rs --envelope-only` or `--envelope-compare FILE` (label envelope-only). That profile makes every probe node nonblocking: a failure only lowers l1_pass in envelope.json, and only `--envelope-compare` turns a count below the baseline into a failed run. A typical failure is strict mode refusing a syscall that /bin/date -u +%Y makes: Hermit exits nonzero and the probe scores 0."########,
        labels: &[r########"envelope-only"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/target/debug/hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --strict -- /bin/date -u +%Y </dev/null"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"envelope.build"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(4294967296),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 30,
        cpu_timeout: 60,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"envelope"########,
        job: r########"date_l2"########,
        desc: r########"envelope date: L2 --strict --verify"########,
        description: r########"Runs `$PWD/target/debug/hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --strict --verify -- /bin/date -u +%Y </dev/null` on the default ptrace backend: Hermit runs /bin/date -u +%Y twice under strict mode and compares the two runs' exit status, output and logs. The title says L2, but plain `--verify` selects the lossy Stripped comparator, which erases numbers, addresses, /tmp paths and timestamps before diffing. Without `--verify-strict`, `--verify-json` and a `bitwise_parity: true` check, this is not canonical L2 as AGENTS.md defines it, only stripped repeat parity. It passes on exit 0 within 30 s wall, 60 CPU-s and 4 GiB. The result is the exit status, with no structured results; the node is nonblocking and counted in l2_pass. envelope.date_l4 depends on it. A divergence prints "Failure: nondeterministic." and exits nonzero; a first run that exits nonzero stops with "First run errored during --verify, not continuing to a second.""########,
        labels: &[r########"envelope-only"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/target/debug/hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --strict --verify -- /bin/date -u +%Y </dev/null"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"envelope.build"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(4294967296),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 30,
        cpu_timeout: 60,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"envelope"########,
        job: r########"date_l3"########,
        desc: r########"envelope date: L3 --verify --detlog-heap --detlog-stack"########,
        description: r########"Runs the L2 command with heap and stack logging added: `$PWD/target/debug/hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --strict --verify --detlog-heap --detlog-stack -- /bin/date -u +%Y </dev/null` (the title omits --strict, but the command passes it). Detcore adds hash lines for the guest's heap and stack mappings to the log, so the two runs are also compared on memory contents; the stack hash covers argv and the environment. It inherits plain `--verify`, so the comparison is the lossy Stripped one: this is memory-hash parity under the stripped comparator, not canonical L3. It passes on exit 0 within 30 s wall, 60 CPU-s and 4 GiB. The node is nonblocking, counted in l3_pass of envelope.json, with no structured results. A heap or stack hash that differs between the two runs makes Hermit print "Failure: nondeterministic." and exit nonzero."########,
        labels: &[r########"envelope-only"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/target/debug/hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --strict --verify --detlog-heap --detlog-stack -- /bin/date -u +%Y </dev/null"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"envelope.build"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(4294967296),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 30,
        cpu_timeout: 60,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"envelope"########,
        job: r########"date_l4"########,
        desc: r########"envelope date: L4 = L2 stress x20 (no divergence)"########,
        description: r########"Repeats the L2 command 20 times: `i=0; while [ $i -lt 20 ]; do timeout 30s $PWD/target/debug/hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --strict --verify -- /bin/date -u +%Y </dev/null || exit 1; i=$((i+1)); done`, and stops at the first failure. AGENTS.md defines L4 as canonical L2/L3 repeated 20 times; this loop repeats the Stripped-comparator L2 command, so it measures stress at stripped parity only. It depends on envelope.date_l2, so when L2 fails it is skipped and scores 0 (this reproduces validate.sh's `p2 == 1` guard). Limits: 660 s wall, 1200 CPU-s, 4 GiB. The node is nonblocking and counted in l4_pass, with no structured results. The count of 20 is fixed in the committed graph: validate.rs refuses an L4_REPS override instead of applying it. One diverging or 30-second-timed-out repetition anywhere in the loop fails the node with exit 1."########,
        labels: &[r########"envelope-only"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; i=0; while [ $i -lt 20 ]; do timeout 30s $PWD/target/debug/hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --strict --verify -- /bin/date -u +%Y </dev/null || exit 1; i=$((i+1)); done"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"envelope.date_l2"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(4294967296),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 660,
        cpu_timeout: 1200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"envelope"########,
        job: r########"date_rr"########,
        desc: r########"envelope date: rr record/replay end-to-end"########,
        description: r########"Runs `$PWD/target/debug/hermit record start --verify -- /bin/date -u +%Y </dev/null`: Hermit records /bin/date -u +%Y, replays the recording immediately, and compares the two runs' output and logs with the Stripped comparator, because `--verify-strict` is not passed. Here 'rr' means Hermit's own record/replay, not the rr debugger. Recording does not use `run --strict`'s configuration: it reads the real host clock, does not make I/O deterministic, and uses the host base environment rather than minimal. So this measures replay fidelity, not agreement between independent runs. It passes on exit 0 within 30 s wall, 60 CPU-s and 4 GiB. The node is nonblocking and counted in rr_pass of envelope.json, with no structured results. A replay that departs from the recording prints "Recording output did not match replay output!" and exits nonzero."########,
        labels: &[r########"envelope-only"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; $PWD/target/debug/hermit record start --verify -- /bin/date -u +%Y </dev/null"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"envelope.build"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(4294967296),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 30,
        cpu_timeout: 60,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"e2e"########,
        job: r########"manifest_applications_on_host"########,
        desc: r########"Portable manifest bucket: applications"########,
        description: r########"Hosted-portable twin of e2e.manifest_applications. It runs the same test-harness selection, `./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category applications --ci-only --allow-empty --prebuilt --exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap`, with the same results.jsonl and junit paths. It runs directly on the host, not through ci/hermetic/run-in-pinned-root.sh, so there is no no-network or build-dependency assertion and the cells use the host's own tools and libraries. Its only label is hosted-portable (`./scripts/validate.rs --hosted-portable-only`, run by .github/workflows/ci-portable.yml), and it uses host producers (build.e2e_artifact_on_host, build.manifest_guests). `--exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap` drops the 3 KVM cells, 2 liteinst cells and 2 in-guest-trap cells, leaving 4 of the twin's 11 result rows at this change, because GitHub-hosted runners have no PMU for the KVM guest clock and no CPUID faulting for the in-guest LiteInst runtime. Otherwise its cells and failure modes are those described for e2e.manifest_applications: a cell whose two runs disagree is a failed result row and fails the node."########,
        labels: &[r########"hosted-portable"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category applications --ci-only --allow-empty --prebuilt --exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap --results "$E2E_RESULT_ROOT/portable/manifest_applications/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_applications/junit.xml""########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"portable"########,
            category: r########"applications"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact"########,
            r########"build.manifest_guests"########,
            r########"gate.manifest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[(r########"manifest_guest"########, 1)],
            est_duration_s: 120.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(3221225472),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"e2e"########,
        job: r########"manifest_bin_c_on_host"########,
        desc: r########"Portable manifest bucket: bin-c"########,
        description: r########"Hosted-portable twin of e2e.manifest_bin_c. It runs the same test-harness selection, `./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category bin-c --ci-only --allow-empty --prebuilt --exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap`, with the same results.jsonl and junit paths. It runs directly on the host, not through ci/hermetic/run-in-pinned-root.sh, so there is no no-network or build-dependency assertion and the cells use the host's own tools and libraries. Its only label is hosted-portable (`./scripts/validate.rs --hosted-portable-only`, run by .github/workflows/ci-portable.yml), and it uses host producers (build.e2e_artifact_on_host, build.manifest_guests). `--exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap` drops nothing, because this bucket has no KVM, liteinst or in-guest-trap cells; all 4 of the twin's result rows remain, because GitHub-hosted runners have no PMU for the KVM guest clock and no CPUID faulting for the in-guest LiteInst runtime. Otherwise its cells and failure modes are those described for e2e.manifest_bin_c: a cell whose two runs disagree is a failed result row and fails the node."########,
        labels: &[r########"hosted-portable"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category bin-c --ci-only --allow-empty --prebuilt --exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap --results "$E2E_RESULT_ROOT/portable/manifest_bin_c/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_bin_c/junit.xml""########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"portable"########,
            category: r########"bin-c"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact"########,
            r########"build.manifest_guests"########,
            r########"gate.manifest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[(r########"manifest_guest"########, 1)],
            est_duration_s: 5.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(3221225472),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"e2e"########,
        job: r########"manifest_c_programs_on_host"########,
        desc: r########"Portable manifest bucket: c-programs"########,
        description: r########"FOLD 2026-09-28 (https://github.com/rrnewton/hermit/issues/3301): the backend-parity-c bucket was folded into c-programs, so this node now also runs the 276 portable cells that e2e.manifest_backend_parity_c ran (713 portable cells in all), and that node is gone. Validation run validate-coord-s15-fdd8f55a8e7c (Hermit fdd8f55a8e7c, 2026-09-28) measured the two nodes at 128.62 s and 75.76 s of wall time (about 204 s together), 844 s and 510 s of CPU time (1354 s together, under the unchanged 7200 s CPU bound), and peaks of 3862880256 and 3859349504 bytes at width 8. The wall bound rises from 600 s to 900 s and the estimate from 300 s to 480 s, the sum of the two former estimates; memory bounds are unchanged because peak memory follows the worker width, which stays 8, not the number of cells. The selector no longer passes --allow-empty: a c-programs node that selects no cells now fails instead of passing having run nothing. WORKER WIDTH measured 2026-08-23: recent 20-way validation runs rotated an identical empty early-Run1 no_result across unrelated ptrace cells, while each affected cell passed in another run. A focused run at eight workers completed all 127 selected strict rows in 264.3s with 127 canonical matches and no no_result, leaving a measured 335.7s margin to the unchanged 600s hang bound. This node reserves all 8 manifest_guest slots and passes the same width to the harness; ordinary Hermit gates may overlap now that their unsupported exclusive resource is removed. Tradeoff: blocking validation still does not exercise the former 20-way manifest pressure; every cell and the strict comparator remain enabled and unchanged. MEMORY measured 2026-08-25 at Hermit 16f70d9994 with the complete current-main artifact and width 8 under ambient load 40-107: five uncapped cgroup peaks were 2855145472-2894295040 bytes; three stricter 4-GiB-cap repetitions completed all 128 cells with peaks up to 3025047552 bytes and no cgroup kill. The 4-GiB baseline rounds above the observed high-water mark; the 6-GiB hard cap preserves nearly 3 GiB of runaway headroom without reserving the former unmeasured 32 GiB."########,
        labels: &[r########"hosted-portable"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category c-programs --ci-only --prebuilt --exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap --results "$E2E_RESULT_ROOT/portable/manifest_c_programs/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_c_programs/junit.xml""########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"portable"########,
            category: r########"c-programs"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact"########,
            r########"build.manifest_guests"########,
            r########"gate.manifest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[(r########"manifest_guest"########, 8)],
            est_duration_s: 480.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(6442450944),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: Some(8),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 900,
        cpu_timeout: 7200,
        jobs_flag: Some(r########"--jobs"########),
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"e2e"########,
        job: r########"manifest_chaos_c_on_host"########,
        desc: r########"Portable manifest bucket: chaos-c"########,
        description: r########"Hosted-portable twin of e2e.manifest_chaos_c. It runs the same test-harness selection, `./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category chaos-c --ci-only --allow-empty --prebuilt --exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap`, with the same results.jsonl and junit paths. It runs directly on the host, not through ci/hermetic/run-in-pinned-root.sh, so there is no no-network or build-dependency assertion and the cells use the host's own tools and libraries. Its only label is hosted-portable (`./scripts/validate.rs --hosted-portable-only`, run by .github/workflows/ci-portable.yml), and it uses host producers (build.e2e_artifact_on_host, build.manifest_guests). `--exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap` drops the 1 KVM cell, leaving 2 of the twin's 3 result rows at this change, because GitHub-hosted runners have no PMU for the KVM guest clock and no CPUID faulting for the in-guest LiteInst runtime. Otherwise its cells and failure modes are those described for e2e.manifest_chaos_c: a cell whose two runs disagree is a failed result row and fails the node."########,
        labels: &[r########"hosted-portable"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category chaos-c --ci-only --allow-empty --prebuilt --exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap --results "$E2E_RESULT_ROOT/portable/manifest_chaos_c/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_chaos_c/junit.xml""########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"portable"########,
            category: r########"chaos-c"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact"########,
            r########"build.manifest_guests"########,
            r########"gate.manifest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[(r########"manifest_guest"########, 1)],
            est_duration_s: 5.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(3221225472),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"e2e"########,
        job: r########"manifest_compat_on_host"########,
        desc: r########"Portable manifest bucket: compat, the strict compatibility corpus on the host"########,
        description: r########"The hosted-portable twin of e2e.manifest_compat: the same 189 cells on the hosted runner, with that runner's own Hermit (build.e2e_artifact_on_host) and the fixtures compatprep.fixtures_on_host prepares. Like every hosted bucket it excludes KVM cells; the corpus has none."########,
        labels: &[r########"hosted-portable"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category compat --ci-only --prebuilt --diagnostic-results --exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap --results "$E2E_RESULT_ROOT/portable/manifest_compat/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_compat/junit.xml""########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"portable"########,
            category: r########"compat"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"compatprep.fixtures_on_host"########,
            r########"gate.manifest"########,
        ],
        env: &[(
            r########"HERMIT_E2E_EMPTY_WORKDIR"########,
            r########"/test"########,
        )],
        hint: HintSpec {
            resources: &[(r########"manifest_guest"########, 8)],
            est_duration_s: 120.0,
            rss_baseline_bytes: Some(4294967296),
            hard_mem_max_bytes: Some(6442450944),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: Some(8),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 1800,
        jobs_flag: Some(r########"--jobs"########),
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"e2e"########,
        job: r########"manifest_data_handling_on_host"########,
        desc: r########"Portable manifest bucket: data-handling"########,
        description: r########"Hosted-portable twin of e2e.manifest_data_handling. It runs the same test-harness selection, `./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category data-handling --ci-only --allow-empty --prebuilt --exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap`, with the same results.jsonl and junit paths. It runs directly on the host, not through ci/hermetic/run-in-pinned-root.sh, so there is no no-network or build-dependency assertion and the cells use the host's own tools and libraries. Its only label is hosted-portable (`./scripts/validate.rs --hosted-portable-only`, run by .github/workflows/ci-portable.yml), and it uses host producers (build.e2e_artifact_on_host, build.manifest_guests). `--exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap` drops the 6 KVM cells, leaving 6 of the twin's 12 result rows at this change, because GitHub-hosted runners have no PMU for the KVM guest clock and no CPUID faulting for the in-guest LiteInst runtime. Otherwise its cells and failure modes are those described for e2e.manifest_data_handling: a cell whose two runs disagree is a failed result row and fails the node."########,
        labels: &[r########"hosted-portable"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category data-handling --ci-only --allow-empty --prebuilt --exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap --results "$E2E_RESULT_ROOT/portable/manifest_data_handling/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_data_handling/junit.xml""########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"portable"########,
            category: r########"data-handling"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact"########,
            r########"build.manifest_guests"########,
            r########"gate.manifest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[(r########"manifest_guest"########, 1)],
            est_duration_s: 90.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(3221225472),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"e2e"########,
        job: r########"manifest_debugger_c_on_host"########,
        desc: r########"Portable manifest bucket: debugger-c"########,
        description: r########"Hosted-portable twin of e2e.manifest_debugger_c. It runs the same test-harness selection, `./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category debugger-c --ci-only --allow-empty --prebuilt --exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap`, with the same results.jsonl and junit paths. It runs directly on the host, not through ci/hermetic/run-in-pinned-root.sh, so there is no no-network or build-dependency assertion and the cells use the host's own tools and libraries. Its only label is hosted-portable (`./scripts/validate.rs --hosted-portable-only`, run by .github/workflows/ci-portable.yml), and it uses host producers (build.e2e_artifact_on_host, build.manifest_guests). `--exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap` drops the 1 KVM cell, 1 liteinst cell and 1 in-guest-trap cell, leaving 3 of the twin's 6 result rows at this change, because GitHub-hosted runners have no PMU for the KVM guest clock and no CPUID faulting for the in-guest LiteInst runtime. Otherwise its cells and failure modes are those described for e2e.manifest_debugger_c: a cell whose two runs disagree is a failed result row and fails the node."########,
        labels: &[r########"hosted-portable"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category debugger-c --ci-only --allow-empty --prebuilt --exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap --results "$E2E_RESULT_ROOT/portable/manifest_debugger_c/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_debugger_c/junit.xml""########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"portable"########,
            category: r########"debugger-c"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact"########,
            r########"build.manifest_guests"########,
            r########"gate.manifest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[(r########"manifest_guest"########, 1)],
            est_duration_s: 5.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(3221225472),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"e2e"########,
        job: r########"manifest_determinism_stress_c_on_host"########,
        desc: r########"Portable manifest bucket: determinism-stress-c"########,
        description: r########"Hosted-portable twin of e2e.manifest_determinism_stress_c. It runs the same test-harness selection, `./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category determinism-stress-c --ci-only --allow-empty --prebuilt --exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap`, with the same results.jsonl and junit paths. It runs directly on the host, not through ci/hermetic/run-in-pinned-root.sh, so there is no no-network or build-dependency assertion and the cells use the host's own tools and libraries. Its only label is hosted-portable (`./scripts/validate.rs --hosted-portable-only`, run by .github/workflows/ci-portable.yml), and it uses host producers (build.e2e_artifact_on_host, build.manifest_guests). `--exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap` drops the 9 KVM cells, 4 liteinst cells and 4 in-guest-trap cells, leaving 17 of the twin's 34 result rows at this change, because GitHub-hosted runners have no PMU for the KVM guest clock and no CPUID faulting for the in-guest LiteInst runtime. Otherwise its cells and failure modes are those described for e2e.manifest_determinism_stress_c: a cell whose two runs disagree is a failed result row and fails the node."########,
        labels: &[r########"hosted-portable"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category determinism-stress-c --ci-only --allow-empty --prebuilt --exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap --results "$E2E_RESULT_ROOT/portable/manifest_determinism_stress_c/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_determinism_stress_c/junit.xml""########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"portable"########,
            category: r########"determinism-stress-c"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact"########,
            r########"build.manifest_guests"########,
            r########"gate.manifest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[(r########"manifest_guest"########, 1)],
            est_duration_s: 5.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(3221225472),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"e2e"########,
        job: r########"manifest_determinism_stress_on_host"########,
        desc: r########"Portable manifest bucket: determinism-stress"########,
        description: r########"Hosted-portable twin of e2e.manifest_determinism_stress. It runs the same test-harness selection, `./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category determinism-stress --ci-only --allow-empty --prebuilt --exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap`, with the same results.jsonl and junit paths. It runs directly on the host, not through ci/hermetic/run-in-pinned-root.sh, so there is no no-network or build-dependency assertion and the cells use the host's own tools and libraries. Its only label is hosted-portable (`./scripts/validate.rs --hosted-portable-only`, run by .github/workflows/ci-portable.yml), and it uses host producers (build.e2e_artifact_on_host, build.manifest_guests). `--exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap` drops the 4 KVM cells, 1 liteinst cell and 1 in-guest-trap cell, leaving 6 of the twin's 12 result rows at this change, because GitHub-hosted runners have no PMU for the KVM guest clock and no CPUID faulting for the in-guest LiteInst runtime. Otherwise its cells and failure modes are those described for e2e.manifest_determinism_stress: a cell whose two runs disagree is a failed result row and fails the node."########,
        labels: &[r########"hosted-portable"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category determinism-stress --ci-only --allow-empty --prebuilt --exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap --results "$E2E_RESULT_ROOT/portable/manifest_determinism_stress/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_determinism_stress/junit.xml""########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"portable"########,
            category: r########"determinism-stress"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact"########,
            r########"build.manifest_guests"########,
            r########"gate.manifest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[(r########"manifest_guest"########, 1)],
            est_duration_s: 150.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(3221225472),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"e2e"########,
        job: r########"manifest_language_runtimes_on_host"########,
        desc: r########"Portable manifest bucket: language-runtimes"########,
        description: r########"Hosted-portable twin of e2e.manifest_language_runtimes. It runs the same test-harness selection, `./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category language-runtimes --ci-only --allow-empty --prebuilt --exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap`, with the same results.jsonl and junit paths. It runs directly on the host, not through ci/hermetic/run-in-pinned-root.sh, so there is no no-network or build-dependency assertion and the cells use the host's own tools and libraries. Its only label is hosted-portable (`./scripts/validate.rs --hosted-portable-only`, run by .github/workflows/ci-portable.yml), and it uses host producers (build.e2e_artifact_on_host, build.manifest_guests). `--exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap` drops the 17 KVM cells, 2 liteinst cells and 2 in-guest-trap cells, leaving 21 of the twin's 42 result rows at this change, because GitHub-hosted runners have no PMU for the KVM guest clock and no CPUID faulting for the in-guest LiteInst runtime. Otherwise its cells and failure modes are those described for e2e.manifest_language_runtimes: a cell whose two runs disagree is a failed result row and fails the node."########,
        labels: &[r########"hosted-portable"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category language-runtimes --ci-only --allow-empty --prebuilt --exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap --results "$E2E_RESULT_ROOT/portable/manifest_language_runtimes/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_language_runtimes/junit.xml""########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"portable"########,
            category: r########"language-runtimes"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact"########,
            r########"build.manifest_guests"########,
            r########"gate.manifest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[(r########"manifest_guest"########, 1)],
            est_duration_s: 120.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(3221225472),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"e2e"########,
        job: r########"manifest_shared_futex_c_on_host"########,
        desc: r########"Portable manifest bucket: shared-futex-c"########,
        description: r########"Hosted-portable twin of e2e.manifest_shared_futex_c. It runs the same test-harness selection, `./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category shared-futex-c --ci-only --allow-empty --prebuilt --exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap`, with the same results.jsonl and junit paths. It runs directly on the host, not through ci/hermetic/run-in-pinned-root.sh, so there is no no-network or build-dependency assertion and the cells use the host's own tools and libraries. Its only label is hosted-portable (`./scripts/validate.rs --hosted-portable-only`, run by .github/workflows/ci-portable.yml), and it uses host producers (build.e2e_artifact_on_host, build.manifest_guests). `--exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap` drops the 1 KVM cell, 1 liteinst cell and 1 in-guest-trap cell, leaving 2 of the twin's 5 result rows at this change, because GitHub-hosted runners have no PMU for the KVM guest clock and no CPUID faulting for the in-guest LiteInst runtime. Otherwise its cells and failure modes are those described for e2e.manifest_shared_futex_c: a cell whose two runs disagree is a failed result row and fails the node."########,
        labels: &[r########"hosted-portable"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category shared-futex-c --ci-only --allow-empty --prebuilt --exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap --results "$E2E_RESULT_ROOT/portable/manifest_shared_futex_c/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_shared_futex_c/junit.xml""########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"portable"########,
            category: r########"shared-futex-c"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact"########,
            r########"build.manifest_guests"########,
            r########"gate.manifest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[(r########"manifest_guest"########, 1)],
            est_duration_s: 5.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(3221225472),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"e2e"########,
        job: r########"manifest_system_utils_on_host"########,
        desc: r########"Portable manifest bucket: system-utils"########,
        description: r########"WORKER-WIDTH DERIVATION 2026-08-24: a current-main sweep of the 28 ptrace cells found 0 failures at --jobs 1 and 2, then rotating failures at every wider sampled setting: 2 at 8, 2 at 16, 6 at 28, and 3 at 64; system-utils/mktemp-name failed in the 28- and 64-wide buckets and passed alone at width 1. The counts are intentionally not treated as monotonic or stable; the durable fact is that widening manufactures contention-sensitive reds while the production width of 1 is clean. Keep --jobs 1 explicit until the cells are made concurrency-safe; system-utils/harness-width-contract reads the exact parsed worker capacity and fails if this control changes without a new measurement."########,
        labels: &[r########"hosted-portable"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category system-utils --ci-only --allow-empty --prebuilt --exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap --jobs 1 --results "$E2E_RESULT_ROOT/portable/manifest_system_utils/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_system_utils/junit.xml""########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"portable"########,
            category: r########"system-utils"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact"########,
            r########"build.manifest_guests"########,
            r########"gate.manifest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[(r########"manifest_guest"########, 1)],
            est_duration_s: 120.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(3221225472),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: Some(1),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: Some(r########""########),
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"e2e"########,
        job: r########"manifest_util_c_on_host"########,
        desc: r########"Portable manifest bucket: util-c"########,
        description: r########"Hosted-portable twin of e2e.manifest_util_c. It runs the same test-harness selection, `./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category util-c --ci-only --allow-empty --prebuilt --exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap`, with the same results.jsonl and junit paths. It runs directly on the host, not through ci/hermetic/run-in-pinned-root.sh, so there is no no-network or build-dependency assertion and the cells use the host's own tools and libraries. Its only label is hosted-portable (`./scripts/validate.rs --hosted-portable-only`, run by .github/workflows/ci-portable.yml), and it uses host producers (build.e2e_artifact_on_host, build.manifest_guests). `--exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap` drops the 1 KVM cell, 1 liteinst cell and 1 in-guest-trap cell, leaving 2 of the twin's 5 result rows at this change, because GitHub-hosted runners have no PMU for the KVM guest clock and no CPUID faulting for the in-guest LiteInst runtime. Otherwise its cells and failure modes are those described for e2e.manifest_util_c: a cell whose two runs disagree is a failed result row and fails the node."########,
        labels: &[r########"hosted-portable"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category util-c --ci-only --allow-empty --prebuilt --exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap --results "$E2E_RESULT_ROOT/portable/manifest_util_c/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_util_c/junit.xml""########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"portable"########,
            category: r########"util-c"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"build.e2e_artifact"########,
            r########"build.manifest_guests"########,
            r########"gate.manifest"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[(r########"manifest_guest"########, 1)],
            est_duration_s: 5.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(3221225472),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"scorecard"########,
        job: r########"compatibility_on_host"########,
        desc: r########"Verify fresh per-cell results and print the compatibility table"########,
        description: r########"The GitHub-hosted copy of scorecard.compatibility: the same `scorecard.rs verify-results --lanes portable` check, run on the host after the thirteen e2e.manifest_*_on_host buckets, with `--exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap` added. Hosted runners expose /dev/kvm through nested virtualization but have no PMU, so KVM guests fail when their clock opens the retired-branch counter, and they have no CPUID faulting, so the in-guest LiteInst runtime of the liteinst and in-guest-trap backends refuses to start; the profile drops those cells, and the scorecard prints them on an "Omitted by --exclude-backend" line instead of counting them. Every other selected portable cell still needs one fresh PASS at HEAD from one Hermit binary and one run per cell. A bucket that wrote no row for a cell fails it as missing; if the plan ever selects no KVM cell, `--exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap matches no selected cell in the named lanes` turns it red rather than silently excluding nothing."########,
        labels: &[r########"hosted-portable"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/compat-envelope/scorecard.rs verify-results --results "$E2E_RESULT_ROOT" --lanes portable --exclude-backend kvm --exclude-backend liteinst --exclude-backend in-guest-trap"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[
            r########"e2e.manifest_applications_on_host"########,
            r########"e2e.manifest_bin_c_on_host"########,
            r########"e2e.manifest_c_programs_on_host"########,
            r########"e2e.manifest_chaos_c_on_host"########,
            r########"e2e.manifest_compat_on_host"########,
            r########"e2e.manifest_data_handling_on_host"########,
            r########"e2e.manifest_debugger_c_on_host"########,
            r########"e2e.manifest_determinism_stress_on_host"########,
            r########"e2e.manifest_determinism_stress_c_on_host"########,
            r########"e2e.manifest_language_runtimes_on_host"########,
            r########"e2e.manifest_shared_futex_c_on_host"########,
            r########"e2e.manifest_system_utils_on_host"########,
            r########"e2e.manifest_util_c_on_host"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(1073741824),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 120,
        cpu_timeout: 120,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"pre"########,
        job: r########"reverie_pin_on_host"########,
        desc: r########"Reverie pin consistency"########,
        description: r########"Same command as pre.reverie_pin (`ci/run-reverie-pin-check.sh --repo "$PWD"`, through with-proxy when it exists), run by the hosted-privileged profile on the self-hosted runner. scripts/check-reverie-pin.rs requires every Reverie revision in tracked Cargo.toml and Cargo.lock files, the LiteInst cache keys, the DBT budget bindings and hermit-cli/BUCK to name one commit that is an ancestor of Reverie main and not behind the pin on origin/main; scripts/check-git-pin-uniformity.rs refuses any git dependency or gitlink at two revisions. Unlike pre.reverie_pin it has no pre.submodules dependency and a 120-second wall cap, and all ten hosted-privileged nodes depend on it. An older Reverie in Cargo.lock prints "REVERIE PIN REGRESSION - BLOCKED"."########,
        labels: &[r########"hosted-privileged"########],
        cmd: crate::validation_dag::PIN_GATE_COMMAND,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(2147483648),
            hard_mem_max_bytes: Some(2147483648),
            classification: StepClass::Light,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 120,
        cpu_timeout: 300,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"build"########,
        job: r########"rust_scripts_on_host"########,
        desc: r########"Build every tracked rust-script before graph consumers run"########,
        description: RUST_SCRIPT_PRODUCER_DESCRIPTION,
        labels: &[r########"hosted-privileged"########],
        cmd: RUST_SCRIPT_PRODUCER_COMMAND,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"pre.reverie_pin_on_host"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 190.0,
            rss_baseline_bytes: Some(RUST_SCRIPT_PRODUCER_RSS_BASELINE_BYTES),
            hard_mem_max_bytes: Some(RUST_SCRIPT_PRODUCER_HARD_MEM_MAX_BYTES),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: Some(RUST_SCRIPT_PRODUCER_INNER_JOBS),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: RUST_SCRIPT_PRODUCER_WALL_SECONDS,
        cpu_timeout: 7200,
        jobs_flag: Some(r########""########),
        jobs_env: Some(r########"CARGO_BUILD_JOBS"########),
    },
    StaticStepSpec {
        group: r########"setup"########,
        job: r########"manifest_plan_on_host"########,
        desc: r########"Build the manifest-plan binaries the metadata validation runs"########,
        description: r########"Same command as setup.manifest_plan: `cargo build -p hermit-manifest-plan --bins` in the dev profile, building test-harness, generate-validation-dag, generate-test-footprints and the other manifest tools, here on the hosted-privileged runner after build.rust_scripts_on_host. gate.manifest_on_host, privileged-build.manifest_guests_on_host and the three privileged-only-e2e.*_on_host buckets run the target/debug/test-harness it builds, which resolves the repository from its compile-time CARGO_MANIFEST_DIR, so it must be built in this checkout. It declares no inner width and a 180-second wall cap, unlike the local twin's width 8 and 300 seconds, whose measurements do not apply. A compile error in ci/manifest-plan stops every hosted-privileged node except the pin check."########,
        labels: &[r########"hosted-privileged"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; cargo build -p hermit-manifest-plan --bins"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"build.rust_scripts_on_host"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 60.0,
            rss_baseline_bytes: Some(2147483648),
            hard_mem_max_bytes: Some(2147483648),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 180,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: Some(r########"CARGO_BUILD_JOBS"########),
    },
    StaticStepSpec {
        group: r########"gate"########,
        job: r########"manifest_on_host"########,
        desc: r########"Centralized test manifest and inventory"########,
        description: MANIFEST_GATE_DESCRIPTION,
        labels: &[r########"hosted-privileged"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; target/debug/test-harness validate"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"setup.manifest_plan_on_host"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 0.0,
            rss_baseline_bytes: Some(5368709120),
            hard_mem_max_bytes: Some(5368709120),
            classification: StepClass::Light,
            preferred_inner_jobs: Some(MANIFEST_GATE_INNER_JOBS),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 180,
        cpu_timeout: 600,
        jobs_flag: Some(r########""########),
        jobs_env: Some(r########"CARGO_BUILD_JOBS"########),
    },
    StaticStepSpec {
        group: r########"privileged-only-build"########,
        job: r########"privileged_tests_on_host"########,
        desc: r########"Build Hermit and the focused test binaries used by the privileged lane"########,
        description: r########"MEM-CAP DERIVATION 2026-08-04 (follow-on to task memory-caps-must-scale-with-job-count / #1583, which MISSED this privileged-lane build node): pinned_jobs=8 literal via CARGO_BUILD_JOBS=8 on all three Cargo commands (was unset -> cargo defaulted build phases to nproc; that cc1plus fan-out is the OOM class the pin bounds; the job-pin is the actual fix). The final command prebuilds the exact cli and hermit_modes integration binaries consumed by the privileged test nodes, so their wall budgets measure test execution instead of Cargo compilation or target-lock waiting. After the bin build, the content-addressed publisher verifies source type/mode/size, hashes before and after copying, verifies the published hash and atomically updates the pointer before any later Cargo invocation can relink target/debug/hermit. This privileged lane deliberately publishes a binary-only artifact; unlike portable DBT/SaBRe/LiteInst cells, it does not consume install_pkg resources. Cap KEPT at the existing generous 8.0GiB: a warm/incremental measurement read peak_bytes~1.64GiB @j8, but that reused build.workspace artifacts and UNDER-estimates a cold from-scratch privileged build, so the 8.0GiB headroom is retained rather than tightened on an unreliable warm figure. rss_baseline=5.0GiB (scheduling reservation). hermit@b384187e."########,
        labels: &[r########"hosted-privileged"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; CARGO_BUILD_JOBS=8 cargo build -p hermit --features third-party-backends --bin hermit && ./ci/publish-hermit-e2e-artifact.sh target/debug/hermit target/ci/hermit-e2e-artifacts target/ci/hermit-e2e-artifact.path && ./ci/nextest-binaries.rs prepare privileged"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"gate.manifest_on_host"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 60.0,
            rss_baseline_bytes: Some(5368709120),
            hard_mem_max_bytes: Some(8589934592),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 120,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"privileged-only-cpuid"########,
        job: r########"faulting_on_host"########,
        desc: r########"CPUID-faulting smoke: Detcore masks host RDRAND/RDSEED feature bits"########,
        description: r########"REQUIRES A MACHINE FACILITY, DECLARED 2026-08-12 (hermit#2135, hermit#2148, hermit#2205). rdrand_rdseed_is_masked can only observe Detcore masking host feature bits if the kernel can trap the guest's CPUID, which needs arch_prctl(ARCH_SET_CPUID, 0) to succeed. Where it cannot, this node used to FAIL in 0.11 s with exit 101 and an empty detail block -- indistinguishable from a broken build -- and its eager-exit aborted the twelve other in-flight nodes and filtered twenty-seven more, so a machine that runs 31 of 33 nodes in 3m22s produced no receipt at all. `requires_host_capability` moves that judgement OUT of the node, to an out-of-band probe run during plan construction, and makes the outcome a third recorded state: host-inapplicable, which is neither a pass nor a failure and is written to the ledger as a typed intentional skip. THIS DOES NOT WEAKEN THE TEST. Where the capability is present the node runs unchanged and its assertions keep full force; the probe fails closed toward running, so a probe error or an unexpected errno still runs it. The capability name is checked against the closed vocabulary in scripts/lib/validate_plan.rs::HostCapability, and an unknown name refuses the whole run."########,
        labels: &[r########"hosted-privileged"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; tests_misc="$(./ci/nextest-binaries.rs executable hermit-detcore tests_misc)" || exit 1; status=0; timeout --kill-after=5s 30s "$tests_misc" rdrand_rdseed_is_masked --exact || status=$?; if [ "$status" -eq 124 ] || [ "$status" -eq 137 ]; then printf 'test hermit-detcore/tests_misc::rdrand_rdseed_is_masked exceeded 30 s (innermost exact test timeout: exit %s)\n' "$status" >&2; fi; exit "$status""########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"privileged-only-build.privileged_tests_on_host"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 15.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(17179869184),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 40,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"privileged-only-pmu"########,
        job: r########"preemption_on_host"########,
        desc: r########"PMU smoke: retired-conditional-branch overflow delivery and skid measurement"########,
        description: r########"The hosted-privileged copy of privileged-only-pmu.preemption, run by `ci/run-dag.sh privileged` on the self-hosted runner labelled pmu: the same `cc -O2 -Wall -Wextra -Werror tests/util/pmu_skid.c` and `timeout 20 target/ci-pmu-skid --iterations 16 --period 100000`, but directly on the runner host with its own compiler instead of in the pinned root. The program requires each of 16 overflows of 100000 retired conditional branches to stop a traced, CPU-pinned child mid-loop with SIGUSR1, and prints the skid without bounding it. privileged-only-test.pmu_buck_chaos_cases_on_host depends on it. A runner whose kernel refuses perf_event_open, a virtual machine without a usable PMU, or no overflow signal within 20 seconds turns it red."########,
        labels: &[r########"hosted-privileged"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; cc -O2 -Wall -Wextra -Werror tests/util/pmu_skid.c -o target/ci-pmu-skid && timeout 20 target/ci-pmu-skid --iterations 16 --period 100000"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"privileged-only-build.privileged_tests_on_host"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 5.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(17179869184),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 30,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"privileged-only-test"########,
        job: r########"pmu_buck_chaos_cases_on_host"########,
        desc: r########"Run the six measured-passing Buck chaos cases under PMU preemption"########,
        description: r########"The six enabled cases passed direct measurement on the privileged host. chaos_buck_nanosleep_parallel remains ignored because an interrupted nanosleep reports EINTR; chaos_buck_mem_race remains ignored because it produced no verdict within 300 seconds. Both remain visible in super.pmu_buck_chaos_cases. Depending on pmu.preemption keeps this coverage in the existing privileged PMU partition without changing quick or portable-only."########,
        labels: &[r########"hosted-privileged"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; set -uo pipefail; status=0; ./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit --features third-party-backends --test hermit_modes -j 1 -E 'test(/^(chaos_buck_getpid|chaos_buck_uname|chaos_buck_sysinfo|chaos_buck_wait_on_child|chaos_buck_clone|chaos_buck_hello_alarm)$/)' || status=$?; exit "$status""########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: Some(&[r########"hermit_modes"########]),
        deps: &[r########"privileged-only-pmu.preemption_on_host"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 45.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(17179869184),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 120,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"privileged-build"########,
        job: r########"manifest_guests_on_host"########,
        desc: r########"Prepare every CI-enabled privileged manifest guest"########,
        description: r########"Same command as privileged-build.manifest_guests, run on the host by the hosted-privileged profile: `test-harness build --lane privileged --ci-only --allow-empty` prepares the privileged lane's CI test programs once each under $E2E_BUILD_ROOT/<test-id>, compiling tests/c/cpuid_probe.c with -Werror and running the shell programs' --prepare steps (sysfs-sanitized-prefixes.sh runs its fixture self-test there). Unlike the local host copy, this one has dependents: the three hosted buckets privileged-only-e2e.manifest_{applications,c_programs,system_utils}_on_host run against what it builds. A new compiler warning in cpuid_probe.c, or a failing --prepare self-test, stops it and those buckets."########,
        labels: &[r########"hosted-privileged"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; target/debug/test-harness build --lane privileged --ci-only --allow-empty"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        deps: &[r########"gate.manifest_on_host"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 75.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(3221225472),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: Some(3),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 120,
        cpu_timeout: 7200,
        jobs_flag: Some(r########"--jobs"########),
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"privileged-only-e2e"########,
        job: r########"manifest_applications_on_host"########,
        desc: r########"Privileged manifest bucket: applications"########,
        description: r########"The selected KVM cell has a measured 74 s wall backstop and may retry once. The shared 600 s E2E-class node bound remains a generous backup after per-machine scaling, while the typed inner CPU/wall policy identifies the actual stop."########,
        labels: &[r########"hosted-privileged"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh target/debug/test-harness run --lane privileged --category applications --ci-only --allow-empty --prebuilt --results "$E2E_RESULT_ROOT/privileged/manifest_applications/results.jsonl" --junit "$E2E_RESULT_ROOT/privileged/manifest_applications/junit.xml""########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"privileged"########,
            category: r########"applications"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"gate.manifest_on_host"########,
            r########"privileged-build.manifest_guests_on_host"########,
            r########"privileged-only-build.privileged_tests_on_host"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 90.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(17179869184),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"privileged-only-e2e"########,
        job: r########"manifest_c_programs_on_host"########,
        desc: r########"Privileged manifest bucket: c-programs"########,
        description: r########"The selected cells are the five cpuid-probe verify cells (dbt, in-guest-trap, kvm, liteinst, ptrace): the kvm and ptrace cells that the privileged backend-parity-c node ran before that bucket was folded into c-programs (https://github.com/rrnewton/hermit/issues/3301), the dbt cell that slice S13 of the same issue carried over from the retired DBT parity matrix, and the in-guest LiteInst cells (liteinst, and in-guest-trap with site patching off) that returned on the new architecture after the LiteInst reset (https://github.com/rrnewton/hermit/issues/3745), each run with the maximum timeslice disabled. The selector no longer passes --allow-empty, so selecting no cells fails. They use the 57 s wall backstop and may retry once. The shared 600 s E2E-class node bound remains a generous backup after per-machine scaling, while the typed inner CPU/wall policy identifies the actual stop."########,
        labels: &[r########"hosted-privileged"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh target/debug/test-harness run --lane privileged --category c-programs --ci-only --prebuilt --results "$E2E_RESULT_ROOT/privileged/manifest_c_programs/results.jsonl" --junit "$E2E_RESULT_ROOT/privileged/manifest_c_programs/junit.xml""########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"privileged"########,
            category: r########"c-programs"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"gate.manifest_on_host"########,
            r########"privileged-build.manifest_guests_on_host"########,
            r########"privileged-only-build.privileged_tests_on_host"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 5.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(17179869184),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"privileged-only-e2e"########,
        job: r########"manifest_system_utils_on_host"########,
        desc: r########"Privileged manifest bucket: system-utils"########,
        description: r########"HOST-REQUIREMENT ROUTING 2026-09-28: system-utils/sysfs-sanitized-prefixes is the only cell in this privileged bucket. Its guest reads one live leaf under each of eight host sysfs prefixes (block, hwmon, rtc, node, btrfs, irq, uevent, module) and refuses when a prefix has no readable leaf, so it needs a host that has that hardware and a mounted btrfs. GitHub-hosted run https://github.com/rrnewton/hermit/actions/runs/36485831200 failed it with "hwmon has no readable sanitized leaf". The manifest entry is therefore lane: privileged, which keeps it out of the hosted-portable selection while the full profile still runs it. Wall evidence from the ledger series of the development host recorded in docs/TESTING_ENVIRONMENTS.md, "Named measurement hosts" (2026-08..09): verify/ptrace 148 samples, per-run median 2096 ms, max 15438 ms; verify/kvm 29 samples, median 2316 ms, max 3467 ms. Memory hints match the portable system-utils bucket, whose 3 GiB hard cap already contained both cells at --jobs 1. The shared 600 s E2E-class node bound is the backstop."########,
        labels: &[r########"hosted-privileged"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh target/debug/test-harness run --lane privileged --category system-utils --ci-only --allow-empty --prebuilt --jobs 1 --results "$E2E_RESULT_ROOT/privileged/manifest_system_utils/results.jsonl" --junit "$E2E_RESULT_ROOT/privileged/manifest_system_utils/junit.xml""########,
        cmdtype: CmdType::Unknown,
        manifest: Some(ManifestSpec {
            lane: r########"privileged"########,
            category: r########"system-utils"########,
            test: None,
            mode: None,
            backend: None,
            label: None,
        }),
        integration_test_binaries: None,
        deps: &[
            r########"gate.manifest_on_host"########,
            r########"privileged-build.manifest_guests_on_host"########,
            r########"privileged-only-build.privileged_tests_on_host"########,
        ],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 30.0,
            rss_baseline_bytes: Some(1073741824),
            hard_mem_max_bytes: Some(3221225472),
            classification: StepClass::LatencyBound,
            preferred_inner_jobs: Some(1),
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 600,
        cpu_timeout: 7200,
        jobs_flag: Some(r########""########),
        jobs_env: None,
    },
    StaticStepSpec {
        group: r########"privileged-only-test"########,
        job: r########"cli_kvm_on_host"########,
        desc: r########"KVM CLI tests and initialized-VM setup cleanup"########,
        description: r########"Runs all 55 KVM-specific hermit CLI tests and the initialized-VM setup cleanup control after requiring /dev/kvm. The inventory gate requires all 55 distinct, nonignored CLI tests including the self-SIGKILL, synchronous-fault, root-exit reparenting, exec timer, nonleader-exec refusal and unreadable futex word regressions and exactly the named setup test. The portable lane continues to skip run_kvm_ because these tests self-guard without /dev/kvm and would otherwise report silent passes. KVM consumers may overlap: /dev/kvm supports multiple concurrent guests, and no repository or host constraint establishes it as an exclusive resource."########,
        labels: &[r########"hosted-privileged"########],
        cmd: r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; set -uo pipefail; if ! exec 9<>/dev/kvm; then printf 'test.cli_kvm: /dev/kvm could not be opened, so the selected run_kvm_ tests would self-guard and report silent passes. Refusing rather than reporting a green that measured nothing.\n' >&2; exit 1; fi; exec 9<&-; log=$(mktemp); trap 'rm -f "$log"' EXIT; if ! ./ci/nextest-binaries.rs list ${CI:+--profile ci} -p hermit --features third-party-backends,kvm-execution-tests --lib --test cli -E 'test(/^run_kvm_/) | test(=kvm_execution_tests::initialized_vm_setup_failures_consume_detcore_state_without_further_guest_execution)' --message-format json >"$log"; then exit 1; fi; if ! jq -e '[."rust-suites"[] | .testcases | to_entries[] | select(.value."filter-match".status == "matches")] as $selected | ($selected | length == 56) and ([$selected[].key] | unique | length == 56) and ($selected | all(.value.ignored == false)) and ([$selected[] | select(.key == "run_kvm_self_sigkill_from_nonleader_is_group_fatal")] | length == 1) and ([$selected[] | select(.key == "run_kvm_synchronous_root_segv_preserves_guest_exit")] | length == 1) and ([$selected[] | select(.key == "run_kvm_synchronous_orphan_segv_preserves_root_success")] | length == 1) and ([$selected[] | select(.key == "run_kvm_root_exit_reparents_live_child_and_grandchild")] | length == 1) and ([$selected[] | select(.key == "run_kvm_exec_deletes_posix_timers_and_preserves_itimer")] | length == 1) and ([$selected[] | select(.key == "run_kvm_nonleader_exec_is_policy_refusal")] | length == 1) and ([$selected[] | select(.key == "run_kvm_futex_wait_on_an_unreadable_word_matches_linux")] | length == 1) and ([$selected[] | select(.key | startswith("run_kvm_"))] | length == 55) and ([$selected[] | select(.key == "kvm_execution_tests::initialized_vm_setup_failures_consume_detcore_state_without_further_guest_execution")] | length == 1)' "$log" >/dev/null; then printf 'test.cli_kvm: expected exactly 55 distinct, nonignored run_kvm_ tests including self-SIGKILL, both synchronous-fault regressions, root-exit reparenting, exec timer lifetime, nonleader-exec refusal, the unreadable futex word, and the named setup test; the inventory changed. Update the tests and this gate together.\n' >&2; exit 1; fi; ./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit --features third-party-backends,kvm-execution-tests --lib --test cli -j 1 -E 'test(/^run_kvm_/) | test(=kvm_execution_tests::initialized_vm_setup_failures_consume_detcore_state_without_further_guest_execution)'; exit $?"########,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: Some(&[r########"cli"########]),
        deps: &[r########"privileged-only-build.privileged_tests_on_host"########],
        env: &[],
        hint: HintSpec {
            resources: &[],
            est_duration_s: 60.0,
            rss_baseline_bytes: Some(8589934592),
            hard_mem_max_bytes: Some(17179869184),
            classification: StepClass::CpuBound,
            preferred_inner_jobs: None,
            measured_effective_cores: None,
            measured_cpu_utilization: None,
        },
        networkonly: false,
        engine_only: false,
        timeout: 180,
        cpu_timeout: 7200,
        jobs_flag: None,
        jobs_env: None,
    },
];
