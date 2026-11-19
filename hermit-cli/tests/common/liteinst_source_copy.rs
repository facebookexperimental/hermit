//! Exercise the real source factories in owned Gitless and Cargo-checkout copies.
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

/// Wall hang backstop for each Runner: 8x the former wall-primary budgets
/// (90 s child, 120 s parent), capped at the owner rule's 600 s. CPU is the
/// primary limit for the child run below; donor job eb64ff5f (2026-11-05,
/// load ~980) failed this test at 105.8 s wall for ~5 s of CPU work
/// (whole test measured 5.08 s CPU locally, 2026-11-05).
const WALL_BACKSTOP: Duration = Duration::from_secs(600);
/// CPU budget for the whole child test process tree (cgroup-accounted by
/// `Runner::run_cpu_bounded`): ~3x the measured whole-test CPU above, the
/// child being only part of it.
const CHILD_CPU_BUDGET: Duration = Duration::from_secs(15);

const CHILD_ROOT: &str = "HERMIT_SOURCE_COPY_TEST_CHILD";
const TEST_NAME: &str = "source_copy::gitless_standalone_compiler_sources";
const TRANSACTION_TEST_NAME: &str =
    "source_copy::compiled_source_transaction_closes_both_result_paths";
const SOURCE_FILES: [&str; 10] = [
    // detcore-model inherits its Reverie dependencies from the root
    // workspace manifest, so Cargo metadata needs that root in the copy.
    "Cargo.toml",
    "liteinst-runtime-build/Cargo.toml",
    "liteinst-runtime-build/Cargo.lock",
    "liteinst-runtime-build/runtime/Cargo.toml",
    "liteinst-runtime-build/runtime/src/lib.rs",
    "liteinst-runtime-build/build.rs",
    "liteinst-runtime-build/artifact.rs",
    // Cargo still requires the build-only library's declared target to exist.
    "liteinst-runtime-build/src/lib.rs",
    // Cargo metadata validates optional local dependencies even with native-exec
    // disabled. Retain the real manifest and declared target, not a stub crate.
    "detcore-model/Cargo.toml",
    "detcore-model/src/lib.rs",
];

fn io<T>(result: std::io::Result<T>) -> Result<T> {
    result.map_err(|e| e.to_string())
}

fn text(path: &Path) -> Result<String> {
    Ok(path.to_str().ok_or("fixture path is not UTF-8")?.into())
}

/// The one named Reverie repository these Hermit source-copy
/// qualifications expect (see the same-named constant in
/// liteinst-runtime-build).
const REVERIE_GIT_REPOSITORY: &str = "https://github.com/facebookexperimental/reverie.git";

/// This consumer's declared workspace facts for the compiled-source
/// factories: the Reverie tree's relative path, the leaf package, the
/// producer packages and their lockfile, and both workspace
/// inventories, spelled exactly as this tree's manifests name them.
fn consumer_source_declaration() -> crate::artifact::ConsumerSourceDeclaration<'static> {
    crate::artifact::ConsumerSourceDeclaration {
        reverie_relative: Path::new("reverie"),
        leaf: Some(crate::artifact::DeclaredLeafPackage {
            name: "detcore-liteinst",
            target: "detcore_liteinst",
            manifest_relative: "detcore-liteinst/Cargo.toml",
            version: "0.5.7",
        }),
        runtime_packages: &[
            crate::artifact::DeclaredRuntimePackage {
                name: "hermit-liteinst-runtime-build",
                manifest_relative: "liteinst-runtime-build/Cargo.toml",
                reverie_dependency: "reverie-liteinst-artifact",
                dependency_kind: Some("build"),
                target: None,
                version: None,
            },
            crate::artifact::DeclaredRuntimePackage {
                name: "hermit-liteinst-runtime-artifact",
                manifest_relative: "liteinst-runtime-build/runtime/Cargo.toml",
                reverie_dependency: "reverie-liteinst-preload",
                dependency_kind: None,
                target: Some("hermit_liteinst_runtime_artifact"),
                version: Some("0.5.1"),
            },
        ],
        runtime_lock_relative: "liteinst-runtime-build/Cargo.lock",
        standalone_workspace_files: &[
            "liteinst-runtime-build/Cargo.toml",
            "liteinst-runtime-build/Cargo.lock",
            "liteinst-runtime-build/runtime/Cargo.toml",
            "liteinst-runtime-build/runtime/src/lib.rs",
            "liteinst-runtime-build/build.rs",
            "liteinst-runtime-build/artifact.rs",
        ],
        leaf_workspace_files: &[
            "liteinst-runtime-build/Cargo.toml",
            "liteinst-runtime-build/Cargo.lock",
            "liteinst-runtime-build/runtime/Cargo.toml",
            "liteinst-runtime-build/runtime/src/lib.rs",
            "liteinst-runtime-build/build.rs",
            "liteinst-runtime-build/artifact.rs",
            "Cargo.toml",
            "Cargo.lock",
            "detcore-liteinst/Cargo.toml",
            "detcore-liteinst/src/lib.rs",
        ],
    }
}

fn reverie_repository() -> reverie_liteinst_artifact::private_static::CargoGitRepository<'static> {
    reverie_liteinst_artifact::private_static::CargoGitRepository::new(REVERIE_GIT_REPOSITORY)
        .expect("the named Reverie repository is a valid https URL")
}

/// Whether the Reverie root carries its own Git metadata: a `.git`
/// directory or file at the Reverie root itself, the same test the
/// content factory's pin proof uses for its canonical root (via the
/// logical link here, which names the same entry). No
/// parent-directory discovery (see liteinst-runtime-build).
fn reverie_root_has_git_metadata(reverie_root: &Path) -> bool {
    reverie_root.join(".git").exists()
}

/// Whether the workspace's nested manifest spells its Reverie
/// dependencies by path (`true`) or by git revision (`false`): every
/// non-comment Reverie dependency line must agree.
fn manifest_is_path_sourced(root: &Path) -> Result<bool> {
    let text = io(fs::read_to_string(
        root.join("liteinst-runtime-build/Cargo.toml"),
    ))?;
    let mut path_sourced: Option<bool> = None;
    for line in text.lines() {
        let line = line.trim_start();
        if line.starts_with('#') || !line.contains("reverie") {
            continue;
        }
        let vote = if line.contains("git =") {
            Some(false)
        } else if line.contains("path =") && line.contains("reverie/") {
            Some(true)
        } else {
            None
        };
        if let Some(vote) = vote {
            match path_sourced {
                Some(seen) if seen != vote => {
                    return Err("manifest mixes git and path Reverie dependencies".into());
                }
                Some(_) => {}
                None => path_sourced = Some(vote),
            }
        }
    }
    path_sourced.ok_or_else(|| "manifest names no Reverie git or path dependency".into())
}

/// The routed layout for this workspace (see the same route in
/// liteinst-runtime-build): `PathCheckout` only for a gitless
/// path-sourced Reverie root; a root with its own Git metadata takes
/// `GitlinkCheckout`, so the pin proof runs for a real checkout.
fn source_layout(root: &Path) -> Result<CompiledSourceLayout> {
    Ok(
        if manifest_is_path_sourced(root)? && !reverie_root_has_git_metadata(&root.join("reverie"))
        {
            CompiledSourceLayout::PathCheckout
        } else {
            CompiledSourceLayout::GitlinkCheckout
        },
    )
}

fn resolve(root: &Path, runner: &mut Runner) -> Result<VerifiedCompiledSources> {
    resolve_as(root, source_layout(root)?, runner)
}

fn resolve_as(
    root: &Path,
    layout: CompiledSourceLayout,
    runner: &mut Runner,
) -> Result<VerifiedCompiledSources> {
    VerifiedCompiledSources::resolve_content_with_repository(
        &CompiledSourceExpectation {
            layout,
            image_kind: FixedImageKind::Standalone,
            workspace_root: root,
            manifest: &root.join("liteinst-runtime-build/Cargo.toml"),
            reverie_pin: env!("HERMIT_REVERIE_PIN"),
            consumer: Some(consumer_source_declaration()),
        },
        reverie_repository(),
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

/// Path-sourced counterpart of the git checks below: the copied
/// workspace carries its own private `reverie` tree behind the logical
/// `<workspace>/reverie` link, with no Cargo git checkout anywhere.
/// The gitless copy always routes to the content-only layout. The
/// original routes by its own Reverie root: content-only when that
/// root is gitless too (the fbsource sibling link), in which case the
/// two content authorities hash identically over byte-identical
/// trees; the Git layout (reviewed catalog plus pin proof) when it
/// is a real checkout. Each layout's record is pinned exactly below.
/// A changed runtime source must refuse both an existing binding and
/// a fresh factory, and retargeting the logical link must break it.
fn copied_path_source_checks(case: &Path, original: &Path, runner: &mut Runner) -> Result<()> {
    let copied = case.join("source");
    assert!(!copied.join(".git").exists());
    let private_reverie = case.join("reverie-private");
    assert!(!private_reverie.join(".git").exists());
    assert_eq!(
        source_layout(&copied)?,
        CompiledSourceLayout::PathCheckout,
        "gitless copy must route to the content-only layout",
    );
    let original_context = resolve(original, runner)?;
    // The original's own resolution may run the Git pin proof; the
    // gitless copy's must not run git at all (asserted at the end).
    let before_copied = runner.commands().len();
    let context = resolve(&copied, runner)?;
    let expected = record(&original_context)?;
    let observed = record(&context)?;
    assert_eq!(observed["layout"], "HermitPath");
    assert_eq!(observed["authority"]["kind"], "declared_content");
    assert_eq!(expected["authority"]["kind"], "declared_content");
    // The workspace authority is layout-independent: the same six
    // declared files with the same bytes.
    assert_eq!(
        expected["authority"]["workspace_sha256"],
        observed["authority"]["workspace_sha256"],
    );
    for key in ["workspace_sha256", "reverie_sha256"] {
        assert_eq!(observed["authority"][key].as_str().unwrap().len(), 64);
        assert_eq!(expected["authority"][key].as_str().unwrap().len(), 64);
    }
    assert_eq!(
        observed["authority"]["workspace_inventory"]
            .as_array()
            .unwrap()
            .len(),
        6
    );
    assert_eq!(
        expected["authority"]["workspace_inventory"]
            .as_array()
            .unwrap()
            .len(),
        6
    );
    match source_layout(original)? {
        CompiledSourceLayout::PathCheckout => {
            assert_eq!(expected["layout"], "HermitPath");
            assert_eq!(
                expected["authority"]["reverie_sha256"], observed["authority"]["reverie_sha256"],
                "gitless original and gitless copy must bind identical content",
            );
            assert_eq!(
                expected["authority"]["reverie_inventory"]
                    .as_array()
                    .unwrap()
                    .len(),
                507,
            );
        }
        CompiledSourceLayout::GitlinkCheckout => {
            assert_eq!(expected["layout"], "HermitGit");
            // The Git layout binds the reviewed catalog (the 89-entry
            // set the git family's checks pin for the same arm: the
            // 88-row CONTENT_REVERIE_CATALOG plus the qualifier
            // lib.rs) and the pin proof that ran above; the gitless
            // copy binds the full compiled closure instead.
            assert_eq!(
                expected["authority"]["reverie_inventory"]
                    .as_array()
                    .unwrap()
                    .len(),
                89,
            );
        }
        other => panic!("path-sourced family routed the original to {other:?}"),
    }
    // The path inventory is the reviewed catalog plus every
    // source-shaped file in the compiled closure, not the git arm's
    // catalog alone: 507 entries on this tree (506 measured
    // 2026-11-16, plus reverie-liteinst/tests/aarch64_trap_install.rs
    // from the landed aarch64 trap-install change).
    let reverie_inventory = observed["authority"]["reverie_inventory"]
        .as_array()
        .unwrap();
    assert_eq!(reverie_inventory.len(), 507);
    let mut inventory_paths: Vec<&str> = reverie_inventory
        .iter()
        .map(|entry| entry["relative"].as_str().unwrap())
        .collect();
    assert!(
        inventory_paths.contains(&"reverie-liteinst/tests/aarch64_trap_install.rs"),
        "path inventory lost the aarch64 trap-install test source",
    );
    inventory_paths.sort_unstable();
    // Digest input format, exactly: the sorted inventory relative
    // paths, one per line — joined with "\n" and terminated by one
    // trailing "\n" — as UTF-8 bytes. Any inventory addition,
    // removal, or rename changes this digest and must update the
    // constant deliberately, like the count above.
    assert_eq!(
        crate::artifact::sha256(format!("{}\n", inventory_paths.join("\n")).as_bytes()),
        "12925ab51846415091a7e663f27445afb0e53e3f7a60aac85d782051781399ba",
    );
    assert_eq!(
        context.compiler_manifest_path()?.as_os_str(),
        copied
            .join("reverie/reverie-liteinst-preload/Cargo.toml")
            .as_os_str(),
    );

    // The recorded package root is canonical (the private tree); the
    // compiler spelling asserted above stays logical.
    let physical_root = reverie_root(&context)?;
    assert_eq!(physical_root, private_reverie);
    let role = private_reverie.join("reverie-inguest/src/guest/tool_region.rs");
    let bytes = io(fs::read(&role))?;
    let mut changed = bytes.clone();
    changed.extend_from_slice(b"\n// source-copy regression mutation\n");
    io(fs::write(&role, changed))?;
    let after = context.verify_unchanged(runner).unwrap_err();
    assert!(after.contains("catalog"), "{after}");
    let before = match resolve(&copied, runner) {
        Ok(_) => panic!("a fresh factory accepted changed runtime source"),
        Err(error) => error,
    };
    assert!(before.contains("catalog"), "{before}");
    io(fs::write(&role, &bytes))?;
    assert_eq!(io(fs::read(&role))?, bytes);
    context.verify_unchanged(runner)?;
    resolve(&copied, runner)?;

    // Byte-identical sources at a new physical root must not reuse an
    // already-resolved logical root binding.
    let moved = case.join("reverie-moved");
    io(fs::rename(&private_reverie, &moved))?;
    io(fs::remove_file(copied.join("reverie")))?;
    io(symlink(&moved, copied.join("reverie")))?;
    let retarget = context.verify_unchanged(runner).unwrap_err();
    assert!(retarget.contains("mapping changed"), "{retarget}");
    io(fs::remove_file(copied.join("reverie")))?;
    io(fs::rename(&moved, &private_reverie))?;
    io(symlink(&private_reverie, copied.join("reverie")))?;
    context.verify_unchanged(runner)?;
    assert!(
        runner.commands()[before_copied..]
            .iter()
            .all(|command| command.program() != "git"),
        "gitless copy resolution ran git",
    );
    Ok(())
}

fn copied_source_checks(case: &Path, original: &Path, runner: &mut Runner) -> Result<()> {
    // The check family follows the manifest spelling; the routed
    // layout of each tree is asserted inside its own family.
    if manifest_is_path_sourced(original)? {
        return copied_path_source_checks(case, original, runner);
    }
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
        // Standalone DeclaredContent inventory, by its list
        // definition in content_reverie_inventory_for_image
        // (reverie-liteinst-artifact/src/lib.rs): every relative in
        // the 88-row CONTENT_REVERIE_CATALOG plus the separately
        // bound qualifier reverie-liteinst-artifact/src/lib.rs:
        // 88 + 1 = 89. Detcore adds the fixed-gate row for 90; the
        // library test pins both (89/90). Measured in the Git
        // layout 2026-11-17: the GitlinkCheckout arm above failed
        // left 89 vs the stale 87 pinned here and there.
        89
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
            .commands()
            .iter()
            .all(|command| command.program() != "git")
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
        let mut runner = Runner::new(case.join("child-logs"), WALL_BACKSTOP, 8 << 20)?;
        return copied_source_checks(&case, original, &mut runner);
    }
    run_private_copy_test(original, TEST_NAME)
}

/// Routing control: path-form manifests over a Reverie root that IS a
/// Git checkout must run the Git-layout pin proof, not the gitless
/// content-only layout. The fixture is the gitless private copy above
/// made into a fresh Git repository whose HEAD is a new commit, not
/// the pin: resolution must refuse it by the pin check's own name.
#[test]
fn git_checkout_at_wrong_head_refused() -> Result<()> {
    let original = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("Hermit source root missing")?;
    // Premise gate: this control is about the path-form route. A
    // git-form tree already selects the Git layout on every route,
    // so a non-path-spelled fixture must fail loudly here rather
    // than silently skip the pin-proof control.
    let nested = io(fs::read_to_string(
        original.join("liteinst-runtime-build/Cargo.toml"),
    ))?;
    assert!(
        nested
            .lines()
            .any(|line| line.contains("path =") && line.contains("reverie/")),
        "wrong-head control requires path-spelled Reverie manifests",
    );
    let directory = tempfile::Builder::new()
        .prefix("liteinst-wrong-head-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .map_err(|e| e.to_string())?;
    let case = io(fs::canonicalize(directory.path()))?;
    let copied = case.join("source");
    for relative in SOURCE_FILES {
        let target = copied.join(relative);
        io(fs::create_dir_all(target.parent().unwrap()))?;
        io(fs::copy(original.join(relative), target))?;
    }
    let mut runner = Runner::new(case.join("logs"), WALL_BACKSTOP, 8 << 20)?;
    let source_reverie = io(fs::canonicalize(original.join("reverie")))?;
    // A real directory at the workspace spelling, not a link: the
    // Git-layout binding admits only the canonical spelling (or the
    // Cargo-home checkout alias), and this control must reach the
    // pin proof, not stop at the alias gate.
    let private_reverie = copied.join("reverie");
    runner.run(
        "cp",
        &[
            "-a".into(),
            "--reflink=auto".into(),
            "--".into(),
            text(&source_reverie)?,
            text(&private_reverie)?,
        ],
        &case,
    )?;
    // Start from the gitless copy, then make it a Git checkout at a
    // fresh HEAD: exactly the tree the content-only route must not
    // certify on location plus catalogued content.
    let copied_git = private_reverie.join(".git");
    if copied_git.is_dir() {
        io(fs::remove_dir_all(&copied_git))?;
    } else if copied_git.exists() {
        io(fs::remove_file(&copied_git))?;
    }
    let git = |args: &[&str]| -> Result<String> {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(&private_reverie)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .map_err(|e| e.to_string())?;
        if !output.status.success() {
            return Err(format!(
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        String::from_utf8(output.stdout).map_err(|e| e.to_string())
    };
    git(&["init", "-q"])?;
    git(&["add", "--", "Cargo.toml"])?;
    git(&[
        "-c",
        "user.name=wrong-head-fixture",
        "-c",
        "user.email=wrong-head-fixture@example.invalid",
        "commit",
        "-q",
        "-m",
        "wrong-head fixture",
    ])?;
    let head = git(&["rev-parse", "HEAD"])?.trim().to_owned();
    assert_eq!(head.len(), 40, "{head}");
    assert_ne!(head, env!("HERMIT_REVERIE_PIN"));
    match resolve(&copied, &mut runner) {
        Ok(_) => panic!(
            "git checkout at wrong HEAD {head} was accepted; the route must \
             run the Git-layout pin check for a Reverie root with .git"
        ),
        Err(error) => assert!(
            error.contains("content Reverie checkout HEAD differs from independent full pin"),
            "{error}",
        ),
    }
    assert!(
        runner
            .commands()
            .iter()
            .any(|command| command.program() == "git"),
        "pin proof never ran git",
    );
    Ok(())
}

/// Path-sourced setup: own a private copy of the whole Reverie tree
/// (the compiled closure, not a Cargo checkout), link it at the copied
/// workspace's logical `reverie`, and re-execute only this source check
/// in the child. No CARGO_HOME surgery: path dependencies never consult
/// a git checkout, and negative mutations stay inside the private copy.
fn run_private_path_copy_child(
    directory: tempfile::TempDir,
    case: &Path,
    copied: &Path,
    original: &Path,
    original_context: &VerifiedCompiledSources,
    test_name: &str,
    runner: &mut Runner,
) -> Result<()> {
    let source_reverie = io(fs::canonicalize(original.join("reverie")))?;
    let private_reverie = case.join("reverie-private");
    runner.run(
        "cp",
        &[
            "-a".into(),
            "--reflink=auto".into(),
            "--".into(),
            text(&source_reverie)?,
            text(&private_reverie)?,
        ],
        case,
    )?;
    // The copy is gitless even when the source tree was a submodule.
    let copied_git = private_reverie.join(".git");
    if copied_git.is_dir() {
        io(fs::remove_dir_all(&copied_git))?;
    } else if copied_git.exists() {
        io(fs::remove_file(&copied_git))?;
    }
    io(symlink(&private_reverie, copied.join("reverie")))?;
    let child = vec![
        format!("{CHILD_ROOT}={}", text(case)?),
        "CARGO_NET_OFFLINE=true".into(),
        text(&io(std::env::current_exe())?)?,
        "--exact".into(),
        test_name.into(),
        "--nocapture".into(),
        "--test-threads=1".into(),
    ];
    let child_run = runner
        .run_cpu_bounded("env", &child, copied, CHILD_CPU_BUDGET)
        .or_else(|error| {
            if error.contains("producer CPU accounting unavailable") {
                eprintln!(
                    "source-copy child: CPU accounting unavailable ({error}); \
                     running under the wall backstop only"
                );
                runner.run("env", &child, copied)
            } else {
                Err(error)
            }
        });
    if let Err(error) = child_run {
        let retained = directory.keep();
        return Err(format!("{error}; retained fixture {}", retained.display()));
    }
    original_context.verify_unchanged(runner)?;
    Ok(())
}

fn run_private_copy_test(original: &Path, test_name: &str) -> Result<()> {
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
    let mut runner = Runner::new(case.join("parent-logs"), WALL_BACKSTOP, 8 << 20)?;
    let original_context = resolve(original, &mut runner)?;
    if manifest_is_path_sourced(original)? {
        return run_private_path_copy_child(
            directory,
            &case,
            &copied,
            original,
            &original_context,
            test_name,
            &mut runner,
        );
    }
    let cargo_home = match std::env::var_os("CARGO_HOME") {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(std::env::var_os("HOME").ok_or("Cargo home missing")?).join(".cargo"),
    };
    let cargo_home = io(fs::canonicalize(cargo_home))?;
    let source_reverie = reverie_root(&original_context)?;
    private_cargo_home(
        &cargo_home,
        &source_reverie,
        &case.join("cargo-home"),
        &mut runner,
    )?;
    io(symlink(case.join("cargo-home"), case.join("cargo-alias")))?;
    // Change the child's environment only. Concurrent allocator workload tests
    // retain their original cache, and only this source check is re-executed.
    let mut child = vec![
        format!("{CHILD_ROOT}={}", text(&case)?),
        format!("CARGO_HOME={}", text(&case.join("cargo-alias"))?),
        "CARGO_NET_OFFLINE=true".into(),
    ];
    if test_name == TRANSACTION_TEST_NAME {
        // cp -a can retain the source cache's owner in the pinned root. Trust
        // exactly this owned copied checkout in the child only, preserving any
        // inherited URL rewrites and leaving all source-cleanliness checks on.
        let count = std::env::var("GIT_CONFIG_COUNT")
            .unwrap_or_else(|_| "0".into())
            .parse::<usize>()
            .map_err(|e| format!("inherited Git config count: {e}"))?;
        let private_reverie = case.join("cargo-home").join(
            source_reverie
                .strip_prefix(&cargo_home)
                .map_err(|_| "selected Reverie is outside the actual Cargo cache")?,
        );
        child.extend([
            format!("GIT_CONFIG_COUNT={}", count + 1),
            format!("GIT_CONFIG_KEY_{count}=safe.directory"),
            format!("GIT_CONFIG_VALUE_{count}={}", text(&private_reverie)?),
        ]);
    }
    child.extend([
        text(&io(std::env::current_exe())?)?,
        "--exact".into(),
        test_name.into(),
        "--nocapture".into(),
        "--test-threads=1".into(),
    ]);
    // The child run is CPU-bounded (primary) under the Runner's wall
    // backstop. Where cgroup CPU accounting is unavailable the command is
    // not started; fall back to the wall backstop alone and say so, rather
    // than failing a test the host cannot account.
    let child_run = runner
        .run_cpu_bounded("env", &child, &copied, CHILD_CPU_BUDGET)
        .or_else(|error| {
            if error.contains("producer CPU accounting unavailable") {
                eprintln!(
                    "source-copy child: CPU accounting unavailable ({error}); \
                     running under the wall backstop only"
                );
                runner.run("env", &child, &copied)
            } else {
                Err(error)
            }
        });
    if let Err(error) = child_run {
        let retained = directory.keep();
        return Err(format!("{error}; retained fixture {}", retained.display()));
    }
    original_context.verify_unchanged(&mut runner)?;
    Ok(())
}

/// Path-sourced closing checks: the Git factory's `with_resolved`
/// transaction refuses the path layout by name, so the content
/// factory's own closing scan (`verify_unchanged`, the same scan the
/// Git transaction closes with) must catch a mutation made while a
/// resolved context is in use, on both a succeeding and a failing
/// body, with both errors retained on the failure path.
fn path_transaction_checks(case: &Path, runner: &mut Runner) -> Result<()> {
    let copied = case.join("source");
    assert_eq!(
        source_layout(&copied)?,
        CompiledSourceLayout::PathCheckout,
        "gitless copy must route to the content-only layout",
    );
    let private_reverie = case.join("reverie-private");
    let expected = CompiledSourceExpectation {
        layout: CompiledSourceLayout::PathCheckout,
        image_kind: FixedImageKind::Standalone,
        workspace_root: &copied,
        manifest: &copied.join("liteinst-runtime-build/Cargo.toml"),
        reverie_pin: env!("HERMIT_REVERIE_PIN"),
        consumer: Some(consumer_source_declaration()),
    };
    let refused = VerifiedCompiledSources::with_resolved_with_repository(
        &expected,
        reverie_repository(),
        runner,
        |_| Ok(()),
    );
    let error = match refused {
        Ok(_) => panic!("the Git transaction accepted the path layout"),
        Err(error) => error,
    };
    assert!(
        error.contains("path-sourced layout requires the declared-content factory"),
        "{error}",
    );

    let role = private_reverie.join("reverie-inguest/src/guest/tool_region.rs");
    let bytes = io(fs::read(&role))?;
    let mut changed = bytes.clone();
    changed.extend_from_slice(b"\n// transaction closing-scan regression mutation\n");

    // Success path: a mutation during use fails the mandatory close.
    let context = resolve(&copied, runner)?;
    io(fs::write(&role, &changed))?;
    let close = context.verify_unchanged(runner);
    io(fs::write(&role, &bytes))?;
    assert_eq!(io(fs::read(&role))?, bytes);
    let error = close.unwrap_err();
    assert!(error.contains("catalog"), "{error}");

    // Failure path: the body error and the closing error both surface.
    let context = resolve(&copied, runner)?;
    io(fs::write(&role, &changed))?;
    let body_error = "deliberate receipt-only body failure";
    let close = context.verify_unchanged(runner);
    io(fs::write(&role, &bytes))?;
    assert_eq!(io(fs::read(&role))?, bytes);
    let combined = format!(
        "{body_error}; compiled source transaction close failed: {}",
        close.unwrap_err(),
    );
    assert!(
        combined.contains("deliberate receipt-only body failure") && combined.contains("catalog"),
        "{combined}",
    );
    resolve(&copied, runner)?;
    assert!(
        runner
            .commands()
            .iter()
            .all(|command| command.program() != "git")
    );
    Ok(())
}

fn transaction_checks(case: &Path, runner: &mut Runner) -> Result<()> {
    if case.join("reverie-private").exists() {
        return path_transaction_checks(case, runner);
    }
    // Use the existing content factory only to locate our owned physical Cargo
    // checkout. The transaction itself must exercise the real Git workspace
    // factory and fresh locked/offline metadata, not a synthesized binding.
    let content = resolve(&case.join("source"), runner)?;
    let root = reverie_root(&content)?;
    assert!(root.starts_with(case.join("cargo-home")));
    assert!(root.join(".git").exists());
    prepare_private_index(&root, runner)?;
    let manifest = root.join("Cargo.toml");
    let expected = CompiledSourceExpectation {
        layout: CompiledSourceLayout::ReverieWorkspace,
        image_kind: FixedImageKind::Standalone,
        workspace_root: &root,
        manifest: &manifest,
        reverie_pin: env!("HERMIT_REVERIE_PIN"),
        consumer: None,
    };
    let (value, owned_record) =
        VerifiedCompiledSources::with_resolved(&expected, runner, |context| {
            assert_eq!(context.package().name, "reverie-liteinst-preload");
            assert_eq!(
                context.package().manifest,
                root.join("reverie-liteinst-preload/Cargo.toml"),
            );
            Ok("receipt-only body completed")
        })?;
    assert_eq!(value, "receipt-only body completed");
    let owned_record = serde_json::to_value(owned_record).map_err(|e| e.to_string())?;
    assert_eq!(owned_record["authority"]["kind"], "git");
    assert_eq!(owned_record["reverie_pin"], env!("HERMIT_REVERIE_PIN"));

    let clean_failure = VerifiedCompiledSources::with_resolved(&expected, runner, |_| {
        Err::<(), _>("unchanged receipt-only body failure".into())
    });
    let error = match clean_failure {
        Ok(_) => panic!("a clean failed body unexpectedly returned success"),
        Err(error) => error,
    };
    assert_eq!(error, "unchanged receipt-only body failure");

    let role = root.join("reverie-inguest/src/guest/tool_region.rs");
    let bytes = io(fs::read(&role))?;
    let mut changed = bytes.clone();
    changed.extend_from_slice(b"\n// transaction closing-scan regression mutation\n");
    let mut entered = false;
    let success = VerifiedCompiledSources::with_resolved(&expected, runner, |_| {
        entered = true;
        io(fs::write(&role, &changed))?;
        Ok(())
    });
    // Restore only our private copy before inspecting either outcome, so the
    // next independent resolution starts from the original tracked contents.
    io(fs::write(&role, &bytes))?;
    assert_eq!(io(fs::read(&role))?, bytes);
    assert!(entered, "mutating success body was never admitted");
    let error = match success {
        Ok(_) => panic!("a successful body published sources changed during use"),
        Err(error) => error,
    };
    assert!(
        error.contains("compiled source has dirty tracked files"),
        "closing success failed for an unrelated reason: {error}",
    );

    entered = false;
    let failure = VerifiedCompiledSources::with_resolved(&expected, runner, |_| {
        entered = true;
        io(fs::write(&role, &changed))?;
        Err::<(), _>("deliberate receipt-only body failure".into())
    });
    io(fs::write(&role, &bytes))?;
    assert_eq!(io(fs::read(&role))?, bytes);
    assert!(entered, "mutating failure body was never admitted");
    let error = match failure {
        Ok(_) => panic!("a failed body unexpectedly published compiled sources"),
        Err(error) => error,
    };
    assert!(
        error.contains("deliberate receipt-only body failure")
            && error.contains("compiled source has dirty tracked files"),
        "failed body did not retain both body and mandatory closing errors: {error}",
    );
    // Recheck the restored real checkout rather than letting a failed control
    // authorize any later source use.
    VerifiedCompiledSources::resolve(&expected, runner)?;
    assert!(
        runner
            .commands()
            .iter()
            .any(|command| command.program() == "git")
    );
    Ok(())
}

fn prepare_private_index(root: &Path, runner: &mut Runner) -> Result<()> {
    // cp -a preserves the old stat cache, but every copied inode/ctime changes.
    // Refresh only our owned copy once. The real source factories below keep
    // their ordinary read-only checks, including both deliberate mutations.
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_IMPLICIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_GRAFT_FILE",
        "GIT_SHALLOW_FILE",
        "GIT_PREFIX",
    ] {
        if std::env::var_os(name).is_some() {
            return Err(format!("private index preparation refuses {name}"));
        }
    }
    let mut status_args = vec!["GIT_OPTIONAL_LOCKS=1".into(), "git".into()];
    let mut pending = vec![root.to_owned()];
    while let Some(worktree) = pending.pop() {
        let owned = io(fs::canonicalize(&worktree))?;
        let metadata = worktree.join(".git");
        assert_eq!(owned, worktree);
        assert!(owned.starts_with(root));
        assert!(metadata.is_dir());
        assert_eq!(io(fs::canonicalize(&metadata))?, metadata);
        let safe_directory = format!("safe.directory={}", text(&worktree)?);
        let paths = runner.text(
            "git",
            &[
                "-c",
                &safe_directory,
                "--no-replace-objects",
                "rev-parse",
                "--show-toplevel",
                "--absolute-git-dir",
                "--git-common-dir",
                "--git-path",
                "index",
            ],
            &worktree,
        )?;
        let actual = paths
            .lines()
            .map(|path| io(fs::canonicalize(worktree.join(path))))
            .collect::<Result<Vec<_>>>()?;
        assert_eq!(
            actual,
            [
                worktree.clone(),
                metadata.clone(),
                metadata.clone(),
                metadata.join("index")
            ],
        );
        // Git forwards these exact owned-path allowances to its recursive
        // submodule status children; a root allowance alone is not recursive.
        status_args.extend(["-c".into(), safe_directory.clone()]);
        let entries = runner.text(
            "git",
            &[
                "-c",
                &safe_directory,
                "--no-replace-objects",
                "ls-files",
                "--stage",
                "-z",
            ],
            &worktree,
        )?;
        // Cargo leaves an empty untracked completion marker in each nested
        // checkout. In our private copies only, remove that bookkeeping so
        // the recursive status below can still require no submodule changes.
        // Keep the top-level marker: Cargo uses it to recognize this checkout.
        let marker = worktree.join(".cargo-ok");
        if worktree != root && io(marker.try_exists())? {
            assert!(io(fs::symlink_metadata(&marker))?.file_type().is_file());
            assert!(io(fs::read(&marker))?.is_empty());
            assert!(
                !entries.split('\0').any(|entry| {
                    entry
                        .split_once('\t')
                        .is_some_and(|(_, path)| path == ".cargo-ok")
                }),
                "private Cargo completion marker is tracked",
            );
            io(fs::remove_file(marker))?;
        }
        for entry in entries
            .split('\0')
            .filter(|entry| entry.starts_with("160000 "))
        {
            let (_, relative) = entry.split_once('\t').ok_or("malformed private gitlink")?;
            pending.push(worktree.join(relative));
        }
    }
    status_args.extend(
        [
            "--no-replace-objects",
            "status",
            "--porcelain",
            "--untracked-files=no",
            "--ignore-submodules=none",
        ]
        .map(str::to_owned),
    );
    assert!(
        runner.run("env", &status_args, root)?.is_empty(),
        "private copied checkout changed before its source transactions",
    );
    Ok(())
}

#[test]
fn compiled_source_transaction_closes_both_result_paths() -> Result<()> {
    let original = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("Hermit source root missing")?;
    if let Some(case) = std::env::var_os(CHILD_ROOT) {
        let case = PathBuf::from(case);
        let mut runner = Runner::new(case.join("child-logs"), WALL_BACKSTOP, 8 << 20)?;
        return transaction_checks(&case, &mut runner);
    }
    run_private_copy_test(original, TRANSACTION_TEST_NAME)
}
