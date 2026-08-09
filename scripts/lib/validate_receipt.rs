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
//! apply-local-label` is handed `--ledger <path>` and re-derives the receipt
//! itself; this driver only supplies the PR number and the shard path. That
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

use std::path::Path;
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

/// The five conditions `validate.sh:1735` requires before publishing.
///
/// Expressed as a pure predicate so it can be bracketed on both sides. Note
/// `profile == "full"`: a focused or selective green is real evidence about what
/// it ran, but it is not the full-suite receipt the label claims, so it must not
/// mint one.
pub fn eligible(
    exit_code: u8,
    failures: usize,
    label_pr: bool,
    commit_anchored: bool,
    tree_dirty: bool,
    profile: &str,
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
    Ok(())
}

fn has_cmd(name: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {name} >/dev/null 2>&1")])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Locate `ci-hub`: `$CI_HUB_APPLY_LOCAL_LABEL`, else
/// `$DEV_HERMIT_PARENT/ci-hub/ci-hub` (validate.sh:1647-1652).
fn ci_hub_path() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("CI_HUB_APPLY_LOCAL_LABEL") {
        if !p.is_empty() {
            return Some(std::path::PathBuf::from(p));
        }
    }
    let parent = std::env::var("DEV_HERMIT_PARENT").ok().filter(|p| !p.is_empty())?;
    Some(std::path::PathBuf::from(parent).join("ci-hub").join("ci-hub"))
}

/// Resolve the PR: `$PR_NUMBER`, else ask `gh` (through `with-proxy` when
/// available, because networked `gh` requires it here).
fn resolve_pr() -> Option<String> {
    if let Ok(p) = std::env::var("PR_NUMBER") {
        if !p.is_empty() {
            return Some(p);
        }
    }
    if !has_cmd("gh") {
        return None;
    }
    let (prog, pre): (&str, &[&str]) =
        if has_cmd("with-proxy") { ("with-proxy", &["gh"]) } else { ("gh", &[]) };
    let out = Command::new(prog)
        .args(pre)
        .args(["pr", "view", "--repo", LABEL_REPO, "--json", "number", "-q", ".number"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// Publish the receipt and apply `locally-validated`, FROM the ledger record.
///
/// Always returns; never fails the run.
pub fn publish(ledger: &Path) -> Publication {
    let Some(ci_hub) = ci_hub_path() else {
        let msg = "the ci-hub receipt publisher is unavailable (no CI_HUB_APPLY_LOCAL_LABEL and no DEV_HERMIT_PARENT)";
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
        let msg = format!("the ci-hub receipt publisher is not executable at {}", ci_hub.display());
        eprintln!("⚠️  counted validation recorded, but {msg}; not applying locally-validated");
        return Publication::Unavailable(msg);
    }
    let Some(pr) = resolve_pr() else {
        eprintln!("⚠️  counted validation recorded, but no PR was found; not applying locally-validated");
        return Publication::Unavailable("no PR was found".into());
    };
    let status = Command::new(&ci_hub)
        .args(["apply-local-label", "--pr", &pr, "--repo", LABEL_REPO, "--ledger"])
        .arg(ledger)
        .status();
    match status {
        Ok(s) if s.success() => {
            println!("📎 receipt published; locally-validated applied to {LABEL_REPO}#{pr} from {}", ledger.display());
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
    // Positive: exactly one qualifying combination must be ACCEPTED, so the
    // predicate is not vacuously restrictive.
    eligible(0, 0, true, true, false, "full")
        .map_err(|e| format!("receipt: the one qualifying case must be eligible, got: {e}"))?;
    let mut accepted = 1usize;
    // Negative: each condition, spoiled alone, must be REFUSED.
    let negatives: Vec<(&str, Result<(), String>)> = vec![
        ("nonzero exit", eligible(1, 0, true, true, false, "full")),
        ("nonzero failures", eligible(0, 1, true, true, false, "full")),
        ("--no-label-pr", eligible(0, 0, false, true, false, "full")),
        ("not commit-anchored", eligible(0, 0, true, false, false, "full")),
        ("dirty tree", eligible(0, 0, true, true, true, "full")),
        ("quick profile", eligible(0, 0, true, true, false, "quick")),
        ("portable-only profile", eligible(0, 0, true, true, false, "portable-only")),
        ("super profile", eligible(0, 0, true, true, false, "super")),
        ("envelope-only profile", eligible(0, 0, true, true, false, "envelope-only")),
        ("selective profile", eligible(0, 0, true, true, false, "selective")),
        ("focused compat profile", eligible(0, 0, true, true, false, "strict-compat-only")),
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
    if let Err(e) = eligible(0, 0, true, true, false, "super") {
        if !e.contains("not the full suite") {
            return Err(format!("receipt: super must be refused BY THE PROFILE gate, got: {e}"));
        }
    }
    accepted += 0;
    Ok(format!(
        "receipt: eligibility bracketed {accepted} accept / {refused} refuse (no label was \
         touched; the predicate is pure)"
    ))
}
