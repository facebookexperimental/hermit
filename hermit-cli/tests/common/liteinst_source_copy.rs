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

const CHILD_ROOT: &str = "HERMIT_SOURCE_COPY_TEST_CHILD";
const TEST_NAME: &str = "source_copy::gitless_standalone_compiler_sources";
const TRANSACTION_TEST_NAME: &str =
    "source_copy::compiled_source_transaction_closes_both_result_paths";
const SOURCE_FILES: [&str; 9] = [
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
        // The reviewed catalog binds 53 runtime/artifact inputs, including the
        // separate Bootstrap descriptor, role, env/aux and site leaves.
        // The separately bound qualifier source adds one input.
        54
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
    run_private_copy_test(original, TEST_NAME)
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
    let mut runner = Runner::new(case.join("parent-logs"), Duration::from_secs(120), 8 << 20)?;
    let original_context = resolve(original, &mut runner)?;
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
    if let Err(error) = runner.run("env", &child, &copied) {
        let retained = directory.keep();
        return Err(format!("{error}; retained fixture {}", retained.display()));
    }
    original_context.verify_unchanged(&mut runner)?;
    Ok(())
}

fn transaction_checks(case: &Path, runner: &mut Runner) -> Result<()> {
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
            .commands
            .iter()
            .any(|command| command.program == "git")
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
        let mut runner = Runner::new(case.join("child-logs"), Duration::from_secs(90), 8 << 20)?;
        return transaction_checks(&case, &mut runner);
    }
    run_private_copy_test(original, TRANSACTION_TEST_NAME)
}
