#[path = "../../../hermit-cli/src/canonical_verdict.rs"]
pub mod canonical_verdict;
pub mod ci_selection;
pub mod cli_help;
pub mod cpu_evidence;
pub mod environmental_block;
mod git_environment;
pub mod host_capability;
pub mod imported_results;
pub mod invocation_cgroup;
pub mod ledger;
#[path = "../../../hermit-cli/src/logdiff_report.rs"]
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
#[path = "../../../hermit-cli/tests/common/proc_locks_lease.rs"]
mod proc_locks_lease;
pub mod retired_ids;
pub mod runner;
pub mod self_test_selection;
pub mod service_result;
pub mod stress_series;
pub mod timeouts;
pub mod validation_dag;
mod validation_dag_static;

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
#[path = "../../../scripts/lib/validate_corpus.rs"]
mod validate_corpus;
#[allow(dead_code, clippy::too_many_arguments)]
#[path = "../../../scripts/lib/validate_generator.rs"]
mod validate_generator;
#[allow(dead_code, unused_imports, clippy::too_many_arguments)]
#[path = "../../../scripts/lib/validate_plan.rs"]
mod validate_plan;
#[allow(dead_code)]
#[path = "../../../scripts/lib/validate_super.rs"]
mod validate_super;
pub mod validation_inventory;
