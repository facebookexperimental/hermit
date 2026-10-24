#!/usr/bin/env -S rust-script --force
//! Check the pinned validation image's tracked dual-build receipt for consistency.
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
const SUPERSEDED_REFERENCE: &str = "localhost/hermit-hermetic-validate@sha256:d4e585cb6baebf51ef3e52a3488f8fa6c1401379f2c0aa5e5e5a17121092ec95";
const SUPERSEDED_ARCHIVE_SHA256: &str =
    "085b605dd6d17c59795ccc92b5cbd761424810ab156520a10474dc9e4a3c57d0";
/// The replaced d4e5 root lacked the curl executable; the rationale must name
/// the executable whose absence this repin repairs.
const RATIONALE_MARKER: &str = "curl";
/// Both isolated builds recorded `nix --version` before building.
const RECORDED_NIX_VERSION: &str = "nix (Nix) 2.35.2";
const INPUT_PATHS: [&str; 4] = [
    "ci/hermetic/build-image.sh",
    "ci/hermetic/flake.lock",
    "ci/hermetic/flake.nix",
    "ci/hermetic/guest-paths.txt",
];
const EVIDENCE_LIMITATIONS: [&str; 6] = [
    "Each service ran with the systemd user manager's environment plus PATH and HOME; only those variable names were recorded, not their values.",
    "Neither build used the network. Each new store could substitute only from the build host's default Nix store, and only paths signed with the cache.nixos.org key, the one key trusted, or content-addressed (derivation files and fixed-output sources, which Nix checks by hash); substituted paths are trusted by Nix signature or content hash and NAR hash, not rebuilt. Every unsigned path in the closure, including rust-script, cargo-nextest and coreutils, was rebuilt in each store.",
    "Both builds ran in the Nix build sandbox with /bin/sh bound to the static busybox at /nix/store/yysvxw5iwwijaci7ggrnms4mavwcjnpk-busybox-1.37.0 (NAR hash sha256-Qlx+BoVIaa3RubvM7Lr733QAcJpy4XZPFDX0d5XN9WA=), because the build host's /bin/sh links to a dynamically linked bash; that shell is unsigned and is not in the image.",
    "Build logs, unit records and Nix path-info records were retained only on the build host; this receipt does not authenticate them.",
    "The superseded d4e5 archive was not rebuilt for this receipt; its 085b60 SHA-256 is carried from the d4e5 receipt.",
    "The recorded build-source commit and tree are historical review evidence. Rebases can rewrite their ancestry; live verification establishes equivalence only for the four enumerated image input files, not the complete source tree or archive.",
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
        run_id: "hermetic-b898-a-curl.service",
        source_snapshot_identity: "source-a.git-archive.tar@47:1981730721",
        nix_store_identity: "build-a/nix-root/nix@47:1981733241",
        podman_store_identity: "build-a/podman-data",
        memory_peak_bytes: 7_528_996_864,
        cpu_nanoseconds: 1_180_895_998_000,
    },
    ExpectedRun {
        run_id: "hermetic-b898-b-curl.service",
        source_snapshot_identity: "source-b.git-archive.tar@47:1981733239",
        nix_store_identity: "build-b/nix-root/nix@47:1981735766",
        podman_store_identity: "build-b/podman-data",
        memory_peak_bytes: 7_536_754_688,
        cpu_nanoseconds: 1_179_648_165_000,
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
         Check {PROVENANCE}, the current source inputs, and {PIN} for tracked consistency.\n\
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
    Ok(())
}

fn validate_semantics(root: &Value) -> Result<(), String> {
    exact_keys(
        root,
        &[
            "claim_scope",
            "evidence_limitations",
            "method",
            "nix",
            "repin",
            "runs",
            "schema_version",
            "source",
        ],
        "provenance",
    )?;
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
            "repin.superseded_reference must retain the replaced d4e5 reference {SUPERSEDED_REFERENCE:?}"
        ));
    }
    let old_archive = string(repin, "superseded_archive_sha256", "repin")?;
    if old_archive != SUPERSEDED_ARCHIVE_SHA256 {
        return Err(format!(
            "repin.superseded_archive_sha256 must retain the d4e5 receipt's 085b60 archive SHA-256 {SUPERSEDED_ARCHIVE_SHA256:?}"
        ));
    }
    if old == new || !string(repin, "rationale", "repin")?.contains(RATIONALE_MARKER) {
        return Err(format!(
            "repin must explain the real change: its rationale must name {RATIONALE_MARKER}, the executable the d4e5 root lacked"
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
    for (index, input) in inputs.iter().enumerate() {
        let context = format!("source.inputs[{index}]");
        exact_keys(input, &["path", "sha256", "size_bytes"], &context)?;
        input_paths.insert(string(input, "path", &context)?);
        require_flat_sha256(
            string(input, "sha256", &context)?,
            &format!("{context}.sha256"),
        )?;
    }
    if input_paths != INPUT_PATHS.into_iter().collect() {
        return Err(format!("source.inputs paths differ: {input_paths:?}"));
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

fn verify(repo: &Path) -> Result<(), String> {
    let bytes = fs::read(repo.join(PROVENANCE))
        .map_err(|error| format!("cannot read {PROVENANCE}: {error}"))?;
    let root = parse_provenance(&bytes)?;
    validate_semantics(&root)?;
    verify_files(repo, &root)?;
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
                "check-image-provenance: PASS -- tracked pin, source inputs, and dual-build receipt are internally consistent; historical evidence remains review-bound"
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
    fn accepts_rebased_unrelated_head_with_identical_image_inputs() {
        let root = repo_root().unwrap();
        let fixture = tempfile::tempdir().unwrap();
        for path in INPUT_PATHS.into_iter().chain([PIN, PROVENANCE]) {
            let destination = fixture.path().join(path);
            fs::create_dir_all(destination.parent().unwrap()).unwrap();
            fs::copy(root.join(path), destination).unwrap();
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
                .contains("must retain the replaced d4e5 reference")
        );

        let mut archive = record();
        *mutate(&mut archive, "/repin/superseded_archive_sha256") = Value::String("a".repeat(64));
        assert!(
            validate_semantics(&archive)
                .unwrap_err()
                .contains("must retain the d4e5 receipt's 085b60 archive SHA-256")
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
                .contains("rationale must name curl")
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
