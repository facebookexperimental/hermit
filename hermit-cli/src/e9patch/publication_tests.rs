/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Exercise the production publication boundary without e9tool or a guest.
// Equal content is insufficient: replacing the named inode unlinks an image
// that a running loader may still need to reopen through /proc/self/exe.
use std::sync::Arc;
use std::sync::Barrier;

use super::*;

const EXECUTABLE_BYTES: &[u8] = b"immutable rewritten executable fixture\n";
const EXECUTABLE_MODE: u32 = 0o755;
const PUBLISHERS: usize = 8;

fn candidate(parent: &Path, bytes: &[u8], mode: u32) -> (tempfile::TempDir, PathBuf) {
    let temporary = tempfile::Builder::new()
        .prefix(".publication-candidate-")
        .tempdir_in(parent)
        .unwrap();
    let source = temporary.path().join("guest");
    fs::write(&source, bytes).unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(mode)).unwrap();
    (temporary, source)
}

fn identity(metadata: &fs::Metadata) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
}

fn assert_live_named_inode(file: &File, destination: &Path, expected: (u64, u64)) {
    let opened = file.metadata().unwrap();
    assert_eq!(identity(&opened), expected, "the held descriptor changed");
    assert!(opened.nlink() > 0, "publication unlinked the opened inode");
    assert_eq!(
        identity(&fs::symlink_metadata(destination).unwrap()),
        expected,
        "publication replaced the named inode"
    );
    let descriptor = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
    assert_eq!(
        fs::read_link(&descriptor).unwrap(),
        destination,
        "the opened executable must retain its live name, without a deleted suffix"
    );
}

#[test]
fn repeated_publication_preserves_open_executable_inode() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = fs::canonicalize(temporary.path()).unwrap();
    let destination = directory.join("artifact.e9patch");
    let digest = Digest::new(EXECUTABLE_BYTES);
    let (first_temporary, first_source) = candidate(&directory, EXECUTABLE_BYTES, EXECUTABLE_MODE);
    publish_executable(&first_source, &destination, digest, EXECUTABLE_MODE).unwrap();
    // Match the real rewrite producer: its staging directory is retired before
    // callers execute the returned artifact. No staging hard link may save it.
    drop(first_temporary);
    let opened = File::open(&destination).unwrap();
    let first = identity(&opened.metadata().unwrap());
    assert_live_named_inode(&opened, &destination, first);

    for _ in 0..PUBLISHERS {
        let (staging, source) = candidate(&directory, EXECUTABLE_BYTES, EXECUTABLE_MODE);
        publish_executable(&source, &destination, digest, EXECUTABLE_MODE).unwrap();
        drop(staging);
        assert_live_named_inode(&opened, &destination, first);
        assert_eq!(fs::read(&destination).unwrap(), EXECUTABLE_BYTES);
        assert_eq!(
            fs::metadata(&destination).unwrap().mode() & 0o7777,
            EXECUTABLE_MODE
        );
    }
}

#[test]
fn concurrent_publication_preserves_one_executable_inode() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = fs::canonicalize(temporary.path()).unwrap();
    let destination = directory.join("artifact.e9patch");
    let digest = Digest::new(EXECUTABLE_BYTES);
    let start = Arc::new(Barrier::new(PUBLISHERS + 1));
    // All candidates exist before the common start; this includes the cold
    // create-if-absent race rather than eight warm prepare/cache-hit calls.
    let candidates: Vec<_> = (0..PUBLISHERS)
        .map(|_| candidate(&directory, EXECUTABLE_BYTES, EXECUTABLE_MODE))
        .collect();
    let handles: Vec<_> = candidates
        .into_iter()
        .map(|(staging, source)| {
            let start = start.clone();
            let destination = destination.clone();
            std::thread::spawn(move || {
                start.wait();
                publish_executable(&source, &destination, digest, EXECUTABLE_MODE).unwrap();
                let opened = File::open(&destination).unwrap();
                drop(staging);
                // Return the open descriptor, so later publishers cannot hide
                // an unlinked inode by making every observer reopen the winner.
                opened
            })
        })
        .collect();
    start.wait();
    let opened: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().expect("publisher thread panicked"))
        .collect();
    assert_eq!(opened.len(), PUBLISHERS);
    let winner = identity(&fs::symlink_metadata(&destination).unwrap());
    for file in &opened {
        assert_live_named_inode(file, &destination, winner);
    }
    assert_eq!(fs::read(&destination).unwrap(), EXECUTABLE_BYTES);
    assert_eq!(
        fs::metadata(&destination).unwrap().mode() & 0o7777,
        EXECUTABLE_MODE
    );
}

#[test]
fn publication_refuses_existing_wrong_digest_without_mutation() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = fs::canonicalize(temporary.path()).unwrap();
    let destination = directory.join("artifact.e9patch");
    let existing_bytes = b"conflicting executable contents\n";
    fs::write(&destination, existing_bytes).unwrap();
    fs::set_permissions(&destination, fs::Permissions::from_mode(EXECUTABLE_MODE)).unwrap();
    let opened = File::open(&destination).unwrap();
    let original = identity(&opened.metadata().unwrap());
    let (_staging, source) = candidate(&directory, EXECUTABLE_BYTES, EXECUTABLE_MODE);

    let result = publish_executable(
        &source,
        &destination,
        Digest::new(EXECUTABLE_BYTES),
        EXECUTABLE_MODE,
    );
    assert!(
        result.is_err(),
        "conflicting digest was admitted: {result:?}"
    );
    assert_live_named_inode(&opened, &destination, original);
    assert_eq!(fs::read(&destination).unwrap(), existing_bytes);
    assert_eq!(
        fs::metadata(&destination).unwrap().mode() & 0o7777,
        EXECUTABLE_MODE
    );
    assert_eq!(fs::read(&source).unwrap(), EXECUTABLE_BYTES);
}

#[test]
fn publication_refuses_existing_wrong_mode_without_mutation() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = fs::canonicalize(temporary.path()).unwrap();
    let destination = directory.join("artifact.e9patch");
    let existing_mode = 0o700;
    fs::write(&destination, EXECUTABLE_BYTES).unwrap();
    fs::set_permissions(&destination, fs::Permissions::from_mode(existing_mode)).unwrap();
    let opened = File::open(&destination).unwrap();
    let original = identity(&opened.metadata().unwrap());
    let (_staging, source) = candidate(&directory, EXECUTABLE_BYTES, EXECUTABLE_MODE);

    let result = publish_executable(
        &source,
        &destination,
        Digest::new(EXECUTABLE_BYTES),
        EXECUTABLE_MODE,
    );
    assert!(result.is_err(), "conflicting mode was admitted: {result:?}");
    assert_live_named_inode(&opened, &destination, original);
    assert_eq!(fs::read(&destination).unwrap(), EXECUTABLE_BYTES);
    assert_eq!(
        fs::metadata(&destination).unwrap().mode() & 0o7777,
        existing_mode,
        "refusal must not repair the preexisting inode in place"
    );
    assert_eq!(fs::read(&source).unwrap(), EXECUTABLE_BYTES);
}

#[test]
fn publication_refuses_existing_symlink_without_mutation() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = fs::canonicalize(temporary.path()).unwrap();
    let destination = directory.join("artifact.e9patch");
    let target = directory.join("symlink-target");
    fs::write(&target, EXECUTABLE_BYTES).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(EXECUTABLE_MODE)).unwrap();
    std::os::unix::fs::symlink(&target, &destination).unwrap();
    let link_before = fs::symlink_metadata(&destination).unwrap();
    let target_before = fs::metadata(&target).unwrap();
    let (_staging, source) = candidate(&directory, EXECUTABLE_BYTES, EXECUTABLE_MODE);

    let result = publish_executable(
        &source,
        &destination,
        Digest::new(EXECUTABLE_BYTES),
        EXECUTABLE_MODE,
    );
    assert!(
        result.is_err(),
        "symlink destination was admitted: {result:?}"
    );
    let link_after = fs::symlink_metadata(&destination).unwrap();
    assert!(link_after.file_type().is_symlink());
    assert_eq!(identity(&link_after), identity(&link_before));
    assert_eq!(fs::read_link(&destination).unwrap(), target);
    assert_eq!(
        identity(&fs::metadata(&target).unwrap()),
        identity(&target_before)
    );
    assert_eq!(
        fs::metadata(&target).unwrap().mode() & 0o7777,
        EXECUTABLE_MODE
    );
    assert_eq!(fs::read(&target).unwrap(), EXECUTABLE_BYTES);
    assert_eq!(fs::read(&source).unwrap(), EXECUTABLE_BYTES);
}
