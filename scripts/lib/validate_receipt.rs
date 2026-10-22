// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Receipt publication and the `locally-validated` label
//! (`publish_receipt_backed_label`, validate.sh:1645).
//!
//! # This module never decides that a run was green
//!
//! It publishes FROM the ledger record, it does not assert one. `ci-hub
//! apply-local-label` re-reads the one canonical ledger union itself; this
//! driver supplies only the PR number and the fixed repository. That
//! separation is the point: the label is a CACHE of a ledger fact, and the
//! authority that mints it must dereference the ledger rather than trust a
//! caller's "it passed".
//!
//! Consequently [`publish`] must be called AFTER the ledger row is appended.
//! `validate.sh` had the same ordering constraint inside `cleanup`
//! (append_validation_ledger at :1733, publish at :1738); getting it backwards
//! publishes against the PREVIOUS run's newest row.
//!
//! # Non-fatal by construction
//!
//! Every failure path warns and returns. Publication is observability, not a
//! gate: a missing `gh`, an absent PR, or a proxy failure must never turn a
//! green validation red. The bash had the same contract.
//!
//! # What this module is NOT
//!
//! It is not the label-STRIP trail. That is `scripts/label-strip-evidence.sh`,
//! which `validate.sh` never calls: its callers are the merge gate's
//! `invalidate-local-validation` job and agents doing a manual strip. Nothing
//! about porting the driver changes it, and pulling it in here would put a
//! GitHub-mutating side effect on the validation hot path.

use std::process::Command;

/// Repository the label is applied to. Hard-coded in the bash and kept hard-coded
/// here: this driver must not be usable to label an arbitrary repository.
pub const LABEL_REPO: &str = "rrnewton/hermit";

/// Why publication did not happen, or that it did. Returned (rather than only
/// printed) so `--self-test` can bracket the decision without any network call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Publication {
    /// Eligible, but a prerequisite was missing (no ci-hub, no PR).
    /// Eligibility itself is the caller's check, via [`eligible`].
    Unavailable(String),
    /// `ci-hub apply-local-label` was invoked for this PR.
    Attempted { pr: String },
}

/// Builder whose E2E payload is the debug binary a receipt certifies.
pub const RELEASE_BUILDER_CARGO: &str = "cargo";

/// The five conditions `validate.sh:1735` requires before publishing, plus the
/// builder.
///
/// Expressed as a pure predicate so it can be bracketed on both sides. Note
/// `profile == "full"`: a focused or selective green is real evidence about what
/// it ran, but it is not the full-suite receipt the label claims, so it must not
/// mint one. The same holds for a Buck run: its E2E cells executed the release
/// payload, without debug assertions or overflow checks.
pub fn eligible(
    exit_code: u8,
    failures: usize,
    label_pr: bool,
    commit_anchored: bool,
    tree_dirty: bool,
    profile: &str,
    release_builder: &str,
) -> Result<(), String> {
    if exit_code != 0 || failures != 0 {
        return Err("the run was not green".into());
    }
    if !label_pr {
        return Err("--no-label-pr / VALIDATE_LABEL_PR=0".into());
    }
    if !commit_anchored {
        return Err("the run was not commit-anchored".into());
    }
    if tree_dirty {
        return Err("the tree was dirty".into());
    }
    if profile != "full" {
        return Err(format!("profile is {profile}, not the full suite"));
    }
    if release_builder != RELEASE_BUILDER_CARGO {
        return Err(format!(
            "release builder is {release_builder}: its E2E cells ran the release payload \
             without debug assertions or overflow checks, which is supplemental evidence, \
             not the full-suite receipt"
        ));
    }
    Ok(())
}

fn has_cmd(name: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {name} >/dev/null 2>&1")])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Locate `ci-hub` in the executable checkout. The historical explicit path
/// and parent-relative forms remain only for callers that do not supply the
/// admitted launcher's tool root.
fn ci_hub_path_from(
    explicit: Option<String>,
    tool_root: Option<String>,
    parent: Option<String>,
) -> Option<std::path::PathBuf> {
    if let Some(root) = tool_root.filter(|path| !path.is_empty()) {
        return Some(std::path::PathBuf::from(root).join("ci-hub").join("ci-hub"));
    }
    if let Some(path) = explicit.filter(|path| !path.is_empty()) {
        return Some(std::path::PathBuf::from(path));
    }
    let parent = parent.filter(|path| !path.is_empty())?;
    Some(
        std::path::PathBuf::from(parent)
            .join("ci-hub")
            .join("ci-hub"),
    )
}

fn ci_hub_path() -> Option<std::path::PathBuf> {
    ci_hub_path_from(
        std::env::var("CI_HUB_APPLY_LOCAL_LABEL").ok(),
        std::env::var("DEV_HERMIT_TOOL_ROOT").ok(),
        std::env::var("DEV_HERMIT_PARENT").ok(),
    )
}

/// Where the PR to label comes from, decided without the network.
#[derive(Debug, PartialEq, Eq)]
enum PrLookup {
    /// `$PR_NUMBER` names it.
    Env(String),
    /// Ask `gh` for the PR of the checked-out branch.
    AskGh,
    /// There can be no PR to label, for this reason.
    NoPr(&'static str),
}

/// Decide how to find the PR from `$PR_NUMBER` (PR_ENV), the checked-out branch
/// (BRANCH, `None` when HEAD is detached) and whether HEAD is already on
/// `origin/main` (ON_MAIN). `gh pr view --json` is a GraphQL call, and on a
/// detached HEAD, on `main`, or for a commit already on `origin/main` it can
/// find no PR, so those cases make no call. Before this, every validate of
/// main spent one GraphQL request (10-15 a day) that could only fail.
fn pr_lookup(pr_env: Option<&str>, branch: Option<&str>, on_main: bool) -> PrLookup {
    if let Some(pr) = pr_env.filter(|pr| !pr.is_empty()) {
        return PrLookup::Env(pr.to_string());
    }
    match branch {
        None => PrLookup::NoPr("HEAD is detached, so it has no branch PR"),
        Some("main") => PrLookup::NoPr("main has no PR"),
        Some(_) if on_main => PrLookup::NoPr("HEAD is already on origin/main"),
        Some(_) => PrLookup::AskGh,
    }
}

fn git_output(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Resolve the PR: `$PR_NUMBER`, else, for a named branch not yet on
/// `origin/main`, ask `gh` (through `with-proxy` when available, because
/// networked `gh` requires it here). Err carries why there is no PR.
fn resolve_pr() -> Result<String, String> {
    let pr_env = std::env::var("PR_NUMBER").ok();
    let branch = git_output(&["symbolic-ref", "--quiet", "--short", "HEAD"]);
    let on_main = Command::new("git")
        .args(["merge-base", "--is-ancestor", "HEAD", "origin/main"])
        .status()
        .is_ok_and(|status| status.success());
    match pr_lookup(pr_env.as_deref(), branch.as_deref(), on_main) {
        PrLookup::Env(pr) => return Ok(pr),
        PrLookup::NoPr(reason) => return Err(reason.into()),
        PrLookup::AskGh => {}
    }
    if !has_cmd("gh") {
        return Err("gh is not installed".into());
    }
    let (prog, pre): (&str, &[&str]) = if has_cmd("with-proxy") {
        ("with-proxy", &["gh"])
    } else {
        ("gh", &[])
    };
    let out = Command::new(prog)
        .args(pre)
        .args([
            "pr", "view", "--repo", LABEL_REPO, "--json", "number", "-q", ".number",
        ])
        .output()
        .map_err(|error| format!("gh could not run: {error}"))?;
    if !out.status.success() {
        return Err("gh found no PR for this branch".into());
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        Err("gh found no PR for this branch".into())
    } else {
        Ok(s)
    }
}

fn apply_local_label_args(pr: &str) -> [&str; 5] {
    ["apply-local-label", "--pr", pr, "--repo", LABEL_REPO]
}

/// Publish the receipt and apply `locally-validated`, FROM the ledger record.
///
/// Always returns; never fails the run.
pub fn publish() -> Publication {
    let Some(ci_hub) = ci_hub_path() else {
        let msg = "the ci-hub receipt publisher is unavailable (no CI_HUB_APPLY_LOCAL_LABEL, DEV_HERMIT_TOOL_ROOT, or DEV_HERMIT_PARENT)";
        eprintln!("⚠️  counted validation recorded, but {msg}; not applying locally-validated");
        return Publication::Unavailable(msg.into());
    };
    let is_exec = std::fs::metadata(&ci_hub)
        .map(|m| {
            use std::os::unix::fs::PermissionsExt;
            m.is_file() && m.permissions().mode() & 0o111 != 0
        })
        .unwrap_or(false);
    if !is_exec {
        let msg = format!(
            "the ci-hub receipt publisher is not executable at {}",
            ci_hub.display()
        );
        eprintln!("⚠️  counted validation recorded, but {msg}; not applying locally-validated");
        return Publication::Unavailable(msg);
    }
    let pr = match resolve_pr() {
        Ok(pr) => pr,
        Err(reason) => {
            eprintln!(
                "⚠️  counted validation recorded, but no PR was found ({reason}); not applying locally-validated"
            );
            return Publication::Unavailable(format!("no PR was found: {reason}"));
        }
    };
    let status = Command::new(&ci_hub)
        .args(apply_local_label_args(&pr))
        .status();
    match status {
        Ok(s) if s.success() => {
            println!(
                "📎 receipt published; locally-validated applied to {LABEL_REPO}#{pr} from the canonical ledger"
            );
        }
        _ => {
            eprintln!(
                "⚠️  receipt publication failed for PR #{pr}; locally-validated was not authorized"
            );
        }
    }
    Publication::Attempted { pr }
}

/// Inert brackets for the eligibility predicate.
///
/// **Nothing here can publish.** It exercises only the pure predicate — no
/// `ci-hub` invocation, no `gh` call, no label mutation — because a bracket that
/// planted a real `locally-validated` label would itself be the authorization it
/// claims to test.
pub fn self_test() -> Result<String, String> {
    let split = ci_hub_path_from(
        Some("/caller-selected".into()),
        Some("/tool-root".into()),
        Some("/state-root".into()),
    );
    if split.as_deref() != Some(std::path::Path::new("/tool-root/ci-hub/ci-hub")) {
        return Err("receipt: publisher did not prefer the executable tool root".into());
    }
    if ci_hub_path_from(Some("/explicit/ci-hub".into()), None, None).as_deref()
        != Some(std::path::Path::new("/explicit/ci-hub"))
    {
        return Err("receipt: legacy explicit publisher path was treated as a root".into());
    }
    // The canonical consumer deliberately has no caller-selected ledger path.
    // Reintroducing `--ledger` would let this producer point the verifier at a
    // retired or forged shadow and recreate the drift this bracket closes.
    let invocation = apply_local_label_args("123");
    if invocation != ["apply-local-label", "--pr", "123", "--repo", LABEL_REPO]
        || invocation.contains(&"--ledger")
    {
        return Err("receipt: publisher must use the fixed canonical ledger consumer".into());
    }
    // Positive: exactly one qualifying combination must be ACCEPTED, so the
    // predicate is not vacuously restrictive.
    eligible(0, 0, true, true, false, "full", "cargo")
        .map_err(|e| format!("receipt: the one qualifying case must be eligible, got: {e}"))?;
    let mut accepted = 1usize;
    // Negative: each condition, spoiled alone, must be REFUSED.
    let negatives: Vec<(&str, Result<(), String>)> = vec![
        (
            "nonzero exit",
            eligible(1, 0, true, true, false, "full", "cargo"),
        ),
        (
            "nonzero failures",
            eligible(0, 1, true, true, false, "full", "cargo"),
        ),
        (
            "--no-label-pr",
            eligible(0, 0, false, true, false, "full", "cargo"),
        ),
        (
            "not commit-anchored",
            eligible(0, 0, true, false, false, "full", "cargo"),
        ),
        (
            "dirty tree",
            eligible(0, 0, true, true, true, "full", "cargo"),
        ),
        (
            "quick profile",
            eligible(0, 0, true, true, false, "quick", "cargo"),
        ),
        (
            "portable-only profile",
            eligible(0, 0, true, true, false, "portable-only", "cargo"),
        ),
        (
            "super profile",
            eligible(0, 0, true, true, false, "super", "cargo"),
        ),
        (
            "envelope-only profile",
            eligible(0, 0, true, true, false, "envelope-only", "cargo"),
        ),
        (
            "selective profile",
            eligible(0, 0, true, true, false, "selective", "cargo"),
        ),
        (
            "focused compat profile",
            eligible(0, 0, true, true, false, "sabre-compat-only", "cargo"),
        ),
        (
            "cell requalification profile",
            eligible(0, 0, true, true, false, "cell-requalification", "cargo"),
        ),
        (
            "Buck release payload",
            eligible(0, 0, true, true, false, "full", "buck"),
        ),
    ];
    let mut refused = 0usize;
    for (why, r) in &negatives {
        if r.is_ok() {
            return Err(format!("receipt: publication must be refused when {why}"));
        }
        refused += 1;
    }
    // A green super or envelope run is real, but it is NOT the full-suite
    // receipt; confirm the profile gate is what refuses it (not some other
    // condition), so the refusal cannot silently move.
    if let Err(e) = eligible(0, 0, true, true, false, "super", "cargo") {
        if !e.contains("not the full suite") {
            return Err(format!(
                "receipt: super must be refused BY THE PROFILE gate, got: {e}"
            ));
        }
    }
    let focused_error = eligible(0, 0, true, true, false, "cell-requalification", "cargo")
        .err()
        .ok_or("focused evidence must not authorize a full-suite receipt")?;
    if focused_error != "profile is cell-requalification, not the full suite" {
        return Err(format!(
            "receipt: requalification must be refused BY THE PROFILE gate, got: {focused_error}"
        ));
    }
    let buck_error = eligible(0, 0, true, true, false, "full", "buck")
        .err()
        .ok_or("a Buck release-payload green must not authorize a full-suite receipt")?;
    if !buck_error.starts_with("release builder is buck:") {
        return Err(format!(
            "receipt: a Buck green must be refused BY THE BUILDER gate, got: {buck_error}"
        ));
    }
    // The PR lookup makes no network call where no PR can exist.
    for (pr_env, branch, on_main, want) in [
        (Some("42"), None, true, PrLookup::Env("42".into())),
        (Some(""), Some("feature"), false, PrLookup::AskGh),
        (None, Some("feature"), false, PrLookup::AskGh),
        (
            None,
            None,
            false,
            PrLookup::NoPr("HEAD is detached, so it has no branch PR"),
        ),
        (None, Some("main"), true, PrLookup::NoPr("main has no PR")),
        (None, Some("main"), false, PrLookup::NoPr("main has no PR")),
        (
            None,
            Some("landed"),
            true,
            PrLookup::NoPr("HEAD is already on origin/main"),
        ),
    ] {
        let got = pr_lookup(pr_env, branch, on_main);
        if got != want {
            return Err(format!(
                "receipt: pr_lookup({pr_env:?}, {branch:?}, {on_main}) gave {got:?}, want {want:?}"
            ));
        }
    }
    accepted += 0;
    Ok(format!(
        "receipt: eligibility bracketed {accepted} accept / {refused} refuse (no label was \
         touched; the predicate is pure)"
    ))
}
