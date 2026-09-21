/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Pin the `plan` CALL SITE, not the helper it calls.
//!
//! ⚠️ WHY THIS FILE EXISTS. `ManifestSet::knows_test` has a unit test, and that test
//! stays green when the refusal in `print_plan` is deleted outright:
//! `agent(hermit-004)` measured exactly that on review — replacing
//! `if !manifests.knows_test(id)` with `if false` restored the original defect and
//! every suite still passed. "The helper answers correctly" and "`plan` consults it
//! and refuses" are different facts, and this pull request is for the second one.
//!
//! These are subprocess cases on purpose. `fail()` calls `process::exit`, so an
//! in-process test cannot observe the refusal, and any pure function extracted to
//! make it testable would reintroduce the same gap one layer down: a test on that
//! function would still not prove `plan` calls it.
//!
//! ⚠️ AND THE TWO rc=0 CASES CARRY AS MUCH WEIGHT AS THE rc=2 ONE. Without them the
//! guard could be widened to `cells.is_empty()` — the wrong fix, which would turn
//! `audit-gaps`'s legitimate "no gaps" answer into a failure — and a suite containing
//! only the refusal case would stay green through that change.

use std::path::PathBuf;
use std::process::Command;

/// A real test id that exists in the portable manifests but has no cells in the
/// privileged lane, so a lane-mismatched query for it is legitimately empty.
const REAL_ID: &str = "applications/c-toolchain-workflow";
const UNKNOWN_ID: &str = "no-such-test-xyz";
/// A second real id, so a repeated `--test` can be built from two values that are
/// both accepted and therefore isolates the duplicate check.
const SECOND_REAL_ID: &str = "applications/git-repository-workflow";

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is <root>/ci/manifest-plan.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("ci/manifest-plan must sit two levels below the repo root")
        .to_path_buf()
}

/// Run `test-harness` with `args` from the repo root and return its exit code.
///
/// The binary resolves its sibling `hermit-manifest-plan` relative to its own path,
/// and Cargo builds both bins of this package for an integration test, so the pair
/// is coherent. A `None` code means a signal, which is a harness failure rather than
/// a verdict and is asserted against explicitly.
/// Run `test-harness` and return `(exit code, stdout, stderr)`.
///
/// ⚠️ THE EXIT CODE ALONE IS NOT ENOUGH FOR MOST OF THESE CASES, which is why this
/// exists alongside [`harness`]. `agent(hermit-126)` found that four of the five
/// repeated-selector cases below were satisfied by validation this change did not
/// add -- an invalid `--lane`/`--mode`/`--backend` value, or the unknown-id guard for
/// `--test`. All of those also exit 2, so an exit-code assertion cannot tell the new
/// duplicate check from the old value check. Requiring the duplicate-specific
/// diagnostic is what separates them.
fn harness_output(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_test-harness"))
        .current_dir(repo_root())
        .args(args)
        .output()
        .expect("failed to run test-harness");
    let code = out.status.code().unwrap_or_else(|| {
        panic!(
            "test-harness died on a signal for args {args:?}; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (
        code,
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn harness(args: &[&str]) -> i32 {
    let out = Command::new(env!("CARGO_BIN_EXE_test-harness"))
        .current_dir(repo_root())
        .args(args)
        .output()
        .expect("failed to run test-harness");
    out.status.code().unwrap_or_else(|| {
        panic!(
            "test-harness died on a signal for args {args:?}; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

#[test]
fn plan_refuses_an_unknown_test_id() {
    assert_eq!(
        harness(&["plan", "--lane", "portable", "--test", UNKNOWN_ID]),
        2,
        "`plan` must REFUSE an id that is in no manifest. It exited 0 with an empty \
         list before this guard, and exited 0 for a real id too, so its exit code \
         carried no information either way -- a bisection driving off `plan` reads a \
         typo as 'nothing failed here' and converges on the wrong commit."
    );
}

///
/// ⚠️ IT ASSERTS THE EMPTY RESULT, NOT ONLY rc=0. An earlier version discarded the
/// output, so it could not distinguish "correctly returned nothing" from "returned
/// something it should not have" -- and the whole point of this case is WHICH empty
/// answer is legitimate. Found by `agent(hermit-126)`.
#[test]
fn plan_still_accepts_a_real_id_that_selects_nothing_in_this_lane() {
    let (code, stdout, stderr) =
        harness_output(&["plan", "--lane", "privileged", "--test", REAL_ID]);
    assert_eq!(
        code, 0,
        "a REAL id with no cells in the requested lane is a correct, well-formed query \
         and must stay rc=0. This is the case a `cells.is_empty()` guard would have \
         refused, which is why the check is 'unknown id' instead. stderr: {stderr:?}"
    );
    assert!(
        stdout.trim().is_empty(),
        "and it must be EMPTY -- an empty plan is the answer, so printing cells here \
         would mean the lane filter was ignored: {stdout:?}"
    );
}

#[test]
fn audit_gaps_still_reports_no_gaps_as_success() {
    let (code, stdout, stderr) =
        harness_output(&["audit-gaps", "--lane", "privileged", "--test", REAL_ID]);
    assert_eq!(
        code, 0,
        "`print_plan` also serves `audit-gaps`, where an empty answer legitimately \
         means NO GAPS. Turning that into a failure would trade a silent false pass \
         for a loud false failure. stderr: {stderr:?}"
    );
    assert!(
        stdout.trim().is_empty(),
        "no gaps means NO OUTPUT; anything here is a gap this control would hide: \
         {stdout:?}"
    );
}

/// ⚠️ THE SECOND CHANGED COMMAND PATH. `plan` had an unknown-id case from the start;
/// `audit-gaps` shares `print_plan` and the same guard, and had none -- so half the
/// surface this change touches was unpinned. Found by `agent(hermit-126)`.
#[test]
fn audit_gaps_refuses_an_unknown_test_id() {
    let (code, _, stderr) =
        harness_output(&["audit-gaps", "--lane", "portable", "--test", UNKNOWN_ID]);
    assert_eq!(code, 2, "audit-gaps must refuse an id in no manifest too");
    assert!(
        stderr.contains("unknown test id"),
        "and it must refuse with the unknown-id diagnostic rather than some other \
         check that also exits 2: {stderr:?}"
    );
}

#[test]
fn plan_accepts_a_real_id_in_its_own_lane() {
    assert_eq!(
        harness(&["plan", "--lane", "portable", "--test", REAL_ID]),
        0,
        "the ordinary path must be untouched"
    );
}

/// ⚠️ THE CONTROL THAT MUST FAIL. Every assertion above is an exit code, and a
/// `test-harness` that could not run at all would exit non-zero for everything --
/// which satisfies the refusal case while telling us nothing. An unfiltered `plan`
/// must succeed AND print cells, so a broken binary cannot masquerade as a working
/// guard.
#[test]
fn an_unfiltered_plan_still_produces_cells() {
    let out = Command::new(env!("CARGO_BIN_EXE_test-harness"))
        .current_dir(repo_root())
        .args(["plan", "--lane", "portable", "--format", "json"])
        .output()
        .expect("failed to run test-harness");
    assert_eq!(
        out.status.code(),
        Some(0),
        "an unfiltered plan must succeed"
    );
    // ⚠️ PARSED, NOT SNIFFED. Checking `starts_with('[')` and a length over 2 accepts
    // `[{}]`, `[ ]]` and any malformed string that happens to open with a bracket.
    // The control exists to prove real cells came back, so it has to decode them.
    // Found by `agent(hermit-126)`.
    let text = String::from_utf8_lossy(&out.stdout);
    let cells: serde_json::Value = serde_json::from_str(text.trim()).unwrap_or_else(|error| {
        panic!(
            "an unfiltered plan must print valid JSON: {error}; stdout was {:?}",
            text.chars().take(200).collect::<String>()
        )
    });
    let array = cells
        .as_array()
        .unwrap_or_else(|| panic!("an unfiltered plan must print a JSON ARRAY, got {cells}"));
    assert!(
        !array.is_empty(),
        "an unfiltered plan must print a NON-EMPTY cell array, or the exit codes above \
         prove nothing about the guard"
    );
}

/// ⚠️ A REPEATED SELECTOR MUST REFUSE, BECAUSE LAST-VALUE-WINS UNDID THE GUARD ABOVE.
/// Measured at `979a50b17a75`, before the fix:
///
/// ```text
/// --test no-such-test-xyz                             rc=2
/// --test no-such-test-xyz --test applications/...      rc=0   []      <- the defect
/// --test applications/... --test no-such-test-xyz      rc=2
/// ```
///
/// The asymmetry is the whole finding: only the LAST occurrence was ever looked up,
/// so an unknown id in any position but the last produced a silent green -- the exact
/// failure the unknown-id refusal was added to remove, reappearing in the argument
/// parser instead of the id lookup. Found by the codex lane on this head and confirmed
/// independently by `agent(hermit-012)`; the claude lane, and I, had approved it.
#[test]
fn a_repeated_test_flag_is_refused_in_either_order() {
    assert_eq!(
        harness(&[
            "plan", "--lane", "portable", "--test", UNKNOWN_ID, "--test", REAL_ID
        ]),
        2,
        "an unknown id followed by a real one must REFUSE; before this fix it exited 0 \
         with an empty plan because the second --test overwrote the first"
    );
    assert_eq!(
        harness(&[
            "plan", "--lane", "portable", "--test", REAL_ID, "--test", UNKNOWN_ID
        ]),
        2,
        "and the reverse order too -- the fix must not depend on which one is last"
    );
}

/// Every single-valued selector, not just the one the defect was found through.
/// `--jobs` already refused a repeat; these did not, and there is no reason for the
/// rule to differ per flag.
///
/// ⚠️ EVERY VALUE HERE IS VALID ON PURPOSE, AND AN EARLIER VERSION USED `"a"`/`"b"`.
/// With those, four of the five cases passed WITHOUT the duplicate check: `"a"` is an
/// invalid `--lane`, `--mode` and `--backend` value, so existing value validation
/// already exits 2, and it is an unknown `--test` id, so this change's own unknown-id
/// guard exits 2. Only `--category` isolated the duplicate. The loop went red under a
/// mutation of `set_once` and therefore looked discriminating, because that one good
/// case carried the other four. Found by `agent(hermit-126)`; a whole-function
/// mutation cannot see it, because the test does fail -- just for one fifth of the
/// reasons it claims.
#[test]
fn every_single_valued_selector_refuses_a_repeat() {
    // Two ACCEPTED values per selector, so nothing but the repeat can refuse them.
    for (flag, first, second) in [
        ("--lane", "portable", "privileged"),
        ("--category", "applications", "detcore"),
        ("--test", REAL_ID, SECOND_REAL_ID),
        ("--mode", "verify", "chaos"),
        ("--backend", "ptrace", "sabre"),
    ] {
        for (a, b) in [(first, second), (second, first)] {
            let (code, _, stderr) = harness_output(&["plan", flag, a, flag, b]);
            assert_eq!(
                code, 2,
                "{flag} must be refused when repeated ({a} then {b})"
            );
            assert!(
                stderr.contains(&format!("{flag} may be specified only once")),
                "{flag} repeated as {a} then {b} must fail with the DUPLICATE \
                 diagnostic, not with some other check that also exits 2; got: {stderr:?}"
            );
        }
    }
}

/// The single-occurrence control, over every selector rather than two of them.
///
/// Without this the loop above proves nothing: a `test-harness` that refused every
/// invocation would satisfy all ten assertions.
#[test]
fn a_single_occurrence_of_every_selector_is_still_accepted() {
    for (flag, value) in [
        ("--lane", "portable"),
        ("--category", "applications"),
        ("--test", REAL_ID),
        ("--mode", "verify"),
        ("--backend", "ptrace"),
    ] {
        let (code, _, stderr) = harness_output(&["plan", flag, value]);
        assert_eq!(
            code, 0,
            "a single {flag} {value} must still be accepted; stderr: {stderr:?}"
        );
    }
}

/// ⚠️ THE CONTROL, and it is not optional here. Every assertion above is an exit code
/// of 2, and a `test-harness` that cannot run at all exits 2 for everything --
/// `agent(hermit-012)` nearly filed a refutation from exactly that, and so did I,
/// because only one of the package's two binaries had been built. A SINGLE selector
/// must still be accepted, or the refusals above prove nothing.
#[test]
fn a_single_occurrence_of_each_selector_is_still_accepted() {
    assert_eq!(
        harness(&["plan", "--lane", "portable", "--test", REAL_ID]),
        0
    );
    assert_eq!(harness(&["plan", "--lane", "portable"]), 0);
}
