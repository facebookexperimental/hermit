/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

#[path = "../cmake_source.rs"]
mod cmake_source;

fn scratch() -> PathBuf {
    let executable = std::env::current_exe()
        .expect("locate integration test")
        .canonicalize()
        .expect("resolve integration test");
    let deps = executable.parent().expect("test executable has no parent");
    assert_eq!(deps.file_name(), Some(OsStr::new("deps")));
    let profile = deps
        .parent()
        .expect("test executable has no Cargo profile directory");
    let root = profile.join(format!(
        "cmake-source-identity-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).expect("reserve a fresh target fixture");
    eprintln!("CMake fixture retained at {}", root.display());
    root
}

fn configure(root: &Path, label: &str, source: &Path, build: &Path, success: bool) -> PathBuf {
    let stderr = root.join(format!("{label}.stderr"));
    let status = Command::new("cmake")
        .arg("-S")
        .arg(source)
        .arg("-B")
        .arg(build)
        .env("LC_ALL", "C")
        .stdout(fs::File::create_new(root.join(format!("{label}.stdout"))).unwrap())
        .stderr(fs::File::create_new(&stderr).unwrap())
        .status()
        .expect("run CMake configuration control");
    fs::write(root.join(format!("{label}.status")), status.to_string()).unwrap();
    assert_eq!(
        status.success(),
        success,
        "CMake {label}: {status}; retained diagnostics at {}",
        stderr.display()
    );
    if !success {
        assert_eq!(status.code(), Some(1), "CMake mismatch must exit normally");
    }
    stderr
}

fn assert_home(build: &Path, source: &Path) {
    let cache = fs::read_to_string(build.join("CMakeCache.txt")).unwrap();
    let homes: Vec<_> = cache
        .lines()
        .filter_map(|line| line.strip_prefix("CMAKE_HOME_DIRECTORY:INTERNAL="))
        .collect();
    assert_eq!(homes.len(), 1, "missing or duplicate CMake source identity");
    assert_eq!(Path::new(homes[0]), source);
}

#[test]
fn different_and_nested_sources_keep_disjoint_cmake_caches() {
    let root = scratch();
    let first = root.join("sources with spaces/first");
    let sources = [
        first.clone(),
        root.join("sources with spaces/other"),
        first.join("CMakeFiles"),
        first.join("CMakeFiles/cmake.check_cache"),
    ];
    for source in &sources {
        fs::create_dir_all(source).unwrap();
        fs::write(
            source.join("CMakeLists.txt"),
            "cmake_minimum_required(VERSION 3.14)\nproject(source_identity NONE)\n",
        )
        .unwrap();
    }

    // The original shared cache must reject the second source, not quietly reuse it.
    let old_cache = root.join("old single cache");
    configure(&root, "old-first", &sources[0], &old_cache, true);
    let error = configure(&root, "old-other", &sources[1], &old_cache, false);
    assert!(
        fs::read_to_string(error)
            .unwrap()
            .contains("does not match the source")
    );

    let build_root = root.join("builds");
    let paths: Vec<_> = sources
        .iter()
        .map(|source| cmake_source::source_and_build_dir(source, &build_root).unwrap())
        .collect();
    for (index, (_, build)) in paths.iter().enumerate() {
        assert!(build.starts_with(&build_root));
        for (_, other) in &paths[..index] {
            assert!(!build.starts_with(other) && !other.starts_with(build));
        }
    }
    for round in 0..2 {
        for (index, (source, build)) in paths.iter().enumerate() {
            configure(
                &root,
                &format!("round-{round}-{index}"),
                source,
                build,
                true,
            );
            assert_home(build, source);
        }
    }

    let alias = root.join("source alias");
    symlink(&sources[0], &alias).unwrap();
    let alias_paths = cmake_source::source_and_build_dir(&alias, &build_root).unwrap();
    assert_eq!(alias_paths, paths[0]);
    configure(&root, "alias", &alias_paths.0, &alias_paths.1, true);
    assert_home(&alias_paths.1, &paths[0].0);
}
