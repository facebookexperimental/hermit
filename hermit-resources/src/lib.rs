/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Discovery of resources shipped beside the Hermit executable.

use std::env;
use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::path::PathBuf;

/// Environment variable selecting a Hermit installation directory.
// TODO-HUMAN-REVIEW(PR-1002): Review the unified installation-directory contract.
pub const INSTALL_DIR_ENV: &str = "HERMIT_INSTALL_DIR";

fn invoked_executable(argv0: Option<&OsStr>, current_dir: &Path) -> Option<PathBuf> {
    let argv0 = Path::new(argv0?);
    if !argv0.is_absolute() && argv0.components().count() <= 1 {
        return None;
    }
    Some(if argv0.is_absolute() {
        argv0.to_path_buf()
    } else {
        current_dir.join(argv0)
    })
}

fn has_resources(directory: &Path) -> bool {
    directory.join("rsrcs").is_dir()
}

fn discover_install_dir_from(
    explicit: Option<&OsStr>,
    argv0: Option<&OsStr>,
    executable: &Path,
    current_dir: &Path,
) -> io::Result<Option<PathBuf>> {
    if let Some(explicit) = explicit {
        if explicit.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{INSTALL_DIR_ENV} is empty"),
            ));
        }
        return Ok(Some(PathBuf::from(explicit)));
    }

    let invoked = invoked_executable(argv0, current_dir);
    let invoked_directory = invoked.as_deref().and_then(Path::parent);
    let executable_directory = executable.parent();
    let built_in_place = executable_directory
        .and_then(Path::parent)
        .map(|target| target.join("install_pkg"));

    Ok(invoked_directory
        .filter(|directory| has_resources(directory))
        .map(Path::to_path_buf)
        .or_else(|| {
            executable_directory
                .filter(|directory| has_resources(directory))
                .map(Path::to_path_buf)
        })
        .or_else(|| built_in_place.filter(|directory| has_resources(directory))))
}

/// Returns the selected installation directory, if a packaged installation is available.
// TODO-HUMAN-REVIEW(PR-1002): Review executable-relative resource discovery.
pub fn install_dir() -> io::Result<Option<PathBuf>> {
    let executable = env::current_exe()?;
    let current_dir = env::current_dir()?;
    discover_install_dir_from(
        env::var_os(INSTALL_DIR_ENV).as_deref(),
        env::args_os().next().as_deref(),
        &executable,
        &current_dir,
    )
}

/// Returns a path below the selected installation's `rsrcs` directory.
// TODO-HUMAN-REVIEW(PR-1002): Review the shared backend-resource layout.
pub fn resource(relative: impl AsRef<Path>) -> io::Result<Option<PathBuf>> {
    Ok(install_dir()?.map(|directory| directory.join("rsrcs").join(relative)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_directory(name: &str) -> PathBuf {
        let directory =
            env::temp_dir().join(format!("hermit-resources-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn explicit_install_directory_has_priority() {
        let root = test_directory("explicit");
        let discovered = discover_install_dir_from(
            Some(root.as_os_str()),
            None,
            Path::new("/tmp/target/release/hermit"),
            Path::new("/tmp"),
        )
        .unwrap();
        assert_eq!(discovered.as_deref(), Some(root.as_path()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invoked_symlink_location_finds_colocated_resources() {
        let root = test_directory("argv0");
        std::fs::create_dir(root.join("rsrcs")).unwrap();
        let invoked = root.join("hermit");
        let discovered = discover_install_dir_from(
            None,
            Some(invoked.as_os_str()),
            Path::new("/tmp/target/release/hermit"),
            Path::new("/tmp"),
        )
        .unwrap();
        assert_eq!(discovered.as_deref(), Some(root.as_path()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn release_binary_finds_target_install_package() {
        let root = test_directory("build-tree");
        let install = root.join("install_pkg");
        std::fs::create_dir_all(install.join("rsrcs")).unwrap();
        let executable = root.join("release/hermit");
        let discovered = discover_install_dir_from(None, None, &executable, &root).unwrap();
        assert_eq!(discovered.as_deref(), Some(install.as_path()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn empty_explicit_directory_is_rejected() {
        let error = discover_install_dir_from(
            Some(OsStr::new("")),
            None,
            Path::new("/tmp/hermit"),
            Path::new("/tmp"),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
