use std::fs;
use std::path::Path;
use std::path::PathBuf;

use serde_json::Value;

/// Bind the host proof implementation to the same Cargo-resolved checkout as
/// the preload. No populated Hermit gitlink or installed helper is consulted.
pub(crate) fn constructor_qualifier_source(
    metadata: &Value,
    preload_manifest: &Path,
    revision: &str,
) -> Result<PathBuf, String> {
    let packages = metadata["packages"]
        .as_array()
        .ok_or("Cargo metadata has no packages")?;
    let helpers: Vec<_> = packages
        .iter()
        .filter(|package| package["name"] == "reverie-liteinst-artifact")
        .collect();
    if helpers.len() != 1 {
        return Err("expected one resolved standalone constructor qualifier".into());
    }
    let helper = helpers[0];
    let version = helper["version"]
        .as_str()
        .ok_or("constructor qualifier has no version")?;
    if helper["source"]
        != format!("git+https://github.com/rrnewton/reverie.git?rev={revision}#{revision}")
        || helper["id"]
            != format!(
                "git+https://github.com/rrnewton/reverie.git?rev={revision}#reverie-liteinst-artifact@{version}"
            )
    {
        return Err("constructor qualifier differs from the exact preload Git source".into());
    }
    let preload_manifest = fs::canonicalize(preload_manifest)
        .map_err(|error| format!("canonicalize preload manifest: {error}"))?;
    let checkout = preload_manifest
        .parent()
        .and_then(Path::parent)
        .ok_or("preload manifest has no checkout root")?;
    if preload_manifest != checkout.join("reverie-liteinst-preload/Cargo.toml") {
        return Err("preload manifest is outside its expected checkout location".into());
    }
    let helper_manifest = helper["manifest_path"]
        .as_str()
        .ok_or("constructor qualifier has no manifest")?;
    let helper_manifest = fs::canonicalize(helper_manifest)
        .map_err(|error| format!("canonicalize qualifier manifest: {error}"))?;
    if helper_manifest != checkout.join("reverie-liteinst-artifact/Cargo.toml") {
        return Err("constructor qualifier is not in the preload's canonical checkout".into());
    }
    let targets = helper["targets"]
        .as_array()
        .ok_or("constructor qualifier has no targets")?;
    if targets.len() != 1
        || targets[0]["name"] != "reverie_liteinst_artifact"
        || targets[0]["kind"] != serde_json::json!(["lib"])
        || targets[0]["crate_types"] != serde_json::json!(["lib"])
    {
        return Err("constructor qualifier is not the exact host-only library target".into());
    }
    let source = targets[0]["src_path"]
        .as_str()
        .ok_or("constructor qualifier library has no source")?;
    let source = fs::canonicalize(source)
        .map_err(|error| format!("canonicalize qualifier source: {error}"))?;
    if source != checkout.join("scripts/m1_artifact.rs") || !source.is_file() {
        return Err("constructor qualifier does not compile the shared proof source".into());
    }
    Ok(source)
}

pub(crate) fn liteinst_cdylib_from_cargo_messages(
    messages: &str,
    package_id: &str,
    manifest: &Path,
    allocator_fixture: bool,
) -> Result<PathBuf, String> {
    let mut artifacts = Vec::new();
    for (index, line) in messages.lines().filter(|line| !line.is_empty()).enumerate() {
        let message: Value = serde_json::from_str(line)
            .map_err(|error| format!("invalid Cargo JSON on line {}: {error}", index + 1))?;
        let target = &message["target"];
        let is_preload = message["reason"] == "compiler-artifact"
            && target["name"] == "reverie_liteinst_preload";
        if !is_preload {
            continue;
        }
        if message["package_id"] != package_id
            || message["manifest_path"].as_str().map(Path::new) != Some(manifest)
            || target["kind"] != serde_json::json!(["cdylib"])
            || target["crate_types"] != serde_json::json!(["cdylib"])
        {
            return Err(format!(
                "LiteInst preload artifact on line {} has the wrong package, manifest or cdylib-only target",
                index + 1
            ));
        }
        let expected_features = if allocator_fixture {
            serde_json::json!(["allocator-fixture"])
        } else {
            serde_json::json!([])
        };
        if message["features"] != expected_features {
            return Err(format!(
                "LiteInst preload artifact on line {} has unexpected features",
                index + 1
            ));
        }
        let filenames = message["filenames"].as_array().ok_or_else(|| {
            format!(
                "LiteInst compiler-artifact on line {} has no filenames",
                index + 1
            )
        })?;
        if filenames.len() != 1 {
            return Err("LiteInst preload must report exactly one current cdylib output".into());
        }
        let filename = filenames[0]
            .as_str()
            .ok_or("LiteInst preload filename is not a string")?;
        let path = PathBuf::from(filename);
        if path.file_name().and_then(|name| name.to_str()) != Some("libreverie_liteinst_preload.so")
        {
            return Err("LiteInst preload output does not have the leaf's exact filename".into());
        }
        artifacts.push(path);
    }
    if artifacts.len() != 1 {
        return Err(format!(
            "expected exactly one LiteInst preload cdylib in current Cargo output, found {artifacts:?}"
        ));
    }
    Ok(artifacts.remove(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PACKAGE: &str = "git+https://github.com/rrnewton/reverie.git?rev=0123456789012345678901234567890123456789#reverie-liteinst-preload@0.4.1";
    const MANIFEST: &str = "/git/reverie/reverie-liteinst-preload/Cargo.toml";

    #[test]
    fn refuses_unbound_constructor_qualifier_packages_and_targets() {
        const REVISION: &str = "0123456789012345678901234567890123456789";
        struct Checkout(PathBuf);
        impl Drop for Checkout {
            fn drop(&mut self) {
                fs::remove_dir_all(&self.0).expect("remove owned qualifier control checkout");
            }
        }
        let serial = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "hermit-constructor-qualifier-{}-{serial}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        let owned = Checkout(root);
        for relative in [
            "reverie-liteinst-preload",
            "reverie-liteinst-artifact",
            "scripts",
            "other-checkout/reverie-liteinst-artifact",
            "other-checkout/scripts",
        ] {
            fs::create_dir_all(owned.0.join(relative)).unwrap();
        }
        for relative in [
            "reverie-liteinst-preload/Cargo.toml",
            "reverie-liteinst-artifact/Cargo.toml",
            "scripts/m1_artifact.rs",
            "scripts/unrelated.rs",
            "other-checkout/reverie-liteinst-artifact/Cargo.toml",
            "other-checkout/scripts/m1_artifact.rs",
        ] {
            fs::write(owned.0.join(relative), b"source identity control").unwrap();
        }
        let root = fs::canonicalize(&owned.0).unwrap();
        let preload = root.join("reverie-liteinst-preload/Cargo.toml");
        let source = root.join("scripts/m1_artifact.rs");
        let helper = serde_json::json!({
            "name": "reverie-liteinst-artifact", "version": "0.4.1",
            "source": format!("git+https://github.com/rrnewton/reverie.git?rev={REVISION}#{REVISION}"),
            "id": format!("git+https://github.com/rrnewton/reverie.git?rev={REVISION}#reverie-liteinst-artifact@0.4.1"),
            "manifest_path": root.join("reverie-liteinst-artifact/Cargo.toml"),
            "targets": [{"name": "reverie_liteinst_artifact", "kind": ["lib"],
                "crate_types": ["lib"], "src_path": source}],
        });
        let bind = |packages| {
            constructor_qualifier_source(
                &serde_json::json!({"packages": packages}),
                &preload,
                REVISION,
            )
        };
        assert_eq!(bind(vec![helper.clone()]).unwrap(), source);
        let mut alias = helper.clone();
        alias["targets"][0]["src_path"] =
            serde_json::json!(root.join("scripts/../scripts/m1_artifact.rs"));
        assert_eq!(bind(vec![alias]).unwrap(), source);
        assert!(bind(Vec::<Value>::new()).is_err());
        assert!(bind(vec![helper.clone(), helper.clone()]).is_err());
        for (field, value) in [
            ("source", serde_json::json!(null)),
            (
                "source",
                serde_json::json!(format!(
                    "git+https://github.com/other/reverie.git?rev={REVISION}#{REVISION}"
                )),
            ),
            (
                "source",
                serde_json::json!(format!(
                    "git+https://github.com/rrnewton/reverie.git?rev={REVISION}#aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                )),
            ),
            ("id", serde_json::json!(PACKAGE)),
            (
                "manifest_path",
                serde_json::json!(root.join("other-checkout/reverie-liteinst-artifact/Cargo.toml")),
            ),
        ] {
            let mut wrong = helper.clone();
            wrong[field] = value;
            assert!(bind(vec![wrong]).is_err(), "accepted wrong {field}");
        }
        for (field, value) in [
            ("name", serde_json::json!("lookalike")),
            ("kind", serde_json::json!(["bin"])),
            ("crate_types", serde_json::json!(["rlib"])),
            (
                "src_path",
                serde_json::json!(root.join("scripts/unrelated.rs")),
            ),
            (
                "src_path",
                serde_json::json!(root.join("other-checkout/scripts/m1_artifact.rs")),
            ),
        ] {
            let mut wrong = helper.clone();
            wrong["targets"][0][field] = value;
            assert!(bind(vec![wrong]).is_err(), "accepted wrong target {field}");
        }
        let mut duplicate = helper.clone();
        duplicate["targets"] = serde_json::json!([helper["targets"][0], helper["targets"][0]]);
        assert!(bind(vec![duplicate]).is_err());
    }

    fn artifact() -> Value {
        serde_json::json!({
            "reason": "compiler-artifact", "package_id": PACKAGE,
            "manifest_path": MANIFEST,
            "target": {"name":"reverie_liteinst_preload", "kind":["cdylib"], "crate_types":["cdylib"]},
            "features": [],
            "filenames": ["/isolated/libreverie_liteinst_preload.so"], "fresh":true
        })
    }

    fn select(messages: &str) -> Result<PathBuf, String> {
        liteinst_cdylib_from_cargo_messages(messages, PACKAGE, Path::new(MANIFEST), false)
    }

    #[test]
    fn selects_only_the_current_liteinst_cdylib_message() {
        let messages = format!(
            "{}\n{}\n{}\n{}\n",
            serde_json::json!({"reason":"compiler-artifact","target":{"name":"unrelated","kind":["cdylib"]},"filenames":["/warm/libreverie_liteinst-stale.so"]}),
            serde_json::json!({"reason":"compiler-artifact","target":{"name":"reverie_liteinst","kind":["rlib"]},"filenames":["/isolated/libreverie_liteinst.rlib"]}),
            artifact(),
            serde_json::json!({"reason":"build-finished","success":true}),
        );

        assert_eq!(
            select(&messages).unwrap(),
            PathBuf::from("/isolated/libreverie_liteinst_preload.so")
        );
    }

    #[test]
    fn rejects_non_json_output() {
        let error = select("not cargo json").unwrap_err();
        assert!(error.contains("invalid Cargo JSON on line 1"));
    }

    #[test]
    fn refuses_missing_or_stale_core_artifacts() {
        assert!(select("").is_err());
        let stale = serde_json::json!({
            "reason":"compiler-artifact",
            "target":{"name":"reverie_liteinst","kind":["rlib","cdylib"]},
            "filenames":["/warm/libreverie_liteinst.so"]
        });
        assert!(select(&stale.to_string()).is_err());
    }

    #[test]
    fn refuses_duplicate_current_artifact_messages() {
        let message = artifact();
        assert!(select(&format!("{message}\n{message}\n")).is_err());
    }

    #[test]
    fn refuses_wrong_source_target_features_or_filename() {
        for (field, value) in [
            (
                "package_id",
                serde_json::json!(
                    "git+https://github.com/rrnewton/reverie.git?rev=1111111111111111111111111111111111111111#reverie-liteinst-preload@0.4.1"
                ),
            ),
            ("manifest_path", serde_json::json!("/lookalike/Cargo.toml")),
            ("features", serde_json::json!(["allocator-fixture"])),
            (
                "filenames",
                serde_json::json!(["/warm/libreverie_liteinst.so"]),
            ),
        ] {
            let mut message = artifact();
            message[field] = value;
            assert!(
                select(&message.to_string()).is_err(),
                "accepted wrong {field}"
            );
        }
        for field in ["kind", "crate_types"] {
            let mut message = artifact();
            message["target"][field] = serde_json::json!(["rlib", "cdylib"]);
            assert!(
                select(&message.to_string()).is_err(),
                "accepted wrong {field}"
            );
        }
    }

    #[test]
    fn refuses_missing_malformed_or_multiple_filenames() {
        for value in [
            Value::Null,
            serde_json::json!([]),
            serde_json::json!([42]),
            serde_json::json!([
                "/isolated/libreverie_liteinst_preload.so",
                "/warm/libreverie_liteinst_preload.so"
            ]),
        ] {
            let mut message = artifact();
            message["filenames"] = value;
            assert!(select(&message.to_string()).is_err());
        }
    }

    #[test]
    fn selects_only_the_explicit_allocator_fixture_feature_mode() {
        let select_fixture = |message: &str| {
            liteinst_cdylib_from_cargo_messages(message, PACKAGE, Path::new(MANIFEST), true)
        };
        assert!(select_fixture(&artifact().to_string()).is_err());
        let mut message = artifact();
        message["features"] = serde_json::json!(["allocator-fixture"]);
        assert!(select(&message.to_string()).is_err());
        assert_eq!(
            select_fixture(&message.to_string()).unwrap(),
            PathBuf::from("/isolated/libreverie_liteinst_preload.so")
        );
        assert!(select_fixture("").is_err());
        assert!(select_fixture(&format!("{message}\n{message}\n")).is_err());
        for features in [
            Value::Null,
            serde_json::json!([]),
            serde_json::json!(["other"]),
            serde_json::json!(["allocator-fixture", "other"]),
            serde_json::json!(["allocator-fixture", "allocator-fixture"]),
        ] {
            let mut wrong = message.clone();
            wrong["features"] = features;
            assert!(select_fixture(&wrong.to_string()).is_err());
        }
        for (field, value) in [
            ("package_id", serde_json::json!("wrong source")),
            ("manifest_path", serde_json::json!("/lookalike/Cargo.toml")),
            ("filenames", Value::Null),
            ("filenames", serde_json::json!([])),
            ("filenames", serde_json::json!([42])),
            (
                "filenames",
                serde_json::json!(["/warm/libreverie_liteinst.so"]),
            ),
            (
                "filenames",
                serde_json::json!([
                    "/isolated/libreverie_liteinst_preload.so",
                    "/warm/libreverie_liteinst_preload.so"
                ]),
            ),
        ] {
            let mut wrong = message.clone();
            wrong[field] = value;
            assert!(
                select_fixture(&wrong.to_string()).is_err(),
                "accepted wrong {field}"
            );
        }
        for field in ["kind", "crate_types"] {
            let mut wrong = message.clone();
            wrong["target"][field] = serde_json::json!(["rlib", "cdylib"]);
            assert!(
                select_fixture(&wrong.to_string()).is_err(),
                "accepted wrong {field}"
            );
        }
    }
}
