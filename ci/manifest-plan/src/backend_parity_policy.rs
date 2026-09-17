//! The two existing manifest selectors which request ptrace parity.
//!
//! The generator and retained-plan reader share exact command bytes; a reader
//! never infers execution policy from a substring of an arbitrary shell command.

use dagrun::model::Step;

pub(crate) const PORTABLE_PARITY_COMMAND: &str = r########"./ci/hermetic/run-in-pinned-root.sh --src . --out ignored/hermetic/split --src-rw --cargo-home ignored/hermetic/split/cargo --env CARGO_BUILD_JOBS --env DAGRUN_STEP_STARTED_MONOTONIC_NS --env DAGRUN_TEST_COUNTS_PATH --env E2E_BUILD_ROOT --env E2E_KERNEL_VERSION --env E2E_MACHINE_SHORTNAME --env E2E_RESULT_ROOT --env E2E_RUN_ID --env HERMIT_E2E_EMPTY_WORKDIR --env HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT --env L4_REPS --env PR_NUMBER --env SUPER_REPETITIONS --env THIRD_PARTY_BUILD_JOBS --env VALIDATE_VERBOSITY -- bash -c '/src/ci/hermetic/assert-no-network.sh && /src/ci/hermetic/assert-build-dependencies.sh && exec bash -c "$1"' bash 'export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category backend-parity-c --ci-only --allow-empty --prebuilt --parity-reference ptrace --jobs 8 --results "$E2E_RESULT_ROOT/portable/manifest_backend_parity_c/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_backend_parity_c/junit.xml"'"########;
pub(crate) const PORTABLE_ORDINARY_COMMAND: &str = r########"./ci/hermetic/run-in-pinned-root.sh --src . --out ignored/hermetic/split --src-rw --cargo-home ignored/hermetic/split/cargo --env CARGO_BUILD_JOBS --env DAGRUN_STEP_STARTED_MONOTONIC_NS --env DAGRUN_TEST_COUNTS_PATH --env E2E_BUILD_ROOT --env E2E_KERNEL_VERSION --env E2E_MACHINE_SHORTNAME --env E2E_RESULT_ROOT --env E2E_RUN_ID --env HERMIT_E2E_EMPTY_WORKDIR --env HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT --env L4_REPS --env PR_NUMBER --env SUPER_REPETITIONS --env THIRD_PARTY_BUILD_JOBS --env VALIDATE_VERBOSITY -- bash -c '/src/ci/hermetic/assert-no-network.sh && /src/ci/hermetic/assert-build-dependencies.sh && exec bash -c "$1"' bash 'export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category backend-parity-c --ci-only --allow-empty --prebuilt --jobs 8 --results "$E2E_RESULT_ROOT/portable/manifest_backend_parity_c/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_backend_parity_c/junit.xml"'"########;
pub(crate) const HOSTED_PARITY_COMMAND: &str = r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category backend-parity-c --ci-only --allow-empty --prebuilt --parity-reference ptrace --jobs 8 --results "$E2E_RESULT_ROOT/portable/manifest_backend_parity_c/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_backend_parity_c/junit.xml""########;
pub(crate) const HOSTED_ORDINARY_COMMAND: &str = r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category backend-parity-c --ci-only --allow-empty --prebuilt --jobs 8 --results "$E2E_RESULT_ROOT/portable/manifest_backend_parity_c/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_backend_parity_c/junit.xml""########;

pub(crate) fn selects_ptrace_parity(step: &Step) -> Result<bool, String> {
    let expected = match step.tag().as_str() {
        "e2e.manifest_backend_parity_c" => {
            // Generate trusted expected wrapper bytes with the same policy as
            // the DAG producer. Never normalize the received command: doing so
            // would silently restore a removed environment or argv guard.
            let expected = |command| {
                crate::validation_dag::refresh_pinned_root_environment(&step.tag(), command)
            };
            Some((
                expected(PORTABLE_PARITY_COMMAND)?,
                expected(PORTABLE_ORDINARY_COMMAND)?,
            ))
        }
        "e2e.manifest_backend_parity_c_on_host" => Some((
            HOSTED_PARITY_COMMAND.to_owned(),
            HOSTED_ORDINARY_COMMAND.to_owned(),
        )),
        _ => None,
    };
    let Some((parity, ordinary)) = expected else {
        if step.cmd.contains("--parity-reference") {
            return Err(format!(
                "{} has unrecognized backend parity policy",
                step.tag()
            ));
        }
        return Ok(false);
    };
    if step.cmd != parity && step.cmd != ordinary {
        return Err(format!(
            "{} differs from its declared backend parity command",
            step.tag()
        ));
    }
    let selector = step
        .manifest
        .as_ref()
        .ok_or_else(|| format!("{} omitted its backend parity selector", step.tag()))?;
    if selector.lane != "portable"
        || selector.category != "backend-parity-c"
        || selector.test.is_some()
        || selector.mode.is_some()
        || selector.backend.is_some()
    {
        return Err(format!(
            "{} changed its backend parity population",
            step.tag()
        ));
    }
    Ok(step.cmd == parity)
}
