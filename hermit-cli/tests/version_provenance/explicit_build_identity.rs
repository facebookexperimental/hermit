/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;

use tempfile::TempDir;
use toml::Value;

use super::build_script;

const SOURCE_MANIFEST: &str = include_str!("../../Cargo.toml");

fn source_manifest() -> Value {
    super::without_inherited_repository_location();
    toml::from_str(SOURCE_MANIFEST).expect("parse the actual CLI source manifest")
}

// Identity classification must remain independent of a backend projection.
// Plant the former aggregate only in these test fixtures: the core manifest
// must not regain third-party features or dependencies to exercise this case.
fn source_manifest_with_backend_fixture() -> Value {
    let mut manifest = source_manifest();
    let backend_fixture: Value = toml::from_str(
        r#"[features]
dbt = ["dep:detcore-dbt", "dep:reverie-dbt", "dep:hermit-test-workdir"]
third-party-backends = ["dbt"]

[dependencies]
detcore-dbt = { version = "0.0.0", optional = true }
reverie-dbt = { version = "0.0.0", optional = true }
hermit-test-workdir = { version = "0.0.0", optional = true }
"#,
    )
    .expect("parse the synthetic backend projection fixture");
    for table in ["features", "dependencies"] {
        let actual = manifest[table].as_table_mut().unwrap();
        for (name, value) in backend_fixture[table].as_table().unwrap() {
            assert!(
                actual.insert(name.clone(), value.clone()).is_none(),
                "core source unexpectedly declares synthetic backend member {name}"
            );
        }
    }
    manifest
}

fn fixture(text: &str) -> TempDir {
    // Join this binary's existing startup Once before tempfile reads its
    // environment, preserving the provenance tests' Git-environment contract.
    super::without_inherited_repository_location();
    let dir = tempfile::tempdir().expect("create manifest fixture");
    fs::write(dir.path().join("Cargo.toml"), text).expect("write manifest fixture");
    dir
}

fn classified(manifest: &Value) -> bool {
    let text = toml::to_string(manifest).expect("serialize projected manifest");
    let dir = fixture(&text);
    build_script::registry_build_in(dir.path()).expect("classify manifest fixture")
}

#[test]
fn actual_source_package_has_explicit_source_identity() {
    let dir = fixture(SOURCE_MANIFEST);
    assert!(!build_script::registry_build_in(dir.path()).unwrap());
    const { assert!(!cfg!(hermit_registry_build)) };
}

#[test]
fn a_core_source_manifest_without_backend_features_is_still_source() {
    let mut source = source_manifest();
    source.as_table_mut().unwrap().remove("features");
    assert!(!classified(&source));
}

#[test]
fn registry_projection_survives_manifest_serialization_without_dbt() {
    let mut registry = source_manifest_with_backend_fixture();
    let features = registry["features"].as_table_mut().unwrap();
    features.remove("dbt");
    features["third-party-backends"]
        .as_array_mut()
        .unwrap()
        .retain(|feature| feature.as_str() != Some("dbt"));
    for dependency in ["detcore-dbt", "reverie-dbt", "hermit-test-workdir"] {
        registry["dependencies"]
            .as_table_mut()
            .unwrap()
            .remove(dependency);
    }
    // This is the explicit edit the registry producer must add to its backend
    // projection. The original source manifest remains a source package.
    registry["package"]["metadata"]["hermit"]["build-identity"] = Value::String("registry".into());
    assert!(classified(&registry));
    assert!(!classified(&source_manifest()));

    // Cargo also retains Cargo.toml.orig. The normalized manifest is the build
    // input, so an adjacent source-identity original must not override it.
    let dir = fixture(&toml::to_string(&registry).unwrap());
    fs::write(dir.path().join("Cargo.toml.orig"), SOURCE_MANIFEST).unwrap();
    assert!(build_script::registry_build_in(dir.path()).unwrap());
}

#[test]
fn identity_is_independent_of_declared_backend_features() {
    let mut manifest = source_manifest_with_backend_fixture();
    assert!(manifest["features"].get("dbt").is_some());
    manifest["package"]["metadata"]["hermit"]["build-identity"] = Value::String("registry".into());
    assert!(classified(&manifest));
}

#[test]
fn missing_unknown_or_nonstring_identity_is_refused() {
    let source = source_manifest();
    for identity in [
        None,
        Some(Value::String(String::new())),
        Some(Value::String("Source".into())),
        Some(Value::String("registry ".into())),
        Some(Value::Boolean(true)),
        Some(Value::Array(vec![Value::String("source".into())])),
    ] {
        let mut manifest = source.clone();
        let metadata = manifest["package"]["metadata"]["hermit"]
            .as_table_mut()
            .unwrap();
        match identity {
            Some(identity) => {
                metadata.insert("build-identity".into(), identity);
            }
            None => {
                metadata.remove("build-identity");
            }
        }
        let dir = fixture(&toml::to_string(&manifest).unwrap());
        let error = build_script::registry_build_in(dir.path()).unwrap_err();
        assert!(error.contains("package.metadata.hermit.build-identity"));
    }
}

#[test]
fn unrelated_tables_and_comments_do_not_supply_package_identity() {
    let dir = fixture(
        r#"[package]
name = "hermit-run"
version = "0.0.0"
# [package.metadata.hermit]
# build-identity = "registry"
description = '''
[package.metadata.hermit]
build-identity = "registry"
'''
[workspace.metadata.hermit]
build-identity = "registry"
"#,
    );
    assert!(build_script::registry_build_in(dir.path()).is_err());
}

#[test]
fn invalid_duplicate_or_unreadable_manifest_is_refused() {
    for text in [
        "[package\n",
        "[package.metadata.hermit]\nbuild-identity = \"source\"\nbuild-identity = \"registry\"\n",
    ] {
        let dir = fixture(text);
        assert!(build_script::registry_build_in(dir.path()).is_err());
    }
    let dir = tempfile::tempdir().unwrap();
    assert!(build_script::registry_build_in(dir.path()).is_err());
}
