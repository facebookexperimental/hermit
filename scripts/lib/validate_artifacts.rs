// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// Immutable validation artifact publication, extracted from Hermit
// 9a0b6b782c955feb542102ba01aee88c2ebad240,
// https://github.com/rrnewton/hermit/pull/2969.

use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ArtifactPublicationFailure {
    ArtifactAncestorSync,
}

fn require_normal_component(value: &str, description: &str) -> Result<(), String> {
    let mut components = Path::new(value).components();
    if !matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(component)), None) if component == value
    ) {
        return Err(format!("{description} is not one normal path component"));
    }
    Ok(())
}

fn require_plain_directory(path: &Path, description: &str) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect {description} {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "{description} {} is not a non-symlink directory",
            path.display()
        ));
    }
    Ok(())
}

fn create_plain_directory_path_below(
    root: &Path,
    relative: &Path,
    description: &str,
    failure: Option<ArtifactPublicationFailure>,
) -> Result<PathBuf, String> {
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!("{description} is not a normal relative path"));
    }
    require_plain_directory(root, "retained validation state root")?;
    let mut current = root.to_owned();
    let mut chain = vec![root.to_owned()];
    let mut created = Vec::new();
    let create_and_sync = (|| -> Result<PathBuf, String> {
        for component in relative.components() {
            let Component::Normal(component) = component else {
                unreachable!("relative path was checked above")
            };
            current.push(component);
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                    return Err(format!(
                        "{description} {} is not a non-symlink directory",
                        current.display()
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir(&current).map_err(|error| {
                        format!("cannot create {description} {}: {error}", current.display())
                    })?;
                    created.push(current.clone());
                }
                Err(error) => {
                    return Err(format!(
                        "cannot inspect {description} {}: {error}",
                        current.display()
                    ));
                }
            }
            chain.push(current.clone());
        }
        if !created.is_empty() {
            for directory in &chain {
                if failure == Some(ArtifactPublicationFailure::ArtifactAncestorSync)
                    && directory == root
                {
                    return Err(
                        "injected failure syncing retained validation artifact ancestor".into(),
                    );
                }
                sync_directory(directory, description)?;
            }
        }
        Ok(current.clone())
    })();
    match create_and_sync {
        Ok(path) => Ok(path),
        Err(error) => match remove_created_directory_chain(&created, description) {
            Ok(()) => Err(error),
            Err(cleanup_error) => Err(format!(
                "{error}; cannot clean newly created directory chain: {cleanup_error}"
            )),
        },
    }
}

fn remove_created_directory_chain(created: &[PathBuf], description: &str) -> Result<(), String> {
    for directory in created.iter().rev() {
        fs::remove_dir(directory).map_err(|error| {
            format!(
                "cannot remove newly created {description} {}: {error}",
                directory.display()
            )
        })?;
        let parent = directory.parent().ok_or_else(|| {
            format!(
                "newly created directory {} has no parent",
                directory.display()
            )
        })?;
        sync_directory(parent, description)?;
    }
    Ok(())
}

fn sync_directory(path: &Path, description: &str) -> Result<(), String> {
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(path)
        .map_err(|error| format!("cannot open {description} {}: {error}", path.display()))?;
    directory
        .sync_all()
        .map_err(|error| format!("cannot sync {description} {}: {error}", path.display()))
}

fn publish_file_noclobber(path: &Path, bytes: &[u8], description: &str) -> Result<(), String> {
    publish_file_noclobber_with_sync(path, bytes, description, sync_directory)
}

// Keep the production sync operation unchanged while allowing deterministic
// coverage of publication and cleanup failures in the module's unit tests.
fn publish_file_noclobber_with_sync(
    path: &Path,
    bytes: &[u8],
    description: &str,
    mut sync_parent: impl FnMut(&Path, &str) -> Result<(), String>,
) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{description} {} has no parent", path.display()))?;
    require_plain_directory(parent, "retained validation artifact directory")?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".artifact.")
        .tempfile_in(parent)
        .map_err(|error| {
            format!(
                "cannot create temporary {description} beside {}: {error}",
                path.display()
            )
        })?;
    temporary
        .write_all(bytes)
        .and_then(|()| temporary.flush())
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|error| format!("cannot write temporary {description}: {error}"))?;
    temporary.persist_noclobber(path).map_err(|error| {
        format!(
            "cannot publish {description} to {} without replacement: {}",
            path.display(),
            error.error
        )
    })?;
    if let Err(error) = sync_parent(parent, "retained validation artifact directory") {
        return match fs::remove_file(path) {
            Ok(()) => match sync_parent(parent, "retained validation artifact directory") {
                Ok(()) => Err(error),
                Err(cleanup_error) => Err(format!(
                    "{error}; cannot sync cleanup of failed {description}: {cleanup_error}"
                )),
            },
            Err(cleanup_error) => Err(format!(
                "{error}; cannot remove failed {description} {}: {cleanup_error}",
                path.display()
            )),
        };
    }
    Ok(())
}

/// Publish one immutable run artifact without replacing existing evidence.
/// Publication syncs the file and its directory; a failed directory sync
/// removes the published file and syncs the cleanup before returning failure.
pub(crate) fn publish_run_artifact_noclobber(
    parent: &Path,
    run_id: &str,
    name: &str,
    bytes: &[u8],
    description: &str,
) -> Result<String, String> {
    require_normal_component(run_id, "retained validation run_id")?;
    require_normal_component(name, "retained validation artifact name")?;
    let relative_directory = PathBuf::from("ignored")
        .join("validate")
        .join("artifacts")
        .join(run_id);
    let artifact_dir = create_plain_directory_path_below(
        parent,
        &relative_directory,
        "retained validation artifact directory",
        None,
    )?;
    let artifact = artifact_dir.join(name);
    if fs::symlink_metadata(&artifact).is_ok() {
        return Err(format!(
            "retained validation artifact already exists: {}",
            artifact.display()
        ));
    }
    publish_file_noclobber(&artifact, bytes, description)?;
    artifact
        .strip_prefix(parent)
        .map_err(|_| "retained validation artifact is outside parent root".to_string())
        .map(|relative| relative.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn artifact_publication_preserves_bytes_and_refuses_replacement() {
        let root = tempfile::tempdir().unwrap();
        let relative = publish_run_artifact_noclobber(
            root.path(),
            "fixture-run",
            "results.jsonl",
            b"original\n",
            "fixture artifact",
        )
        .unwrap();
        assert_eq!(
            relative,
            "ignored/validate/artifacts/fixture-run/results.jsonl"
        );
        let path = root.path().join(relative);
        assert_eq!(fs::read(&path).unwrap(), b"original\n");
        let error = publish_run_artifact_noclobber(
            root.path(),
            "fixture-run",
            "results.jsonl",
            b"replacement\n",
            "fixture artifact",
        )
        .unwrap_err();
        assert!(error.contains("already exists"), "{error}");
        assert_eq!(fs::read(&path).unwrap(), b"original\n");
        let error =
            publish_file_noclobber(&path, b"replacement\n", "fixture artifact").unwrap_err();
        assert!(error.contains("without replacement"), "{error}");
        assert_eq!(fs::read(&path).unwrap(), b"original\n");
    }

    #[test]
    fn artifact_publication_refuses_non_component_run_ids_and_names() {
        let root = tempfile::tempdir().unwrap();
        for invalid in [
            "",
            ".",
            "..",
            "../escape",
            "nested/name",
            "/absolute",
            "name/",
        ] {
            let run_error = publish_run_artifact_noclobber(
                root.path(),
                invalid,
                "results.jsonl",
                b"new",
                "fixture artifact",
            )
            .unwrap_err();
            assert!(run_error.contains("normal path component"), "{run_error}");
            let name_error = publish_run_artifact_noclobber(
                root.path(),
                "fixture-run",
                invalid,
                b"new",
                "fixture artifact",
            )
            .unwrap_err();
            assert!(name_error.contains("normal path component"), "{name_error}");
        }
        assert!(!root.path().join("ignored").exists());
    }

    #[test]
    fn artifact_publication_refuses_symlink_ancestors_and_destinations() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), root.path().join("ignored")).unwrap();
        let error = publish_run_artifact_noclobber(
            root.path(),
            "fixture-run",
            "results.jsonl",
            b"new",
            "fixture artifact",
        )
        .unwrap_err();
        assert!(error.contains("non-symlink directory"), "{error}");
        assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
        fs::remove_file(root.path().join("ignored")).unwrap();

        let artifact_dir = root.path().join("ignored/validate/artifacts/fixture-run");
        fs::create_dir_all(&artifact_dir).unwrap();
        let original = outside.path().join("original");
        fs::write(&original, b"original").unwrap();
        symlink(&original, artifact_dir.join("results.jsonl")).unwrap();
        let error = publish_run_artifact_noclobber(
            root.path(),
            "fixture-run",
            "results.jsonl",
            b"new",
            "fixture artifact",
        )
        .unwrap_err();
        assert!(error.contains("already exists"), "{error}");
        assert_eq!(fs::read(original).unwrap(), b"original");
    }

    #[test]
    fn artifact_ancestor_sync_failure_removes_the_new_directory_chain() {
        let root = tempfile::tempdir().unwrap();
        let error = create_plain_directory_path_below(
            root.path(),
            Path::new("ignored/validate/artifacts/fixture-run"),
            "retained validation artifact directory",
            Some(ArtifactPublicationFailure::ArtifactAncestorSync),
        )
        .unwrap_err();
        assert!(error.contains("artifact ancestor"), "{error}");
        assert!(
            !root.path().join("ignored").exists(),
            "an ancestor sync failure left the newly created artifact directory chain"
        );
    }

    #[test]
    fn artifact_directory_sync_failure_removes_publication_and_reports_cleanup_failure() {
        for cleanup_fails in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let artifact = root.path().join("results.jsonl");
            let mut calls = 0;
            let error = publish_file_noclobber_with_sync(
                &artifact,
                b"new",
                "fixture artifact",
                |parent, description| {
                    calls += 1;
                    if calls == 1 {
                        Err("injected publication directory sync failure".into())
                    } else if cleanup_fails {
                        Err("injected cleanup directory sync failure".into())
                    } else {
                        sync_directory(parent, description)
                    }
                },
            )
            .unwrap_err();
            assert_eq!(
                calls, 2,
                "cleanup must sync the directory after removing the file"
            );
            assert!(!artifact.exists(), "failed publication remained visible");
            assert!(
                error.contains("publication directory sync failure"),
                "{error}"
            );
            if cleanup_fails {
                assert!(error.contains("cannot sync cleanup"), "{error}");
                assert!(error.contains("cleanup directory sync failure"), "{error}");
            } else {
                assert_eq!(error, "injected publication directory sync failure");
            }
        }
    }
}
