/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Exact command bytes for the two backend-parity-c manifest selectors.
//!
//! Those selectors no longer exist in a generated plan: slice S6 of
//! <https://github.com/rrnewton/hermit/issues/3301> folded the backend-parity-c
//! bucket into c-programs and removed `e2e.manifest_backend_parity_c` and its
//! `_on_host` twin. Their tags, commands and category below are the spellings
//! of retained plans, which a reader must still match byte for byte.
//!
//! The generator emits only the ordinary spellings: since
//! <https://github.com/rrnewton/hermit/issues/3301> no newly constructed plan
//! asks the harness for a ptrace reference run, and the generator's invariants
//! refuse a step that does. The parity spellings stay here only so that
//! schema-10 plans retained before that change keep reading with the cells and
//! parity relations they were published with.
//!
//! The generator and retained-plan reader share exact command bytes; a reader
//! never infers execution policy from a substring of an arbitrary shell command.

use dagrun::model::Step;

pub(crate) const PORTABLE_PARITY_COMMAND: &str = r########"./ci/hermetic/run-in-pinned-root.sh --src . --out ignored/hermetic/split --src-rw --cargo-home ignored/hermetic/split/cargo --env CARGO_BUILD_JOBS --env DAGRUN_STEP_STARTED_MONOTONIC_NS --env DAGRUN_TEST_COUNTS_PATH --env E2E_BUILD_ROOT --env E2E_KERNEL_VERSION --env E2E_MACHINE_SHORTNAME --env E2E_RESULT_ROOT --env E2E_RUN_ID --env HERMIT_E2E_EMPTY_WORKDIR --env HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT --env L4_REPS --env PR_NUMBER --env SUPER_REPETITIONS --env THIRD_PARTY_BUILD_JOBS --env VALIDATE_VERBOSITY -- bash -c '/src/ci/hermetic/assert-no-network.sh && /src/ci/hermetic/assert-build-dependencies.sh && exec bash -c "$1"' bash 'export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category backend-parity-c --ci-only --allow-empty --prebuilt --parity-reference ptrace --results "$E2E_RESULT_ROOT/portable/manifest_backend_parity_c/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_backend_parity_c/junit.xml"'"########;
pub(crate) const PORTABLE_ORDINARY_COMMAND: &str = r########"./ci/hermetic/run-in-pinned-root.sh --src . --out ignored/hermetic/split --src-rw --cargo-home ignored/hermetic/split/cargo --env CARGO_BUILD_JOBS --env DAGRUN_STEP_STARTED_MONOTONIC_NS --env DAGRUN_TEST_COUNTS_PATH --env E2E_BUILD_ROOT --env E2E_KERNEL_VERSION --env E2E_MACHINE_SHORTNAME --env E2E_RESULT_ROOT --env E2E_RUN_ID --env HERMIT_E2E_EMPTY_WORKDIR --env HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT --env L4_REPS --env PR_NUMBER --env SUPER_REPETITIONS --env THIRD_PARTY_BUILD_JOBS --env VALIDATE_VERBOSITY -- bash -c '/src/ci/hermetic/assert-no-network.sh && /src/ci/hermetic/assert-build-dependencies.sh && exec bash -c "$1"' bash 'export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category backend-parity-c --ci-only --allow-empty --prebuilt --results "$E2E_RESULT_ROOT/portable/manifest_backend_parity_c/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_backend_parity_c/junit.xml"'"########;
pub(crate) const HOSTED_PARITY_COMMAND: &str = r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category backend-parity-c --ci-only --allow-empty --prebuilt --parity-reference ptrace --results "$E2E_RESULT_ROOT/portable/manifest_backend_parity_c/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_backend_parity_c/junit.xml""########;
pub(crate) const HOSTED_ORDINARY_COMMAND: &str = r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category backend-parity-c --ci-only --allow-empty --prebuilt --results "$E2E_RESULT_ROOT/portable/manifest_backend_parity_c/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_backend_parity_c/junit.xml""########;

/// Pinned-root names forwarded since the Buck release build joined validate.
pub(crate) const RELEASE_BUILD_ENV: &str =
    " --env HERMIT_VALIDATE_RELEASE_BUILD_MODE --env HERMIT_VALIDATE_BUCK_DOTSLASH";

/// Pinned-root names forwarded since the parity post-pass runs in every
/// harness process (<https://github.com/rrnewton/hermit/issues/3301>): the one
/// guest epoch pinned per run, and the post-pass's two activation variables.
/// They were added together after the release-build names, so every older
/// spelling lacks all three.
pub(crate) const PARITY_POST_PASS_ENV: [&str; 3] = [
    "HERMIT_EPOCH",
    crate::parity::PARITY_POST_PASS_ENV,
    crate::parity::PARITY_SELECT_ENV,
];

/// `command` without the [`PARITY_POST_PASS_ENV`] names: the spelling of a
/// plan retained before they were forwarded. Each name must appear exactly
/// once; the hosted command, which forwards nothing, must have none.
pub(crate) fn without_parity_post_pass_env(tag: &str, command: &str) -> Result<String, String> {
    let words = PARITY_POST_PASS_ENV.map(|name| format!(" --env {name} "));
    if tag == "e2e.manifest_backend_parity_c_on_host"
        && words.iter().all(|word| !command.contains(word.as_str()))
    {
        return Ok(command.to_owned());
    }
    let mut older = command.to_owned();
    for (name, word) in PARITY_POST_PASS_ENV.iter().zip(&words) {
        if older.matches(word.as_str()).count() != 1 {
            return Err(format!(
                "{tag} has no single {name} environment name to date"
            ));
        }
        older = older.replacen(word.as_str(), " ", 1);
    }
    Ok(older)
}

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
    // Plans retained before the release-build names were forwarded omit
    // exactly that contiguous run; the refresher appends missing names in
    // declaration order, so this is the exact older spelling, not a relaxation.
    let pre_release_env = |command: String| match command.matches(RELEASE_BUILD_ENV).count() {
        0 if step.tag() == "e2e.manifest_backend_parity_c_on_host" => Ok(command),
        1 => Ok(command.replacen(RELEASE_BUILD_ENV, "", 1)),
        _ => Err(format!(
            "{} has no single release-build environment run to date",
            step.tag()
        )),
    };
    // Plans retained before the parity post-pass names were forwarded omit
    // exactly those names from the current spelling, for the same reason.
    let pre_epoch_parity = without_parity_post_pass_env(&step.tag(), &parity)?;
    let pre_epoch_ordinary = without_parity_post_pass_env(&step.tag(), &ordinary)?;
    let previous_parity = pre_release_env(pre_epoch_parity.clone())?;
    let previous_ordinary = pre_release_env(pre_epoch_ordinary.clone())?;
    let legacy_parity = previous_parity.replacen(" --results ", " --jobs 8 --results ", 1);
    let legacy_ordinary = previous_ordinary.replacen(" --results ", " --jobs 8 --results ", 1);
    let selects_parity = match step.cmd.as_str() {
        command if command == parity => true,
        command if command == ordinary => false,
        command if command == pre_epoch_parity => true,
        command if command == pre_epoch_ordinary => false,
        command if command == previous_parity => true,
        command if command == previous_ordinary => false,
        // Schema-10 artifacts written before scheduler-owned width retain the
        // literal worker count. They remain readable as evidence, but an
        // unrecognized command still cannot pass this exact-command check.
        command if command == legacy_parity => true,
        command if command == legacy_ordinary => false,
        _ => {
            return Err(format!(
                "{} differs from its declared backend parity command",
                step.tag()
            ));
        }
    };
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
    Ok(selects_parity)
}
