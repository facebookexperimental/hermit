/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs::File;
use std::os::fd::OwnedFd;

use super::*;

fn input_at_owned_slot() -> (OwnedFd, File, reverie::InheritedInputIdentity) {
    // Reserve a real destination without replacing the test harness's stdin.
    let destination: OwnedFd = File::open("/dev/null").unwrap().into();
    let input = reverie::InheritedInput::pipe(destination.as_raw_fd()).unwrap();
    let identity = input.identity().clone();
    let (reader, writer) = input.into_reader_and_writer();
    assert_ne!(reader.as_raw_fd(), destination.as_raw_fd());
    // SAFETY: both descriptors are owned here; dup2 atomically replaces only
    // this test's reserved destination, just as the launcher's stdio mapping does.
    assert_eq!(
        unsafe { libc::dup2(reader.as_raw_fd(), destination.as_raw_fd()) },
        destination.as_raw_fd(),
    );
    (destination, writer, identity)
}

fn flags(fd: RawFd) -> (i32, i32) {
    // SAFETY: these no-argument fcntl queries inspect a descriptor owned by the test.
    let result = unsafe {
        (
            libc::fcntl(fd, libc::F_GETFL),
            libc::fcntl(fd, libc::F_GETFD),
        )
    };
    assert!(result.0 >= 0 && result.1 >= 0);
    result
}

fn install(
    metadata: &mut FileMetadata,
    owner: DetTid,
    identity: reverie::InheritedInputIdentity,
) -> Result<(), Errno> {
    metadata.apply_exec_fd_state(
        owner,
        ExecFdState {
            initial_input: Some(identity),
            ..Default::default()
        },
        true,
    )
}

#[test]
fn authentic_input_seed_preserves_pipe_type_and_blocking_flags() {
    let owner = DetTid::from_raw(9);
    let (reader, _writer, identity) = input_at_owned_slot();
    let fd = reader.as_raw_fd();
    let before = flags(fd);
    assert_eq!(
        before.0 & (libc::O_ACCMODE | libc::O_NONBLOCK),
        libc::O_RDONLY
    );
    let mut metadata = FileMetadata::new(owner);
    metadata
        .discover_fd_from_current_process(owner, fd)
        .unwrap();
    assert!(
        metadata.file_handles[&fd]
            .inherited_external_read()
            .is_none()
    );
    metadata.file_handles[&fd].set_resource(ResourceID::Device(Device::ContainerStdin));

    install(&mut metadata, owner, identity.clone()).unwrap();
    let detfd = &metadata.file_handles[&fd];
    let origin = detfd.inherited_external_read().unwrap();
    assert_eq!(detfd.ty(), FdType::Pipe);
    assert!(!detfd.is_nonblocking());
    assert!(!detfd.physically_nonblocking());
    assert!(!detfd.is_socketpair_endpoint());
    assert_eq!(origin.file_type, libc::S_IFIFO);
    assert_eq!(origin.access_mode, libc::O_RDONLY);
    assert_eq!(
        (origin.dev, origin.inode, origin.rdev),
        (identity.dev(), identity.inode(), identity.rdev())
    );
    assert_eq!(origin.origin, detfd.open_file_id());
    assert_eq!(
        origin.resource,
        Some(ResourceID::Device(Device::ContainerStdin))
    );
    assert_eq!(flags(fd), before);

    let mut separate_table = FileMetadata::new(owner);
    separate_table
        .apply_exec_fd_state(
            owner,
            ExecFdState {
                initial_input: Some(identity),
                ..Default::default()
            },
            false,
        )
        .unwrap();
    assert!(separate_table.inherited_reads_for_exec().is_empty());
}

#[test]
fn input_aliases_keep_origin_through_modeled_fork_and_exec() {
    let owner = DetTid::from_raw(9);
    let child_owner = DetTid::from_raw(10);
    let (reader, _writer, identity) = input_at_owned_slot();
    let fd = reader.as_raw_fd();
    let alias = reader.try_clone().unwrap();
    let cloexec = reader.try_clone().unwrap();
    // SAFETY: change only this owned alias's descriptor flag, not OFD status flags.
    assert_eq!(
        unsafe { libc::fcntl(alias.as_raw_fd(), libc::F_SETFD, 0) },
        0
    );
    let before = [
        flags(fd),
        flags(alias.as_raw_fd()),
        flags(cloexec.as_raw_fd()),
    ];
    let mut metadata = FileMetadata::new(owner);
    install(&mut metadata, owner, identity).unwrap();
    metadata
        .dup_fd(fd, alias.as_raw_fd(), OFlag::empty())
        .unwrap();
    metadata
        .dup_fd(fd, cloexec.as_raw_fd(), OFlag::O_CLOEXEC)
        .unwrap();
    let original = metadata.file_handles[&fd]
        .inherited_external_read()
        .unwrap();
    for alias in [alias.as_raw_fd(), cloexec.as_raw_fd()] {
        assert_eq!(
            metadata.file_handles[&alias].inherited_external_read(),
            Some(original.clone())
        );
    }

    let child = metadata.fork_for(child_owner);
    let child: FileMetadata = serde_json::from_slice(&serde_json::to_vec(&child).unwrap()).unwrap();
    let state = ExecFdState {
        inherited_reads: child.inherited_reads_for_exec(),
        ..Default::default()
    };
    assert_eq!(state.inherited_reads.len(), 2);
    assert!(!state.inherited_reads.contains_key(&cloexec.as_raw_fd()));
    // Preparing an exec (including one that will fail) does not discard the
    // current image's aliases or mutate the real descriptor flags.
    assert!(
        child.file_handles[&cloexec.as_raw_fd()]
            .inherited_external_read()
            .is_some()
    );
    assert_eq!(
        [
            flags(fd),
            flags(alias.as_raw_fd()),
            flags(cloexec.as_raw_fd())
        ],
        before
    );

    let mut restored = FileMetadata::new(child_owner);
    restored
        .apply_exec_fd_state(
            child_owner,
            serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap(),
            true,
        )
        .unwrap();
    assert_eq!(restored.file_handles.len(), 2);
    assert_eq!(
        restored.file_handles[&fd].open_file_id(),
        restored.file_handles[&alias.as_raw_fd()].open_file_id()
    );
    assert_eq!(restored.file_handles[&fd].open_file_alias_count(), 2);
    for slot in [fd, alias.as_raw_fd()] {
        assert_eq!(
            restored.file_handles[&slot].inherited_external_read(),
            Some(original.clone())
        );
        assert_eq!(restored.file_handles[&slot].ty(), FdType::Pipe);
        assert!(!restored.file_handles[&slot].physically_nonblocking());
    }
    assert_eq!(
        [
            flags(fd),
            flags(alias.as_raw_fd()),
            flags(cloexec.as_raw_fd())
        ],
        before
    );
}

#[test]
fn an_unseeded_pipe_and_its_stdio_alias_never_gain_input_origin() {
    let owner = DetTid::from_raw(9);
    let (reader, _writer) = nix::unistd::pipe().unwrap();
    let fd = reader.as_raw_fd();
    let mut metadata = FileMetadata::new(owner);
    metadata
        .discover_fd_from_current_process(owner, fd)
        .unwrap();
    assert_eq!(metadata.file_handles[&fd].ty(), FdType::Pipe);
    metadata.dup_fd(fd, 0, OFlag::empty()).unwrap();
    assert!(
        metadata.file_handles[&0]
            .inherited_external_read()
            .is_none()
    );
    let state = ExecFdState {
        inherited_reads: metadata.inherited_reads_for_exec(),
        ..Default::default()
    };
    assert!(state.inherited_reads.is_empty());
    let mut restored = FileMetadata::new(owner);
    restored.apply_exec_fd_state(owner, state, true).unwrap();
    restored
        .discover_fd_from_current_process(owner, fd)
        .unwrap();
    assert!(
        restored.file_handles[&fd]
            .inherited_external_read()
            .is_none()
    );
}

#[test]
fn both_initial_seed_and_exec_carry_refuse_a_reused_pipe_slot() {
    let owner = DetTid::from_raw(9);
    let (reader, _writer, identity) = input_at_owned_slot();
    let fd = reader.as_raw_fd();
    let mut metadata = FileMetadata::new(owner);
    install(&mut metadata, owner, identity.clone()).unwrap();
    let state = ExecFdState {
        inherited_reads: metadata.inherited_reads_for_exec(),
        ..Default::default()
    };
    metadata.remove_fd(fd);
    let (replacement, _replacement_writer) = nix::unistd::pipe().unwrap();
    // SAFETY: replace only this test's still-owned descriptor slot.
    assert_eq!(unsafe { libc::dup2(replacement.as_raw_fd(), fd) }, fd);

    let mut initial = FileMetadata::new(owner);
    assert_eq!(install(&mut initial, owner, identity), Err(Errno::EIO));
    assert!(
        initial.file_handles[&fd]
            .inherited_external_read()
            .is_none()
    );
    let mut successor = FileMetadata::new(owner);
    assert_eq!(
        successor.apply_exec_fd_state(owner, state, true),
        Err(Errno::EIO)
    );
    assert!(
        successor.file_handles[&fd]
            .inherited_external_read()
            .is_none()
    );
}

#[test]
fn a_writer_with_the_same_pipe_identity_cannot_be_seeded_as_input() {
    let owner = DetTid::from_raw(9);
    let (reader, writer, identity) = input_at_owned_slot();
    let fd = reader.as_raw_fd();
    // The inode is still the authentic pipe's inode; its wrong access direction
    // must independently prevent an external-read grant.
    assert_eq!(unsafe { libc::dup2(writer.as_raw_fd(), fd) }, fd);
    let mut metadata = FileMetadata::new(owner);
    assert_eq!(install(&mut metadata, owner, identity), Err(Errno::EIO));
    assert_eq!(metadata.file_handles[&fd].ty(), FdType::Pipe);
    assert!(
        metadata.file_handles[&fd]
            .inherited_external_read()
            .is_none()
    );
}
