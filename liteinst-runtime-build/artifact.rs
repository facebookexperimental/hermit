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
    if source != checkout.join("reverie-liteinst-artifact/src/lib.rs") || !source.is_file() {
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
            "reverie-liteinst-artifact/src",
            "scripts",
            "other-checkout/reverie-liteinst-artifact/src",
            "other-checkout/scripts",
        ] {
            fs::create_dir_all(owned.0.join(relative)).unwrap();
        }
        for relative in [
            "reverie-liteinst-preload/Cargo.toml",
            "reverie-liteinst-artifact/Cargo.toml",
            "reverie-liteinst-artifact/src/lib.rs",
            "scripts/unrelated.rs",
            "scripts/m1_artifact.rs",
            "other-checkout/reverie-liteinst-artifact/Cargo.toml",
            "other-checkout/reverie-liteinst-artifact/src/lib.rs",
        ] {
            fs::write(owned.0.join(relative), b"source identity control").unwrap();
        }
        let root = fs::canonicalize(&owned.0).unwrap();
        let preload = root.join("reverie-liteinst-preload/Cargo.toml");
        let source = root.join("reverie-liteinst-artifact/src/lib.rs");
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
            serde_json::json!(root.join("reverie-liteinst-artifact/src/../src/lib.rs"));
        assert_eq!(bind(vec![alias]).unwrap(), source);
        let mut outside_package = helper.clone();
        outside_package["targets"][0]["src_path"] =
            serde_json::json!(root.join("scripts/m1_artifact.rs"));
        assert!(bind(vec![outside_package]).is_err());
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
                serde_json::json!(root.join("other-checkout/reverie-liteinst-artifact/src/lib.rs")),
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
        let native = serde_json::json!({
            "reason": "compiler-artifact", "package_id": "actual-local-detcore-package",
            "manifest_path": "/isolated/hermit/detcore-liteinst/Cargo.toml",
            "target": {"name": "detcore_liteinst", "kind": ["cdylib"], "crate_types": ["cdylib"]},
            "features": ["exec-native-injection", "exec-private-init"],
            "profile": {"opt_level": "3", "debuginfo": 0, "debug_assertions": true,
                "overflow_checks": true, "test": false},
            "filenames": ["/isolated/native-target/libdetcore_liteinst.so"]
        });
        let finished = serde_json::json!({"reason":"build-finished", "success":true});
        let select_native = |messages: &str| {
            native_cdylib_from_cargo_messages(
                messages,
                "actual-local-detcore-package",
                Path::new("/isolated/hermit/detcore-liteinst/Cargo.toml"),
            )
        };
        assert_eq!(
            select_native(&format!("{native}\n{finished}\n")).unwrap(),
            PathBuf::from("/isolated/native-target/libdetcore_liteinst.so")
        );
        // This is the canonical Detcore validate tuple, independently spelled
        // in the artifact fixture. Outer dev/release cannot select another ELF.
        for (field, wrong, wrong_type) in [
            ("opt_level", serde_json::json!("0"), serde_json::json!(3)),
            ("debuginfo", serde_json::json!(2), serde_json::json!("0")),
            (
                "debug_assertions",
                serde_json::json!(false),
                serde_json::json!("true"),
            ),
            (
                "overflow_checks",
                serde_json::json!(false),
                serde_json::json!("true"),
            ),
            ("test", serde_json::json!(true), serde_json::json!("false")),
        ] {
            for value in [Some(wrong), Some(wrong_type), None] {
                let mut bad = native.clone();
                if let Some(value) = value {
                    bad["profile"][field] = value;
                } else {
                    bad["profile"].as_object_mut().unwrap().remove(field);
                }
                let error = select_native(&format!("{bad}\n{finished}\n")).unwrap_err();
                assert!(
                    error.contains("validate profile"),
                    "native Cargo profile {field}: {error}"
                );
            }
        }
        for profile in [serde_json::Value::Null, serde_json::json!({})] {
            let mut bad = native.clone();
            bad["profile"] = profile;
            assert!(
                select_native(&format!("{bad}\n{finished}\n"))
                    .unwrap_err()
                    .contains("validate profile")
            );
        }
        let mut bad = native.clone();
        bad.as_object_mut().unwrap().remove("profile");
        assert!(
            select_native(&format!("{bad}\n{finished}\n"))
                .unwrap_err()
                .contains("validate profile")
        );
        let mut bad = native.clone();
        bad["profile"]["unexpected"] = serde_json::json!(true);
        assert!(
            select_native(&format!("{bad}\n{finished}\n"))
                .unwrap_err()
                .contains("validate profile")
        );
        assert!(select_native(&native.to_string()).is_err());
        assert!(select_native(&format!("{native}\n{native}\n{finished}\n")).is_err());
        assert!(select_native(&format!("{native}\n{finished}\n{finished}\n")).is_err());
        assert!(
            select_native(&format!(
                "{native}\n{}",
                serde_json::json!({"reason":"build-finished","success":false})
            ))
            .is_err()
        );
        for (field, value) in [
            ("package_id", serde_json::json!("old-package")),
            (
                "manifest_path",
                serde_json::json!("/old/detcore-liteinst/Cargo.toml"),
            ),
            ("features", serde_json::json!([])),
            ("features", serde_json::json!(["exec-private-init"])),
            (
                "features",
                serde_json::json!([
                    "exec-native-injection",
                    "exec-private-init",
                    "allocator-fixture"
                ]),
            ),
            (
                "filenames",
                serde_json::json!(["/old/libreverie_liteinst.so"]),
            ),
            (
                "filenames",
                serde_json::json!(["/a/libdetcore_liteinst.so", "/b/libdetcore_liteinst.so"]),
            ),
        ] {
            let mut bad = native.clone();
            bad[field] = value;
            assert!(
                select_native(&format!("{bad}\n{finished}\n")).is_err(),
                "native Cargo {field}"
            );
        }
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

/// Actual native artifact selection is separate from the ordinary preload.
#[cfg(any(feature = "native-exec", test))]
pub(crate) fn native_cdylib_from_cargo_messages(
    messages: &str,
    package_id: &str,
    manifest: &Path,
) -> Result<PathBuf, String> {
    let mut selected = None;
    let mut finished = false;
    for line in messages.lines().filter(|line| !line.is_empty()) {
        let value: Value =
            serde_json::from_str(line).map_err(|e| format!("native Cargo JSON: {e}"))?;
        if finished {
            return Err("native Cargo stream has trailing messages after build-finished".into());
        }
        if value["reason"] == "build-finished" {
            if value["success"] != true {
                return Err("native Cargo build did not finish successfully".into());
            }
            finished = true;
        } else if value["reason"] == "compiler-artifact"
            && value["target"]["name"] == "detcore_liteinst"
        {
            if selected.is_some()
                || value["package_id"] != package_id
                || value["manifest_path"].as_str().map(Path::new) != Some(manifest)
                || value["target"]["kind"] != serde_json::json!(["cdylib"])
                || value["target"]["crate_types"] != serde_json::json!(["cdylib"])
                || value["features"]
                    != serde_json::json!(["exec-native-injection", "exec-private-init"])
            {
                return Err("native Detcore artifact has ambiguous package/target/features".into());
            }
            // Native Detcore uses the same checked validate code generation
            // as the maintained allocator producer, independent of the host
            // CLI/producer profile. Authenticate the actual Cargo artifact.
            if value["profile"]
                != serde_json::json!({"opt_level": "3", "debuginfo": 0,
                    "debug_assertions": true, "overflow_checks": true, "test": false})
            {
                return Err(
                    "native Detcore artifact differs from the exact validate profile".into(),
                );
            }
            let filenames = value["filenames"]
                .as_array()
                .ok_or("native artifact filenames absent")?;
            if filenames.len() != 1 {
                return Err("native Detcore must emit one current cdylib".into());
            }
            let path = PathBuf::from(filenames[0].as_str().ok_or("native filename is not text")?);
            if path.file_name().and_then(|s| s.to_str()) != Some("libdetcore_liteinst.so") {
                return Err("native Detcore filename differs".into());
            }
            selected = Some(path);
        }
    }
    if !finished {
        return Err("native Cargo stream lacks actual successful build-finished".into());
    }
    selected.ok_or_else(|| "current native Detcore artifact absent".into())
}

#[cfg(all(feature = "native-exec", not(test)))]
pub(crate) mod native {
    use std::env;
    use std::time::Duration;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use goblin::elf::Elf;
    use goblin::elf::program_header;
    use reverie_liteinst_artifact as qualifier;
    use sha2::Digest;
    use sha2::Sha256;

    use super::*;

    const GATE: u64 = 0x5000_0000_0000;
    const STREAM_CAP: usize = 16 * 1024 * 1024;
    fn text(path: &Path) -> String {
        path.to_str()
            .expect("native compiler path is not UTF8")
            .to_owned()
    }
    fn hex_array(hex: &str) -> [u8; 32] {
        assert!(hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()));
        std::array::from_fn(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
    }
    fn retain(
        runner: &mut qualifier::Runner,
        program: &str,
        args: Vec<String>,
        cwd: &Path,
    ) -> Vec<u8> {
        runner
            .run(program, &args, cwd)
            .unwrap_or_else(|e| panic!("native producer command failed: {e}"))
    }
    fn gate_digest(path: &Path) -> [u8; 32] {
        let bytes = fs::read(path).expect("read current native bootstrap");
        let elf = Elf::parse(&bytes).expect("parse current native bootstrap");
        let segments: Vec<_> = elf
            .program_headers
            .iter()
            .filter(|p| {
                p.p_type == program_header::PT_LOAD
                    && GATE >= p.p_vaddr
                    && GATE.checked_add(4096).is_some_and(|end| {
                        p.p_vaddr
                            .checked_add(p.p_filesz)
                            .is_some_and(|hi| end <= hi)
                    })
            })
            .collect();
        assert_eq!(segments.len(), 1, "gate page not uniquely file backed");
        let segment = segments[0];
        assert_eq!(segment.p_flags, program_header::PF_R | program_header::PF_X);
        let offset = usize::try_from(
            segment
                .p_offset
                .checked_add(GATE - segment.p_vaddr)
                .unwrap(),
        )
        .unwrap();
        Sha256::digest(
            bytes
                .get(offset..offset + 4096)
                .expect("truncated native gate page"),
        )
        .into()
    }
    fn publish_file(
        name: &str,
        identity: &qualifier::FileIdentity,
        destination: &Path,
        stage_root: &Path,
    ) {
        identity.verify().unwrap();
        let temporary = destination.join(name);
        super::super::copy_into_protected_stage(&identity.path, &temporary).unwrap();
        let copied = qualifier::FileIdentity::read(&temporary).unwrap();
        assert_eq!(copied.sha256, identity.sha256, "native bundle copy differs");
        identity.verify().unwrap();
        let published = stage_root.join(name);
        fs::rename(&temporary, &published).unwrap();
        assert_eq!(
            qualifier::FileIdentity::read(&published).unwrap().sha256,
            identity.sha256,
            "published native bundle copy differs"
        );
    }
    fn publish_digest(name: &str, digest: &[u8; 32], destination: &Path, stage_root: &Path) {
        let temporary = destination.join(name);
        qualifier::write_new(&temporary, digest).unwrap();
        assert_eq!(fs::read(&temporary).unwrap().as_slice(), digest);
        let published = stage_root.join(name);
        fs::rename(&temporary, &published).unwrap();
        qualifier::real_file(&published).unwrap();
        assert_eq!(fs::read(&published).unwrap().as_slice(), digest);
    }
    pub(crate) fn build() {
        println!("cargo:rerun-if-env-changed=HERMIT_NATIVE_EXEC_STAGE");
        println!("cargo:rerun-if-env-changed=HERMIT_LITEINST_REVERIE_PIN");
        println!("cargo:rerun-if-env-changed=PROFILE");
        for file in ["artifact.rs", "build.rs", "Cargo.toml", "Cargo.lock"] {
            println!("cargo:rerun-if-changed={file}");
        }
        let stage_root = PathBuf::from(
            env::var_os("HERMIT_NATIVE_EXEC_STAGE")
                .expect("native stage must be the real host OUT_DIR"),
        );
        assert!(stage_root.is_absolute() && stage_root.is_dir());
        let manifest_dir =
            fs::canonicalize(PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap())).unwrap();
        let compiled_manifest = fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap();
        assert_eq!(
            manifest_dir, compiled_manifest,
            "native producer compile/run source roots differ"
        );
        let root = manifest_dir.parent().unwrap();
        let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let evidence = out.join(format!(
            "native-qualification-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir(&evidence).unwrap();
        let mut runner =
            qualifier::Runner::new(evidence.join("logs"), Duration::from_secs(1200), STREAM_CAP)
                .unwrap();
        let pin =
            env::var("HERMIT_LITEINST_REVERIE_PIN").expect("actual native full RV pin absent");
        assert!(
            pin.len() == 40
                && pin
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
        let source_manifest = root.join("detcore-liteinst/Cargo.toml");
        // The Detcore content profile binds the actual root workspace and its
        // lock, then independently selects this exact runtime leaf. The
        // standalone producer has its own manifest/lock and qualifier source.
        let qualification_manifest = root.join("Cargo.toml");
        let sources = qualifier::VerifiedCompiledSources::resolve_content(
            &qualifier::CompiledSourceExpectation {
                layout: qualifier::CompiledSourceLayout::HermitGit,
                image_kind: qualifier::FixedImageKind::Detcore,
                workspace_root: root,
                manifest: &qualification_manifest,
                reverie_pin: &pin,
            },
            &mut runner,
        )
        .unwrap_or_else(|e| panic!("native compiled-source qualification refused: {e}"));
        let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let metadata_bytes = retain(
            &mut runner,
            &cargo,
            vec![
                "metadata".into(),
                "--locked".into(),
                "--offline".into(),
                "--manifest-path".into(),
                text(&source_manifest),
                "--format-version=1".into(),
            ],
            root,
        );
        let metadata: Value = serde_json::from_slice(&metadata_bytes).unwrap();
        let packages = metadata["packages"].as_array().unwrap();
        let selected: Vec<_> = packages
            .iter()
            .filter(|p| p["name"] == "detcore-liteinst")
            .collect();
        assert_eq!(selected.len(), 1);
        let package = selected[0];
        let package_manifest = PathBuf::from(package["manifest_path"].as_str().unwrap());
        assert_eq!(
            fs::canonicalize(&package_manifest).unwrap(),
            fs::canonicalize(&source_manifest).unwrap()
        );
        assert_eq!(package["id"].as_str().unwrap(), sources.package().id);
        let rv_packages: Vec<_> = packages
            .iter()
            .filter(|p| p["name"] == "reverie-inguest")
            .collect();
        assert_eq!(rv_packages.len(), 1);
        let rv_package = rv_packages[0];
        let expected_source =
            format!("git+https://github.com/rrnewton/reverie.git?rev={pin}#{pin}");
        assert_eq!(rv_package["source"], expected_source);
        let rv_manifest = fs::canonicalize(rv_package["manifest_path"].as_str().unwrap()).unwrap();
        let rv = rv_manifest.parent().unwrap().parent().unwrap();
        assert_eq!(rv_manifest, rv.join("reverie-inguest/Cargo.toml"));
        let qualifier_manifest = rv.join("reverie-liteinst-artifact/Cargo.toml");
        let proof_source =
            qualifier::FileIdentity::read(&rv.join("reverie-liteinst-artifact/src/lib.rs"))
                .unwrap();
        assert_eq!(
            proof_source.sha256,
            qualifier::qualifier_source_sha256(),
            "native proof source differs from actual shared compiler input"
        );
        let helper: Vec<_> = packages
            .iter()
            .filter(|p| p["name"] == "reverie-liteinst-artifact")
            .collect();
        // The runtime graph need not include the host-only qualifier package.
        // Its independent build dependency still has to be this exact source;
        // compile-time source SHA and actual resolved RV path establish that join.
        if !helper.is_empty() {
            assert_eq!(helper.len(), 1);
            assert_eq!(helper[0]["source"], expected_source);
            assert_eq!(
                fs::canonicalize(helper[0]["manifest_path"].as_str().unwrap()).unwrap(),
                qualifier_manifest
            );
        }
        let native_sources = rv.join("reverie-inguest/native_exec");
        let source_names = [
            "bootstrap-image.c",
            "bootstrap-image.h",
            "private-bootstrap.c",
            "private-control.h",
            "private-entry.S",
            "private-fs.S",
            "private-kernel-state.c",
            "private-maps.c",
            "private-bootstrap.ld",
            "private-runner.c",
        ];
        let source_files: Vec<_> = source_names
            .iter()
            .map(|name| {
                let path = native_sources.join(name);
                println!("cargo:rerun-if-changed={}", path.display());
                qualifier::FileIdentity::read(&path).unwrap()
            })
            .collect();
        let nested = out.join("native-runtime-target");
        let build_stdout = retain(
            &mut runner,
            &cargo,
            vec![
                "build".into(),
                "--locked".into(),
                "--offline".into(),
                "--manifest-path".into(),
                text(&qualification_manifest),
                "-p".into(),
                "detcore-liteinst".into(),
                "--lib".into(),
                "--no-default-features".into(),
                "--features".into(),
                "exec-native-injection".into(),
                "--profile".into(),
                "liteinst-runtime".into(),
                "--target-dir".into(),
                text(&nested),
                "--message-format=json-render-diagnostics".into(),
            ],
            root,
        );
        let runtime_path = native_cdylib_from_cargo_messages(
            std::str::from_utf8(&build_stdout).unwrap(),
            package["id"].as_str().unwrap(),
            &package_manifest,
        )
        .unwrap();
        assert!(
            fs::canonicalize(&runtime_path)
                .unwrap()
                .starts_with(fs::canonicalize(&nested).unwrap())
        );
        sources.verify_unchanged(&mut runner).unwrap();
        let qualification = qualifier::qualify_detcore_native_entry_with_sources(
            &runtime_path,
            &mut runner,
            &sources,
        )
        .unwrap_or_else(|e| panic!("actual native runtime entry refused: {e}"));
        let destination = stage_root.join(format!("native-bundle-{}-{stamp}", std::process::id()));
        fs::create_dir(&destination).unwrap();
        let cc = env::var("CC").unwrap_or_else(|_| "cc".into());
        let ld = env::var("LD").unwrap_or_else(|_| "ld".into());
        let common = [
            "-O2",
            "-g",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-fno-stack-protector",
            "-fno-builtin",
            "-ffreestanding",
            "-fno-asynchronous-unwind-tables",
            "-fcf-protection=branch",
        ];
        let mut objects = Vec::new();
        for name in [
            "bootstrap-image.c",
            "private-entry.S",
            "private-bootstrap.c",
            "private-fs.S",
            "private-kernel-state.c",
            "private-maps.c",
        ] {
            let object = destination.join(format!("{name}.boot.o"));
            let mut args = common.iter().map(|s| s.to_string()).collect::<Vec<_>>();
            args.extend([
                "-mcmodel=large".into(),
                "-fno-pic".into(),
                "-fno-pie".into(),
                "-DEXEC_PRIVATE_BOOTSTRAP_IMAGE".into(),
                "-c".into(),
                text(&native_sources.join(name)),
                "-o".into(),
                text(&object),
            ]);
            retain(&mut runner, &cc, args, &native_sources);
            objects.push(object);
        }
        let bootstrap = destination.join("private-bootstrap");
        let mut args = vec![
            "-T".into(),
            text(&native_sources.join("private-bootstrap.ld")),
            "--defsym=GATE_PAGE=0x500000000000".into(),
            "-z".into(),
            "noexecstack".into(),
            "-o".into(),
            text(&bootstrap),
        ];
        args.extend(objects.iter().map(|p| text(p)));
        retain(&mut runner, &ld, args, &native_sources);
        let runtime = destination.join("libdetcore_liteinst.so");
        super::super::copy_into_protected_stage(&runtime_path, &runtime).unwrap();
        let runner_file = destination.join("private-runner");
        let mut args = common.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        args.extend(["-fPIE".into(), "-pie".into(), "-Wl,-z,now,-z,relro".into()]);
        args.extend(
            [
                "private-runner.c",
                "private-fs.S",
                "private-kernel-state.c",
                "private-maps.c",
            ]
            .iter()
            .map(|p| text(&native_sources.join(p))),
        );
        args.extend([
            format!("-L{}", destination.display()),
            "-Wl,--no-as-needed".into(),
            "-ldetcore_liteinst".into(),
            "-Wl,--as-needed".into(),
            "-o".into(),
            text(&runner_file),
        ]);
        retain(&mut runner, &cc, args, &native_sources);
        for source in &source_files {
            source.verify().unwrap();
        }
        proof_source.verify().unwrap();
        sources.verify_unchanged(&mut runner).unwrap();
        let runtime_identity = qualifier::FileIdentity::read(&runtime).unwrap();
        assert_eq!(runtime_identity.sha256, qualification.artifact.sha256);
        qualification.artifact.verify().unwrap();
        let bootstrap_identity = qualifier::FileIdentity::read(&bootstrap).unwrap();
        let runner_identity = qualifier::FileIdentity::read(&runner_file).unwrap();
        let source_digest = detcore_model::liteinst_native_exec_sources::digest(
            reverie_inguest::native_exec_sources::digest(),
        );
        assert_ne!(source_digest, [0; 32]);
        let gate_digest = gate_digest(&bootstrap);
        // Join the actual compiled bootstrap page to the complete template
        // authenticated in the actual runtime. A matching entry address alone
        // does not bind the other bytes that runtime adoption will compare.
        qualification
            .generic_bootstrap
            .fixed_native_gate
            .as_ref()
            .expect("native runtime lacks qualified fixed gate evidence")
            .verify_bootstrap_page_digest(&gate_digest)
            .unwrap_or_else(|e| panic!("native bootstrap/runtime gate pages differ: {e}"));
        // Publish only data, after all existing source/entry/gate qualifications.
        // Each temporary is create-new inside this invocation's unique directory.
        for (name, identity) in [
            ("bootstrap", &bootstrap_identity),
            ("runner", &runner_identity),
            ("runtime", &runtime_identity),
        ] {
            publish_file(
                &format!("native_exec_{name}.bin"),
                identity,
                &destination,
                &stage_root,
            );
            publish_digest(
                &format!("native_exec_{name}.sha256"),
                &hex_array(&identity.sha256),
                &destination,
                &stage_root,
            );
        }
        publish_digest(
            "native_exec_source.sha256",
            &source_digest,
            &destination,
            &stage_root,
        );
        publish_digest(
            "native_exec_gate.sha256",
            &gate_digest,
            &destination,
            &stage_root,
        );
        // Cargo rerun triggers are dependency hints, not source authority.
        // The host build script must watch the actual resolved native checkout
        // as well as its local source dirs after this nested producer returns.
        let dependencies = [
            root.join("detcore-model/src"),
            root.join("detcore/src"),
            root.join("detcore-liteinst/src"),
            root.join("Cargo.toml"),
            root.join("Cargo.lock"),
            rv.join("reverie-liteinst-artifact/src/lib.rs"),
            rv.join("reverie-inguest/src"),
            rv.join("reverie-inguest/native_exec"),
            rv.join("reverie-liteinst/src"),
            rv.join("reverie-rpc-transport/src"),
            rv.join("reverie-process/src"),
        ];
        let mut hints = String::new();
        for path in &dependencies {
            let path = text(path);
            assert!(
                !path.contains(['\n', '\r']),
                "native Cargo dependency hint has line breaks"
            );
            println!("cargo:rerun-if-changed={path}");
            hints.push_str(&path);
            hints.push('\n');
        }
        let hints_temporary = destination.join("native_exec_rerun_paths.txt");
        qualifier::write_new(&hints_temporary, hints.as_bytes()).unwrap();
        fs::rename(
            &hints_temporary,
            stage_root.join("native_exec_rerun_paths.txt"),
        )
        .unwrap();
        qualifier::write_new(&evidence.join("native-qualification.json"), &serde_json::to_vec_pretty(&serde_json::json!({
            "source_digest": source_digest, "compiled_sources": sources.record(), "proof_source": proof_source,
            "native_sources": source_files, "qualification": qualification,
            "bootstrap": bootstrap_identity, "runner": runner_identity, "runtime": runtime_identity,
            "gate_digest": gate_digest, "commands": runner.commands, "executed_tests": 0,
        })).unwrap()).unwrap();
    }
}
