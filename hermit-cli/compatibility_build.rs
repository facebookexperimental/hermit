/* Copyright (c) Meta Platforms, Inc. and affiliates. */

//! Cargo producer for the separate extension compatibility contract.
//!
//! This measures source/build identity. It is not a claim that a Git object
//! was published by a particular account or that a runtime was qualified.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;
use std::process::Command;

use sha2::Digest;
use sha2::Sha256;

use super::compatibility_source::full_oid;
use super::compatibility_source::git;

pub const STAMP_ENV: &str = "HERMIT_STAMP_COMPATIBILITY";
const SCHEMA: &str = "hermit-core-compatibility/v1";
const API: &str = "hermit-cli-static-backend/v1";

fn env_text(key: &str) -> Result<String, String> {
    std::env::var(key).map_err(|error| format!("missing or non-UTF8 build input {key}: {error}"))
}

fn optional_env_text(key: &str) -> Result<String, String> {
    match std::env::var_os(key) {
        Some(value) => value
            .into_string()
            .map_err(|_| format!("non-UTF8 build input {key}")),
        None => Ok(String::new()),
    }
}

fn resolved_build(
    crate_dir: &Path,
    compiler: &[u8],
    compiler_sha256: &str,
) -> Result<
    (
        serde_json::Value,
        serde_json::Value,
        serde_json::Value,
        serde_json::Value,
    ),
    String,
> {
    use super::compatibility_graph::BuildInput;
    use super::compatibility_graph::Graph;
    use super::compatibility_graph::INPUT_ENV;

    let input_path = std::env::var_os(INPUT_ENV)
        .ok_or_else(|| "actual resolved build graph is unavailable; use ci/build-compatible-cli.rs from the top-level workspace (the core checkout's Cargo.lock is not the extension graph)".to_owned())?;
    let input = BuildInput::load(Path::new(&input_path))?;
    let expected_source: serde_json::Value =
        serde_json::from_slice(&input.source_snapshot.read(64 * 1024 * 1024)?)
            .map_err(|error| error.to_string())?;
    let actual_source = super::compatibility_source::snapshot(&input.metadata, true)?;
    if actual_source != expected_source {
        return Err(
            "complete actual core/Reverie source differs from the measured pre-build snapshot"
                .to_owned(),
        );
    }
    if env_text("TARGET")? != input.target {
        return Err("compatibility producer and actual Cargo target disagree".to_owned());
    }
    let graph = Graph::read(&input)?;
    let mut first_input = input.clone();
    first_input.artifacts = input.first_artifacts.clone();
    let first_graph = Graph::read(&first_input)?;
    let first_rustc = super::compatibility_rustc::Bundle::read(&input.rustc_first)?;
    let second_rustc = super::compatibility_rustc::Bundle::read(&input.rustc_second)?;
    let rustc_inputs =
        super::compatibility_rustc::cohort(&first_graph, &first_rustc, &graph, &second_rustc)?;
    super::compatibility_rustc::require_compiler(&first_rustc, compiler, compiler_sha256)?;
    super::compatibility_rustc::require_compiler(&second_rustc, compiler, compiler_sha256)?;
    if !optional_env_text("RUSTC_WORKSPACE_WRAPPER")?.is_empty()
        || std::env::var_os(super::compatibility_rustc::CAPTURE_ENV).is_none()
        || std::env::var_os(super::compatibility_rustc::GATE_ENV) != Some(input_path.clone())
    {
        return Err("a measured stamp requires the actual source-owned final compiler gate; unsupported wrapper composition is refused".to_owned());
    }
    let wrapper = std::env::var_os("RUSTC_WRAPPER")
        .ok_or_else(|| "final build has no actual source-owned compiler wrapper".to_owned())?;
    let wrapper = Path::new(&wrapper);
    let size = std::fs::metadata(wrapper)
        .map_err(|error| error.to_string())?
        .len();
    if size > 128 * 1024 * 1024 {
        return Err("compiler capture producer exceeds its identity bound".to_owned());
    }
    let wrapper_sha256 = format!(
        "{:x}",
        Sha256::digest(std::fs::read(wrapper).map_err(|error| error.to_string())?)
    );
    if wrapper_sha256 != first_rustc.wrapper_sha256 || wrapper_sha256 != second_rustc.wrapper_sha256
    {
        return Err("actual final compiler wrapper differs from both measured builds".to_owned());
    }
    let core = graph.named("hermit-run")?;
    let actual_manifest = crate_dir
        .join("Cargo.toml")
        .canonicalize()
        .map_err(|error| error.to_string())?;
    if core
        .manifest_path
        .canonicalize()
        .map_err(|error| error.to_string())?
        != actual_manifest
    {
        return Err("producer's built core is not this Cargo package".to_owned());
    }
    let primary = graph.named(&input.package)?;
    if !graph.closure(&primary.id)?.contains(&core.id)
        || !graph
            .executables
            .contains_key(&(primary.id.clone(), input.binary.clone()))
    {
        return Err("producer's actual executable does not link this core".to_owned());
    }
    let core_unit = graph.core_library_unit()?;
    let expected_features: BTreeSet<_> = core_unit
        .features
        .iter()
        .map(|feature| {
            format!(
                "CARGO_FEATURE_{}",
                feature.replace('-', "_").to_ascii_uppercase()
            )
        })
        .collect();
    let actual_features: BTreeSet<_> = std::env::vars_os()
        .filter_map(|(name, _)| {
            name.to_str()
                .filter(|name| name.starts_with("CARGO_FEATURE_"))
                .map(str::to_owned)
        })
        .collect();
    if expected_features.len() != core_unit.features.len() || expected_features != actual_features {
        return Err("actual core Cargo features differ from the producer's built unit".to_owned());
    }
    let core_closure = graph.closure(&core.id)?;
    let mut roots = BTreeMap::new();
    let mut commits = BTreeSet::new();
    let mut common_units = BTreeMap::new();
    for (id, units) in &graph.units {
        let package = &graph.packages[id];
        let source = package.source.as_deref().unwrap_or("");
        let Some(rest) = source.strip_prefix("git+https://github.com/rrnewton/reverie.git?rev=")
        else {
            if source.contains("github.com/rrnewton/reverie")
                || package.name.starts_with("reverie-")
                || package.name == "safeptrace"
            {
                return Err(format!(
                    "built Reverie package {} has a noncanonical or patched source",
                    package.name
                ));
            }
            continue;
        };
        let (declared, resolved) = rest
            .split_once('#')
            .ok_or_else(|| "built Reverie source has no resolved commit".to_owned())?;
        if !full_oid(declared) || declared != resolved {
            return Err(
                "built Reverie source must declare and resolve the same full commit".to_owned(),
            );
        }
        commits.insert(resolved.to_owned());
        let directory = package
            .manifest_path
            .parent()
            .ok_or_else(|| "Reverie package has no directory".to_owned())?;
        let repo = std::fs::canonicalize(git(directory, &["rev-parse", "--show-toplevel"])?)
            .map_err(|error| error.to_string())?;
        roots.entry(repo).or_insert_with(|| resolved.to_owned());
        if core_closure.contains(id) {
            for unit in units {
                common_units.insert(unit.key()?, unit.clone());
            }
        }
    }
    if commits.len() != 1 || roots.is_empty() || common_units.is_empty() {
        return Err(
            "actual artifact build requires exactly one nonempty Reverie source cohort".to_owned(),
        );
    }
    let commit = commits
        .into_iter()
        .next()
        .expect("checked one actual cohort");
    if super::build_support::reverie_pin_in(crate_dir) != commit {
        return Err("declared core pin differs from the actual built Reverie cohort".to_owned());
    }
    let mut source_hashes = BTreeSet::new();
    for (root, expected) in roots {
        let measured = super::compatibility_source::cargo_git_source(&root, &expected, true)?;
        if measured.get("commit").and_then(serde_json::Value::as_str) != Some(expected.as_str()) {
            return Err(
                "built Reverie package directory is not its resolved source commit".to_owned(),
            );
        }
        source_hashes.insert(
            measured
                .get("sha256")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "Reverie source measurement has no content digest".to_owned())?
                .to_owned(),
        );
    }
    if source_hashes.len() != 1 {
        return Err("actual Reverie source copies have different complete content".to_owned());
    }
    for file in [
        &input.metadata,
        &input.first_artifacts,
        &input.artifacts,
        &input.rustc_first,
        &input.rustc_second,
        &input.source_snapshot,
        &input.manifest,
        &input.lock,
    ] {
        println!("cargo:rerun-if-changed={}", file.path.display());
    }
    println!(
        "cargo:rerun-if-changed={}",
        Path::new(&input_path).display()
    );
    let reverie = serde_json::json!({"commit":commit,
        "sha256":source_hashes.into_iter().next().expect("checked one digest"),
        "units":common_units.into_values().collect::<Vec<_>>()});
    // A workspace can override the core's assertions/overflow settings without
    // changing any dependency profile. Bind the actual core library unit too.
    let core_source = actual_source
        .get("core")
        .and_then(|core| core.get("identity"))
        .ok_or_else(|| "actual source proof has no canonical core identity".to_owned())?
        .clone();
    Ok((
        reverie,
        core_unit.profile.clone(),
        rustc_inputs,
        core_source,
    ))
}

fn measured(crate_dir: &Path) -> Result<serde_json::Value, String> {
    let rustc = std::env::var_os("RUSTC").ok_or_else(|| "Cargo did not name rustc".to_owned())?;
    let rustc = Path::new(&rustc);
    if !rustc.is_absolute() {
        return Err("Cargo must name the absolute actual compiler frontend".to_owned());
    }
    let mut compiler_bytes = Vec::new();
    std::fs::File::open(rustc)
        .map_err(|error| error.to_string())?
        .take(128 * 1024 * 1024 + 1)
        .read_to_end(&mut compiler_bytes)
        .map_err(|error| error.to_string())?;
    if compiler_bytes.len() > 128 * 1024 * 1024 {
        return Err("compiler frontend exceeds its identity bound".to_owned());
    }
    let compiler_sha256 = format!("{:x}", Sha256::digest(&compiler_bytes));
    let output = Command::new(rustc)
        .arg("-vV")
        .output()
        .map_err(|error| format!("cannot measure compiler identity: {error}"))?;
    if !output.status.success() || !output.stderr.is_empty() || output.stdout.len() > 64 * 1024 {
        return Err("cannot measure compiler identity with rustc -vV".to_owned());
    }
    let compiler = String::from_utf8(output.stdout.clone())
        .map_err(|_| "compiler identity is non-UTF8".to_owned())?;
    if !compiler
        .lines()
        .any(|line| line.strip_prefix("commit-hash: ").is_some_and(full_oid))
    {
        return Err("compatibility requires an exact compiler commit".to_owned());
    }
    let mut features = BTreeSet::new();
    let mut configuration = BTreeMap::new();
    for (key, value) in std::env::vars_os() {
        let Some(name) = key.to_str() else {
            continue;
        };
        if name.starts_with("CARGO_FEATURE_") {
            if value != "1" {
                return Err(format!("unexpected Cargo feature value for {name}"));
            }
            features.insert(name.to_owned());
        } else if name.starts_with("CARGO_CFG_") {
            configuration.insert(
                name.to_owned(),
                value
                    .into_string()
                    .map_err(|_| format!("non-UTF8 semantic build input {name}"))?,
            );
        }
    }
    for name in [
        "PROFILE",
        "OPT_LEVEL",
        "DEBUG",
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTFLAGS",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
        let value = if name == "RUSTC_WRAPPER" {
            // Its exact actual binary is checked against both capture bundles
            // below. The producer path is fingerprint noise across workspaces.
            super::compatibility_rustc::CAPTURE_SCHEMA.to_owned()
        } else if matches!(name, "PROFILE" | "OPT_LEVEL" | "DEBUG") {
            env_text(name)?
        } else {
            optional_env_text(name)?
        };
        configuration.insert(name.to_owned(), value);
    }
    let configuration: Vec<_> = configuration
        .into_iter()
        .map(|(name, value)| serde_json::json!({"name":name,"value":value}))
        .collect();
    let (reverie, core_profile, rustc_inputs, core_source) =
        resolved_build(crate_dir, &output.stdout, &compiler_sha256)?;
    Ok(serde_json::json!({
        "schema":SCHEMA, "api":API, "source":core_source,
        "release":env_text("CARGO_PKG_VERSION")?, "reverie":reverie,
        "core_profile":core_profile,
        "rustc_inputs":rustc_inputs,
        "target":env_text("TARGET")?, "host":env_text("HOST")?,
        "compiler":compiler.trim_end(), "compiler_sha256":compiler_sha256, "features":features,
        "configuration":configuration,
    }))
}

/// Only `1` requests measured extension compatibility. A regular build remains
/// independent of Git, just like the existing presentation revision producer.
pub fn emit(crate_dir: &Path, registry: bool) {
    println!("cargo:rerun-if-env-changed={STAMP_ENV}");
    println!(
        "cargo:rerun-if-env-changed={}",
        super::compatibility_graph::INPUT_ENV
    );
    println!(
        "cargo:rerun-if-env-changed={}",
        super::compatibility_rustc::CAPTURE_ENV
    );
    println!(
        "cargo:rerun-if-env-changed={}",
        super::compatibility_rustc::GATE_ENV
    );
    let record = match std::env::var_os(STAMP_ENV).as_deref() {
        None => serde_json::json!({
            "availability":"unavailable", "reason":"measurement-not-requested",
        }),
        Some(v) if v.is_empty() || v == "0" => serde_json::json!({
            "availability":"unavailable", "reason":"measurement-not-requested",
        }),
        Some(v) if v == "1" => {
            assert!(
                !registry,
                "registry compatibility requires an explicit verified package producer; .cargo_vcs_info.json alone is not a full-source measurement"
            );
            // This refuses unless both complete actual rustc populations match
            // and the current final compiler gate is bound to that producer.
            let stamp =
                measured(crate_dir).unwrap_or_else(|error| panic!("{STAMP_ENV}=1: {error}"));
            serde_json::json!({"availability":"measured","stamp":stamp})
        }
        Some(_) => panic!("{STAMP_ENV} must be 1 (measure) or unset/0"),
    };
    let text = serde_json::to_string(&record).expect("compatibility record is JSON");
    assert!(
        text.len() <= 16 * 1024,
        "compatibility record exceeds its protocol bound"
    );
    println!("cargo:rustc-env=HERMIT_CORE_COMPATIBILITY={text}");
}
