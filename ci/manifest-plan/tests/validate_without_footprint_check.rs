/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `test-harness validate --without-footprint-check`, the mode the Buck-only
//! gate (gate.manifest_<name>) runs on the staged Buck-built harness: every
//! audit runs and prints what it checked, in the full gate's order; the
//! generated test-footprint check is named NOT RUN, never skipped silently;
//! and no other command accepts the flag. Each audit's refusal of a broken
//! input is tested beside it, in test-harness's own tests.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("ci/manifest-plan must sit two levels below the repository root")
        .to_path_buf()
}

/// Git exports these to hooks and `git rebase --exec` steps, and they override
/// the working directory (https://github.com/rrnewton/hermit/issues/3362).
const REPOSITORY_LOCATION_VARIABLES: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
];

fn harness(root: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_test-harness"));
    for name in REPOSITORY_LOCATION_VARIABLES {
        command.env_remove(name);
    }
    command
        .args(args)
        .current_dir(root)
        .output()
        .expect("run test-harness")
}

/// The one stdout line that starts with `prefix`, and its position.
fn line<'a>(stdout: &'a str, prefix: &str) -> (usize, &'a str) {
    let found = stdout
        .lines()
        .enumerate()
        .filter(|(_, line)| line.starts_with(prefix))
        .collect::<Vec<_>>();
    assert_eq!(found.len(), 1, "one line starting {prefix:?} in:\n{stdout}");
    found[0]
}

/// The number written right before `after` in `line`.
fn number_before(line: &str, after: &str) -> usize {
    let head = &line[..line
        .find(after)
        .unwrap_or_else(|| panic!("{after:?} in {line:?}"))];
    head.trim_end()
        .rsplit(' ')
        .next()
        .and_then(|word| word.parse().ok())
        .unwrap_or_else(|| panic!("a number before {after:?} in {line:?}"))
}

#[test]
fn validate_without_footprint_check_runs_and_counts_every_other_audit() {
    let root = repo_root();
    let root_arg = root.to_str().expect("a UTF-8 checkout path");
    // --repo-root, as the Buck-only gate passes it, so the CLI bracket audit
    // hands the same tree words to every harness command it runs.
    let output = harness(
        &root,
        &[
            "validate",
            "--without-footprint-check",
            "--repo-root",
            root_arg,
        ],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stdout}\n{stderr}");

    let (dag_at, dag) = line(&stdout, "DAG correspondence: ");
    assert!(
        number_before(dag, " portable and privileged nodes") > 0,
        "{dag}"
    );
    assert!(number_before(dag, " manifest buckets checked") > 0, "{dag}");
    let (budget_at, budget) = line(&stdout, "budget ordering: ");
    assert!(
        number_before(budget, " portable sharded") + number_before(budget, " privileged nodes") > 0,
        "{budget}"
    );
    let (stress_at, _) = line(
        &stdout,
        "determinism-stress comparison evidence self-test: positive and refusal directions pass",
    );
    let (not_run_at, not_run) = line(&stdout, "NOT RUN: the generated test-footprint check");
    assert!(
        not_run.contains("generate-test-footprints --check")
            && not_run.contains("--without-footprint-check"),
        "{not_run}"
    );
    let (brackets_at, brackets) = line(&stdout, "CLI brackets: ");
    // 11 options, each missing and empty, plus 4 --jobs controls.
    assert_eq!(
        number_before(brackets, " malformed controls refused"),
        26,
        "{brackets}"
    );
    assert!(number_before(brackets, " cells") > 0, "{brackets}");
    let (pass_at, pass) = line(&stdout, "PASS (generated test-footprint check NOT RUN): ");
    assert!(number_before(pass, " YAML manifests") > 0, "{pass}");
    assert!(number_before(pass, " required cells") > 0, "{pass}");
    // The full gate's order, with the NOT RUN line where the footprint check runs.
    assert!(
        dag_at < budget_at
            && budget_at < stress_at
            && stress_at < not_run_at
            && not_run_at < brackets_at
            && brackets_at < pass_at,
        "{stdout}"
    );
    assert!(!stdout.contains("\nPASS: "), "{stdout}");
}

#[test]
fn only_validate_accepts_without_footprint_check() {
    let root = repo_root();
    for command in ["plan", "audit-ci", "build"] {
        let output = harness(&root, &[command, "--without-footprint-check"]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_ne!(output.status.code(), Some(0), "{command}: {stderr}");
        assert!(
            stderr.contains("--without-footprint-check is accepted by validate only"),
            "{command}: {stderr}"
        );
    }
}
