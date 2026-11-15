// Dual Cargo/Buck builds select #[path] modules by cfg below; the
// buck_build cfg is set only by the Buck shim (--cfg=buck_build).
#![allow(unexpected_cfgs)]

/// The `ci/manifest-plan` directory Cargo built the calling crate from, used
/// to derive the checkout root (`../..`) when no explicit root is given.
/// Every caller is in this package (its binaries and its tests), so that is
/// this crate's directory.
///
/// A macro, so `CARGO_MANIFEST_DIR` is read where it expands and never by
/// this library's own build. scripts/validate.rs, dev-hermit's ci-hub.rs
/// and check-series-rows.rs link the library, and the dev-hermit rust-script
/// cache (ci-hub/validate/rust_script_cas.py) refuses to store a binary
/// whose crates read compile-time environment that may hold a path. While
/// this was a library function (2026-11-08 to 2026-11-09) it stored none of
/// the three, and every validation compiled them from cold.
///
/// Buck builds have no such directory: their staged source path is a
/// build artifact, never a checkout, so under `buck_build` this fails
/// closed and callers must pass an explicit root (`--root` at the
/// harvest sites; coordinator decision 2026-11-08, dh-cqlkz).
#[macro_export]
macro_rules! compiled_manifest_dir {
    () => {
        $crate::compiled_manifest_dir_from(option_env!("CARGO_MANIFEST_DIR"))
    };
}

/// The body of [`compiled_manifest_dir!`], which supplies the calling
/// crate's `CARGO_MANIFEST_DIR`. `option_env!` rather than `env!` there,
/// because Buck need not set the variable and fails closed here instead.
#[doc(hidden)]
pub fn compiled_manifest_dir_from(cargo_manifest_dir: Option<&str>) -> std::path::PathBuf {
    #[cfg(not(buck_build))]
    {
        std::path::PathBuf::from(
            cargo_manifest_dir.expect("Cargo sets CARGO_MANIFEST_DIR for every crate it compiles"),
        )
    }
    #[cfg(buck_build)]
    {
        let _ = cargo_manifest_dir;
        panic!(
            "Buck-built manifest-plan binary has no default checkout root: pass --root <HERMIT_CHECKOUT>"
        )
    }
}

pub mod archived;
pub mod bundle_record;
#[cfg_attr(not(buck_build), path = "../../../hermit-cli/src/canonical_verdict.rs")]
#[cfg_attr(buck_build, path = "../mapped/hermit-cli/canonical_verdict.rs")]
pub mod canonical_verdict;
pub mod ci_selection;
pub mod cli_help;
pub mod cpu_evidence;
pub mod cpu_profile_record;
pub mod environmental_block;
pub mod exec_safe_fs;
pub mod executable_binding;
mod git_environment;
pub mod host_capability;
pub mod imported_results;
pub mod invocation_cgroup;
pub mod known_gaps;
pub mod landing_branch;
pub mod ledger;
#[cfg_attr(not(buck_build), path = "../../../hermit-cli/src/logdiff_report.rs")]
#[cfg_attr(buck_build, path = "../mapped/hermit-cli/logdiff_report.rs")]
pub mod logdiff_report;
pub mod manifest_corpus;
pub mod manifest_metadata;
pub mod manifest_value;
pub mod nextest_binaries;
mod nextest_build_selections;
pub mod nextest_cohort;
pub mod nextest_cpu;
pub mod parity;
pub mod parity_outcomes;
#[cfg_attr(
    not(buck_build),
    path = "../../../hermit-cli/tests/common/proc_locks_lease.rs"
)]
#[cfg_attr(buck_build, path = "../mapped/hermit-cli/proc_locks_lease.rs")]
mod proc_locks_lease;
pub mod retired_ids;
pub mod runner;
pub mod self_test_selection;
pub mod service_result;
mod step_limits;
pub mod stress_series;
pub mod timeouts;
pub mod typedlog;
pub mod validation_dag;
mod validation_dag_static;
mod vfork_spawn;

// Lets the shared scripts/lib files below name this library as
// `hermit_manifest_plan::...`, as they do when scripts/validate.rs includes them.
extern crate self as hermit_manifest_plan;

// The validation DAG's generated partition is built by
// scripts/lib/validate_generator.rs, which scripts/validate.rs also includes;
// these four files are shared verbatim so both crates build the same nodes.
// The library calls only `validate_generator::committed_generated_partition`
// (from validation_dag::generate), so most of what the script uses from them
// is unused here; dead_code (and, for validate_plan's re-exports of this
// library's host_capability items, unused_imports) is allowed for that reason
// alone. clippy::too_many_arguments is allowed on the two that need it because
// ci/prepare-rust-scripts.sh waives it for these files as scripts.
#[allow(dead_code)]
#[cfg_attr(not(buck_build), path = "../../../scripts/lib/validate_corpus.rs")]
#[cfg_attr(buck_build, path = "../mapped/scripts/validate_corpus.rs")]
mod validate_corpus;
#[allow(dead_code, clippy::too_many_arguments)]
#[cfg_attr(not(buck_build), path = "../../../scripts/lib/validate_generator.rs")]
#[cfg_attr(buck_build, path = "../mapped/scripts/validate_generator.rs")]
mod validate_generator;
#[allow(dead_code, unused_imports, clippy::too_many_arguments)]
#[cfg_attr(not(buck_build), path = "../../../scripts/lib/validate_plan.rs")]
#[cfg_attr(buck_build, path = "../mapped/scripts/validate_plan.rs")]
mod validate_plan;
#[allow(dead_code)]
#[cfg_attr(not(buck_build), path = "../../../scripts/lib/validate_super.rs")]
#[cfg_attr(buck_build, path = "../mapped/scripts/validate_super.rs")]
mod validate_super;
pub mod validation_inventory;

#[cfg_attr(not(buck_build), path = "../../../hermit-cli/src/verify_run_record.rs")]
#[cfg_attr(buck_build, path = "../mapped/hermit-cli/verify_run_record.rs")]
pub mod verify_run_record;

pub mod execution_owners;

pub mod dag_source_ownership;
