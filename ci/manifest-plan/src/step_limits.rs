// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.

// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Every validation node is bounded by CPU, with its wall bound a hang backstop.
//!
//! OWNER RULE (2026-11-01): a node's CPU bound is about 1.5 times the CPU it
//! has been measured to use, and its wall bound is at most 8 times the CPU
//! bound and at most 600 seconds, unless the node documents why it needs
//! longer. A wall bound fires on host load, a CPU bound does not. On
//! 2026-11-02, four to six concurrent full validations on one 316-core host
//! failed the same nodes on WALL bounds alone: the privileged PMU and CPUID
//! smokes at 30 and 40 seconds with one or two CPU-seconds used, the check.*
//! nodes at 120 seconds, test.detcore_misc 22 times at 720 seconds,
//! doc.rustdoc at 900 seconds and e2e.buck_stage at 1800. Each of those nodes
//! declared a 7200-second CPU bound it could never reach before its wall bound
//! fired, so in practice it had a wall bound only.
//!
//! The bounds were recalibrated from the 999 dagrun journals retained under
//! the dev-hermit parent's ignored/validate/artifacts on 2026-11-02, covering
//! the 30 days before that date (step_end cpu_usage_usec and wall_elapsed_s).
//! For each node with at least 20 passing runs:
//! * CPU = the next bucket (5 s to 60, 10 s to 300, 30 s to 600, 60 s to 1200,
//!   then 300 s) above max(30, 1.5 times the largest CPU of a passing run or a
//!   CPU kill). The factor is 3 for a node that also runs on GitHub-hosted
//!   runners (a hosted twin or the hosted-portable label), whose slower
//!   hardware is not measured. A CPU bound already between that and twice it
//!   was kept.
//! * wall = min(8 x CPU, 600) when that covers 1.5 times the largest wall of a
//!   passing run or a WALL kill; otherwise the bucket above that (an entry in
//!   [`WALL_CEILING_EXCEPTIONS`]). A hosted-shared node's wall is never lowered.
//!
//! A manifest bucket node's wall also stays above its cells' retry windows
//! (scripts/validate.rs `require_manifest_selection_headroom`: two attempts
//! of the largest cell wall at the 1.5 wall multiplier plus grace, 560 s for
//! the default 180-second cell), so none was lowered below 600; a nextest
//! node's stays above one 1.5 x 180 s cap plus grace (272 s).
//!
//! [`assert_cpu_bounded_step_limits`] holds every node to the rule, so a new
//! node cannot be given a wall bound alone.

use dagrun::model::DagConfig;
use dagrun::model::Step;

/// A wall bound may be at most this many times the node's CPU bound.
pub const WALL_TO_CPU_MAX: i64 = 8;
/// A wall bound above this needs an entry in [`WALL_CEILING_EXCEPTIONS`].
pub const WALL_CEILING_SECONDS: i64 = 600;

/// Nodes whose wall bound is above [`WALL_CEILING_SECONDS`], each with the
/// measurement or contract that needs it.
pub(crate) const WALL_CEILING_EXCEPTIONS: &[(&str, &str)] = &[
    (
        "pre.submodules",
        "also runs on GitHub-hosted runners, whose wall is unmeasured, so its wall was not lowered (local: 691 runs, largest wall 26 s)",
    ),
    (
        "pre.reverie_pin",
        "also runs on GitHub-hosted runners, whose wall is unmeasured, so its wall was not lowered (local: 684 runs, largest wall 27 s)",
    ),
    (
        "gate.manifest",
        "also runs on GitHub-hosted runners, whose wall is unmeasured, so its wall was not lowered (local: 667 runs, largest wall 9 s)",
    ),
    (
        "build.rust_scripts",
        "RUST_SCRIPT_PRODUCER_WALL_SECONDS: the rust-script producer contract (assert_rust_script_producer_contract) and the quick and privileged critical paths are built from it",
    ),
    (
        "check.lint_checks",
        "571 passing runs to 2026-11-02 peaked at a 587-second wall; 1.5 times that needs 881 s",
    ),
    (
        "check.script_unit_tests",
        "2 WALL kills at up to 901 s under concurrent validations to 2026-11-02 (553 passing runs, largest wall 896 s); 1.5 times the kill wall needs 1352 s",
    ),
    (
        "build.buck_release_artifact",
        "also runs on GitHub-hosted runners, whose wall is unmeasured, so its wall was not lowered (local: 630 runs, largest wall 3 s)",
    ),
    (
        "e2e.manifest_c_programs",
        "61 passing runs to 2026-11-02 peaked at a 707-second wall; 1.5 times that needs 1061 s",
    ),
    (
        "e2e.manifest_language_runtimes",
        "71 passing runs to 2026-11-02 peaked at a 515-second wall; 1.5 times that needs 772 s",
    ),
    (
        "e2e.manifest_system_utils",
        "1 WALL kills at up to 606 s under concurrent validations to 2026-11-02 (74 passing runs, largest wall 564 s); 1.5 times the kill wall needs 908 s",
    ),
    (
        "lint.clippy",
        "580 passing runs to 2026-11-02 peaked at a 485-second wall; 1.5 times that needs 727 s",
    ),
    (
        "doc.doctests",
        "603 passing runs to 2026-11-02 peaked at a 446-second wall; 1.5 times that needs 669 s",
    ),
    (
        "doc.rustdoc",
        "5 WALL kills at up to 906 s under concurrent validations to 2026-11-02 (577 passing runs, largest wall 896 s); 1.5 times the kill wall needs 1358 s",
    ),
    (
        "test.regular_crates",
        "487 passing runs to 2026-11-02 peaked at a 453-second wall; 1.5 times that needs 679 s",
    ),
    (
        "test.hermit_unit",
        "also runs on GitHub-hosted runners, whose wall is unmeasured, so its wall was not lowered (local: 544 runs, largest wall 389 s)",
    ),
    (
        "test.detcore_unit",
        "559 passing runs to 2026-11-02 peaked at a 440-second wall; 1.5 times that needs 659 s",
    ),
    (
        "test.detcore_misc",
        "6 WALL kills at up to 726 s under concurrent validations to 2026-11-02 (536 passing runs, largest wall 473 s); 1.5 times the kill wall needs 1088 s",
    ),
    (
        "test.detcore_parallel",
        "also runs on GitHub-hosted runners, whose wall is unmeasured, so its wall was not lowered (local: 578 runs, largest wall 180 s)",
    ),
    (
        "test.detcore_time",
        "also runs on GitHub-hosted runners, whose wall is unmeasured, so its wall was not lowered (local: 578 runs, largest wall 197 s)",
    ),
    (
        "test.hermit_integration",
        "also runs on GitHub-hosted runners, whose wall is unmeasured, so its wall was not lowered (local: 569 runs, largest wall 340 s)",
    ),
    (
        "test.hermit_integration_2",
        "a shard of test.hermit_integration, whose bounds every shard copies (materialize_node_shards)",
    ),
    (
        "test.hermit_integration_3",
        "a shard of test.hermit_integration, whose bounds every shard copies (materialize_node_shards)",
    ),
    (
        "test.arbitrary_binaries",
        "also runs on GitHub-hosted runners, whose wall is unmeasured, so its wall was not lowered (local: 576 runs, largest wall 210 s)",
    ),
    (
        "test.cli",
        "463 passing runs to 2026-11-02 peaked at a 428-second wall; 1.5 times that needs 642 s",
    ),
    (
        "test.hermit_modes",
        "also runs on GitHub-hosted runners, whose wall is unmeasured, so its wall was not lowered (local: 578 runs, largest wall 137 s)",
    ),
    (
        "test.app_strict_verify",
        "also runs on GitHub-hosted runners, whose wall is unmeasured, so its wall was not lowered (local: 578 runs, largest wall 184 s)",
    ),
    (
        "test.command_strict_verify",
        "also runs on GitHub-hosted runners, whose wall is unmeasured, so its wall was not lowered (local: 577 runs, largest wall 169 s)",
    ),
    (
        "test.ignored_syscall_regressions",
        "also runs on GitHub-hosted runners, whose wall is unmeasured, so its wall was not lowered (local: 577 runs, largest wall 221 s)",
    ),
    (
        "test.rr_suite_contract",
        "also runs on GitHub-hosted runners, whose wall is unmeasured, so its wall was not lowered (local: 571 runs, largest wall 258 s)",
    ),
    (
        "test.envelope_levels",
        "also runs on GitHub-hosted runners, whose wall is unmeasured, so its wall was not lowered (local: 578 runs, largest wall 230 s)",
    ),
    (
        "selftest.scorecard_commands",
        "also runs on GitHub-hosted runners, whose wall is unmeasured, so its wall was not lowered (local: 602 runs, largest wall 395 s)",
    ),
    (
        "selftest.validate_rs",
        "its submodule fixture child, which this node's CPU bound covers, has a 780-second hang backstop (scripts/validate.rs SUBMODULE_FIXTURE_CHILD_SECONDS); at 300 it was killed at least five times under concurrent validations on 2026-11-02",
    ),
    (
        "selftest.pressure_test",
        "also runs on GitHub-hosted runners, whose wall is unmeasured, so its wall was not lowered (local: 590 runs, largest wall 295 s)",
    ),
    (
        "e2e.buck_stage",
        "2 WALL kills at its former 1800-second bound under concurrent validations to 2026-11-02 (499 passing runs, largest wall 1257 s); 2700 s is 1.5 times that bound, the most PREPARATION_RESERVE_S allows",
    ),
    (
        "e2e.buck_cells",
        "the Buck cell runner: 467 runs to 2026-11-02 peaked at a 1495-second wall, so 1.5 times that needs 2243 seconds",
    ),
    (
        "build.rust_scripts_in_pinned_root",
        "RUST_SCRIPT_PRODUCER_WALL_SECONDS, shared with build.rust_scripts (assert_rust_script_producer_contract)",
    ),
    (
        "build.workspace_compile_in_pinned_root",
        "WORKSPACE_WALL_SECONDS, checked by assert_pinned_workspace_split; its cold-build overrun is fixed in the warm target cache (dev-hermit f854b82c), not by a bigger cap",
    ),
    (
        "build.liteinst_allocator_fixtures_in_pinned_root",
        "594 passing runs to 2026-11-02 peaked at a 540-second wall; 1.5 times that needs 810 s",
    ),
];

/// Families with no retained measurement whose bounds predate the rule. Each
/// still holds at least one node the rule would refuse; when none is left the
/// family's entry must go.
pub(crate) const UNMEASURED_FAMILIES: &[(Family, &str)] = &[
    (
        Family::TagSuffix(crate::validation_dag::HOSTED_VARIANT_SUFFIX),
        "GitHub-hosted twins: dispatch-only, on unmeasured runner hardware; each copies its local node's bounds or carries its own hosted contract",
    ),
    (
        Family::GroupPrefix("privileged-only-"),
        "the privileged lane, whose hosted-privileged twins and 2400-second critical path are bounded by ci-privileged.yml's 2460-second launcher",
    ),
    (
        Family::Group("envelope"),
        "envelope-only nodes, not run by any retained validation",
    ),
    (
        Family::Group("qemu"),
        "qemu-l2-only nodes, not run by any retained validation",
    ),
    (
        Family::Group("quick"),
        "the quick label, not run by any retained validation",
    ),
    (
        Family::GroupPrefix("quick-super-"),
        "the quick and super variants, not run by any retained validation",
    ),
    (
        Family::Group("super"),
        "the super label, not run by any retained validation",
    ),
    (
        Family::Group("supercompat"),
        "the super label, not run by any retained validation",
    ),
    (
        Family::Group("superstress"),
        "the super label, not run by any retained validation",
    ),
];

/// How an [`UNMEASURED_FAMILIES`] entry names its nodes.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Family {
    Group(&'static str),
    GroupPrefix(&'static str),
    TagSuffix(&'static str),
}

impl Family {
    fn contains(self, step: &Step) -> bool {
        match self {
            Family::Group(group) => step.group == group,
            Family::GroupPrefix(prefix) => step.group.starts_with(prefix),
            Family::TagSuffix(suffix) => step.job.ends_with(suffix),
        }
    }
}

/// The rules [`assert_cpu_bounded_step_limits`] applies, each a sentence for
/// the refusal.
fn violations(step: &Step) -> Vec<String> {
    let (wall, cpu) = (step.timeout, step.cpu_timeout);
    let cores = step.hint.preferred_inner_jobs.unwrap_or(1).max(1);
    let mut found = Vec::new();
    if wall > WALL_TO_CPU_MAX * cpu {
        found.push(format!(
            "its {wall}-second wall bound is more than {WALL_TO_CPU_MAX} times its {cpu}-second CPU bound"
        ));
    }
    if cpu > wall * cores {
        found.push(format!(
            "its {cpu}-second CPU bound cannot be reached before its {wall}-second wall bound at {cores} core(s) \
             (preferred_inner_jobs), so only the wall bound can fire"
        ));
    }
    if wall > WALL_CEILING_SECONDS
        && !WALL_CEILING_EXCEPTIONS
            .iter()
            .any(|(tag, _)| *tag == step.tag())
    {
        found.push(format!(
            "its {wall}-second wall bound is above {WALL_CEILING_SECONDS} seconds without an entry in \
             step_limits::WALL_CEILING_EXCEPTIONS"
        ));
    }
    found
}

/// Refuse a node bounded by wall alone, and any stale entry in the tables
/// above (see the module comment).
pub(crate) fn assert_cpu_bounded_step_limits(cfg: &DagConfig) -> Result<(), String> {
    let mut refusals = Vec::new();
    for (family, _) in UNMEASURED_FAMILIES {
        let members = cfg.steps.iter().filter(|step| family.contains(step));
        if !members.clone().any(|step| !violations(step).is_empty()) {
            refusals.push(format!(
                "UNMEASURED_FAMILIES entry {family:?} no longer holds a node the rule refuses; remove it"
            ));
        }
    }
    for step in &cfg.steps {
        if UNMEASURED_FAMILIES
            .iter()
            .any(|(family, _)| family.contains(step))
        {
            continue;
        }
        for violation in violations(step) {
            refusals.push(format!("{}: {violation}", step.tag()));
        }
    }
    for (tag, _) in WALL_CEILING_EXCEPTIONS {
        match cfg.steps.iter().find(|step| step.tag() == *tag) {
            None => refusals.push(format!(
                "WALL_CEILING_EXCEPTIONS names {tag}, which is not a node"
            )),
            Some(step) if step.timeout <= WALL_CEILING_SECONDS => refusals.push(format!(
                "WALL_CEILING_EXCEPTIONS names {tag}, whose {}-second wall bound no longer needs it; remove the entry",
                step.timeout
            )),
            Some(_) => {}
        }
    }
    if refusals.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} step-limit refusal(s); a node must be bounded by CPU, with a wall bound at most \
             {WALL_TO_CPU_MAX} times its CPU bound and at most {WALL_CEILING_SECONDS} seconds unless \
             documented (ci/manifest-plan/src/step_limits.rs):\n  {}",
            refusals.len(),
            refusals.join("\n  ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use dagrun::io::dag_from_json;

    use super::*;

    fn committed() -> DagConfig {
        dag_from_json(include_str!("../../dag/validate.json")).unwrap()
    }

    fn step_mut<'a>(cfg: &'a mut DagConfig, tag: &str) -> &'a mut Step {
        cfg.steps.iter_mut().find(|step| step.tag() == tag).unwrap()
    }

    fn refusal(cfg: &DagConfig) -> String {
        assert_cpu_bounded_step_limits(cfg).unwrap_err()
    }

    #[test]
    fn the_committed_graph_is_cpu_bounded() {
        assert_cpu_bounded_step_limits(&committed()).unwrap();
    }

    #[test]
    fn a_wall_bound_alone_is_refused() {
        // privileged-pmu.preemption as it was on 2026-11-02: killed at its
        // 30-second wall with one or two of its 7200 CPU-seconds used.
        let mut cfg = committed();
        let step = step_mut(&mut cfg, "privileged-pmu.preemption");
        (step.timeout, step.cpu_timeout) = (30, 7200);
        let said = refusal(&cfg);
        assert!(
            said.contains("privileged-pmu.preemption: its 7200-second CPU bound cannot be reached before its 30-second wall bound"),
            "{said}"
        );
    }

    #[test]
    fn a_wall_bound_more_than_eight_times_the_cpu_bound_is_refused() {
        let mut cfg = committed();
        let step = step_mut(&mut cfg, "privileged-pmu.preemption");
        (step.timeout, step.cpu_timeout) = (241, 30);
        assert!(refusal(&cfg).contains("more than 8 times its 30-second CPU bound"));
    }

    #[test]
    fn a_wall_bound_above_the_ceiling_needs_an_exception() {
        let mut cfg = committed();
        let step = step_mut(&mut cfg, "check.backend_abstraction");
        (step.timeout, step.cpu_timeout) = (660, 190);
        assert!(refusal(&cfg).contains(
            "check.backend_abstraction: its 660-second wall bound is above 600 seconds without an entry"
        ));
    }

    #[test]
    fn stale_entries_are_refused() {
        let mut cfg = committed();
        step_mut(&mut cfg, "test.detcore_misc").timeout = 600;
        assert!(refusal(&cfg).contains(
            "WALL_CEILING_EXCEPTIONS names test.detcore_misc, whose 600-second wall bound no longer needs it"
        ));
        cfg.steps.retain(|step| step.group != "envelope");
        assert!(
            refusal(&cfg).contains("UNMEASURED_FAMILIES entry Group(\"envelope\") no longer holds")
        );
    }
}
