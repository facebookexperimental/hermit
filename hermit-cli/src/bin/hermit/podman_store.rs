/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Discovery of OCI images already present in the local containers/storage
//! image store, via the `podman` CLI.
//!
//! # Why the CLI and not a library
//!
//! `containers/image` and `containers/storage` — the components that own the
//! image store — are Go libraries. Linking them into this Rust binary means cgo
//! plus a compiled-in pin of one store implementation, while the on-disk format
//! is versioned and can differ per install (libpod state may be sqlite while
//! containers/storage metadata is still JSON). The store's *documented* surface
//! is the `podman` CLI, so that is what this module binds to. Parsing
//! `overlay-images/images.json` or `db.sql` directly would bind hermit to an
//! internal format and is deliberately not done here.
//!
//! # Relationship to [`crate::image`]
//!
//! `buildah` (used by [`crate::image::materialize_rootfs`]) and `podman` share
//! the same graph root, so `--image` already reads this same store. This module
//! adds the piece that was missing: turning a user-supplied *reference* into a
//! stable image *identity* before anything is cached against it, plus the
//! listing and inspection surface behind `hermit oci`.

use std::collections::BTreeMap;
use std::process::Command;
use std::process::Stdio;

use hermit::Context;
use hermit::Error;
use serde::Deserialize;

/// Program used to talk to the local image store.
const PODMAN: &str = "podman";

/// A resolved local image: the reference the user gave, plus the identity the
/// store assigned it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedImage {
    /// The reference as the user wrote it (tag, short id, digest, ...).
    pub reference: String,
    /// The store's canonical 64-hex image ID. This — not [`Self::reference`] —
    /// is the identity anything may be cached against.
    pub id: String,
    /// The image's manifest digest (`sha256:...`), when the store records one.
    pub digest: Option<String>,
}

/// A row of `hermit oci ls`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ImageSummary {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Names", default)]
    pub names: Option<Vec<String>>,
    #[serde(rename = "Size", default)]
    pub size: u64,
}

impl ImageSummary {
    /// The best human-readable name for this image, or `<none>` when the image
    /// is untagged (an intermediate build layer, typically).
    pub fn display_name(&self) -> String {
        self.names
            .as_ref()
            .and_then(|names| names.first().cloned())
            .unwrap_or_else(|| "<none>".to_string())
    }
}

/// A handle to the local image store.
pub(crate) struct PodmanStore {
    /// The store's graph root, as podman reports it. Recorded so error messages
    /// and `hermit oci inspect` can say *which* store was consulted.
    graph_root: String,
}

/// Run a podman command and capture stdout, mapping a nonzero exit to an error
/// that includes podman's own stderr (which is where its diagnostics go).
fn podman_output(args: &[&str]) -> Result<String, Error> {
    let output = Command::new(PODMAN)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| {
            format!(
                "Failed to run `{PODMAN} {}`. `hermit oci` needs a rootless \
                 `{PODMAN}` on PATH.",
                args.join(" ")
            )
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::msg(format!(
            "`{PODMAN} {}` failed ({}): {}",
            args.join(" "),
            output.status,
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

impl PodmanStore {
    /// Detect a usable local image store. Returns `Ok(None)` when podman is
    /// absent or reports no store, so callers can fall back rather than fail.
    pub fn probe() -> Result<Option<Self>, Error> {
        match podman_output(&["info", "--format", "{{.Store.GraphRoot}}"]) {
            Ok(graph_root) if !graph_root.is_empty() => Ok(Some(Self { graph_root })),
            // A podman that exists but cannot report a store is not usable, and
            // neither is a missing podman. Both are "no store here".
            _ => Ok(None),
        }
    }

    pub fn graph_root(&self) -> &str {
        &self.graph_root
    }

    /// List the images in the store. `all` includes untagged intermediates.
    pub fn list(&self, all: bool) -> Result<Vec<ImageSummary>, Error> {
        let mut args = vec!["images", "--format", "json"];
        if all {
            args.push("--all");
        }
        let stdout = podman_output(&args)?;
        if stdout.is_empty() {
            return Ok(Vec::new());
        }
        serde_json::from_str(&stdout)
            .context("Failed to parse `podman images --format json` output")
    }

    /// Resolve a reference to the store's canonical image identity **without
    /// touching the network**.
    ///
    /// `podman pull --policy=never` is the right primitive here: it accepts
    /// every reference form (tag, short id, full id, `name@sha256:...`),
    /// returns the canonical 64-hex ID, and is guaranteed not to contact a
    /// registry. That last property is what makes this safe to call on the
    /// normal `hermit oci run` path — resolution never silently downloads.
    pub fn resolve(&self, reference: &str) -> Result<ResolvedImage, Error> {
        let id = podman_output(&["pull", "--policy=never", reference]).map_err(|e| {
            Error::msg(format!(
                "No local image matches {reference:?} in the store at {}. \
                 Use `hermit oci ls` to see what is available, or \
                 `hermit oci pull {reference}` to download it.\n  ({e})",
                self.graph_root
            ))
        })?;
        self.identify(reference, &id)
    }

    /// Download `reference` into the store, then resolve it. This is the only
    /// method here that contacts a registry.
    pub fn pull(&self, reference: &str) -> Result<ResolvedImage, Error> {
        let id = podman_output(&["pull", reference]).map_err(|e| {
            Error::msg(format!(
                "Failed to pull {reference:?}. If the registry is reachable only \
                 through a proxy, run hermit under that proxy.\n  ({e})"
            ))
        })?;
        self.identify(reference, &id)
    }

    /// Turn a store-reported ID into a [`ResolvedImage`], attaching the
    /// manifest digest when the store records one.
    fn identify(&self, reference: &str, id: &str) -> Result<ResolvedImage, Error> {
        let id = normalize_image_id(id).ok_or_else(|| {
            Error::msg(format!(
                "podman returned an unrecognized image id for {reference:?}: {id:?}"
            ))
        })?;
        // An image built locally has no manifest digest until it is pushed or
        // pulled, so a missing digest is normal and must not be an error.
        let digest = podman_output(&["image", "inspect", &id, "--format", "{{.Digest}}"])
            .ok()
            .filter(|d| d.starts_with("sha256:"));
        Ok(ResolvedImage {
            reference: reference.to_string(),
            id,
            digest,
        })
    }

    /// The image's declared `Config.Env` and `Config.WorkingDir`.
    pub fn config(&self, image: &ResolvedImage) -> Result<ImageRunConfig, Error> {
        let stdout = podman_output(&[
            "image",
            "inspect",
            &image.id,
            "--format",
            "{{json .Config}}",
        ])?;
        let raw: RawConfig = serde_json::from_str(&stdout)
            .context("Failed to parse `podman image inspect` config output")?;
        Ok(ImageRunConfig {
            env: raw
                .env
                .unwrap_or_default()
                .iter()
                .filter_map(|entry| entry.split_once('='))
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            workdir: raw.working_dir.filter(|w| !w.is_empty() && w != "/"),
            cmd: raw.cmd.unwrap_or_default(),
            entrypoint: raw.entrypoint.unwrap_or_default(),
        })
    }

    /// The overlay directories backing this image's filesystem, in the order
    /// an `overlay` mount wants them (topmost first).
    ///
    /// This is what makes a future copy-free materialization possible: the
    /// merged rootfs can be mounted directly from the store's own layers inside
    /// hermit's user namespace, instead of being copied out.
    pub fn overlay_dirs(&self, image: &ResolvedImage) -> Result<Vec<String>, Error> {
        let stdout = podman_output(&[
            "image",
            "inspect",
            &image.id,
            "--format",
            "{{json .GraphDriver}}",
        ])?;
        let graph: GraphDriver = serde_json::from_str(&stdout)
            .context("Failed to parse `podman image inspect` GraphDriver output")?;
        if graph.name != "overlay" {
            return Err(Error::msg(format!(
                "image {} uses the {:?} storage driver; only `overlay` exposes \
                 mountable layer directories",
                image.id, graph.name
            )));
        }
        Ok(overlay_layer_order(
            graph.data.get("UpperDir").map(String::as_str),
            graph.data.get("LowerDir").map(String::as_str),
        ))
    }
}

/// The subset of an image's config that affects how hermit runs it.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ImageRunConfig {
    pub env: Vec<(String, String)>,
    pub workdir: Option<String>,
    pub cmd: Vec<String>,
    pub entrypoint: Vec<String>,
}

#[derive(Deserialize)]
struct RawConfig {
    #[serde(rename = "Env")]
    env: Option<Vec<String>>,
    #[serde(rename = "WorkingDir")]
    working_dir: Option<String>,
    #[serde(rename = "Cmd")]
    cmd: Option<Vec<String>>,
    #[serde(rename = "Entrypoint")]
    entrypoint: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct GraphDriver {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Data", default)]
    data: BTreeMap<String, String>,
}

/// Assemble the overlay layer list, topmost first.
///
/// podman reports `UpperDir` (the image's own top layer) separately from the
/// colon-separated `LowerDir` chain, and `LowerDir` is absent entirely for a
/// single-layer image. An `overlay` mount wants all of them as `lowerdir` in
/// topmost-first order, because hermit supplies its own writable upper layer.
fn overlay_layer_order(upper: Option<&str>, lower: Option<&str>) -> Vec<String> {
    let mut dirs = Vec::new();
    if let Some(upper) = upper.map(str::trim).filter(|u| !u.is_empty()) {
        dirs.push(upper.to_string());
    }
    if let Some(lower) = lower {
        dirs.extend(
            lower
                .split(':')
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .map(str::to_string),
        );
    }
    dirs
}

/// Accept the forms podman prints for an image id and reduce them to bare
/// 64-hex. `podman pull` prints the bare id; some paths print `sha256:<id>`.
fn normalize_image_id(raw: &str) -> Option<String> {
    let candidate = raw.trim();
    let candidate = candidate.strip_prefix("sha256:").unwrap_or(candidate);
    (candidate.len() == 64 && candidate.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| candidate.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_image_id_accepts_bare_and_prefixed_hex() {
        let id = "1ee23df5e47b5b7331304af929f5461f9f516f5c166c3031832d2adfd22f323c";
        assert_eq!(normalize_image_id(id).as_deref(), Some(id));
        assert_eq!(
            normalize_image_id(&format!("sha256:{id}")).as_deref(),
            Some(id)
        );
        assert_eq!(
            normalize_image_id(&format!("  {id}\n")).as_deref(),
            Some(id)
        );
        assert_eq!(
            normalize_image_id(&id.to_ascii_uppercase()).as_deref(),
            Some(id),
            "podman ids are case-insensitive hex; normalize to lowercase"
        );
    }

    // A short id is a *reference*, not an identity: accepting one here would
    // reintroduce exactly the aliasing that keying on the resolved id removes.
    #[test]
    fn normalize_image_id_rejects_short_ids_and_non_hex() {
        assert_eq!(normalize_image_id("1ee23df5e47b"), None);
        assert_eq!(normalize_image_id(""), None);
        assert_eq!(normalize_image_id("not-an-id"), None);
        // 64 chars but not hex.
        assert_eq!(normalize_image_id(&"z".repeat(64)), None);
    }

    // A single-layer image reports no LowerDir at all; the upper layer alone is
    // the whole filesystem.
    #[test]
    fn overlay_layer_order_handles_single_layer_image() {
        assert_eq!(
            overlay_layer_order(Some("/store/overlay/aaa/diff"), None),
            vec!["/store/overlay/aaa/diff".to_string()]
        );
    }

    // Multi-layer: upper first, then the LowerDir chain in the order podman
    // already gives it (topmost first).
    #[test]
    fn overlay_layer_order_puts_upper_before_the_lower_chain() {
        assert_eq!(
            overlay_layer_order(
                Some("/store/overlay/top/diff"),
                Some("/store/overlay/mid/diff:/store/overlay/base/diff"),
            ),
            vec![
                "/store/overlay/top/diff".to_string(),
                "/store/overlay/mid/diff".to_string(),
                "/store/overlay/base/diff".to_string(),
            ]
        );
    }

    #[test]
    fn overlay_layer_order_skips_empty_components() {
        assert_eq!(
            overlay_layer_order(Some("  "), Some("/a/diff::/b/diff:")),
            vec!["/a/diff".to_string(), "/b/diff".to_string()]
        );
        assert!(overlay_layer_order(None, None).is_empty());
    }

    #[test]
    fn image_summary_display_name_falls_back_for_untagged_images() {
        let untagged = ImageSummary {
            id: "a".repeat(64),
            names: None,
            size: 0,
        };
        assert_eq!(untagged.display_name(), "<none>");

        let named = ImageSummary {
            id: "b".repeat(64),
            names: Some(vec!["localhost/example:1".to_string()]),
            size: 1,
        };
        assert_eq!(named.display_name(), "localhost/example:1");
    }
}
