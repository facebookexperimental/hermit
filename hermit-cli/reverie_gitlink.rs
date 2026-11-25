/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// The gitlink half of the build scripts' Reverie pin reader, kept apart
// from `reverie_pin.rs` on purpose: hermit-cli's library test module
// includes `reverie_pin.rs` alone to bracket `parse_reverie_pin`, and
// these functions — used only by build-side consumers — would be dead
// code there (and an `expect(dead_code)` here would be unfulfilled in
// the build scripts that do call them; the fbsource cargo clippy job
// compiles both shapes and refuses either mismatch). Build-side
// consumers include `reverie_pin.rs` first, then this file, in one
// scope: `reverie_pin_at` calls `parse_reverie_pin` from it.
//
// Read and watch the SAME source (seat round 4): the embedded pin is
// the submodule CHECKOUT's HEAD — the sources Cargo actually compiles
// through the path dependencies — and `reverie_pin_watch_paths_in`
// watches exactly that file (plus the Eden marker). V0.7 read the
// superproject index while watching the submodule HEAD, so the
// documented advance sequence (checkout, build, stage) embedded the
// pre-advance pin. Where there is no submodule checkout (Eden), the
// marker is read instead; the recorded gitlink, the tree's release
// values, and the manifest parse remain as fallbacks for trees without
// either.

fn pin_git(root: &std::path::Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn pin_full_sha(value: &str) -> Option<String> {
    (value.len() == 40 && value.chars().all(|c| c.is_ascii_hexdigit())).then(|| value.to_owned())
}

/// The submodule checkout's HEAD: the Reverie sources a build in this
/// tree actually compiles. Requires an initialized submodule
/// (`reverie/.git` present, so Git cannot discover the superproject
/// instead and answer for the wrong repository).
fn reverie_checkout_head(root: &std::path::Path) -> Option<String> {
    let submodule = root.join("reverie");
    if !submodule.join(".git").exists() {
        return None;
    }
    pin_git(&submodule, &["rev-parse", "HEAD"]).and_then(|text| pin_full_sha(text.trim()))
}

/// The pin recorded by the Eden/Sapling marker, `reverie.submodule.txt`
/// holding exactly `Subproject commit <40-hex>` on one line — the only
/// spelling CodeSync produces or consumes. Anything else yields `None`
/// (fall through), never a partial read.
fn reverie_marker_pin(root: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join("reverie.submodule.txt")).ok()?;
    let line = text.strip_suffix('\n').unwrap_or(&text);
    if line.contains('\n') {
        return None;
    }
    line.strip_prefix("Subproject commit ")
        .and_then(pin_full_sha)
}

/// The Reverie revision recorded by the `reverie` submodule gitlink of the
/// checkout rooted at `root`, or `None` when it cannot be read: the index
/// entry when an advance is staged, else HEAD's tree entry — the same
/// pair `scripts/check-reverie-pin.rs` reads. Fallback only (see
/// `reverie_pin_at`): provenance for trees with no submodule checkout
/// and no marker (tarballs, uninitialized submodules), where the value
/// degrades to `"unknown"` at callers exactly as an unparseable
/// manifest did.
fn reverie_gitlink_pin(root: &std::path::Path) -> Option<String> {
    // Index entry: `160000 <sha> <stage>\treverie`. No let-chain here:
    // version_provenance's fixture crates compile this file at edition
    // 2021, where let-chains are rejected.
    if let Some(text) = pin_git(root, &["ls-files", "--stage", "--", "reverie"]) {
        for line in text.lines() {
            let mut fields = line.split_whitespace();
            let sha = (fields.next() == Some("160000"))
                .then(|| fields.next().and_then(pin_full_sha))
                .flatten();
            if let Some(sha) = sha {
                return Some(sha);
            }
        }
    }
    // HEAD tree entry: `160000 commit <sha>\treverie`.
    let text = pin_git(root, &["ls-tree", "HEAD", "--", "reverie"])?;
    let mut fields = text.split_whitespace();
    if fields.next() != Some("160000") || fields.next() != Some("commit") {
        return None;
    }
    fields.next().and_then(pin_full_sha)
}

/// The Reverie pin recorded in the tree's own release values. The
/// open-source tree keeps that stand-in under the shim cell; the internal
/// Hermit tree keeps the file it loads at `fb-only/release.bzl`. If both
/// exist they must agree, exactly as the pin checker requires every
/// derived site to name the recorded pin.
fn reverie_release_pin(root: &std::path::Path) -> Option<String> {
    let mut found: Option<String> = None;
    for relative in [
        "shim/hermetic_infra/hermit/fb-only/release.bzl",
        "fb-only/release.bzl",
    ] {
        let Ok(text) = std::fs::read_to_string(root.join(relative)) else {
            continue;
        };
        let pin = parse_reverie_release_pin(&text)?;
        if found.as_deref().is_some_and(|seen| seen != pin) {
            return None;
        }
        found = Some(pin);
    }
    found
}

/// The Reverie pin of the checkout rooted at `root`, in the order the
/// build actually sees the sources: submodule checkout HEAD, Eden
/// marker, recorded gitlink, release values, pre-migration manifest.
/// `None` renders as `"unknown"` at callers.
fn reverie_pin_at(root: &std::path::Path) -> Option<String> {
    if let Some(pin) = reverie_checkout_head(root) {
        return Some(pin);
    }
    if let Some(pin) = reverie_marker_pin(root) {
        return Some(pin);
    }
    if let Some(pin) = reverie_gitlink_pin(root) {
        return Some(pin);
    }
    if let Some(pin) = reverie_release_pin(root) {
        return Some(pin);
    }
    let text = std::fs::read_to_string(root.join("detcore/Cargo.toml")).ok()?;
    parse_reverie_pin(&text)
}
