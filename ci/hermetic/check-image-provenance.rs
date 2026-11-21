#!/usr/bin/env -S rust-script --force
//! Check the pinned validation image's tracked dual-build receipt and every
//! composed image receipt under ci/hermetic/images for consistency.
//!
//! ```cargo
//! [dependencies]
//! serde = "1"
//! serde_json = "1"
//! sha2 = "0.10"
//! tempfile = "3"
//! ```

#[path = "../../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
#[cfg(test)]
use std::process::Command;
use std::process::ExitCode;

use serde::Deserialize;
use serde::Deserializer;
use serde::de;
use serde::de::MapAccess;
use serde::de::SeqAccess;
use serde::de::Visitor;
use serde_json::Map;
use serde_json::Number;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;

const PROVENANCE: &str = "ci/hermetic/image.provenance.json";
const PIN: &str = "ci/hermetic/image.digest";
const METHOD: &str = "two-clean-isolated-nix-stores-v1";
const SUPERSEDED_REFERENCE: &str = "localhost/hermit-hermetic-validate@sha256:4e1605ecbb13a6c5c6f45f4259f49e2c07efb21a7e6e7b7aabb5b97329232d6e";
const SUPERSEDED_ARCHIVE_SHA256: &str =
    "df1f0dd4f9aa0c82fd6fedceac64d5607143add2f8d0d40a0163850b19289453";
/// The replaced 4e1605ec root pins nightly Rust; the rationale must name the property
/// this repin repairs (the pin is now the reproducible isolated build).
const RATIONALE_MARKER: &str = "reproducible";
/// Both isolated builds recorded `nix --version` before building.
const RECORDED_NIX_VERSION: &str = "nix (Nix) 2.35.2";
const INPUT_PATHS: [&str; 4] = [
    "ci/hermetic/build-image.sh",
    "ci/hermetic/flake.lock",
    "ci/hermetic/flake.nix",
    "ci/hermetic/guest-paths.txt",
];
const EVIDENCE_LIMITATIONS: [&str; 7] = [
    "Each service ran under systemd-run with only PATH and NIX_SSL_CERT_FILE set explicitly; fetches went through the host forward proxy (with-proxy). Only variable names were recorded, not their values.",
    "Each build substituted only from cache.nixos.org under its trusted key; the removed realized inputs (rust-script, cargo-nextest and coreutils) were rebuilt in each store. Substituted paths are trusted by Nix signature or content hash and NAR hash, not rebuilt.",
    "Both builds forced the Nix sandbox on (--option sandbox true), so builders ran as unprivileged nixbld users with a fixed build directory; no host shell or host path is an image input. This is the issue-5031 fix the previous receipt could not claim (its builds ran unsandboxed).",
    "Build logs, unit records and Nix path-info records were retained only on the build host; this receipt does not authenticate them.",
    "The superseded 4e1605ec archive was not rebuilt for this receipt; its df1f0dd4 SHA-256 is carried from the prior receipt that produced the replaced pin.",
    "The recorded build-source commit and tree are historical review evidence. Rebases can rewrite their ancestry; live verification establishes equivalence only for the four enumerated image input files, not the complete source tree or archive.",
    "The flake.nix input was re-attested by evaluation, not by rebuild: the builds below used flake.nix 663f05a9; the current file is b0ac537c, accepted only because the pinned derivation re-evaluates identically with groups.nix aborting (flake_reattestation).",
];

struct ExpectedRun {
    run_id: &'static str,
    source_snapshot_identity: &'static str,
    nix_store_identity: &'static str,
    podman_store_identity: &'static str,
    memory_peak_bytes: u64,
    cpu_nanoseconds: u64,
}

const EXPECTED_RUNS: [ExpectedRun; 2] = [
    ExpectedRun {
        run_id: "hermetic-stable-abseil-repin5-c.service",
        source_snapshot_identity: "src@50:3834828530",
        nix_store_identity: "nix-root/nix@50:3834843063",
        podman_store_identity: "musestable-5213-abseil-repin/c/podman-data",
        memory_peak_bytes: 8590163968,
        cpu_nanoseconds: 1774605025000,
    },
    ExpectedRun {
        run_id: "hermetic-stable-abseil-repin5-d.service",
        source_snapshot_identity: "src@50:3834836059",
        nix_store_identity: "nix-root/nix@50:3834843081",
        podman_store_identity: "musestable-5213-abseil-repin/d/podman-data",
        memory_peak_bytes: 8590176256,
        cpu_nanoseconds: 1776617838000,
    },
];

struct StrictValue(Value);

struct StrictValueVisitor;

impl<'de> Visitor<'de> for StrictValueVisitor {
    type Value = StrictValue;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Number(value.into())))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Number(value.into())))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Number::from_f64(value)
            .map(Value::Number)
            .map(StrictValue)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_string(value.to_owned())
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::String(value)))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Null))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Null))
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        StrictValue::deserialize(deserializer)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(StrictValue(value)) = sequence.next_element()? {
            values.push(value);
        }
        Ok(StrictValue(Value::Array(values)))
    }

    fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = Map::new();
        while let Some(key) = object.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(de::Error::custom(format!(
                    "duplicate JSON object key {key:?}"
                )));
            }
            let StrictValue(value) = object.next_value()?;
            values.insert(key, value);
        }
        Ok(StrictValue(Value::Object(values)))
    }
}

impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictValueVisitor)
    }
}

fn parse_provenance(bytes: &[u8]) -> Result<Value, String> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let StrictValue(value) = StrictValue::deserialize(&mut deserializer)
        .map_err(|error| format!("cannot parse {PROVENANCE}: {error}"))?;
    deserializer
        .end()
        .map_err(|error| format!("cannot parse {PROVENANCE}: {error}"))?;
    Ok(value)
}

fn usage() {
    println!(
        "usage: check-image-provenance.rs\n\n\
         Check {PROVENANCE}, the current source inputs, {PIN}, and every receipt\n\
         under {IMAGES_DIR} for tracked consistency.\n\
         Historical build evidence remains review-bound: this checker does not authenticate it,\n\
         build, load, or inspect an image.\n\n\
         options:\n  -h, --help  Show this help"
    );
}

fn object<'a>(value: &'a Value, context: &str) -> Result<&'a Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("{context} must be an object"))
}

fn array<'a>(value: &'a Value, context: &str) -> Result<&'a Vec<Value>, String> {
    value
        .as_array()
        .ok_or_else(|| format!("{context} must be an array"))
}

fn field<'a>(value: &'a Value, key: &str, context: &str) -> Result<&'a Value, String> {
    object(value, context)?
        .get(key)
        .ok_or_else(|| format!("{context}.{key} is required"))
}

fn string<'a>(value: &'a Value, key: &str, context: &str) -> Result<&'a str, String> {
    field(value, key, context)?
        .as_str()
        .ok_or_else(|| format!("{context}.{key} must be a string"))
}

fn unsigned(value: &Value, key: &str, context: &str) -> Result<u64, String> {
    field(value, key, context)?
        .as_u64()
        .ok_or_else(|| format!("{context}.{key} must be an unsigned integer"))
}

fn boolean(value: &Value, key: &str, context: &str) -> Result<bool, String> {
    field(value, key, context)?
        .as_bool()
        .ok_or_else(|| format!("{context}.{key} must be a boolean"))
}

fn exact_keys(value: &Value, expected: &[&str], context: &str) -> Result<(), String> {
    let actual = object(value, context)?
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let expected = expected.iter().copied().collect::<BTreeSet<_>>();
    if actual != expected {
        return Err(format!(
            "{context} fields differ: actual={actual:?}, expected={expected:?}"
        ));
    }
    Ok(())
}

fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn require_sha256(value: &str, context: &str) -> Result<(), String> {
    let digest = value
        .strip_prefix("sha256:")
        .ok_or_else(|| format!("{context} must begin with sha256:"))?;
    if !is_lower_hex(digest, 64) {
        return Err(format!(
            "{context} must contain 64 lowercase hexadecimal digits"
        ));
    }
    Ok(())
}

fn require_flat_sha256(value: &str, context: &str) -> Result<(), String> {
    if !is_lower_hex(value, 64) {
        return Err(format!("{context} must be 64 lowercase hexadecimal digits"));
    }
    Ok(())
}

fn require_nix_sha256(value: &str, context: &str) -> Result<(), String> {
    let encoded = value
        .strip_prefix("sha256-")
        .ok_or_else(|| format!("{context} must begin with sha256-"))?;
    if encoded.len() != 44
        || !encoded.ends_with('=')
        || !encoded[..43]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/')
    {
        return Err(format!(
            "{context} must contain a canonical base64-encoded SHA-256 digest"
        ));
    }
    Ok(())
}

fn require_nix_store_path(value: &str, context: &str) -> Result<(), String> {
    let suffix = value
        .strip_prefix("/nix/store/")
        .ok_or_else(|| format!("{context} must begin with /nix/store/"))?;
    let (hash, name) = suffix
        .split_once('-')
        .ok_or_else(|| format!("{context} must contain a Nix store hash and name"))?;
    const NIX_BASE32: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";
    if hash.len() != 32
        || !hash.bytes().all(|byte| NIX_BASE32.contains(&byte))
        || name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"+-._?=".contains(&byte))
    {
        return Err(format!("{context} is not a canonical Nix store path"));
    }
    Ok(())
}

fn validate_named_rows(value: &Value, names: &[&str], context: &str) -> Result<(), String> {
    let rows = array(value, context)?;
    if rows.len() != names.len() {
        return Err(format!(
            "{context} must contain exactly {} rows",
            names.len()
        ));
    }
    let actual = rows
        .iter()
        .map(|row| string(row, "name", context))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let expected = names.iter().copied().collect::<BTreeSet<_>>();
    if actual != expected {
        return Err(format!("{context} names differ: {actual:?}"));
    }
    Ok(())
}

fn validate_locked_inputs(value: &Value) -> Result<(), String> {
    validate_named_rows(value, &["nixpkgs", "rust-overlay"], "nix.locked_inputs")?;
    for (index, row) in array(value, "nix.locked_inputs")?.iter().enumerate() {
        let context = format!("nix.locked_inputs[{index}]");
        exact_keys(row, &["name", "nar_hash", "revision"], &context)?;
        if !is_lower_hex(string(row, "revision", &context)?, 40) {
            return Err(format!(
                "{context} must retain a full revision and NAR hash"
            ));
        }
        require_nix_sha256(
            string(row, "nar_hash", &context)?,
            &format!("{context}.nar_hash"),
        )?;
    }
    Ok(())
}

fn validate_realized_inputs(value: &Value) -> Result<(), String> {
    validate_named_rows(
        value,
        &["cargo-nextest", "coreutils", "rust-script"],
        "nix.realized_inputs",
    )?;
    for (index, row) in array(value, "nix.realized_inputs")?.iter().enumerate() {
        let context = format!("nix.realized_inputs[{index}]");
        exact_keys(
            row,
            &["name", "nar_hash", "nar_size_bytes", "path"],
            &context,
        )?;
        require_nix_store_path(string(row, "path", &context)?, &format!("{context}.path"))?;
        require_nix_sha256(
            string(row, "nar_hash", &context)?,
            &format!("{context}.nar_hash"),
        )?;
        if unsigned(row, "nar_size_bytes", &context)? == 0 {
            return Err(format!(
                "{context} must retain a store path and nonempty NAR"
            ));
        }
    }
    Ok(())
}

fn validate_artifact(artifact: &Value, context: &str) -> Result<(), String> {
    exact_keys(
        artifact,
        &[
            "archive_sha256",
            "archive_size_bytes",
            "config_digest",
            "config_size_bytes",
            "diff_ids",
            "layers",
            "loaded_reference",
            "manifest_digest",
            "manifest_media_type",
            "manifest_size_bytes",
        ],
        context,
    )?;
    require_flat_sha256(
        string(artifact, "archive_sha256", context)?,
        &format!("{context}.archive_sha256"),
    )?;
    if unsigned(artifact, "archive_size_bytes", context)? == 0 {
        return Err(format!("{context}.archive_size_bytes must be nonzero"));
    }
    let manifest = string(artifact, "manifest_digest", context)?;
    require_sha256(manifest, &format!("{context}.manifest_digest"))?;
    require_sha256(
        string(artifact, "config_digest", context)?,
        &format!("{context}.config_digest"),
    )?;
    if string(artifact, "manifest_media_type", context)?
        != "application/vnd.docker.distribution.manifest.v2+json"
        || unsigned(artifact, "manifest_size_bytes", context)? == 0
        || unsigned(artifact, "config_size_bytes", context)? == 0
    {
        return Err(format!(
            "{context} must retain the nonempty Docker-v2 manifest and config descriptors"
        ));
    }
    if string(artifact, "loaded_reference", context)?
        != format!("localhost/hermit-hermetic-validate@{manifest}")
    {
        return Err(format!(
            "{context}.loaded_reference does not name its manifest digest"
        ));
    }
    let layers = array(
        field(artifact, "layers", context)?,
        &format!("{context}.layers"),
    )?;
    let diff_ids = array(
        field(artifact, "diff_ids", context)?,
        &format!("{context}.diff_ids"),
    )?;
    if layers.is_empty() || layers.len() != diff_ids.len() {
        return Err(format!(
            "{context} must have equal nonempty layer and diff-ID lists"
        ));
    }
    for (index, (layer, diff_id)) in layers.iter().zip(diff_ids).enumerate() {
        let layer_context = format!("{context}.layers[{index}]");
        exact_keys(layer, &["digest", "size_bytes"], &layer_context)?;
        let digest = string(layer, "digest", &layer_context)?;
        require_sha256(digest, &format!("{layer_context}.digest"))?;
        if unsigned(layer, "size_bytes", &layer_context)? == 0 {
            return Err(format!("{layer_context}.size_bytes must be nonzero"));
        }
        if diff_id.as_str() != Some(digest) {
            return Err(format!(
                "{context}.diff_ids[{index}] does not match its archive layer"
            ));
        }
    }
    // The recorded manifest must be the compact Docker-v2 manifest those
    // descriptors reconstruct to, in Podman's stored descriptor key
    // order (mediaType, size, digest): both its length and its digest
    // are asserted. Length alone once let a wrong 391 stand for the
    // real 598, and the same 598 bytes in (mediaType, digest, size)
    // order hash to a different digest than the pin (dh-bca16a).
    let reconstructed = reconstruct_manifest(
        string(artifact, "manifest_media_type", context)?,
        string(artifact, "config_digest", context)?,
        unsigned(artifact, "config_size_bytes", context)?,
        layers,
        context,
    )?;
    if reconstructed.len() as u64 != unsigned(artifact, "manifest_size_bytes", context)? {
        return Err(format!(
            "{context}.manifest_size_bytes does not equal the reconstructed manifest length {}",
            reconstructed.len()
        ));
    }
    let reconstructed_digest = format!("sha256:{}", sha256(reconstructed.as_bytes()));
    if reconstructed_digest != manifest {
        return Err(format!(
            "{context}.manifest_digest does not equal the reconstructed manifest digest {reconstructed_digest}"
        ));
    }
    Ok(())
}

fn validate_semantics(root: &Value) -> Result<(), String> {
    exact_keys(
        root,
        &[
            "claim_scope",
            "evidence_limitations",
            "flake_reattestation",
            "method",
            "nix",
            "repin",
            "runs",
            "schema_version",
            "source",
        ],
        "provenance",
    )?;
    let reatt = field(root, "flake_reattestation", "provenance")?;
    exact_keys(
        reatt,
        &[
            "built_flake_sha256",
            "current_flake_sha256",
            "evaluated_commit",
            "evaluated_drv_path",
            "nix_version",
            "stub_text",
        ],
        "flake_reattestation",
    )?;
    if string(reatt, "built_flake_sha256", "flake_reattestation")?
        != "663f05a959d1c741729e0b79a8dc39a7bf55503c98df758a54f3976f99469a53"
        || string(reatt, "stub_text", "flake_reattestation")?
            != "throw \"groups.nix evaluated: monolith depends on groups\"\n"
        || string(reatt, "evaluated_drv_path", "flake_reattestation")?
            != "/nix/store/0am28327vyciiq0s4ahlz03d2kfc2rbk-hermit-hermetic-validate.tar.gz.drv"
        || string(reatt, "nix_version", "flake_reattestation")? != "nix (Nix) 2.36.0"
    {
        return Err("flake_reattestation must record the abort-stub evaluation exactly".into());
    }
    require_flat_sha256(
        string(reatt, "current_flake_sha256", "flake_reattestation")?,
        "flake_reattestation.current_flake_sha256",
    )?;
    if string(reatt, "current_flake_sha256", "flake_reattestation")?
        != "b0ac537cbbf3978e03c7ff2b1980945ec2f677821fcc902fe1d7eb05f53f5393"
    {
        return Err(
            "flake_reattestation.current_flake_sha256 must pin the evaluated b0ac537c flake; a later flake needs a new evaluation"
                .into(),
        );
    }
    let evaluated_commit = string(reatt, "evaluated_commit", "flake_reattestation")?;
    if !is_lower_hex(evaluated_commit, 40) {
        return Err(
            "flake_reattestation.evaluated_commit must be a full lowercase commit hash".into(),
        );
    }
    if unsigned(root, "schema_version", "provenance")? != 1 {
        return Err("provenance.schema_version must be 1".into());
    }
    if string(root, "method", "provenance")? != METHOD {
        return Err(format!("provenance.method must be {METHOD:?}"));
    }

    let claim = field(root, "claim_scope", "provenance")?;
    exact_keys(
        claim,
        &["cross_host_reproducible", "note", "same_host_reproducible"],
        "claim_scope",
    )?;
    if !boolean(claim, "same_host_reproducible", "claim_scope")?
        || boolean(claim, "cross_host_reproducible", "claim_scope")?
        || !string(claim, "note", "claim_scope")?.contains("does not establish cross-host")
    {
        return Err("claim_scope must state same-host evidence without a cross-host claim".into());
    }

    let repin = field(root, "repin", "provenance")?;
    exact_keys(
        repin,
        &[
            "rationale",
            "replacement_reference",
            "superseded_archive_sha256",
            "superseded_reference",
        ],
        "repin",
    )?;
    let old = string(repin, "superseded_reference", "repin")?;
    let new = string(repin, "replacement_reference", "repin")?;
    if old != SUPERSEDED_REFERENCE {
        return Err(format!(
            "repin.superseded_reference must retain the replaced 4e1605ec reference {SUPERSEDED_REFERENCE:?}"
        ));
    }
    let old_archive = string(repin, "superseded_archive_sha256", "repin")?;
    if old_archive != SUPERSEDED_ARCHIVE_SHA256 {
        return Err(format!(
            "repin.superseded_archive_sha256 must retain the 4e1605ec build's df1f0dd4 archive SHA-256 {SUPERSEDED_ARCHIVE_SHA256:?}"
        ));
    }
    if old == new || !string(repin, "rationale", "repin")?.contains(RATIONALE_MARKER) {
        return Err(format!(
            "repin must explain the real change: its rationale must name {RATIONALE_MARKER}, the property this repin repairs"
        ));
    }
    for (reference, context) in [
        (old, "repin.superseded_reference"),
        (new, "repin.replacement_reference"),
    ] {
        let digest = reference
            .strip_prefix("localhost/hermit-hermetic-validate@")
            .ok_or_else(|| format!("{context} must use the canonical local repository name"))?;
        require_sha256(digest, context)?;
    }
    require_flat_sha256(old_archive, "repin.superseded_archive_sha256")?;

    let source = field(root, "source", "provenance")?;
    exact_keys(
        source,
        &[
            "clean",
            "git_archive_sha256",
            "git_archive_size_bytes",
            "git_commit",
            "git_tree",
            "inputs",
            "repository",
        ],
        "source",
    )?;
    if !boolean(source, "clean", "source")? {
        return Err("source.clean must be true".into());
    }
    if string(source, "repository", "source")? != "https://github.com/rrnewton/hermit"
        || unsigned(source, "git_archive_size_bytes", "source")? == 0
    {
        return Err("source must retain the canonical repository and nonempty Git archive".into());
    }
    if !is_lower_hex(string(source, "git_commit", "source")?, 40)
        || !is_lower_hex(string(source, "git_tree", "source")?, 40)
    {
        return Err("source commit and tree must be full lowercase Git object IDs".into());
    }
    require_flat_sha256(
        string(source, "git_archive_sha256", "source")?,
        "source.git_archive_sha256",
    )?;
    let inputs = array(field(source, "inputs", "source")?, "source.inputs")?;
    if inputs.len() != INPUT_PATHS.len() {
        return Err(format!(
            "source.inputs must contain exactly {} files",
            INPUT_PATHS.len()
        ));
    }
    let mut input_paths = BTreeSet::new();
    let mut flake_input_hash = None;
    for (index, input) in inputs.iter().enumerate() {
        let context = format!("source.inputs[{index}]");
        exact_keys(input, &["path", "sha256", "size_bytes"], &context)?;
        let path = string(input, "path", &context)?;
        input_paths.insert(path);
        if path == "ci/hermetic/flake.nix" {
            flake_input_hash = Some(string(input, "sha256", &context)?.to_string());
        }
        require_flat_sha256(
            string(input, "sha256", &context)?,
            &format!("{context}.sha256"),
        )?;
    }
    if input_paths != INPUT_PATHS.into_iter().collect() {
        return Err(format!("source.inputs paths differ: {input_paths:?}"));
    }
    if flake_input_hash.as_deref()
        != Some(string(
            reatt,
            "current_flake_sha256",
            "flake_reattestation",
        )?)
    {
        return Err(
            "flake_reattestation.current_flake_sha256 must equal the flake.nix input hash".into(),
        );
    }

    let nix = field(root, "nix", "provenance")?;
    exact_keys(
        nix,
        &[
            "derivation_path",
            "derivation_sha256",
            "locked_inputs",
            "output_nar_hash",
            "output_nar_size_bytes",
            "output_path",
            "realized_inputs",
            "system",
            "version",
        ],
        "nix",
    )?;
    let nix_version = string(nix, "version", "nix")?;
    if nix_version.is_empty() || string(nix, "system", "nix")? != "x86_64-linux" {
        return Err("nix version status and x86_64-linux system must be explicit".into());
    }
    require_flat_sha256(
        string(nix, "derivation_sha256", "nix")?,
        "nix.derivation_sha256",
    )?;
    let derivation_path = string(nix, "derivation_path", "nix")?;
    require_nix_store_path(derivation_path, "nix.derivation_path")?;
    require_nix_store_path(string(nix, "output_path", "nix")?, "nix.output_path")?;
    require_nix_sha256(
        string(nix, "output_nar_hash", "nix")?,
        "nix.output_nar_hash",
    )?;
    if !derivation_path.ends_with(".drv") || unsigned(nix, "output_nar_size_bytes", "nix")? == 0 {
        return Err("nix derivation and output identities are incomplete".into());
    }
    if string(reatt, "evaluated_drv_path", "flake_reattestation")? != derivation_path {
        return Err(
            "flake_reattestation.evaluated_drv_path must equal the receipt derivation".into(),
        );
    }
    validate_locked_inputs(field(nix, "locked_inputs", "nix")?)?;
    validate_realized_inputs(field(nix, "realized_inputs", "nix")?)?;

    let limitations = array(
        field(root, "evidence_limitations", "provenance")?,
        "evidence_limitations",
    )?;
    let actual_limitations = limitations
        .iter()
        .map(|item| item.as_str().ok_or("evidence limitation must be a string"))
        .collect::<Result<Vec<_>, _>>()?;
    if actual_limitations != EVIDENCE_LIMITATIONS {
        return Err("evidence_limitations must retain every known evidence gap".into());
    }
    if nix_version != RECORDED_NIX_VERSION {
        return Err(format!(
            "nix.version must retain the build-time Nix version both runs recorded, {RECORDED_NIX_VERSION:?}"
        ));
    }

    let runs = array(field(root, "runs", "provenance")?, "runs")?;
    if runs.len() != 2 {
        return Err("runs must contain exactly two independent builds".into());
    }
    let mut run_ids = BTreeSet::new();
    let mut sources = BTreeSet::new();
    let mut stores = BTreeSet::new();
    let mut podman_stores = BTreeSet::new();
    for (index, run) in runs.iter().enumerate() {
        let context = format!("runs[{index}]");
        exact_keys(
            run,
            &[
                "artifact",
                "full_rebuild_observed",
                "nix_store_identity",
                "podman_store_identity",
                "resource_bound",
                "result",
                "run_id",
                "source_snapshot_identity",
            ],
            &context,
        )?;
        let run_id = string(run, "run_id", &context)?;
        let expected = EXPECTED_RUNS
            .iter()
            .find(|candidate| candidate.run_id == run_id)
            .ok_or_else(|| format!("{context}.run_id is not a recorded build service"))?;
        let source_identity = string(run, "source_snapshot_identity", &context)?;
        let store_identity = string(run, "nix_store_identity", &context)?;
        let podman_identity = string(run, "podman_store_identity", &context)?;
        if source_identity != expected.source_snapshot_identity
            || store_identity != expected.nix_store_identity
            || podman_identity != expected.podman_store_identity
        {
            return Err(format!(
                "{context} does not retain the exact source, Nix-store, and Podman-store identities"
            ));
        }
        run_ids.insert(run_id);
        sources.insert(source_identity);
        stores.insert(store_identity);
        podman_stores.insert(podman_identity);
        if !boolean(run, "full_rebuild_observed", &context)? {
            return Err(format!("{context}.full_rebuild_observed must be true"));
        }
        let bound = field(run, "resource_bound", &context)?;
        exact_keys(
            bound,
            &[
                "cpu_quota_cores",
                "memory_max_bytes",
                "swap_max_bytes",
                "wall_seconds",
            ],
            &format!("{context}.resource_bound"),
        )?;
        if unsigned(bound, "wall_seconds", "resource_bound")? != 2400
            || unsigned(bound, "cpu_quota_cores", "resource_bound")? != 2
            || unsigned(bound, "memory_max_bytes", "resource_bound")? != 8_589_934_592
            || unsigned(bound, "swap_max_bytes", "resource_bound")? != 0
        {
            return Err(format!(
                "{context} does not retain the exact resource bound"
            ));
        }
        let result = field(run, "result", &context)?;
        exact_keys(
            result,
            &[
                "exit_status",
                "last_observed_cpu_nanoseconds",
                "last_observed_memory_peak_bytes",
                "oom_kills",
                "service_result",
            ],
            &format!("{context}.result"),
        )?;
        if string(result, "service_result", "result")? != "success"
            || unsigned(result, "exit_status", "result")? != 0
            || unsigned(result, "oom_kills", "result")? != 0
            || unsigned(result, "last_observed_memory_peak_bytes", "result")?
                != expected.memory_peak_bytes
            || unsigned(result, "last_observed_cpu_nanoseconds", "result")?
                != expected.cpu_nanoseconds
        {
            return Err(format!(
                "{context} does not retain the exact successful resource result"
            ));
        }
        validate_artifact(
            field(run, "artifact", &context)?,
            &format!("{context}.artifact"),
        )?;
    }
    if run_ids.len() != 2 || sources.len() != 2 || stores.len() != 2 || podman_stores.len() != 2 {
        return Err(
            "the two builds must have distinct run, source, Nix-store, and Podman-store identities"
                .into(),
        );
    }
    let first_artifact = field(&runs[0], "artifact", "runs[0]")?;
    let second_artifact = field(&runs[1], "artifact", "runs[1]")?;
    if first_artifact != second_artifact {
        return Err("the two builds must have identical archive, manifest, config, layers, and loaded digest".into());
    }
    if string(first_artifact, "loaded_reference", "runs[0].artifact")? != new {
        return Err("repin replacement_reference does not match the reproduced artifact".into());
    }
    Ok(())
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn verify_files(repo: &Path, root: &Value) -> Result<(), String> {
    let pin = fs::read_to_string(repo.join(PIN))
        .map_err(|error| format!("cannot read {PIN}: {error}"))?;
    let expected_pin = string(
        field(root, "repin", "provenance")?,
        "replacement_reference",
        "repin",
    )?;
    if pin.trim() != expected_pin {
        return Err(format!(
            "{PIN} is {:?}, expected {expected_pin:?}; restore the proven pin",
            pin.trim()
        ));
    }
    let source = field(root, "source", "provenance")?;
    for (index, input) in array(field(source, "inputs", "source")?, "source.inputs")?
        .iter()
        .enumerate()
    {
        let context = format!("source.inputs[{index}]");
        let path = string(input, "path", &context)?;
        let bytes =
            fs::read(repo.join(path)).map_err(|error| format!("cannot read {path}: {error}"))?;
        let expected_hash = string(input, "sha256", &context)?;
        let expected_size = unsigned(input, "size_bytes", &context)?;
        if sha256(&bytes) != expected_hash || bytes.len() as u64 != expected_size {
            return Err(format!(
                "{path} differs from the proven build input; rebuild twice and replace the provenance together with the pin"
            ));
        }
    }
    Ok(())
}

fn repo_root() -> Result<PathBuf, String> {
    let script = Path::new(file!());
    script
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()
        .map_err(|error| format!("cannot resolve repository root: {error}"))
}

const IMAGES_DIR: &str = "ci/hermetic/images";
const COMPOSED_METHOD: &str = "two-store-same-host-v1";
const COMPOSED_INPUT_PATHS: [&str; 4] = [
    "ci/hermetic/groups.nix",
    "ci/hermetic/flake.nix",
    "ci/hermetic/flake.lock",
    "ci/hermetic/guest-paths.txt",
];
const COMPOSED_NAMES: [&str; 5] = [
    "hermit-hermetic-mini",
    "hermit-hermetic-mini-crypto",
    "hermit-hermetic-mini-python",
    "hermit-hermetic-mini-runner",
    "hermit-hermetic-mini-suite",
];
const COMPOSED_EVIDENCE_LIMITATIONS: [&str; 4] = [
    "Builds came from earlier lane definitions and are equal by drvPath and outPath at the receipted definitions; definition_commit names the commit whose tree holds those definitions.",
    "The default-store run is a pre-existing step-1 build in a long-lived store, not a fresh store; the isolated run is the fresh-store control.",
    "No systemd resource bounds were applied and no realized input was forcibly rebuilt; the isolated store substituted only from cache.nixos.org.",
    "Build logs were retained only on the build host scratch; this receipt does not authenticate them. Repin-grade systemd dual builds remain required before any consumer selects these images.",
];
/// Expected ordered parents per composed image (immediate parent first).
/// Only the L0 base may have none. This is the completeness source the
/// prefix check enforces; receipts may not opt out by declaring fewer.
fn expected_parents(name: &str) -> &'static [&'static str] {
    match name {
        "hermit-hermetic-mini" => &[],
        "hermit-hermetic-mini-crypto" => &["mini"],
        "hermit-hermetic-mini-python" => &["mini"],
        "hermit-hermetic-mini-runner" => &["mini"],
        "hermit-hermetic-mini-suite" => &["mini-runner", "mini"],
        _ => &[],
    }
}

/// The compact Docker-v2 manifest in Podman's stored descriptor key
/// order (mediaType, size, digest), shared by both validators.
fn reconstruct_manifest(
    manifest_media_type: &str,
    config_digest: &str,
    config_size: u64,
    layers: &[Value],
    context: &str,
) -> Result<String, String> {
    let mut s = format!(
        "{{\"schemaVersion\":2,\"mediaType\":\"{manifest_media_type}\",\"config\":{{\"mediaType\":\"application/vnd.docker.container.image.v1+json\",\"size\":{config_size},\"digest\":\"{config_digest}\"}},\"layers\":["
    );
    for (i, layer) in layers.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            "{{\"mediaType\":\"application/vnd.docker.image.rootfs.diff.tar.gzip\",\"size\":{},\"digest\":\"{}\"}}",
            unsigned(layer, "size_bytes", &format!("{context}.layers[{i}]"))?,
            string(layer, "digest", &format!("{context}.layers[{i}]"))?
        ));
    }
    s.push_str("]}");
    Ok(s)
}

fn layer_digests(image: &Value, context: &str) -> Result<Vec<String>, String> {
    array(field(image, "layers", context)?, context)?
        .iter()
        .map(|layer| Ok(string(layer, "digest", context)?.to_string()))
        .collect()
}

fn validate_composed_image(root: &Value, expected_name: &str) -> Result<Value, String> {
    let ctx = format!("composed {expected_name}");
    exact_keys(
        root,
        &[
            "artifact",
            "claim_scope",
            "evidence_limitations",
            "image",
            "method",
            "nix",
            "runs",
            "schema_version",
            "source",
        ],
        &ctx,
    )?;
    let limits = array(field(root, "evidence_limitations", &ctx)?, &ctx)?;
    let actual_limits = limits
        .iter()
        .map(|item| {
            item.as_str()
                .ok_or("composed evidence limitation must be a string")
        })
        .collect::<Result<Vec<_>, _>>()?;
    if actual_limits != COMPOSED_EVIDENCE_LIMITATIONS {
        return Err(format!(
            "{ctx} evidence_limitations must retain every known evidence gap"
        ));
    }
    if unsigned(root, "schema_version", &ctx)? != 1
        || string(root, "method", &ctx)? != COMPOSED_METHOD
    {
        return Err(format!(
            "{ctx} must retain schema 1 and method {COMPOSED_METHOD:?}"
        ));
    }
    let claim = field(root, "claim_scope", &ctx)?;
    exact_keys(
        claim,
        &["cross_host_reproducible", "note", "same_host_reproducible"],
        &ctx,
    )?;
    if !boolean(claim, "same_host_reproducible", &ctx)?
        || boolean(claim, "cross_host_reproducible", &ctx)?
        || !string(claim, "note", &ctx)?.contains("does not establish cross-host")
    {
        return Err(format!(
            "{ctx} claim_scope must state same-host evidence only"
        ));
    }
    let image = field(root, "image", &ctx)?;
    exact_keys(
        image,
        &[
            "config_digest",
            "config_size_bytes",
            "flake_output",
            "layers",
            "manifest_digest",
            "name",
            "parents",
            "reference",
        ],
        &ctx,
    )?;
    if string(image, "name", &ctx)? != expected_name {
        return Err(format!("{ctx} image.name does not match its file"));
    }
    let manifest = string(image, "manifest_digest", &ctx)?;
    require_sha256(manifest, &format!("{ctx}.manifest_digest"))?;
    require_sha256(
        string(image, "config_digest", &ctx)?,
        &format!("{ctx}.config_digest"),
    )?;
    if string(image, "reference", &ctx)? != format!("localhost/{expected_name}@{manifest}") {
        return Err(format!("{ctx} reference does not name its manifest digest"));
    }
    let layers = array(field(image, "layers", &ctx)?, &ctx)?;
    if layers.is_empty() || layers.len() % 2 != 0 {
        return Err(format!("{ctx} must have store+customisation layer pairs"));
    }
    for (i, layer) in layers.iter().enumerate() {
        let lc = format!("{ctx}.layers[{i}]");
        exact_keys(
            layer,
            &["digest", "group", "runner_only", "size_bytes"],
            &lc,
        )?;
        require_sha256(string(layer, "digest", &lc)?, &lc)?;
        if unsigned(layer, "size_bytes", &lc)? == 0 || string(layer, "group", &lc)?.is_empty() {
            return Err(format!("{lc} must name a group and a nonzero size"));
        }
        let _ = boolean(layer, "runner_only", &lc)?;
    }
    let reconstructed = reconstruct_manifest(
        "application/vnd.docker.distribution.manifest.v2+json",
        string(image, "config_digest", &ctx)?,
        unsigned(image, "config_size_bytes", &ctx)?,
        layers,
        &ctx,
    )?;
    let digest = format!("sha256:{}", sha256(reconstructed.as_bytes()));
    if digest != manifest {
        return Err(format!(
            "{ctx}.manifest_digest does not equal the reconstructed manifest digest {digest}"
        ));
    }
    let artifact = field(root, "artifact", &ctx)?;
    exact_keys(artifact, &["archive_sha256", "archive_size_bytes"], &ctx)?;
    let archive_sha = string(artifact, "archive_sha256", &ctx)?;
    require_flat_sha256(archive_sha, &ctx)?;
    if unsigned(artifact, "archive_size_bytes", &ctx)? == 0 {
        return Err(format!("{ctx} archive size must be nonzero"));
    }
    let runs = array(field(root, "runs", &ctx)?, &ctx)?;
    if runs.len() != 2 {
        return Err(format!("{ctx} runs must contain exactly two builds"));
    }
    let mut stores = BTreeSet::new();
    for run in runs {
        exact_keys(run, &["archive_sha256", "store"], &ctx)?;
        if string(run, "archive_sha256", &ctx)? != archive_sha {
            return Err(format!("{ctx} run archive differs from artifact"));
        }
        stores.insert(string(run, "store", &ctx)?);
    }
    if stores != ["default", "isolated"].into_iter().collect() {
        return Err(format!("{ctx} runs must be default plus isolated stores"));
    }
    Ok(root.clone())
}

fn validate_locked_inputs_against_lock(lock: &Value, locked: &Value) -> Result<(), String> {
    let nodes = field(lock, "nodes", "flake.lock")?;
    for item in array(locked, "composed.locked_inputs")? {
        exact_keys(
            item,
            &["name", "nar_hash", "revision"],
            "composed.locked_inputs",
        )?;
        let name = string(item, "name", "composed.locked_inputs")?;
        let node = field(nodes, name, "flake.lock.nodes")?;
        let locked_node = field(node, "locked", "flake.lock.nodes")?;
        if string(locked_node, "rev", "flake.lock")? != string(item, "revision", name)?
            || string(locked_node, "narHash", "flake.lock")? != string(item, "nar_hash", name)?
        {
            return Err(format!(
                "composed locked_input {name} differs from flake.lock"
            ));
        }
    }
    Ok(())
}

fn verify_composed(repo: &Path) -> Result<(), String> {
    let dir = repo.join(IMAGES_DIR);
    if !dir.is_dir() {
        return Err(format!(
            "{IMAGES_DIR} is missing; every composed receipt must be present"
        ));
    }
    let mut present = BTreeSet::new();
    let entries =
        fs::read_dir(&dir).map_err(|error| format!("cannot read {}: {error}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot list {IMAGES_DIR}: {error}"))?;
        present.insert(entry.file_name().to_string_lossy().into_owned());
    }
    let expected: BTreeSet<String> = COMPOSED_NAMES
        .iter()
        .map(|name| {
            format!(
                "{}.json",
                name.strip_prefix("hermit-hermetic-").unwrap_or(name)
            )
        })
        .collect();
    if present != expected {
        return Err(format!(
            "{IMAGES_DIR} files differ from the expected receipts: {present:?}"
        ));
    }
    let mut images: Vec<(String, Value)> = Vec::new();
    for name in COMPOSED_NAMES {
        let file = name.strip_prefix("hermit-hermetic-").unwrap_or(name);
        let path = dir.join(format!("{file}.json"));
        let bytes =
            fs::read(&path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        let root =
            parse_provenance(&bytes).map_err(|error| format!("{}: {error}", path.display()))?;
        images.push((name.to_string(), validate_composed_image(&root, name)?));
    }
    // Live inputs: same four files, same hashes in every receipt.
    let mut expected_inputs: Option<Vec<(String, String, u64)>> = None;
    let mut expected_locked: Option<Value> = None;
    let mut expected_commit: Option<String> = None;
    for (name, root) in &images {
        let source = field(root, "source", name)?;
        exact_keys(source, &["definition_commit", "inputs", "repository"], name)?;
        if string(source, "repository", name)? != "https://github.com/rrnewton/hermit" {
            return Err(format!("{name} source.repository must stay canonical"));
        }
        let commit = string(source, "definition_commit", name)?;
        if !is_lower_hex(commit, 40) {
            return Err(format!(
                "{name} source.definition_commit must be a full lowercase commit hash"
            ));
        }
        match &expected_commit {
            None => expected_commit = Some(commit.to_string()),
            Some(prev) if prev != commit => {
                return Err(format!(
                    "{name} definition_commit differs across composed receipts"
                ));
            }
            _ => {}
        }
        let inputs = array(field(source, "inputs", name)?, name)?;
        let mut seen = Vec::new();
        for (i, input) in inputs.iter().enumerate() {
            let ic = format!("{name}.inputs[{i}]");
            exact_keys(input, &["path", "sha256", "size_bytes"], &ic)?;
            let path = string(input, "path", &ic)?;
            let hash = string(input, "sha256", &ic)?;
            require_flat_sha256(hash, &ic)?;
            let size = unsigned(input, "size_bytes", &ic)?;
            let live = fs::read(repo.join(path))
                .map_err(|error| format!("cannot read {path}: {error}"))?;
            if sha256(&live) != hash || live.len() as u64 != size {
                return Err(format!(
                    "{path} differs from the proven composed build input in {name}"
                ));
            }
            seen.push((path.to_string(), hash.to_string(), size));
        }
        let paths: BTreeSet<&str> = seen.iter().map(|(p, _, _)| p.as_str()).collect();
        if paths != COMPOSED_INPUT_PATHS.into_iter().collect() {
            return Err(format!("{name} source.inputs paths differ: {paths:?}"));
        }
        match &expected_inputs {
            None => expected_inputs = Some(seen),
            Some(prev) if *prev != seen => {
                return Err(format!(
                    "{name} source.inputs differ across composed receipts"
                ));
            }
            _ => {}
        }
        let nix = field(root, "nix", name)?;
        exact_keys(
            nix,
            &[
                "derivation_path",
                "locked_inputs",
                "output_nar_hash",
                "output_nar_size_bytes",
                "output_path",
                "system",
                "version",
            ],
            name,
        )?;
        if string(nix, "system", name)? != "x86_64-linux"
            || string(nix, "version", name)?.is_empty()
        {
            return Err(format!("{name} nix version/system must be explicit"));
        }
        require_nix_store_path(string(nix, "derivation_path", name)?, name)?;
        require_nix_store_path(string(nix, "output_path", name)?, name)?;
        require_nix_sha256(string(nix, "output_nar_hash", name)?, name)?;
        validate_locked_inputs(field(nix, "locked_inputs", name)?)?;
        let locked = field(nix, "locked_inputs", name)?.clone();
        match &expected_locked {
            None => expected_locked = Some(locked),
            Some(prev) if *prev != locked => {
                return Err(format!(
                    "{name} locked_inputs differ across composed receipts"
                ));
            }
            _ => {}
        }
    }
    // Locked inputs must also equal the live flake.lock.
    if let Some(locked) = &expected_locked {
        let lock_bytes = fs::read(repo.join("ci/hermetic/flake.lock"))
            .map_err(|error| format!("cannot read ci/hermetic/flake.lock: {error}"))?;
        let lock_root = parse_provenance(&lock_bytes)?;
        validate_locked_inputs_against_lock(&lock_root, locked)?;
    }
    // Identity binding + distinctness: drv/out/archive/config/manifest
    // must name their own image and differ across receipts.
    let mut seen_ids: Vec<(&str, &str, String)> = Vec::new();
    for (name, root) in &images {
        let shortn = name.strip_prefix("hermit-hermetic-").unwrap_or(name);
        let image = field(root, "image", name)?;
        let nix = field(root, "nix", name)?;
        let artifact = field(root, "artifact", name)?;
        let checks = [
            (
                "derivation",
                string(nix, "derivation_path", name)?.to_string(),
            ),
            ("output", string(nix, "output_path", name)?.to_string()),
            (
                "archive",
                string(artifact, "archive_sha256", name)?.to_string(),
            ),
            ("config", string(image, "config_digest", name)?.to_string()),
            (
                "manifest",
                string(image, "manifest_digest", name)?.to_string(),
            ),
        ];
        if !checks[0]
            .1
            .ends_with(&format!("-hermit-hermetic-{shortn}.tar.gz.drv"))
            || !checks[1]
                .1
                .ends_with(&format!("-hermit-hermetic-{shortn}.tar.gz"))
            || string(image, "flake_output", name)?
                != format!("hermeticImages.x86_64-linux.{shortn}")
        {
            return Err(format!(
                "{name} derivation, output or flake_output does not name this image"
            ));
        }
        for (kind, value) in checks {
            if seen_ids.iter().any(|(_, k, v)| *k == kind && *v == value) {
                return Err(format!("{name} {kind} duplicates another receipt"));
            }
            seen_ids.push((name, kind, value));
        }
    }
    // Group/digest bijection and per-group runner_only across receipts.
    let mut group_digest: std::collections::BTreeMap<String, String> = Default::default();
    let mut digest_group: std::collections::BTreeMap<String, String> = Default::default();
    let mut group_runner: std::collections::BTreeMap<String, bool> = Default::default();
    for (name, root) in &images {
        let image = field(root, "image", name)?;
        for layer in array(field(image, "layers", name)?, name)? {
            let group = string(layer, "group", name)?.to_string();
            let digest = string(layer, "digest", name)?.to_string();
            let runner = boolean(layer, "runner_only", name)?;
            match group_digest.get(&group) {
                Some(prev) if *prev != digest => {
                    return Err(format!("group {group} names two digests across receipts"));
                }
                _ => {
                    group_digest.insert(group.clone(), digest.clone());
                }
            }
            match digest_group.get(&digest) {
                Some(prev) if *prev != group => {
                    return Err(format!("digest {digest} names two groups across receipts"));
                }
                _ => {
                    digest_group.insert(digest, group.clone());
                }
            }
            match group_runner.get(&group) {
                Some(prev) if *prev != runner => {
                    return Err(format!("group {group} runner_only differs across receipts"));
                }
                _ => {
                    group_runner.insert(group, runner);
                }
            }
        }
    }
    // Complete parent chain: declared parents must equal expected, each
    // parent's layers prefix the child's, and each image is its
    // immediate parent plus exactly one store+customisation pair.
    let by_name: std::collections::BTreeMap<&str, &Value> = images
        .iter()
        .map(|(n, r)| (n.as_str(), field(r, "image", n).unwrap()))
        .collect();
    for (name, root) in &images {
        let image = field(root, "image", name)?;
        let declared: Vec<&str> = array(field(image, "parents", name)?, name)?
            .iter()
            .map(|p| {
                p.as_str()
                    .ok_or_else(|| format!("{name} parents must be strings"))
            })
            .collect::<Result<_, _>>()?;
        if declared.as_slice() != expected_parents(name) {
            return Err(format!(
                "{name} parents {declared:?} differ from expected {:?}",
                expected_parents(name)
            ));
        }
        let mine = layer_digests(image, name)?;
        if declared.is_empty() {
            if mine.len() != 2 {
                return Err(format!(
                    "{name} base image must have exactly one layer pair"
                ));
            }
            continue;
        }
        for parent in &declared {
            let parent_image = by_name
                .get(format!("hermit-hermetic-{parent}").as_str())
                .ok_or_else(|| format!("{name} names unknown parent {parent:?}"))?;
            let theirs = layer_digests(parent_image, name)?;
            if mine.len() < theirs.len() || mine[..theirs.len()] != theirs[..] {
                return Err(format!(
                    "{name} layers do not start with parent {parent} layers"
                ));
            }
        }
        let immediate = by_name
            .get(format!("hermit-hermetic-{}", declared[0]).as_str())
            .unwrap();
        if mine.len() != layer_digests(immediate, name)?.len() + 2 {
            return Err(format!(
                "{name} must be its immediate parent plus one layer pair"
            ));
        }
    }
    Ok(())
}

fn verify(repo: &Path) -> Result<(), String> {
    let bytes = fs::read(repo.join(PROVENANCE))
        .map_err(|error| format!("cannot read {PROVENANCE}: {error}"))?;
    let root = parse_provenance(&bytes)?;
    validate_semantics(&root)?;
    verify_files(repo, &root)?;
    verify_composed(repo)?;
    Ok(())
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    let args = env::args().skip(1).collect::<Vec<_>>();
    if matches!(args.as_slice(), [arg] if arg == "-h" || arg == "--help") {
        usage();
        return ExitCode::SUCCESS;
    }
    if !args.is_empty() {
        eprintln!("check-image-provenance: unexpected arguments; run --help for usage");
        return ExitCode::from(2);
    }
    let result = repo_root().and_then(|repo| verify(&repo));
    match result {
        Ok(()) => {
            println!(
                "check-image-provenance: PASS -- tracked pin, source inputs, dual-build receipt, and 5 composed receipts are internally consistent; historical evidence remains review-bound"
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("check-image-provenance: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> Value {
        parse_provenance(include_bytes!("image.provenance.json")).unwrap()
    }

    fn mutate<'a>(root: &'a mut Value, pointer: &str) -> &'a mut Value {
        root.pointer_mut(pointer).unwrap()
    }

    #[test]
    fn accepts_two_distinct_equal_builds() {
        validate_semantics(&record()).unwrap();
    }

    #[test]
    fn rejects_edited_flake_reattestation() {
        for (pointer, value, needle) in [
            (
                "/flake_reattestation/current_flake_sha256",
                Value::String("a".repeat(64)),
                "must pin the evaluated b0ac537c flake",
            ),
            (
                "/flake_reattestation/evaluated_drv_path",
                Value::String(
                    "/nix/store/00000000000000000000000000000000-hermit-hermetic-validate.tar.gz.drv"
                        .into(),
                ),
                "must record the abort-stub evaluation exactly",
            ),
            (
                "/flake_reattestation/stub_text",
                Value::String("throw \"other\"\n".into()),
                "must record the abort-stub evaluation exactly",
            ),
            (
                "/flake_reattestation/evaluated_commit",
                Value::String("1234".into()),
                "full lowercase commit hash",
            ),
        ] {
            let mut wrong = record();
            *mutate(&mut wrong, pointer) = value;
            assert!(
                validate_semantics(&wrong).unwrap_err().contains(needle),
                "{pointer}: want {needle:?}"
            );
        }
        // A syntactically valid but different drv on the receipt side
        // fails the equality with the re-attested evaluation.
        let mut wrong = record();
        *mutate(&mut wrong, "/nix/derivation_path") = Value::String(
            "/nix/store/11111111111111111111111111111111-hermit-hermetic-validate.tar.gz.drv"
                .into(),
        );
        assert!(
            validate_semantics(&wrong)
                .unwrap_err()
                .contains("must equal the receipt derivation")
        );
    }

    #[test]
    fn composed_receipt_validates_and_rejects_edited_manifest() {
        let mini = parse_provenance(include_bytes!("images/mini.json")).unwrap();
        validate_composed_image(&mini, "hermit-hermetic-mini").unwrap();
        let mut wrong = mini;
        *mutate(&mut wrong, "/image/manifest_digest") =
            Value::String(format!("sha256:{}", "a".repeat(64)));
        *mutate(&mut wrong, "/image/reference") = Value::String(format!(
            "localhost/hermit-hermetic-mini@sha256:{}",
            "a".repeat(64)
        ));
        assert!(
            validate_composed_image(&wrong, "hermit-hermetic-mini")
                .unwrap_err()
                .contains("does not equal the reconstructed manifest digest")
        );
        let repo = repo_root().unwrap();
        verify_composed(&repo).unwrap();
    }

    fn composed_fixture(mutate_file: &str, pointer: &str, value: Value) -> tempfile::TempDir {
        let root = repo_root().unwrap();
        let fixture = tempfile::tempdir().unwrap();
        let mut files: Vec<String> = INPUT_PATHS
            .into_iter()
            .chain([PIN, PROVENANCE, "ci/hermetic/groups.nix"])
            .map(str::to_string)
            .collect();
        for name in COMPOSED_NAMES {
            let short = name.strip_prefix("hermit-hermetic-").unwrap_or(name);
            files.push(format!("{IMAGES_DIR}/{short}.json"));
        }
        for path in files {
            let destination = fixture.path().join(&path);
            fs::create_dir_all(destination.parent().unwrap()).unwrap();
            if path == mutate_file {
                let mut value_json =
                    parse_provenance(&fs::read(root.join(&path)).unwrap()).unwrap();
                *mutate(&mut value_json, pointer) = value.clone();
                fs::write(
                    &destination,
                    serde_json::to_vec_pretty(&value_json).unwrap(),
                )
                .unwrap();
            } else {
                fs::copy(root.join(&path), destination).unwrap();
            }
        }
        fixture
    }

    #[test]
    fn rejects_composed_guard_violations() {
        // Each case must fail with its guard's own error text; this is
        // the unit-level mirror of the seat mutation list.
        let cases: Vec<(&str, &str, Value, &str)> = vec![
            (
                "ci/hermetic/images/mini-suite.json",
                "/image/parents",
                Value::Array(vec![]),
                "differ from expected",
            ),
            (
                "ci/hermetic/images/mini-suite.json",
                "/image/layers/4/group",
                Value::String("wrong-store".into()),
                "names two groups",
            ),
            (
                "ci/hermetic/images/mini-crypto.json",
                "/nix/output_path",
                Value::String(
                    "/nix/store/pwgfzq71viss2aj11n6952vb8qxc619q-hermit-hermetic-mini.tar.gz"
                        .into(),
                ),
                "does not name this image",
            ),
            (
                "ci/hermetic/images/mini-crypto.json",
                "/artifact/archive_sha256",
                Value::String(
                    "a04405d8610cabe9667ca05a5f3816bc7db7b818802c7f7d5eb908dd1a6b7810".into(),
                ),
                "run archive differs",
            ),
            (
                "ci/hermetic/images/mini.json",
                "/source/definition_commit",
                Value::String("garbage".into()),
                "full lowercase commit hash",
            ),
            (
                "ci/hermetic/images/mini.json",
                "/evidence_limitations",
                Value::Array(vec![]),
                "retain every known evidence gap",
            ),
            (
                "ci/hermetic/images/mini-runner.json",
                "/image/layers/2/runner_only",
                Value::Bool(false),
                "runner_only differs",
            ),
        ];
        for (file, pointer, value, needle) in cases {
            let fixture = composed_fixture(file, pointer, value);
            let err = verify_composed(fixture.path()).unwrap_err();
            assert!(
                err.contains(needle),
                "{file} {pointer}: want {needle:?} in {err:#}"
            );
        }
        // Missing directory and unknown extra file fail closed.
        let empty = tempfile::tempdir().unwrap();
        assert!(
            verify_composed(empty.path())
                .unwrap_err()
                .contains("is missing")
        );
        let fixture = composed_fixture("", "", Value::Null);
        fs::write(
            fixture.path().join(IMAGES_DIR).join("mini-cc.json"),
            b"not json",
        )
        .unwrap();
        assert!(
            verify_composed(fixture.path())
                .unwrap_err()
                .contains("files differ")
        );
    }

    #[test]
    fn accepts_rebased_unrelated_head_with_identical_image_inputs() {
        let root = repo_root().unwrap();
        let fixture = tempfile::tempdir().unwrap();
        for path in INPUT_PATHS.into_iter().chain([PIN, PROVENANCE]) {
            let destination = fixture.path().join(path);
            fs::create_dir_all(destination.parent().unwrap()).unwrap();
            fs::copy(root.join(path), destination).unwrap();
        }
        // Composed receipts are fail-closed now: the fixture must carry
        // groups.nix and every composed receipt, not skip that half.
        let mut composed_files: Vec<String> = vec!["ci/hermetic/groups.nix".to_string()];
        for name in COMPOSED_NAMES {
            let short = name.strip_prefix("hermit-hermetic-").unwrap_or(name);
            composed_files.push(format!("{IMAGES_DIR}/{short}.json"));
        }
        for path in composed_files {
            let destination = fixture.path().join(&path);
            fs::create_dir_all(destination.parent().unwrap()).unwrap();
            fs::copy(root.join(&path), destination).unwrap();
        }
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.name", "Provenance Test"],
            vec!["config", "user.email", "provenance@example.invalid"],
            vec!["add", "."],
            vec!["commit", "-q", "-m", "unrelated rebased head"],
        ] {
            let mut git = Command::new("git");
            // Git exports its repository-location variables to hooks and `git
            // rebase --exec` steps, and they override the working directory.
            // Inherited, they would point this fixture's `git init`, `git
            // config` and commit at the caller's repository
            // (https://github.com/rrnewton/hermit/issues/3362).
            for name in [
                "GIT_DIR",
                "GIT_WORK_TREE",
                "GIT_INDEX_FILE",
                "GIT_COMMON_DIR",
                "GIT_OBJECT_DIRECTORY",
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                "GIT_NAMESPACE",
                "GIT_PREFIX",
            ] {
                git.env_remove(name);
            }
            assert!(
                git.args(args)
                    .current_dir(fixture.path())
                    .status()
                    .unwrap()
                    .success()
            );
        }

        // The recorded build-source commit is deliberately absent from this
        // fixture repository. Current input equivalence, not rewritten
        // ancestry, is the live check.
        verify(fixture.path()).unwrap();
    }

    #[test]
    fn rejects_one_or_duplicate_store() {
        let mut one = record();
        mutate(&mut one, "/runs").as_array_mut().unwrap().pop();
        assert!(
            validate_semantics(&one)
                .unwrap_err()
                .contains("exactly two")
        );

        let mut duplicate = record();
        let first = duplicate
            .pointer("/runs/0/nix_store_identity")
            .unwrap()
            .clone();
        *mutate(&mut duplicate, "/runs/1/nix_store_identity") = first;
        assert!(
            validate_semantics(&duplicate)
                .unwrap_err()
                .contains("exact source")
        );
    }

    #[test]
    fn rejects_duplicate_or_unknown_json_key() {
        let duplicate = br#"{"schema_version":1,"schema_version":1}"#;
        assert!(
            parse_provenance(duplicate)
                .unwrap_err()
                .contains("duplicate JSON object key \"schema_version\"")
        );

        let mut unknown = record();
        unknown
            .as_object_mut()
            .unwrap()
            .insert("unreviewed_claim".into(), Value::Bool(true));
        assert!(
            validate_semantics(&unknown)
                .unwrap_err()
                .contains("fields differ")
        );
    }

    #[test]
    fn rejects_changed_superseded_reference_or_archive() {
        let mut reference = record();
        *mutate(&mut reference, "/repin/superseded_reference") = Value::String(format!(
            "localhost/hermit-hermetic-validate@sha256:{}",
            "a".repeat(64)
        ));
        assert!(
            validate_semantics(&reference)
                .unwrap_err()
                .contains("must retain the replaced 4e1605ec reference")
        );

        let mut archive = record();
        *mutate(&mut archive, "/repin/superseded_archive_sha256") = Value::String("a".repeat(64));
        assert!(
            validate_semantics(&archive)
                .unwrap_err()
                .contains("must retain the 4e1605ec build's df1f0dd4 archive SHA-256")
        );
    }

    #[test]
    fn rejects_a_manifest_length_that_is_not_the_reconstructed_length() {
        let mut wrong = record();
        *mutate(&mut wrong, "/runs/0/artifact/manifest_size_bytes") = Value::from(391);
        assert!(
            validate_semantics(&wrong)
                .unwrap_err()
                .contains("does not equal the reconstructed manifest length")
        );
    }

    #[test]
    fn rejects_a_manifest_digest_that_is_not_the_reconstructed_digest() {
        // Red control A: both runs record the digest of the same 598
        // bytes serialized in the wrong descriptor key order
        // (mediaType, digest, size). Length, run equality and shape
        // checks all pass; only the reconstructed-digest assert fails.
        let wrong_order = "sha256:4d02d3a7cd7a53bbe2692e675bfc9331cf942b558eb7cf7e99436c5936e16bd6";
        let mut altered_digest = record();
        for run in 0..2 {
            *mutate(
                &mut altered_digest,
                &format!("/runs/{run}/artifact/manifest_digest"),
            ) = Value::String(wrong_order.into());
            *mutate(
                &mut altered_digest,
                &format!("/runs/{run}/artifact/loaded_reference"),
            ) = Value::String(format!("localhost/hermit-hermetic-validate@{wrong_order}"));
        }
        *mutate(&mut altered_digest, "/repin/replacement_reference") =
            Value::String(format!("localhost/hermit-hermetic-validate@{wrong_order}"));
        assert!(
            validate_semantics(&altered_digest)
                .unwrap_err()
                .contains("does not equal the reconstructed manifest digest")
        );

        // Red control B: reordered layer descriptors (same length,
        // same multiset) in both runs.
        let base = record();
        let layer0 = base.pointer("/runs/0/artifact/layers/0").unwrap().clone();
        let layer1 = base.pointer("/runs/0/artifact/layers/1").unwrap().clone();
        let mut reordered = record();
        for run in 0..2 {
            *mutate(&mut reordered, &format!("/runs/{run}/artifact/layers/0")) = layer1.clone();
            *mutate(&mut reordered, &format!("/runs/{run}/artifact/layers/1")) = layer0.clone();
            *mutate(&mut reordered, &format!("/runs/{run}/artifact/diff_ids/0")) =
                layer1.pointer("/digest").unwrap().clone();
            *mutate(&mut reordered, &format!("/runs/{run}/artifact/diff_ids/1")) =
                layer0.pointer("/digest").unwrap().clone();
        }
        assert!(
            validate_semantics(&reordered)
                .unwrap_err()
                .contains("does not equal the reconstructed manifest digest")
        );

        // Red control C: an altered descriptor size with the same
        // digit count, so the reconstructed length is unchanged.
        let config_size = base
            .pointer("/runs/0/artifact/config_size_bytes")
            .unwrap()
            .as_u64()
            .unwrap();
        let mut altered_size = record();
        for run in 0..2 {
            *mutate(
                &mut altered_size,
                &format!("/runs/{run}/artifact/config_size_bytes"),
            ) = Value::from(config_size + 1);
        }
        assert!(
            validate_semantics(&altered_size)
                .unwrap_err()
                .contains("does not equal the reconstructed manifest digest")
        );
    }

    #[test]
    fn rejects_differing_archive_config_layer_or_digest() {
        let mut archive = record();
        *mutate(&mut archive, "/runs/1/artifact/archive_sha256") = Value::String("a".repeat(64));
        assert!(validate_semantics(&archive).is_err());

        let mut config = record();
        *mutate(&mut config, "/runs/1/artifact/config_digest") =
            Value::String(format!("sha256:{}", "a".repeat(64)));
        assert!(validate_semantics(&config).is_err());

        let mut layer = record();
        let different_layer = Value::String(format!("sha256:{}", "a".repeat(64)));
        *mutate(&mut layer, "/runs/1/artifact/layers/0/digest") = different_layer.clone();
        *mutate(&mut layer, "/runs/1/artifact/diff_ids/0") = different_layer;
        assert!(validate_semantics(&layer).is_err());

        let mut manifest = record();
        let different_manifest = format!("sha256:{}", "a".repeat(64));
        *mutate(&mut manifest, "/runs/1/artifact/manifest_digest") =
            Value::String(different_manifest.clone());
        *mutate(&mut manifest, "/runs/1/artifact/loaded_reference") = Value::String(format!(
            "localhost/hermit-hermetic-validate@{different_manifest}"
        ));
        assert!(validate_semantics(&manifest).is_err());
    }

    #[test]
    fn rejects_unrecorded_nix_version_or_unexplained_repin() {
        let mut version = record();
        *mutate(&mut version, "/nix/version") = Value::String("not-recorded".into());
        assert!(
            validate_semantics(&version)
                .unwrap_err()
                .contains("build-time Nix version both runs recorded")
        );

        let mut rationale = record();
        *mutate(&mut rationale, "/repin/rationale") =
            Value::String("A newer image was built.".into());
        assert!(
            validate_semantics(&rationale)
                .unwrap_err()
                .contains("rationale must name reproducible")
        );
    }

    #[test]
    fn rejects_malformed_nix_identity_or_resource_result() {
        let mut nar_hash = record();
        *mutate(&mut nar_hash, "/nix/locked_inputs/0/nar_hash") =
            Value::String("sha256-not-a-digest".into());
        assert!(
            validate_semantics(&nar_hash)
                .unwrap_err()
                .contains("canonical base64-encoded SHA-256")
        );

        let mut store = record();
        *mutate(&mut store, "/runs/0/nix_store_identity") =
            Value::String("build-a/nix-root/nix@48:1".into());
        assert!(
            validate_semantics(&store)
                .unwrap_err()
                .contains("exact source")
        );

        let mut resource = record();
        *mutate(
            &mut resource,
            "/runs/0/result/last_observed_cpu_nanoseconds",
        ) = Value::Number(0.into());
        assert!(
            validate_semantics(&resource)
                .unwrap_err()
                .contains("exact successful resource result")
        );
    }

    #[test]
    fn rejects_stale_input_or_pin() {
        let root = repo_root().unwrap();
        let fixture = tempfile::tempdir().unwrap();
        for path in INPUT_PATHS {
            let destination = fixture.path().join(path);
            fs::create_dir_all(destination.parent().unwrap()).unwrap();
            fs::copy(root.join(path), destination).unwrap();
        }
        let pin_path = fixture.path().join(PIN);
        fs::create_dir_all(pin_path.parent().unwrap()).unwrap();
        fs::copy(root.join(PIN), &pin_path).unwrap();
        let provenance = record();
        verify_files(fixture.path(), &provenance).unwrap();

        fs::write(fixture.path().join(INPUT_PATHS[0]), b"stale\n").unwrap();
        assert!(
            verify_files(fixture.path(), &provenance)
                .unwrap_err()
                .contains("differs")
        );
        fs::copy(
            root.join(INPUT_PATHS[0]),
            fixture.path().join(INPUT_PATHS[0]),
        )
        .unwrap();
        fs::write(&pin_path, "localhost/example@sha256:deadbeef\n").unwrap();
        assert!(
            verify_files(fixture.path(), &provenance)
                .unwrap_err()
                .contains("restore the proven pin")
        );
    }
}
