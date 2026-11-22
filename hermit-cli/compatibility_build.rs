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

const REVERIE_GIT_PREFIX: &str = "git+https://github.com/facebookexperimental/reverie.git?rev=";

/// The `rev` rest of a package declared from the canonical Reverie git
/// source, which joins the measured cohort; `None` for every other
/// package, whose admission is decided by `admit_noncanonical` once the
/// cohort's checkout is known.
fn canonical_reverie_rest(package: &super::compatibility_graph::Package) -> Option<&str> {
    package
        .source
        .as_deref()
        .and_then(|source| source.strip_prefix(REVERIE_GIT_PREFIX))
}

/// Admission of one built package that is not from the canonical
/// Reverie git source. The one rule is provenance: a package whose
/// name the pinned Reverie workspace defines must come from that
/// canonical commit, whatever its declared source; a member of the
/// building workspace is itself; any other declared source (registry
/// or another git repository) stands as declared. A path package
/// outside the building workspace has no declared canonical source
/// and is refused.
fn admit_noncanonical(
    package: &super::compatibility_graph::Package,
    workspace_members: &BTreeSet<String>,
    reverie_package_names: &BTreeSet<String>,
) -> Result<(), String> {
    if reverie_package_names.contains(&package.name) {
        return Err(format!(
            "built Reverie package {} has a noncanonical or patched source",
            package.name
        ));
    }
    if package.source.is_none() && !workspace_members.contains(&package.id) {
        return Err(format!(
            "built path package {} is not a member of the building workspace",
            package.name
        ));
    }
    Ok(())
}

/// Package names defined by one measured Reverie checkout's workspace:
/// the `[package]` name of every explicit `[workspace]` member, read
/// with a TOML parser. Anything this cannot enumerate exactly (a glob
/// member, a missing or duplicate name) refuses, so the set is never
/// silently partial.
fn reverie_workspace_package_names(root: &Path) -> Result<BTreeSet<String>, String> {
    fn read_bounded(path: &Path) -> Result<toml::Value, String> {
        let bytes = std::fs::read(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if bytes.len() > 1024 * 1024 {
            return Err(format!("{} exceeds its bound", path.display()));
        }
        let text = String::from_utf8(bytes)
            .map_err(|error| format!("{} is not UTF-8: {error}", path.display()))?;
        toml::from_str(&text).map_err(|error| format!("cannot parse {}: {error}", path.display()))
    }
    let workspace = read_bounded(&root.join("Cargo.toml"))?;
    let members = workspace
        .get("workspace")
        .and_then(|workspace| workspace.get("members"))
        .and_then(toml::Value::as_array)
        .ok_or_else(|| "Reverie workspace members are not an array".to_owned())?;
    if members.is_empty() {
        return Err("Reverie workspace defines no members".to_owned());
    }
    let mut names = BTreeSet::new();
    for (index, member) in members.iter().enumerate() {
        let member = member.as_str().ok_or_else(|| {
            format!("Reverie workspace member #{index} is not a string: {member}")
        })?;
        if member.is_empty() || member.contains(['*', '?', '[']) {
            return Err(format!(
                "Reverie workspace member {member} is not an explicit path"
            ));
        }
        let manifest = read_bounded(&root.join(member).join("Cargo.toml"))?;
        let name = manifest
            .get("package")
            .and_then(|package| package.get("name"))
            .and_then(toml::Value::as_str)
            .ok_or_else(|| format!("Reverie workspace member {member} has no package name"))?;
        if name.is_empty() || !names.insert(name.to_owned()) {
            return Err(format!(
                "Reverie workspace member {member} has no unique package name"
            ));
        }
    }
    Ok(names)
}

/// The route-invariant Reverie content digest recorded in the stamp:
/// the content hash every measurement route computes over the same
/// bytes, never a route's materialization hash.
fn reverie_content_digest(measured: &serde_json::Value) -> Result<&str, String> {
    measured
        .get("source_sha256")
        .or_else(|| measured.get("sha256"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "Reverie source measurement has no content digest".to_owned())
}

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
    let submodule = super::compatibility_source::submodule_reverie_root(core)?;
    let mut roots: BTreeMap<
        std::path::PathBuf,
        (String, super::compatibility_source::ReverieRoute),
    > = BTreeMap::new();
    let mut commits = BTreeSet::new();
    let mut common_units = BTreeMap::new();
    let mut noncanonical = Vec::new();
    for (id, units) in &graph.units {
        let package = &graph.packages[id];
        // Cohort membership by route: the canonical git source, or the
        // core checkout's Reverie submodule (identity from its gitlink).
        let cohort = match canonical_reverie_rest(package) {
            Some(rest) => {
                let (declared, resolved) = rest
                    .split_once('#')
                    .ok_or_else(|| "built Reverie source has no resolved commit".to_owned())?;
                if !full_oid(declared) || declared != resolved {
                    return Err(
                        "built Reverie source must declare and resolve the same full commit"
                            .to_owned(),
                    );
                }
                let directory = package
                    .manifest_path
                    .parent()
                    .ok_or_else(|| "Reverie package has no directory".to_owned())?;
                let repo =
                    std::fs::canonicalize(git(directory, &["rev-parse", "--show-toplevel"])?)
                        .map_err(|error| error.to_string())?;
                Some((
                    repo,
                    resolved.to_owned(),
                    super::compatibility_source::ReverieRoute::CargoGit,
                ))
            }
            None => submodule.as_ref().and_then(|submodule_ref| {
                super::compatibility_source::package_in_submodule(package, submodule_ref).then(
                    || {
                        (
                            submodule_ref.1.clone(),
                            submodule_ref.2.clone(),
                            super::compatibility_source::submodule_route(core),
                        )
                    },
                )
            }),
        };
        let Some((repo, resolved, cargo_cache)) = cohort else {
            noncanonical.push(package);
            continue;
        };
        commits.insert(resolved.clone());
        roots.entry(repo).or_insert((resolved, cargo_cache));
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
    let mut reverie_names = BTreeSet::new();
    for root in roots.keys() {
        reverie_names.extend(reverie_workspace_package_names(root)?);
    }
    for package in noncanonical {
        admit_noncanonical(package, &graph.workspace_members, &reverie_names)?;
    }
    let mut source_hashes = BTreeSet::new();
    for (root, (expected, route)) in roots {
        let measured =
            super::compatibility_source::measure_reverie_root(&root, &expected, route, true)?;
        if measured.get("commit").and_then(serde_json::Value::as_str) != Some(expected.as_str()) {
            return Err(
                "built Reverie package directory is not its resolved source commit".to_owned(),
            );
        }
        source_hashes.insert(reverie_content_digest(&measured)?.to_owned());
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

#[cfg(test)]
mod tests {
    use super::super::compatibility_graph::Package;
    use super::*;

    fn package(id: &str, name: &str, source: Option<&str>) -> Package {
        Package {
            id: id.to_owned(),
            name: name.to_owned(),
            version: "0.0.0".to_owned(),
            source: source.map(str::to_owned),
            manifest_path: std::path::PathBuf::from("/ws/Cargo.toml"),
            targets: Vec::new(),
        }
    }

    fn members(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    fn reverie_names() -> BTreeSet<String> {
        members(&["reverie", "reverie-ptrace", "safeptrace"])
    }

    #[test]
    fn workspace_member_path_packages_are_admitted_as_themselves() {
        // The companion graph: its own reverie-* path package is a
        // workspace member and not a name the Reverie workspace
        // defines, so provenance admits it without a cohort.
        let pkg = package(
            "path+file:///ws/reverie-dynamorio",
            "reverie-dynamorio",
            None,
        );
        assert_eq!(
            admit_noncanonical(&pkg, &members(&[pkg.id.as_str()]), &reverie_names()),
            Ok(())
        );
        let core = package("path+file:///ws/hermit-cli", "hermit-cli", None);
        assert_eq!(
            admit_noncanonical(&core, &members(&[core.id.as_str()]), &reverie_names()),
            Ok(())
        );
    }

    #[test]
    fn reverie_git_packages_join_the_cohort() {
        let pkg = package(
            "reverie 0.0.0",
            "reverie",
            Some("git+https://github.com/facebookexperimental/reverie.git?rev=abc#abc"),
        );
        assert_eq!(canonical_reverie_rest(&pkg), Some("abc#abc"));
    }

    #[test]
    fn patched_reverie_package_is_refused() {
        // Negative control, every source class: a package whose name
        // the pinned Reverie workspace defines, from a vendored
        // registry copy, another git host, or a workspace-member path.
        let vendored = package(
            "reverie 0.0.0",
            "reverie-ptrace",
            Some("registry+https://github.com/rust-lang/crates.io-index"),
        );
        assert!(
            admit_noncanonical(&vendored, &members(&[]), &reverie_names())
                .unwrap_err()
                .contains("noncanonical or patched")
        );
        let fork = package(
            "reverie 0.0.0",
            "safeptrace",
            Some("git+https://example.com/reverie-fork?rev=abc#abc"),
        );
        assert!(admit_noncanonical(&fork, &members(&[]), &reverie_names()).is_err());
        let member_copy = package("path+file:///ws/reverie-ptrace", "reverie-ptrace", None);
        assert!(
            admit_noncanonical(
                &member_copy,
                &members(&[member_copy.id.as_str()]),
                &reverie_names()
            )
            .unwrap_err()
            .contains("noncanonical or patched")
        );
    }

    #[test]
    fn path_package_outside_the_workspace_is_refused() {
        let pkg = package("path+file:///elsewhere/serde", "serde", None);
        assert!(
            admit_noncanonical(&pkg, &members(&["path+file:///ws/core"]), &reverie_names())
                .unwrap_err()
                .contains("not a member of the building workspace")
        );
    }

    #[test]
    fn other_declared_sources_stand_as_declared() {
        let pkg = package(
            "serde 1.0.0",
            "serde",
            Some("registry+https://github.com/rust-lang/crates.io-index"),
        );
        assert_eq!(
            admit_noncanonical(&pkg, &members(&[]), &reverie_names()),
            Ok(())
        );
    }

    struct OwnedRoot(std::path::PathBuf);
    impl Drop for OwnedRoot {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn fixture_root(tag: &str) -> (OwnedRoot, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "hermit-reverie-names-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        (OwnedRoot(root.clone()), root)
    }

    fn write_member(root: &std::path::Path, dir: &str, manifest: &str) {
        std::fs::create_dir_all(root.join(dir)).unwrap();
        if !manifest.is_empty() {
            std::fs::write(root.join(dir).join("Cargo.toml"), manifest).unwrap();
        }
    }

    #[test]
    fn stamp_digest_is_the_content_digest_on_every_route() {
        // Reverting the stamp to the Cargo route's materialization
        // hash (`sha256`) fails here: the content digest wins even
        // when both fields are present and differ.
        let cargo = serde_json::json!({"sha256": "materialization", "source_sha256": "content"});
        assert_eq!(reverie_content_digest(&cargo), Ok("content"));
        let strict = serde_json::json!({"sha256": "content"});
        assert_eq!(reverie_content_digest(&strict), Ok("content"));
        assert!(reverie_content_digest(&serde_json::json!({})).is_err());
    }

    #[test]
    fn reverie_workspace_names_come_from_the_checkout() {
        let (owned, root) = fixture_root("ok");
        // Planted spellings a line scanner drops: a `]` inside a
        // comment within the array, and single-quoted members. The
        // TOML parser keeps every member.
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [ # not the end: ]\n    \"reverie-ptrace\", 'safeptrace',\n]\n",
        )
        .unwrap();
        for name in ["reverie-ptrace", "safeptrace"] {
            write_member(
                &root,
                name,
                &format!("[package]\nname = \"{name}\"\nversion = \"0.0.0\"\n"),
            );
        }
        assert_eq!(
            reverie_workspace_package_names(&root).unwrap(),
            members(&["reverie-ptrace", "safeptrace"])
        );
        drop(owned);
    }

    #[test]
    fn reverie_workspace_names_cover_every_core_reverie_dependency() {
        // The real checkout a stamped build reads: the Reverie
        // submodule at its pin, beside this crate. Every Reverie
        // package the core workspace depends on by path must be in
        // the reader's set, or a patched copy of it would be admitted.
        let crate_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let reverie_root = crate_dir.join("../reverie");
        let names = reverie_workspace_package_names(&reverie_root)
            .expect("the pinned Reverie submodule must enumerate exactly");
        assert!(!names.is_empty(), "Reverie name set must be nonempty");
        let root_manifest: toml::Value =
            toml::from_str(&std::fs::read_to_string(crate_dir.join("../Cargo.toml")).unwrap())
                .unwrap();
        let dependencies = root_manifest
            .get("workspace")
            .and_then(|workspace| workspace.get("dependencies"))
            .and_then(toml::Value::as_table)
            .expect("core workspace dependencies table");
        let mut depended = 0usize;
        for dependency in dependencies.values() {
            let Some(path) = dependency.get("path").and_then(toml::Value::as_str) else {
                continue;
            };
            if !path.starts_with("reverie/") {
                continue;
            }
            let manifest: toml::Value = toml::from_str(
                &std::fs::read_to_string(crate_dir.join("..").join(path).join("Cargo.toml"))
                    .unwrap(),
            )
            .unwrap();
            let name = manifest
                .get("package")
                .and_then(|package| package.get("name"))
                .and_then(toml::Value::as_str)
                .expect("Reverie dependency manifest name");
            assert!(
                names.contains(name),
                "core depends on Reverie package {name} at {path}, missing from {names:?}"
            );
            depended += 1;
        }
        assert!(depended > 0, "core must depend on Reverie packages");
    }

    #[test]
    fn reverie_workspace_name_reader_refuses_each_partial_input() {
        // An empty members array must refuse: an empty name set would
        // admit every non-canonical Reverie package in silence.
        let (owned, root) = fixture_root("empty");
        std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();
        assert!(
            reverie_workspace_package_names(&root)
                .unwrap_err()
                .contains("defines no members")
        );
        drop(owned);

        let (owned, root) = fixture_root("glob");
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\n",
        )
        .unwrap();
        assert!(
            reverie_workspace_package_names(&root)
                .unwrap_err()
                .contains("crates/* is not an explicit path")
        );
        drop(owned);

        let (owned, root) = fixture_root("missing");
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"gone\"]\n",
        )
        .unwrap();
        write_member(&root, "gone", "");
        let error = reverie_workspace_package_names(&root).unwrap_err();
        assert!(error.contains("gone/Cargo.toml"), "{error}");
        drop(owned);

        let (owned, root) = fixture_root("noname");
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"noname\"]\n",
        )
        .unwrap();
        write_member(&root, "noname", "[package]\nversion = \"0.0.0\"\n");
        assert!(
            reverie_workspace_package_names(&root)
                .unwrap_err()
                .contains("noname has no package name")
        );
        drop(owned);

        let (owned, root) = fixture_root("dup");
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"a\", \"b\"]\n",
        )
        .unwrap();
        for dir in ["a", "b"] {
            write_member(
                &root,
                dir,
                "[package]\nname = \"same\"\nversion = \"0.0.0\"\n",
            );
        }
        assert!(
            reverie_workspace_package_names(&root)
                .unwrap_err()
                .contains("b has no unique package name")
        );
        drop(owned);
    }
}
