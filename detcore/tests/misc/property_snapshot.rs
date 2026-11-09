/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The guest reads Hermit's frozen copy of a property area
//! (`Config::property_snapshot`): a pure read of a file the snapshot copied,
//! by any name, opens the copy; a write-only open runs on the live file, as
//! natively, and never touches the copy; an open that also reads the file
//! it writes, creates or truncates, and a file in the area the snapshot does
//! not hold, are read live only where the run allows it.

use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;

/// A live area with one file, a copy of it with other contents, and the
/// snapshot configuration that names them.
struct Area {
    live: tempfile::TempDir,
    _copies: tempfile::TempDir,
    snapshot: detcore_model::config::PropertySnapshot,
}

fn area() -> Area {
    let live = tempfile::tempdir().unwrap();
    let copies = tempfile::tempdir().unwrap();
    let name = "u:object_r:test_prop:s0";
    std::fs::write(live.path().join(name), [1, 2, 3, 4]).unwrap();
    std::fs::write(copies.path().join(name), [5, 6, 7, 8]).unwrap();
    let metadata = std::fs::metadata(live.path().join(name)).unwrap();
    let snapshot = detcore_model::config::PropertySnapshot {
        guest_dir: live.path().to_owned(),
        copy_dir: copies.path().to_owned(),
        live_files: vec![detcore_model::config::PropertyFile {
            dev: metadata.dev(),
            ino: metadata.ino(),
            name: name.to_owned(),
        }],
        uncopied_files: Vec::new(),
        consistent_cut: true,
    };
    Area {
        live,
        _copies: copies,
        snapshot,
    }
}

/// A read by its own name or through a symbolic link opens the copy; a file
/// the area gained after the snapshot, and a readable truncating open of
/// a copied file, are refused with `EOPNOTSUPP` in a run that is not
/// fail-closed (the named refusal/errno policy), without any live read or
/// truncation; later pure reads still get the copy. A catalogued file the
/// snapshot could not copy allows a nonreading `O_PATH` open, as
/// natively; its `read` fails with `EBADF` and a readable reopen of it
/// is refused.
#[test]
fn a_property_snapshot_serves_every_read_of_the_files_it_copied() {
    let area = area();
    let file = area.live.path().join("u:object_r:test_prop:s0");
    let link = area.live.path().join("..").join(format!(
        "{}-link",
        area.live.path().file_name().unwrap().to_string_lossy()
    ));
    std::os::unix::fs::symlink(&file, &link).unwrap();
    // A file the area gained after its snapshot was taken.
    let later = area.live.path().join("u:object_r:later_prop:s0");
    std::fs::write(&later, [9]).unwrap();
    // A catalogued file the snapshot could not copy.
    let uncopied = area.live.path().join("u:object_r:uncopied_prop:s0");
    std::fs::write(&uncopied, [13, 14, 15, 16]).unwrap();
    let mut snapshot = area.snapshot.clone();
    let uncopied_metadata = std::fs::metadata(&uncopied).unwrap();
    snapshot
        .uncopied_files
        .push(detcore_model::config::PropertyFile {
            dev: uncopied_metadata.dev(),
            ino: uncopied_metadata.ino(),
            name: "u:object_r:uncopied_prop:s0".to_owned(),
        });
    let config = detcore::Config {
        max_timeslice: None,
        sequentialize_threads: true,
        property_snapshot: Some(snapshot),
        ..Default::default()
    };
    let paths = (file.clone(), link.clone(), later.clone(), uncopied.clone());
    let guest = move || {
        let (file, link, later, uncopied): &(PathBuf, PathBuf, PathBuf, PathBuf) = &paths;
        assert_eq!(std::fs::read(file).unwrap(), [5, 6, 7, 8]);
        assert_eq!(std::fs::read(link).unwrap(), [5, 6, 7, 8]);
        // No live bytes under the opt-in: the named errno, not `[9]`.
        let refused = std::fs::read(later).unwrap_err();
        assert_eq!(refused.raw_os_error(), Some(libc::EOPNOTSUPP), "{refused}");
        let truncated = std::ffi::CString::new(file.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        let fd = unsafe { libc::open(truncated.as_ptr(), libc::O_RDONLY | libc::O_TRUNC) };
        assert_eq!(fd, -1, "a readable truncating open must be refused");
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EOPNOTSUPP)
        );
        assert_eq!(std::fs::read(file).unwrap(), [5, 6, 7, 8]);
        // F5 boundary: the uncopied file allows a nonreading `O_PATH`
        // open; reading it is `EBADF`, and a readable reopen through
        // its descriptor link is refused as its own open.
        {
            let c = std::ffi::CString::new(uncopied.as_os_str().as_encoded_bytes()).unwrap();
            // SAFETY: a valid NUL-terminated path.
            let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
            assert!(
                fd >= 0,
                "uncopied O_PATH: {}",
                std::io::Error::last_os_error()
            );
            let mut byte = [0_u8; 1];
            // SAFETY: `fd` is open and `byte` is writable for its length.
            let read = unsafe { libc::read(fd, byte.as_mut_ptr().cast(), byte.len()) };
            assert_eq!(read, -1, "an O_PATH descriptor reads nothing");
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EBADF)
            );
            let reopen = std::ffi::CString::new(format!("/proc/self/fd/{fd}")).unwrap();
            // SAFETY: a valid NUL-terminated descriptor-link path.
            let reopened = unsafe { libc::open(reopen.as_ptr(), libc::O_RDONLY) };
            assert_eq!(reopened, -1, "a readable reopen must be refused");
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EOPNOTSUPP)
            );
            // SAFETY: the descriptor opened above.
            unsafe { libc::close(fd) };
        }
    };
    detcore_testutils::det_test_fn_with_config(
        true,
        guest,
        config,
        detcore_testutils::expect_success,
    );
    let _ = std::fs::remove_file(&link);
    // No live execution happened: neither file was truncated.
    assert_eq!(std::fs::metadata(Path::new(&file)).unwrap().len(), 4);
    assert_eq!(std::fs::read(&later).unwrap(), [9]);
    assert_eq!(std::fs::read(&uncopied).unwrap(), [13, 14, 15, 16]);
}

/// A file the area gained after its snapshot is refused through every
/// alternate spelling (`..`, a parent symlink, a dirfd), while a copied
/// file through the same spellings still serves its copy (the positive
/// control). In a non-fail-closed run the refusal is `EOPNOTSUPP`.
/// Readable mutations of that late file are refused before they run, so
/// the host file keeps its bytes and no absent file is created; a
/// nonreading `O_PATH` open of it still succeeds, as natively, and its
/// `read` fails with `EBADF`. Creation is likewise refused before it
/// runs where only the kernel-resolved parent is the area: through a
/// dangling final symlink into the area, and through a retained dirfd
/// on the area whose spelling has since been replaced. The parent is
/// an intermediate component of the pending open, so the kernel
/// follows it even where it is a symlink: creation through a
/// symlinked final parent (a dangling link whose target names the
/// area through an alias, and `alias/name` under a dirfd), through a
/// chain of dangling final links, and through a dangling link whose
/// spelling is too long to resolve in the guest (refused fail-closed)
/// are likewise refused before anything is created, while chained
/// and plain creations outside the area still succeed. A dangling
/// chain of exactly the kernel's 40 nested links is likewise
/// refused (Linux creates through 40 traversals), while a 41-link
/// chain reaches the real open and fails there with `ELOOP`, as
/// natively, creating nothing. An overlong guest-only descriptor
/// parent is refused under every spelling that resolves to it in
/// the guest (`.` and `..` components and repeated slashes, by
/// `self`, `thread-self` and `/dev/fd`, through an intermediate
/// symlink alias to the guest descriptor, and including a spelling
/// whose resolution on the host succeeds but names the tracer's
/// own, different directory for the same descriptor number), and
/// a parent that cannot be proven outside the area in the guest
/// (a missing parent, or any overlong spelled parent, including
/// one naming a real outside directory) fails closed with the
/// named refusal rather than being treated as outside on the
/// strength of any host-side resolution; outside creations the
/// guest probe can prove (plain, chained, and short descriptor
/// spellings) still succeed.
#[test]
fn a_late_property_file_is_refused_through_alias_spellings() {
    let area = area();
    let name = "u:object_r:test_prop:s0";
    let file = area.live.path().join(name);
    let later_name = "u:object_r:later_prop:s0";
    let later = area.live.path().join(later_name);
    std::fs::write(&later, [9, 10, 11, 12]).unwrap();
    let absent_name = "u:object_r:absent_prop:s0";
    let absent = area.live.path().join(absent_name);
    // A dangling final symlink, outside the area, naming an absent
    // file inside it: following it with `O_CREAT` creates in the area.
    let dangling_name = "u:object_r:dangling_prop:s0";
    let dangling_target = area.live.path().join(dangling_name);
    // A name only a retained dirfd on the area can create (below).
    let retained_name = "u:object_r:retained_prop:s0";
    let retained_target = area.live.path().join(retained_name);
    // A directory symlink to the area, made outside it.
    let outside = tempfile::tempdir_in(area.live.path().parent().unwrap()).unwrap();
    let alias_dir = outside.path().join("alias-area");
    std::os::unix::fs::symlink(area.live.path(), &alias_dir).unwrap();
    // An intermediate symlink alias to a guest-only descriptor: in
    // the guest it names the area through the guest's fd 900, while
    // the tracer has no fd 900 at all.
    std::os::unix::fs::symlink("/proc/self/fd/900", outside.path().join("fd-alias")).unwrap();
    let dangling_link = outside.path().join("dangling-final");
    std::os::unix::fs::symlink(&dangling_target, &dangling_link).unwrap();
    // A dangling final symlink whose target names the area only
    // through the alias: only the link target's parent, followed as
    // the pending open follows an intermediate component, is the area.
    let alias_absent_name = "u:object_r:alias_absent_prop:s0";
    let alias_absent_target = area.live.path().join(alias_absent_name);
    let alias_dangling_link = outside.path().join("alias-dangling-final");
    std::os::unix::fs::symlink(alias_dir.join(alias_absent_name), &alias_dangling_link).unwrap();
    // A name only `alias-area/<name>` under a dirfd on the outside
    // directory can create, where the alias is the final parent.
    let dirfd_absent_name = "u:object_r:dirfd_absent_prop:s0";
    let dirfd_absent_target = area.live.path().join(dirfd_absent_name);
    // A chain of dangling final links into the area: the first names
    // the second relatively, and only the second names the area.
    let chain_absent_name = "u:object_r:chain_absent_prop:s0";
    let chain_absent_target = area.live.path().join(chain_absent_name);
    let chain_link2 = outside.path().join("chain-final-2");
    std::os::unix::fs::symlink(&chain_absent_target, &chain_link2).unwrap();
    let chain_link1 = outside.path().join("chain-final-1");
    std::os::unix::fs::symlink("chain-final-2", &chain_link1).unwrap();
    // The same chain shape naming an absent file outside the area:
    // creation through it still succeeds (the positive control).
    let outside_chain_target = outside.path().join("outside-chain-absent");
    let outside_chain_link2 = outside.path().join("outside-chain-2");
    std::os::unix::fs::symlink(&outside_chain_target, &outside_chain_link2).unwrap();
    let outside_chain_link1 = outside.path().join("outside-chain-1");
    std::os::unix::fs::symlink("outside-chain-2", &outside_chain_link1).unwrap();
    // A plain absent file outside the area, created by its own name.
    let outside_plain_target = outside.path().join("outside-plain-absent");
    // A dangling final link into the area whose own spelling is too
    // long to stage for resolution in the guest: its creation is
    // refused fail-closed rather than made live first.
    let overlong_absent_name = "u:object_r:overlong_absent_prop:s0";
    let overlong_absent_target = area.live.path().join(overlong_absent_name);
    let overlong_dir = outside
        .path()
        .join("a".repeat(90))
        .join("b".repeat(90))
        .join("c".repeat(90));
    std::fs::create_dir_all(&overlong_dir).unwrap();
    let overlong_link = overlong_dir.join("overlong-final");
    std::os::unix::fs::symlink(&overlong_absent_target, &overlong_link).unwrap();
    assert!(
        overlong_link.as_os_str().as_encoded_bytes().len() > 256,
        "the overlong spelling must exceed the staged resolution bound"
    );
    // Dangling chains at the kernel's nested-link limit, in a fresh
    // outside directory with no symlinked intermediate components:
    // `l0 -> l1 -> ... -> l39 -> <area>/absent` is exactly 40 links
    // (Linux creates through 40 traversals, so the creation must be
    // refused first), and one more link in front is 41 (Linux
    // refuses the real open with `ELOOP`).
    let bound_outside = tempfile::tempdir().unwrap();
    let bound40_absent_name = "u:object_r:bound40_prop:s0";
    let bound40_absent_target = area.live.path().join(bound40_absent_name);
    let bound41_absent_name = "u:object_r:bound41_prop:s0";
    let bound41_absent_target = area.live.path().join(bound41_absent_name);
    for index in 0..40 {
        let next = if index == 39 {
            bound40_absent_target.clone()
        } else {
            PathBuf::from(format!("l{}", index + 1))
        };
        std::os::unix::fs::symlink(next, bound_outside.path().join(format!("l{index}"))).unwrap();
    }
    let bound40_link1 = bound_outside.path().join("l0");
    let bound39_link1 = bound_outside.path().join("l1");
    for index in 0..41 {
        let next = if index == 40 {
            bound41_absent_target.clone()
        } else {
            PathBuf::from(format!("m{}", index + 1))
        };
        std::os::unix::fs::symlink(next, bound_outside.path().join(format!("m{index}"))).unwrap();
    }
    let bound41_link1 = bound_outside.path().join("m0");
    let dotdot = area
        .live
        .path()
        .join("..")
        .join(area.live.path().file_name().unwrap())
        .join(later_name);
    let dotdot_copied = area
        .live
        .path()
        .join("..")
        .join(area.live.path().file_name().unwrap())
        .join(name);
    let config = detcore::Config {
        max_timeslice: None,
        sequentialize_threads: true,
        property_snapshot: Some(area.snapshot.clone()),
        ..Default::default()
    };
    let live_dir = area.live.path().to_owned();
    let outside_dir = outside.path().to_owned();
    let later_host = later.clone();
    let absent_host = absent.clone();
    let dangling_host = dangling_target.clone();
    let retained_host = retained_target.clone();
    let alias_absent_host = alias_absent_target.clone();
    let dirfd_absent_host = dirfd_absent_target.clone();
    let chain_absent_host = chain_absent_target.clone();
    let overlong_absent_host = overlong_absent_target.clone();
    let bound40_absent_host = bound40_absent_target.clone();
    let bound41_absent_host = bound41_absent_target.clone();
    // A guest-only descriptor spelling of the area whose parent is
    // too long to resolve staged: the tracer has no fd 900, so only
    // resolution in the guest's own context can decide it.
    let procfd_absent_host = area.live.path().join("long-proc-fd-created");
    let procfd_outside_host = outside.path().join("proc-fd-outside-created");
    assert!(
        !Path::new("/proc/self/fd/900").exists(),
        "control: fd 900 must be absent in the tracer"
    );
    assert!(
        !Path::new("/nonexistent-4791-property-area").exists(),
        "control: the missing-parent directory must be absent"
    );
    // A descriptor this (tracer) process holds on the outside
    // directory, at a number the guest rebinds to the live area:
    // resolving `/proc/self/fd/902/...` on the host succeeds and
    // names the outside directory, while the guest's own fd 902 is
    // the area, so host evidence must never promote that spelling.
    let tracer_outside = std::fs::File::open(outside.path()).unwrap();
    {
        use std::os::fd::AsRawFd;
        assert!(
            !Path::new("/proc/self/fd/902").exists(),
            "control: fd 902 must be absent in the tracer before setup"
        );
        // SAFETY: duplicating an open directory descriptor.
        assert_eq!(unsafe { libc::dup2(tracer_outside.as_raw_fd(), 902) }, 902);
    }
    {
        let via_fd = std::fs::metadata("/proc/self/fd/902").unwrap();
        let direct = std::fs::metadata(outside.path()).unwrap();
        assert_eq!(
            (via_fd.dev(), via_fd.ino()),
            (direct.dev(), direct.ino()),
            "control: the tracer's fd 902 names the outside directory"
        );
    }
    let outside_chain_host = outside_chain_target.clone();
    let outside_plain_host = outside_plain_target.clone();
    let guest = move || {
        let expect_refused = |path: &Path, what: &str| {
            let error = std::fs::read(path).unwrap_err();
            assert_eq!(
                error.raw_os_error(),
                Some(libc::EOPNOTSUPP),
                "{what}: {error}"
            );
        };
        expect_refused(&dotdot, "dotdot spelling");
        expect_refused(&alias_dir.join(later_name), "parent-symlink spelling");
        // F3: readable mutations are refused before any live open runs.
        // The host-side bytes are asserted after the guest, below.
        let expect_open_refused = |path: &Path, flags: libc::c_int, what: &str| {
            let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
            // SAFETY: a valid NUL-terminated path and a creation mode.
            let fd = unsafe { libc::open(c.as_ptr(), flags, 0o644) };
            assert_eq!(fd, -1, "{what} must be refused");
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EOPNOTSUPP),
                "{what}"
            );
        };
        expect_open_refused(&later, libc::O_RDONLY | libc::O_TRUNC, "direct O_TRUNC");
        expect_open_refused(
            &alias_dir.join(later_name),
            libc::O_RDONLY | libc::O_TRUNC,
            "alias O_TRUNC",
        );
        expect_open_refused(
            &alias_dir.join(absent_name),
            libc::O_RDONLY | libc::O_CREAT | libc::O_EXCL,
            "alias O_CREAT|O_EXCL of an absent file",
        );
        // Only the link's target directory is the area here; the
        // spelled parent is outside it. The host file must stay absent.
        expect_open_refused(
            &dangling_link,
            libc::O_RDONLY | libc::O_CREAT,
            "dangling final symlink O_CREAT",
        );
        expect_open_refused(
            &dangling_link,
            libc::O_WRONLY | libc::O_CREAT,
            "dangling final symlink write-only O_CREAT",
        );
        // F5: a nonreading `O_PATH` open succeeds as natively; reading
        // it is `EBADF`, and a readable reopen through its descriptor
        // link is decided (and refused) as its own open.
        {
            let c =
                std::ffi::CString::new(alias_dir.join(later_name).as_os_str().as_encoded_bytes())
                    .unwrap();
            // SAFETY: a valid NUL-terminated path.
            let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
            assert!(fd >= 0, "O_PATH: {}", std::io::Error::last_os_error());
            let mut byte = [0_u8; 1];
            // SAFETY: `fd` is open and `byte` is writable for its length.
            let read = unsafe { libc::read(fd, byte.as_mut_ptr().cast(), byte.len()) };
            assert_eq!(read, -1, "an O_PATH descriptor reads nothing");
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EBADF)
            );
            let reopen = std::ffi::CString::new(format!("/proc/self/fd/{fd}")).unwrap();
            // SAFETY: a valid NUL-terminated descriptor-link path.
            let reopened = unsafe { libc::open(reopen.as_ptr(), libc::O_RDONLY) };
            assert_eq!(reopened, -1, "a readable reopen must be refused");
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EOPNOTSUPP)
            );
            // SAFETY: the descriptor opened above.
            unsafe { libc::close(fd) };
        }
        // A dirfd on the area, and one on its parent, naming it relatively.
        use std::os::fd::AsRawFd;
        let dir = std::fs::File::open(&live_dir).unwrap();
        let rel = std::ffi::CString::new(later_name).unwrap();
        // SAFETY: valid dirfd and NUL-terminated relative path.
        let fd = unsafe { libc::openat(dir.as_raw_fd(), rel.as_ptr(), libc::O_RDONLY) };
        assert_eq!(fd, -1, "dirfd spelling must be refused");
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EOPNOTSUPP)
        );
        // Positive controls: the copied file, by alias and by dirfd.
        assert_eq!(std::fs::read(&dotdot_copied).unwrap(), [5, 6, 7, 8]);
        assert_eq!(std::fs::read(alias_dir.join(name)).unwrap(), [5, 6, 7, 8]);
        let rel = std::ffi::CString::new(name).unwrap();
        // SAFETY: as above; the descriptor is closed below.
        let fd = unsafe { libc::openat(dir.as_raw_fd(), rel.as_ptr(), libc::O_RDONLY) };
        assert!(fd >= 0, "{}", std::io::Error::last_os_error());
        let mut bytes = [0_u8; 4];
        // SAFETY: `fd` is open and `bytes` is writable for its length.
        let read = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
        assert_eq!(read, 4);
        assert_eq!(bytes, [5, 6, 7, 8]);
        // SAFETY: the descriptor opened above.
        unsafe { libc::close(fd) };
        // F3: a retained dirfd creates in the directory object it
        // holds, not in the directory its spelling now names. The
        // alias is replaced after the descriptor is retained; the
        // host file in the area must stay absent.
        {
            let retained = std::fs::File::open(&alias_dir).unwrap();
            std::fs::remove_file(&alias_dir).unwrap();
            std::os::unix::fs::symlink(&outside_dir, &alias_dir).unwrap();
            let rel = std::ffi::CString::new(retained_name).unwrap();
            // SAFETY: valid dirfd and NUL-terminated relative path.
            let fd = unsafe {
                libc::openat(
                    retained.as_raw_fd(),
                    rel.as_ptr(),
                    libc::O_RDONLY | libc::O_CREAT | libc::O_EXCL,
                    0o600,
                )
            };
            assert_eq!(fd, -1, "retained-dirfd creation must be refused");
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EOPNOTSUPP)
            );
            drop(retained);
            std::fs::remove_file(&alias_dir).unwrap();
            std::os::unix::fs::symlink(&live_dir, &alias_dir).unwrap();
        }
        // A symlinked final parent is followed by the pending open, as
        // every intermediate component is: creation through it is
        // refused before it runs, by each spelling below. Outcomes
        // are collected so one run names every spelling that was not
        // refused; the host files must stay absent either way.
        {
            let mut failures: Vec<String> = Vec::new();
            let mut check_refused = |path: &Path, flags: libc::c_int, what: &str| {
                let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
                // SAFETY: a valid NUL-terminated path and a creation mode.
                let fd = unsafe { libc::open(c.as_ptr(), flags, 0o644) };
                if fd >= 0 {
                    // SAFETY: the descriptor opened above.
                    unsafe { libc::close(fd) };
                    failures.push(format!("{what}: open succeeded"));
                } else if std::io::Error::last_os_error().raw_os_error() != Some(libc::EOPNOTSUPP) {
                    failures.push(format!(
                        "{what}: refused with {}",
                        std::io::Error::last_os_error()
                    ));
                }
            };
            // Only the link target's parent, an alias for the area, is
            // the area; the spelled parent is outside it.
            check_refused(
                &alias_dangling_link,
                libc::O_WRONLY | libc::O_CREAT,
                "alias target-parent dangling write-only O_CREAT",
            );
            check_refused(
                &alias_dangling_link,
                libc::O_RDONLY | libc::O_CREAT,
                "alias target-parent dangling O_CREAT",
            );
            // Only the second link of the chain names the area.
            check_refused(
                &chain_link1,
                libc::O_WRONLY | libc::O_CREAT,
                "chained dangling write-only O_CREAT",
            );
            // The link spelling itself is too long to resolve staged.
            check_refused(
                &overlong_link,
                libc::O_WRONLY | libc::O_CREAT,
                "overlong dangling write-only O_CREAT",
            );
            // Below and at the kernel's 40 nested links: Linux
            // creates through this many traversals, so both must
            // be refused.
            check_refused(
                &bound39_link1,
                libc::O_WRONLY | libc::O_CREAT,
                "39-link dangling chain write-only O_CREAT",
            );
            check_refused(
                &bound40_link1,
                libc::O_WRONLY | libc::O_CREAT,
                "exactly-40 dangling chain write-only O_CREAT",
            );
            // One link past the limit: the real open fails `ELOOP`
            // natively (not the property refusal) and creates nothing.
            {
                let c =
                    std::ffi::CString::new(bound41_link1.as_os_str().as_encoded_bytes()).unwrap();
                // SAFETY: a valid NUL-terminated path and a creation mode.
                let fd = unsafe { libc::open(c.as_ptr(), libc::O_WRONLY | libc::O_CREAT, 0o644) };
                if fd >= 0 {
                    // SAFETY: the descriptor opened above.
                    unsafe { libc::close(fd) };
                    failures
                        .push("41-link dangling chain write-only O_CREAT: open succeeded".into());
                } else if std::io::Error::last_os_error().raw_os_error() != Some(libc::ELOOP) {
                    failures.push(format!(
                        "41-link dangling chain write-only O_CREAT: refused with {}",
                        std::io::Error::last_os_error()
                    ));
                }
            }
            // `alias-area/<name>` under a dirfd on the outside
            // directory: the alias is the final parent component.
            {
                let outside_fd = std::fs::File::open(&outside_dir).unwrap();
                let rel =
                    std::ffi::CString::new(format!("alias-area/{dirfd_absent_name}")).unwrap();
                // SAFETY: valid dirfd and NUL-terminated relative path.
                let fd = unsafe {
                    libc::openat(
                        outside_fd.as_raw_fd(),
                        rel.as_ptr(),
                        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                        0o600,
                    )
                };
                if fd >= 0 {
                    // SAFETY: the descriptor opened above.
                    unsafe { libc::close(fd) };
                    failures.push("dirfd alias-parent write-only O_CREAT: open succeeded".into());
                } else if std::io::Error::last_os_error().raw_os_error() != Some(libc::EOPNOTSUPP) {
                    failures.push(format!(
                        "dirfd alias-parent write-only O_CREAT: refused with {}",
                        std::io::Error::last_os_error()
                    ));
                }
            }
            // Positive controls: creations outside the area, through
            // the same chain shape and by a plain name, still succeed.
            let mut check_created = |path: &Path, what: &str| {
                let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
                // SAFETY: a valid NUL-terminated path and a creation mode.
                let fd = unsafe { libc::open(c.as_ptr(), libc::O_WRONLY | libc::O_CREAT, 0o644) };
                if fd >= 0 {
                    // SAFETY: the descriptor opened above.
                    unsafe { libc::close(fd) };
                } else {
                    failures.push(format!(
                        "{what}: outside creation refused with {}",
                        std::io::Error::last_os_error()
                    ));
                }
            };
            check_created(&outside_chain_link1, "chained dangling outside O_CREAT");
            check_created(&outside_plain_target, "plain outside O_CREAT");
            assert!(failures.is_empty(), "symlinked-parent cases: {failures:?}");
        }
        // A parent named through a guest-only descriptor, too long
        // to resolve staged, cannot be decided from the tracer's
        // descriptor table: creation through it is refused before
        // it runs, under every spelling that resolves to it in the
        // guest, while a short descriptor spelling of an outside
        // directory is decided in the guest and still creates.
        // Outcomes are collected so one run names every spelling
        // that was not refused; the host files must stay absent.
        {
            let directory = std::fs::File::open(&live_dir).unwrap();
            // SAFETY: duplicating an open directory descriptor.
            assert_eq!(unsafe { libc::dup2(directory.as_raw_fd(), 900) }, 900);
            // The guest's fd 902 is the area, where the tracer's
            // own fd 902 (held by the test) names the outside dir.
            // SAFETY: duplicating an open directory descriptor.
            assert_eq!(unsafe { libc::dup2(directory.as_raw_fd(), 902) }, 902);
            let dots = "./".repeat(300);
            let spellings: [(&str, String); 10] = [
                (
                    "long guest-only fd parent",
                    format!("/proc/self/fd/900/{dots}long-proc-fd-created"),
                ),
                (
                    "dot proc self fd parent",
                    format!("/proc/./self/fd/900/{dots}dot-proc-fd-created"),
                ),
                (
                    "dot proc thread-self fd parent",
                    format!("/proc/./thread-self/fd/900/{dots}dot-thread-fd-created"),
                ),
                (
                    "dot dev fd parent",
                    format!("/dev/./fd/900/{dots}dot-dev-fd-created"),
                ),
                (
                    "repeated-slash proc fd parent",
                    format!("//proc//self//fd/900/{dots}slashes-proc-fd-created"),
                ),
                (
                    "dotdot proc fd parent",
                    format!("/tmp/../proc/self/fd/900/{dots}dotdot-proc-fd-created"),
                ),
                (
                    "inner dotdot proc fd parent",
                    format!("/proc/self/fd/../fd/900/{dots}inner-dotdot-proc-fd-created"),
                ),
                (
                    "leading dot proc fd parent",
                    format!("/./proc/self/fd/900/{dots}leading-dot-proc-created"),
                ),
                (
                    "host-success divergent fd parent",
                    format!("/proc/./self/fd/902/{dots}host-alias-fd-created"),
                ),
                (
                    "symlink alias to guest fd parent",
                    format!(
                        "{}/fd-alias/{dots}alias-to-fd-created",
                        outside_dir.display()
                    ),
                ),
            ];
            let mut failures: Vec<String> = Vec::new();
            for (what, spelling) in &spellings {
                assert!(spelling.len() > 512, "{what}");
                let c = std::ffi::CString::new(spelling.as_str()).unwrap();
                // SAFETY: a valid NUL-terminated path and a creation mode.
                let fd = unsafe {
                    libc::open(
                        c.as_ptr(),
                        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                        0o600,
                    )
                };
                if fd >= 0 {
                    // SAFETY: the descriptor opened above.
                    unsafe { libc::close(fd) };
                    failures.push(format!("{what}: open succeeded"));
                } else if std::io::Error::last_os_error().raw_os_error() != Some(libc::EOPNOTSUPP) {
                    failures.push(format!(
                        "{what}: refused with {}",
                        std::io::Error::last_os_error()
                    ));
                }
            }
            // SAFETY: the descriptors duplicated above.
            unsafe { libc::close(900) };
            unsafe { libc::close(902) };
            drop(directory);
            // A parent that cannot be proven outside the area fails
            // closed with the named refusal: a resolution that fails,
            // in the guest probe or on the host, is not proof of
            // outside. Natively both spellings fail `ENOENT`.
            for (what, spelling) in [
                (
                    "short missing parent",
                    "/nonexistent-4791-property-area/missing-short-created".to_owned(),
                ),
                (
                    "long missing parent",
                    format!("/nonexistent-4791-property-area/{dots}missing-long-created"),
                ),
            ] {
                let c = std::ffi::CString::new(spelling).unwrap();
                // SAFETY: a valid NUL-terminated path and a creation mode.
                let fd = unsafe {
                    libc::open(
                        c.as_ptr(),
                        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                        0o600,
                    )
                };
                if fd >= 0 {
                    // SAFETY: the descriptor opened above.
                    unsafe { libc::close(fd) };
                    failures.push(format!("{what}: open succeeded"));
                } else if std::io::Error::last_os_error().raw_os_error() != Some(libc::EOPNOTSUPP) {
                    failures.push(format!(
                        "{what}: refused with {}",
                        std::io::Error::last_os_error()
                    ));
                }
            }
            // An overlong spelling of a real outside directory is
            // the stated fail-closed bound: only host-side
            // resolution could decide it, and host resolution never
            // decides a spelled parent, so it is refused by name
            // even though it is in fact outside. Guest-provable
            // outside creations are the short controls above and
            // below (plain, chained, short descriptor spellings).
            {
                let spelling = format!("{}/{dots}long-outside-created", outside_dir.display());
                assert!(spelling.len() > 512);
                let c = std::ffi::CString::new(spelling).unwrap();
                // SAFETY: a valid NUL-terminated path and a creation mode.
                let fd = unsafe {
                    libc::open(
                        c.as_ptr(),
                        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                        0o600,
                    )
                };
                if fd >= 0 {
                    // SAFETY: the descriptor opened above.
                    unsafe { libc::close(fd) };
                    failures
                        .push("long host-only-resolvable outside parent: open succeeded".into());
                } else if std::io::Error::last_os_error().raw_os_error() != Some(libc::EOPNOTSUPP) {
                    failures.push(format!(
                        "long host-only-resolvable outside parent: refused with {}",
                        std::io::Error::last_os_error()
                    ));
                }
            }
            assert!(failures.is_empty(), "guest-context spellings: {failures:?}");
            let outside_directory = std::fs::File::open(&outside_dir).unwrap();
            // SAFETY: duplicating an open directory descriptor.
            assert_eq!(
                unsafe { libc::dup2(outside_directory.as_raw_fd(), 901) },
                901
            );
            let c = std::ffi::CString::new("/proc/self/fd/901/proc-fd-outside-created").unwrap();
            // SAFETY: a valid NUL-terminated path and a creation mode.
            // No `O_EXCL`: the guest runs more than once.
            let fd = unsafe { libc::open(c.as_ptr(), libc::O_WRONLY | libc::O_CREAT, 0o600) };
            assert!(
                fd >= 0,
                "short proc-fd outside creation: {}",
                std::io::Error::last_os_error()
            );
            // SAFETY: the descriptors opened above.
            unsafe { libc::close(fd) };
            unsafe { libc::close(901) };
        }
        let _ = file;
    };
    detcore_testutils::det_test_fn_with_config(
        true,
        guest,
        config,
        detcore_testutils::expect_success,
    );
    // F3 host-side effects: no refused mutation reached the live file,
    // and the refused creations made no file, by any resolution.
    assert_eq!(std::fs::read(&later_host).unwrap(), [9, 10, 11, 12]);
    assert!(!absent_host.exists(), "a refused O_CREAT made a file");
    assert!(
        !dangling_host.exists(),
        "a refused dangling-symlink O_CREAT made a file"
    );
    assert!(
        !retained_host.exists(),
        "a refused retained-dirfd O_CREAT made a file"
    );
    assert!(
        !alias_absent_host.exists(),
        "a refused alias target-parent O_CREAT made a file"
    );
    assert!(
        !dirfd_absent_host.exists(),
        "a refused dirfd alias-parent O_CREAT made a file"
    );
    assert!(
        !chain_absent_host.exists(),
        "a refused chained dangling O_CREAT made a file"
    );
    assert!(
        !overlong_absent_host.exists(),
        "a refused overlong dangling O_CREAT made a file"
    );
    assert!(
        !bound40_absent_host.exists(),
        "a refused exactly-40 dangling chain O_CREAT made a file"
    );
    assert!(
        !bound41_absent_host.exists(),
        "an ELOOP 41-link dangling chain O_CREAT made a file"
    );
    assert!(
        !procfd_absent_host.exists(),
        "a refused long guest-only fd parent O_CREAT made a file"
    );
    for name in [
        "dot-proc-fd-created",
        "dot-thread-fd-created",
        "dot-dev-fd-created",
        "slashes-proc-fd-created",
        "dotdot-proc-fd-created",
        "inner-dotdot-proc-fd-created",
        "leading-dot-proc-created",
        "host-alias-fd-created",
        "alias-to-fd-created",
    ] {
        assert!(
            !area.live.path().join(name).exists(),
            "a refused {name} O_CREAT made a file"
        );
    }
    assert!(
        !Path::new("/nonexistent-4791-property-area").exists(),
        "a refused missing-parent O_CREAT made its directory"
    );
    assert!(
        !outside.path().join("long-outside-created").exists(),
        "a refused long host-only-resolvable outside O_CREAT made a file"
    );
    assert!(
        procfd_outside_host.exists(),
        "a short proc-fd creation outside the area made no file"
    );
    assert!(
        outside_chain_host.exists(),
        "a chained creation outside the area made no file"
    );
    assert!(
        outside_plain_host.exists(),
        "a plain creation outside the area made no file"
    );
    assert!(
        !outside.path().join("host-alias-fd-created").exists(),
        "a refused host-success divergent fd O_CREAT made a file outside"
    );
    // SAFETY: the descriptor duplicated in this process above.
    unsafe { libc::close(902) };
    drop(tracer_outside);
}

/// A redirected open serves its copy through a non-`AT_FDCWD` dirfd and
/// after the guest changes its working directory, even when the published
/// copy directory is relative: it is bound independently of the guest's
/// dirfd and cwd.
#[test]
fn a_copy_is_served_by_dirfd_after_a_guest_chdir() {
    // Copies in a directory named relative to this process's working
    // directory, which the guest cannot resolve from its own cwd/dirfd.
    // The directory itself lives in a fresh tempdir, never in the source
    // tree: a relative name created in the cwd landed inside /src during
    // validation, where another node's prepare step hashed it as a
    // prepared input and then found it gone (the 5232 ejection).
    let area = area();
    let name = "u:object_r:test_prop:s0";
    let file = area.live.path().join(name);
    let copies_root = std::env::temp_dir().join(format!(
        "hermit-test-relative-copies-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_file(copies_root.join(name));
    let _ = std::fs::remove_dir(&copies_root);
    std::fs::create_dir_all(&copies_root).unwrap();
    std::fs::write(copies_root.join(name), [5, 6, 7, 8]).unwrap();
    // The copy_dir stays relative: expressed from this process's cwd to
    // the tempdir, so the tracer still resolves a relative published
    // directory while nothing is written into the source tree.
    let relative_name = {
        let cwd = std::env::current_dir().unwrap();
        let cwd_parts: Vec<_> = cwd.components().collect();
        let root_parts: Vec<_> = copies_root.components().collect();
        let common = cwd_parts
            .iter()
            .zip(&root_parts)
            .take_while(|(a, b)| a == b)
            .count();
        let mut relative = std::path::PathBuf::new();
        for _ in common..cwd_parts.len() {
            relative.push("..");
        }
        for part in &root_parts[common..] {
            relative.push(part.as_os_str());
        }
        relative.to_string_lossy().into_owned()
    };
    let relative_abs = copies_root.clone();
    let mut snapshot = area.snapshot.clone();
    snapshot.copy_dir = PathBuf::from(&relative_name);
    let config = detcore::Config {
        max_timeslice: None,
        sequentialize_threads: true,
        property_snapshot: Some(snapshot),
        ..Default::default()
    };
    let live_dir = area.live.path().to_owned();
    let guest = move || {
        use std::os::fd::AsRawFd;
        std::env::set_current_dir("/").unwrap();
        let dir = std::fs::File::open(&live_dir).unwrap();
        let rel = std::ffi::CString::new(name).unwrap();
        // SAFETY: valid dirfd and NUL-terminated relative path.
        let fd = unsafe { libc::openat(dir.as_raw_fd(), rel.as_ptr(), libc::O_RDONLY) };
        assert!(fd >= 0, "{}", std::io::Error::last_os_error());
        let mut bytes = [0_u8; 4];
        // SAFETY: `fd` is open and `bytes` is writable for its length.
        let read = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
        assert_eq!(read, 4);
        assert_eq!(bytes, [5, 6, 7, 8]);
        // SAFETY: the descriptor opened above.
        unsafe { libc::close(fd) };
        // And by absolute name from the changed cwd.
        assert_eq!(std::fs::read(&file).unwrap(), [5, 6, 7, 8]);
    };
    detcore_testutils::det_test_fn_with_config(
        true,
        guest,
        config,
        detcore_testutils::expect_success,
    );
    let _ = std::fs::remove_file(relative_abs.join(name));
    let _ = std::fs::remove_dir(&relative_abs);
}

/// A file mapping of a live property descriptor is refused (no live map),
/// while a mapping of a copied descriptor supplies the copied bytes.
#[test]
fn a_live_property_mapping_is_refused_and_a_copied_one_maps_the_copy() {
    let area = area();
    let file = area.live.path().join("u:object_r:test_prop:s0");
    let config = detcore::Config {
        max_timeslice: None,
        sequentialize_threads: true,
        property_snapshot: Some(area.snapshot.clone()),
        ..Default::default()
    };
    let guest = move || {
        use std::os::fd::AsRawFd;
        // A write-only open runs natively, so this descriptor is live.
        let live = std::fs::OpenOptions::new().write(true).open(&file).unwrap();
        // SAFETY: a file mapping of the live descriptor.
        let mapped = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                live.as_raw_fd(),
                0,
            )
        };
        assert_eq!(mapped, libc::MAP_FAILED, "a live mapping must be refused");
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EOPNOTSUPP)
        );
        drop(live);
        // The copied descriptor maps the frozen bytes.
        let copied = std::fs::File::open(&file).unwrap();
        // SAFETY: a file mapping of the copied descriptor, unmapped below.
        let mapped = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                copied.as_raw_fd(),
                0,
            )
        };
        assert_ne!(
            mapped,
            libc::MAP_FAILED,
            "{}",
            std::io::Error::last_os_error()
        );
        // SAFETY: four bytes are mapped from the copy.
        let bytes = unsafe { std::slice::from_raw_parts(mapped.cast::<u8>(), 4) };
        assert_eq!(bytes, [5, 6, 7, 8]);
        // SAFETY: the mapping made above.
        unsafe { libc::munmap(mapped, 4096) };
    };
    detcore_testutils::det_test_fn_with_config(
        true,
        guest,
        config,
        detcore_testutils::expect_success,
    );
}

/// The same reads with Hermit's metadata virtualization off, where the
/// descriptor check takes its own `fstat`.
#[test]
fn a_property_snapshot_serves_reads_with_metadata_virtualization_off() {
    let area = area();
    let file = area.live.path().join("u:object_r:test_prop:s0");
    let config = detcore::Config {
        max_timeslice: None,
        sequentialize_threads: true,
        virtualize_metadata: false,
        property_snapshot: Some(area.snapshot.clone()),
        ..Default::default()
    };
    let guest = move || {
        assert_eq!(std::fs::read(&file).unwrap(), [5, 6, 7, 8]);
        // An unrelated read is checked against the snapshot and passes.
        assert!(std::fs::read("/proc/self/stat").is_ok());
    };
    detcore_testutils::det_test_fn_with_config(
        true,
        guest,
        config,
        detcore_testutils::expect_success,
    );
}

/// In a fail-closed run, an anonymous mapping that names a descriptor of a
/// live property file (opened write-only, which runs natively) succeeds, since
/// Linux ignores the descriptor there, and a file mapping of a descriptor that
/// is not open fails natively with `EBADF`: neither is refused.
#[test]
fn mappings_that_read_no_property_file_are_not_refused() {
    let area = area();
    let file = area.live.path().join("u:object_r:test_prop:s0");
    let config = detcore::Config {
        max_timeslice: None,
        sequentialize_threads: true,
        panic_on_unsupported_syscalls: true,
        shutdown_on_unsupported_syscall: true,
        property_snapshot: Some(area.snapshot.clone()),
        ..Default::default()
    };
    let guest = move || {
        use std::os::fd::AsRawFd;
        let live = std::fs::OpenOptions::new().write(true).open(&file).unwrap();
        // SAFETY: an anonymous private mapping, unmapped below.
        let anonymous = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                live.as_raw_fd(),
                0,
            )
        };
        assert_ne!(
            anonymous,
            libc::MAP_FAILED,
            "{}",
            std::io::Error::last_os_error()
        );
        // SAFETY: the mapping made above.
        unsafe { libc::munmap(anonymous, 4096) };
        // A descriptor number this guest opened and closed, so it is not
        // open (not -1, which skips the check entirely).
        let closed_fd = std::fs::File::open("/dev/null").unwrap().as_raw_fd();
        // SAFETY: a file mapping of a descriptor that is not open fails.
        let closed = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                closed_fd,
                0,
            )
        };
        assert_eq!(closed, libc::MAP_FAILED);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EBADF)
        );
    };
    detcore_testutils::det_test_fn_with_config(
        true,
        guest,
        config,
        detcore_testutils::expect_success,
    );
}

/// Marks a run of this test binary that a refusal test starts for its
/// guest: the refusal ends the tool's process, which in a test is the test
/// binary, so the guest runs in a binary of its own.
const REFUSAL_ENV: &str = "HERMIT_PROPERTY_SNAPSHOT_REFUSAL_INNER";

/// Whether this process is the inner run of the refusal test `test`. If it
/// is not, runs `test` again in a binary of its own and checks that its
/// guest's run ended with the policy-refusal exit, the live-area warning and
/// `why`.
fn is_inner_refusal_run(test: &str, why: &str) -> bool {
    if std::env::var_os(REFUSAL_ENV).is_some() {
        return true;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .arg(format!("property_snapshot::{test}"))
        .args(["--exact", "--nocapture", "--test-threads=1"])
        .env(REFUSAL_ENV, "1")
        .output()
        .expect("run this test binary again");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
        "{stderr}"
    );
    assert!(
        stderr.contains("HERMIT_PROPERTY_AREA_LIVE") && stderr.contains(why),
        "{stderr}"
    );
    false
}

/// Runs `guest` in a fail-closed run under `snapshot`; returns only if the
/// run was not refused.
fn run_fail_closed(
    snapshot: detcore_model::config::PropertySnapshot,
    guest: impl FnOnce() + Send + 'static,
) {
    let config = detcore::Config {
        max_timeslice: None,
        sequentialize_threads: true,
        panic_on_unsupported_syscalls: true,
        shutdown_on_unsupported_syscall: true,
        property_snapshot: Some(snapshot),
        ..Default::default()
    };
    // On a thread of its own with a large stack: Detcore's futures outgrow
    // the test harness thread's.
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(move || {
            let _ = detcore_testutils::test_fn_with_config::<detcore::Detcore, _>(
                move || {
                    guest();
                    std::process::exit(0);
                },
                detcore_testutils::prepare_ptrace_timer_config(config),
                true,
            );
        })
        .unwrap()
        .join()
        .unwrap();
}

/// In a fail-closed run, a read of a file the area gained after its snapshot
/// was taken ends the run with the policy-refusal exit, naming why.
#[test]
fn a_file_the_area_gained_after_its_snapshot_is_refused() {
    if !is_inner_refusal_run(
        "a_file_the_area_gained_after_its_snapshot_is_refused",
        "is in the property area but not in its snapshot",
    ) {
        return;
    }
    let area = area();
    let later = area.live.path().join("u:object_r:later_prop:s0");
    std::fs::write(&later, [9]).unwrap();
    run_fail_closed(area.snapshot.clone(), move || {
        let _ = std::fs::read(&later);
    });
    panic!("the run was not refused");
}

/// A file the capture saw but could not copy (it stopped being readable
/// mid-capture) is known by device and inode: in a fail-closed run, a read
/// of it through a hard link outside the area, a name the snapshot never
/// saw, is refused rather than read live unnoticed.
#[test]
fn a_file_without_a_copy_is_refused_through_an_outside_alias() {
    if !is_inner_refusal_run(
        "a_file_without_a_copy_is_refused_through_an_outside_alias",
        "stopped being readable while the snapshot was taken",
    ) {
        return;
    }
    let mut area = area();
    let gone = area.live.path().join("u:object_r:gone_prop:s0");
    std::fs::write(&gone, [9]).unwrap();
    let metadata = std::fs::metadata(&gone).unwrap();
    area.snapshot
        .uncopied_files
        .push(detcore_model::config::PropertyFile {
            dev: metadata.dev(),
            ino: metadata.ino(),
            name: "u:object_r:gone_prop:s0".to_owned(),
        });
    let outside = tempfile::tempdir_in(area.live.path().parent().unwrap()).unwrap();
    let alias = outside.path().join("alias");
    std::fs::hard_link(&gone, &alias).unwrap();
    run_fail_closed(area.snapshot.clone(), move || {
        let _ = std::fs::read(&alias);
    });
    panic!("the run was not refused");
}

/// Opens `path` with `flags` (and a mode, for `O_CREAT`) in the guest, reads
/// up to four bytes from it if the open succeeds, and closes it.
fn open_in_guest(path: &Path, flags: libc::c_int) {
    let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: a valid NUL-terminated path, and a mode for O_CREAT.
    let fd = unsafe { libc::open(path.as_ptr(), flags, 0o644) };
    if fd >= 0 {
        let mut bytes = [0_u8; 4];
        // SAFETY: `fd` is open and `bytes` is writable for its length.
        unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
        // SAFETY: the descriptor opened above.
        unsafe { libc::close(fd) };
    }
}

/// In a fail-closed run, an open that reads a copied property file and also
/// writes it (`O_RDWR`) is refused before it runs: its descriptor would read
/// the live file, which processes outside the run rewrite, and the frozen
/// copy cannot serve a write. Before, it ran on the live file and the guest
/// read live bytes.
#[test]
fn a_readable_write_open_of_a_copied_property_file_is_refused() {
    if !is_inner_refusal_run(
        "a_readable_write_open_of_a_copied_property_file_is_refused",
        "to read and also to write, create or truncate it",
    ) {
        return;
    }
    let area = area();
    let file = area.live.path().join("u:object_r:test_prop:s0");
    run_fail_closed(area.snapshot.clone(), move || {
        open_in_guest(&file, libc::O_RDWR);
    });
    panic!("the run was not refused");
}

/// The same for `O_RDONLY | O_CREAT` of an existing copied property file:
/// Linux opens the existing file for reading and creates nothing, so its
/// descriptor would read the live file. A fail-closed run refuses it before
/// it runs.
#[test]
fn a_readable_create_open_of_a_copied_property_file_is_refused() {
    if !is_inner_refusal_run(
        "a_readable_create_open_of_a_copied_property_file_is_refused",
        "to read and also to write, create or truncate it",
    ) {
        return;
    }
    let area = area();
    let file = area.live.path().join("u:object_r:test_prop:s0");
    run_fail_closed(area.snapshot.clone(), move || {
        open_in_guest(&file, libc::O_RDONLY | libc::O_CREAT);
    });
    panic!("the run was not refused");
}

/// In a fail-closed run, an open of a copied property file with the
/// reserved access mode 3 runs as natively: Linux checks read and write
/// permission for it but opens a descriptor that can be neither read nor
/// written (`OPEN_FMODE`), so it reads no live bytes and is not refused.
/// Its read fails with `EBADF`, as natively.
#[test]
fn an_access_mode_3_open_of_a_copied_property_file_runs_as_natively() {
    let area = area();
    let file = area.live.path().join("u:object_r:test_prop:s0");
    let native = |path: &Path| -> (bool, Option<i32>) {
        let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path; the descriptor is closed below.
        let fd = unsafe { libc::open(path.as_ptr(), 3) };
        if fd < 0 {
            return (false, std::io::Error::last_os_error().raw_os_error());
        }
        let mut bytes = [0_u8; 4];
        // SAFETY: `fd` is open and `bytes` is writable for its length.
        let read = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
        let errno = (read < 0)
            .then(|| std::io::Error::last_os_error().raw_os_error())
            .flatten();
        // SAFETY: the descriptor opened above.
        unsafe { libc::close(fd) };
        (true, errno)
    };
    assert_eq!(native(&file), (true, Some(libc::EBADF)), "natively");
    let config = detcore::Config {
        max_timeslice: None,
        sequentialize_threads: true,
        panic_on_unsupported_syscalls: true,
        shutdown_on_unsupported_syscall: true,
        property_snapshot: Some(area.snapshot.clone()),
        ..Default::default()
    };
    let guest = move || {
        assert_eq!(native(&file), (true, Some(libc::EBADF)), "under Hermit");
    };
    detcore_testutils::det_test_fn_with_config(
        true,
        guest,
        config,
        detcore_testutils::expect_success,
    );
}
