//! Exercise the real standalone source factory in an owned, Gitless copy.
//! This checks source authority; the Buck-stage node checks the complete build.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

use crate::artifact::CompiledSourceExpectation;
use crate::artifact::CompiledSourceLayout;
use crate::artifact::FixedImageKind;
use crate::artifact::Result;
use crate::artifact::Runner;
use crate::artifact::VerifiedCompiledSources;

const CHILD_ROOT: &str = "HERMIT_SOURCE_COPY_TEST_CHILD";
const TEST_NAME: &str = "source_copy::gitless_standalone_compiler_sources";
const SOURCE_FILES: [&str; 7] = [
    "liteinst-runtime-build/Cargo.toml",
    "liteinst-runtime-build/Cargo.lock",
    "liteinst-runtime-build/runtime/Cargo.toml",
    "liteinst-runtime-build/runtime/src/lib.rs",
    "liteinst-runtime-build/build.rs",
    "liteinst-runtime-build/artifact.rs",
    // Cargo still requires the build-only library's declared target to exist.
    "liteinst-runtime-build/src/lib.rs",
];

fn io<T>(result: std::io::Result<T>) -> Result<T> {
    result.map_err(|e| e.to_string())
}

fn text(path: &Path) -> Result<String> {
    Ok(path.to_str().ok_or("fixture path is not UTF-8")?.into())
}

fn resolve(root: &Path, runner: &mut Runner) -> Result<VerifiedCompiledSources> {
    VerifiedCompiledSources::resolve_content(
        &CompiledSourceExpectation {
            layout: CompiledSourceLayout::HermitGit,
            image_kind: FixedImageKind::Standalone,
            workspace_root: root,
            manifest: &root.join("liteinst-runtime-build/Cargo.toml"),
            reverie_pin: env!("HERMIT_REVERIE_PIN"),
        },
        runner,
    )
}

fn record(context: &VerifiedCompiledSources) -> Result<Value> {
    serde_json::to_value(context.record()).map_err(|e| e.to_string())
}

fn reverie_root(context: &VerifiedCompiledSources) -> Result<PathBuf> {
    Ok(context
        .package()
        .manifest
        .parent()
        .and_then(Path::parent)
        .ok_or("preload manifest has no source root")?
        .into())
}

// Share immutable dependency inputs, but own the selected Reverie checkout so
// every negative mutation is private. Never edit Cargo's shared source cache.
fn private_cargo_home(
    source_home: &Path,
    source_reverie: &Path,
    destination: &Path,
    runner: &mut Runner,
) -> Result<()> {
    io(fs::create_dir(destination))?;
    for name in ["registry", "config", "config.toml"] {
        let input = source_home.join(name);
        if input.exists() {
            io(symlink(input, destination.join(name)))?;
        }
    }
    let checkouts = source_home.join("git/checkouts");
    let selected = source_reverie
        .strip_prefix(&checkouts)
        .map_err(|_| "selected Reverie is outside the actual Cargo cache")?;
    let selected_parent = selected.parent().ok_or("checkout parent missing")?;
    let output = destination.join("git/checkouts");
    io(fs::create_dir_all(&output))?;
    io(symlink(
        source_home.join("git/db"),
        destination.join("git/db"),
    ))?;
    for entry in io(fs::read_dir(&checkouts))? {
        let entry = io(entry)?;
        if Path::new(&entry.file_name()) != selected_parent {
            io(symlink(entry.path(), output.join(entry.file_name())))?;
            continue;
        }
        io(fs::create_dir(output.join(entry.file_name())))?;
        for revision in io(fs::read_dir(entry.path()))? {
            let revision = io(revision)?;
            let target = output.join(entry.file_name()).join(revision.file_name());
            if revision.path() == source_reverie {
                runner.run(
                    "cp",
                    &[
                        "-a".into(),
                        "--reflink=auto".into(),
                        "--".into(),
                        text(source_reverie)?,
                        text(&target)?,
                    ],
                    destination,
                )?;
            } else {
                io(symlink(revision.path(), target))?;
            }
        }
    }
    Ok(())
}

fn copied_source_checks(case: &Path, original: &Path, runner: &mut Runner) -> Result<()> {
    let copied = case.join("source");
    assert!(!copied.join(".git").exists());
    let original_context = resolve(original, runner)?;
    let context = resolve(&copied, runner)?;
    let expected = record(&original_context)?;
    let observed = record(&context)?;
    assert_eq!(observed["authority"]["kind"], "declared_content");
    for key in ["workspace_sha256", "reverie_sha256"] {
        assert_eq!(expected["authority"][key], observed["authority"][key]);
        assert_eq!(observed["authority"][key].as_str().unwrap().len(), 64);
    }
    assert_eq!(
        observed["authority"]["workspace_inventory"]
            .as_array()
            .unwrap()
            .len(),
        6
    );
    assert_eq!(
        observed["authority"]["reverie_inventory"]
            .as_array()
            .unwrap()
            .len(),
        // Shared handler extraction adds installed_tool.rs, guest/mod.rs,
        // guest/host.rs and LiteInst tool_host.rs to the declared source catalog.
        // The existing 14 runtime inputs and the checker remain: 19 total.
        19
    );
    let alias = case.join("cargo-alias");
    let private_home = case.join("cargo-home");
    let physical_root = reverie_root(&context)?;
    assert!(physical_root.starts_with(&private_home));
    let suffix = physical_root.strip_prefix(&private_home).unwrap();
    assert_eq!(
        context.compiler_manifest_path()?.as_os_str(),
        alias
            .join(suffix)
            .join("reverie-liteinst-preload/Cargo.toml")
            .as_os_str(),
    );

    // A changed runtime source is rejected both by an existing binding and by
    // a fresh factory. The latter rules out self-authenticating snapshots.
    let role = physical_root.join("reverie-inguest/src/guest/tool_region.rs");
    let bytes = io(fs::read(&role))?;
    let mut changed = bytes.clone();
    changed.extend_from_slice(b"\n// source-copy regression mutation\n");
    io(fs::write(&role, changed))?;
    let after = context.verify_unchanged(runner).unwrap_err();
    assert!(after.contains("reviewed catalog"), "{after}");
    let before = match resolve(&copied, runner) {
        Ok(_) => panic!("a fresh factory accepted changed runtime source"),
        Err(error) => error,
    };
    assert!(before.contains("reviewed catalog"), "{before}");
    io(fs::write(&role, &bytes))?;
    assert_eq!(io(fs::read(&role))?, bytes);
    context.verify_unchanged(runner)?;
    resolve(&copied, runner)?;

    // Even byte-identical sources at a new physical root must not reuse an
    // already-resolved logical Cargo root binding.
    let moved_home = case.join("cargo-home-moved");
    io(fs::rename(&private_home, &moved_home))?;
    io(fs::remove_file(&alias))?;
    io(symlink(&moved_home, &alias))?;
    let retarget = context.verify_unchanged(runner).unwrap_err();
    assert!(retarget.contains("root mapping changed"), "{retarget}");
    io(fs::remove_file(&alias))?;
    io(fs::rename(&moved_home, &private_home))?;
    io(symlink(&private_home, &alias))?;
    context.verify_unchanged(runner)?;
    assert!(
        runner
            .commands
            .iter()
            .all(|command| command.program != "git")
    );
    Ok(())
}

#[test]
fn gitless_standalone_compiler_sources() -> Result<()> {
    let original = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("Hermit source root missing")?;
    if let Some(case) = std::env::var_os(CHILD_ROOT) {
        let case = PathBuf::from(case);
        let mut runner = Runner::new(case.join("child-logs"), Duration::from_secs(90), 8 << 20)?;
        return copied_source_checks(&case, original, &mut runner);
    }
    let directory = tempfile::Builder::new()
        .prefix("liteinst-source-copy-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .map_err(|e| e.to_string())?;
    let case = io(fs::canonicalize(directory.path()))?;
    let copied = case.join("source");
    for relative in SOURCE_FILES {
        let target = copied.join(relative);
        io(fs::create_dir_all(target.parent().unwrap()))?;
        io(fs::copy(original.join(relative), target))?;
    }
    let mut runner = Runner::new(case.join("parent-logs"), Duration::from_secs(120), 8 << 20)?;
    let original_context = resolve(original, &mut runner)?;
    let cargo_home = match std::env::var_os("CARGO_HOME") {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(std::env::var_os("HOME").ok_or("Cargo home missing")?).join(".cargo"),
    };
    let cargo_home = io(fs::canonicalize(cargo_home))?;
    private_cargo_home(
        &cargo_home,
        &reverie_root(&original_context)?,
        &case.join("cargo-home"),
        &mut runner,
    )?;
    io(symlink(case.join("cargo-home"), case.join("cargo-alias")))?;
    // Change the child's environment only. Concurrent allocator workload tests
    // retain their original cache, and only this source check is re-executed.
    runner.run(
        "env",
        &[
            format!("{CHILD_ROOT}={}", text(&case)?),
            format!("CARGO_HOME={}", text(&case.join("cargo-alias"))?),
            "CARGO_NET_OFFLINE=true".into(),
            text(&io(std::env::current_exe())?)?,
            "--exact".into(),
            TEST_NAME.into(),
            "--nocapture".into(),
            "--test-threads=1".into(),
        ],
        &copied,
    )?;
    original_context.verify_unchanged(&mut runner)?;
    Ok(())
}
