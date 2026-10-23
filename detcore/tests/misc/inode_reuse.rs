/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A host may give a freed inode to a new file (ext4 and XFS do), and whether
//! it does differs between runs, so Detcore retires an inode's number when its
//! file loses its last name and numbers the next file the host gives that
//! inode afresh (https://github.com/rrnewton/hermit/issues/3840).
//!
//! One test makes the host reuse an inode for real: a fresh tmpfs numbers its
//! first file the same as the last one did, and the kernel gives the new mount
//! the freed device number. The others cover the removal paths that retire a
//! number, the removals that must not, and the Linux behaviour that must
//! survive a retirement: an unlinked file reached through a descriptor, or
//! through `/proc/self/fd/N`, keeps its inode number.

use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::DirEntryExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;

use detcore::Config;
use detcore::Detcore;
use reverie::ExitStatus;

fn ino_of_fd(fd: i32) -> u64 {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::fstat(fd, &mut stat) }, 0, "fstat({fd})");
    stat.st_ino
}

fn ino_of_path(path: &Path) -> u64 {
    std::fs::symlink_metadata(path)
        .unwrap_or_else(|error| panic!("lstat {}: {error}", path.display()))
        .ino()
}

fn renameat2(a: &Path, b: &Path, flags: libc::c_uint) -> Result<(), i32> {
    let a = CString::new(a.as_os_str().as_bytes()).unwrap();
    let b = CString::new(b.as_os_str().as_bytes()).unwrap();
    let rc = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            a.as_ptr(),
            libc::AT_FDCWD,
            b.as_ptr(),
            flags,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap())
    }
}

fn renameat2_exchange(a: &Path, b: &Path) {
    renameat2(a, b, libc::RENAME_EXCHANGE).expect("renameat2(RENAME_EXCHANGE)");
}

/// `fstatat(AT_FDCWD, "", AT_EMPTY_PATH)`: the working directory, reached
/// without a path.
fn ino_of_working_directory() -> u64 {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::fstatat(libc::AT_FDCWD, c"".as_ptr(), &mut stat, libc::AT_EMPTY_PATH) };
    assert_eq!(
        rc,
        0,
        "fstatat(AT_EMPTY_PATH): {}",
        std::io::Error::last_os_error()
    );
    stat.st_ino
}

/// `getdents64(fd, buffer)`, returning each entry's name and `d_ino`.
fn getdents64(fd: i32, buffer: &mut [u8]) -> Vec<(String, u64)> {
    let read =
        unsafe { libc::syscall(libc::SYS_getdents64, fd, buffer.as_mut_ptr(), buffer.len()) };
    assert!(read >= 0, "getdents64: {}", std::io::Error::last_os_error());
    let mut entries = Vec::new();
    let mut offset = 0;
    while offset < read as usize {
        let record = &buffer[offset..];
        let ino = u64::from_ne_bytes(record[0..8].try_into().unwrap());
        let length = usize::from(u16::from_ne_bytes(record[16..18].try_into().unwrap()));
        let name = &record[19..length];
        let name = &name[..name.iter().position(|byte| *byte == 0).unwrap()];
        entries.push((String::from_utf8_lossy(name).into_owned(), ino));
        offset += length;
    }
    entries
}

/// Run one raw syscall with `stack` as its stack pointer, as a guest that
/// keeps its operands below its own red zone would.
unsafe fn syscall_on_stack(stack: *mut u8, number: i64, first: usize, second: usize) -> i64 {
    let result: i64;
    unsafe {
        std::arch::asm!(
            "xchg rsp, r12",
            "syscall",
            "xchg rsp, r12",
            inout("r12") stack => _,
            inlateout("rax") number => result,
            in("rdi") first,
            in("rsi") second,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    result
}

/// Each removal kind, printing the inode numbers whose number must be retired
/// and those that must not, after checking that every number the guest can
/// still reach stayed put.
fn removals_guest(root: &tempfile::TempDir) {
    let dir = root.path();

    // Open, unlink, keep using the descriptor.
    let unlinked = dir.join("unlinked");
    let file = File::create(&unlinked).unwrap();
    let fd = file.as_raw_fd();
    let number = ino_of_fd(fd);
    assert_eq!(ino_of_path(&unlinked), number);
    std::fs::remove_file(&unlinked).unwrap();
    assert_eq!(ino_of_fd(fd), number, "fstat after unlink");
    let through_proc = Path::new("/proc/self/fd").join(fd.to_string());
    assert_eq!(
        std::fs::metadata(&through_proc).unwrap().ino(),
        number,
        "stat of /proc/self/fd/N after unlink"
    );
    let reopened = File::open(&through_proc).unwrap();
    assert_eq!(
        ino_of_fd(reopened.as_raw_fd()),
        number,
        "fstat of a reopen through /proc/self/fd/N after unlink"
    );

    // Removing one of two links leaves a name.
    let first = dir.join("first-link");
    let second = dir.join("second-link");
    File::create(&first).unwrap();
    std::fs::hard_link(&first, &second).unwrap();
    let linked = ino_of_path(&first);
    std::fs::remove_file(&first).unwrap();
    assert_eq!(ino_of_path(&second), linked, "the remaining link");

    // A directory has one name.
    let directory = dir.join("directory");
    std::fs::create_dir(&directory).unwrap();
    let removed_directory = ino_of_path(&directory);
    std::fs::remove_dir(&directory).unwrap();

    // A rename replaces its target.
    let source = dir.join("source");
    let target = dir.join("target");
    File::create(&source).unwrap();
    File::create(&target).unwrap();
    let moved = ino_of_path(&source);
    let replaced = ino_of_path(&target);
    std::fs::rename(&source, &target).unwrap();
    assert_eq!(ino_of_path(&target), moved, "the renamed file");

    // A rename onto its own file removes nothing: onto the same name, or onto
    // another link of the file (where Linux leaves both names).
    std::fs::rename(&target, &target).unwrap();
    let third = dir.join("third-link");
    std::fs::hard_link(&second, &third).unwrap();
    std::fs::rename(&third, &second).unwrap();
    assert_eq!(ino_of_path(&second), linked, "a rename between two links");

    // An exchange removes nothing.
    let left = dir.join("left");
    let right = dir.join("right");
    File::create(&left).unwrap();
    File::create(&right).unwrap();
    let (left_number, right_number) = (ino_of_path(&left), ino_of_path(&right));
    renameat2_exchange(&left, &right);
    assert_eq!(ino_of_path(&left), right_number);
    assert_eq!(ino_of_path(&right), left_number);

    // RENAME_NOREPLACE removes nothing: it fails onto an existing name, and
    // otherwise has no target to replace.
    assert_eq!(
        renameat2(&left, &right, libc::RENAME_NOREPLACE),
        Err(libc::EEXIST)
    );
    let fresh_name = dir.join("fresh-name");
    renameat2(&left, &fresh_name, libc::RENAME_NOREPLACE).unwrap();
    assert_eq!(ino_of_path(&fresh_name), right_number);

    // A file that never has a name keeps its number through its descriptor.
    let tmpfile = CString::new(dir.as_os_str().as_bytes()).unwrap();
    let tmpfile = unsafe { libc::open(tmpfile.as_ptr(), libc::O_TMPFILE | libc::O_RDWR, 0o600) };
    assert!(
        tmpfile >= 0,
        "O_TMPFILE: {}",
        std::io::Error::last_os_error()
    );
    let unnamed = ino_of_fd(tmpfile);
    assert_eq!(
        std::fs::metadata(Path::new("/proc/self/fd").join(tmpfile.to_string()))
            .unwrap()
            .ino(),
        unnamed,
        "stat of an O_TMPFILE file through /proc/self/fd/N"
    );
    assert_eq!(ino_of_fd(tmpfile), unnamed, "a second fstat");
    unsafe { libc::close(tmpfile) };

    // A directory stream snapshots the directory at its first `getdents`, so
    // the rest of a listing can name a file whose last name has gone since.
    // That entry, and the descriptor that holds the file, keep its number
    // (codex review of https://github.com/rrnewton/hermit/pull/3849, P1).
    let listed = dir.join("listed");
    std::fs::create_dir(&listed).unwrap();
    let held_path = listed.join("held");
    let held = File::create(&held_path).unwrap();
    let held_number = ino_of_fd(held.as_raw_fd());
    let listing = File::open(&listed).unwrap();
    // One 24-byte record: `.`, the first entry of the sorted stream.
    let first = getdents64(listing.as_raw_fd(), &mut [0u8; 24]);
    assert_eq!(first.len(), 1, "{first:?}");
    std::fs::remove_file(&held_path).unwrap();
    let rest = getdents64(listing.as_raw_fd(), &mut [0u8; 4096]);
    let cached = rest
        .iter()
        .find(|(name, _)| name == "held")
        .unwrap_or_else(|| panic!("the snapshot names the held file: {rest:?}"));
    assert_eq!(cached.1, held_number, "the snapshot's entry");
    assert_eq!(
        ino_of_fd(held.as_raw_fd()),
        held_number,
        "fstat of the held file"
    );
    drop(listing);
    drop(held);

    // The lookup before a removal must leave the call's own paths intact,
    // even where a raw call keeps them below the red zone, inside the stack
    // scratch the lookup would use (the review's P2, and claude's P3-2).
    let mut stack = vec![0u8; 64 * 1024];
    let top = (stack.as_mut_ptr() as usize + stack.len()) & !15;
    let place = |at: usize, path: &Path| -> Vec<u8> {
        let bytes = CString::new(path.as_os_str().as_bytes()).unwrap();
        let bytes = bytes.as_bytes_with_nul().to_vec();
        assert!(
            at + bytes.len() <= top - 128,
            "{} is too long",
            path.display()
        );
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), at as *mut u8, bytes.len()) };
        bytes
    };
    let read_back = |at: usize, len: usize| unsafe {
        std::slice::from_raw_parts(at as *const u8, len).to_vec()
    };
    let victim = dir.join("v");
    File::create(&victim).unwrap();
    let victim_bytes = place(top - 200, &victim);
    let unlinked_raw = unsafe { syscall_on_stack(top as *mut u8, libc::SYS_unlink, top - 200, 0) };
    assert_eq!(unlinked_raw, 0, "unlink with its path in the stack scratch");
    assert!(!victim.exists(), "the raw unlink removed its file");
    assert_eq!(read_back(top - 200, victim_bytes.len()), victim_bytes);
    let (from, to) = (dir.join("f"), dir.join("t"));
    File::create(&from).unwrap();
    File::create(&to).unwrap();
    let from_bytes = place(top - 200, &from);
    let to_bytes = place(top - 260, &to);
    let renamed_raw =
        unsafe { syscall_on_stack(top as *mut u8, libc::SYS_rename, top - 200, top - 260) };
    assert_eq!(
        renamed_raw, 0,
        "rename with both paths in the stack scratch"
    );
    assert!(
        !from.exists() && to.exists(),
        "the raw rename moved its file"
    );
    assert_eq!(read_back(top - 200, from_bytes.len()), from_bytes);
    assert_eq!(read_back(top - 260, to_bytes.len()), to_bytes);
    drop(stack);

    // Straight to descriptor 1: `println!` in a test thread goes to the
    // harness's capture buffer, which the forked guest cannot hand back.
    let report = format!(
        "retired {number} {removed_directory} {replaced} {held_number}\n\
         kept {linked} {moved} {left_number} {right_number} {unnamed}\n"
    );
    std::io::Write::write_all(&mut std::io::stdout(), report.as_bytes()).unwrap();
    drop(reopened);
    drop(file);
}

fn numbers(stdout: &str, label: &str) -> BTreeSet<u64> {
    let line = stdout
        .lines()
        .find_map(|line| line.strip_prefix(label))
        .unwrap_or_else(|| panic!("no {label:?} line in {stdout:?}"));
    line.split_whitespace()
        .map(|number| number.parse().unwrap())
        .collect()
}

#[test]
fn removing_a_last_name_retires_its_number_and_nothing_else() {
    let root = tempfile::tempdir().unwrap();
    let config = Config {
        sequentialize_threads: true,
        max_timeslice: None,
        virtualize_metadata: true,
        ..Default::default()
    };
    let (output, state) = detcore_testutils::test_fn_with_config::<Detcore, _>(
        || removals_guest(&root),
        detcore_testutils::prepare_ptrace_timer_config(config),
        true,
    )
    .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status,
        ExitStatus::Exited(0),
        "guest failed: {stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let retired: BTreeSet<u64> = state
        .retired_inodes()
        .into_iter()
        .map(|number| number.as_raw())
        .collect();
    for number in numbers(&stdout, "retired ") {
        assert!(
            retired.contains(&number),
            "{number} lost its last name but is not retired: {retired:?}"
        );
    }
    for number in numbers(&stdout, "kept ") {
        assert!(
            !retired.contains(&number),
            "{number} still has a name but is retired: {retired:?}"
        );
    }
}

/// Without sequentialized threads another thread could link or rename a file
/// between the lookup before a removal and the removal itself, so nothing is
/// retired (codex review of https://github.com/rrnewton/hermit/pull/3849, P2).
#[test]
fn nothing_is_retired_without_sequentialized_threads() {
    let root = tempfile::tempdir().unwrap();
    let config = Config {
        sequentialize_threads: false,
        max_timeslice: None,
        virtualize_metadata: true,
        ..Default::default()
    };
    let (output, state) = detcore_testutils::test_fn_with_config::<Detcore, _>(
        || {
            let path = root.path().join("removed");
            File::create(&path).unwrap();
            ino_of_path(&path);
            std::fs::remove_file(&path).unwrap();
        },
        detcore_testutils::prepare_ptrace_timer_config(config),
        true,
    )
    .unwrap();
    assert_eq!(
        output.status,
        ExitStatus::Exited(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(state.retired_inodes(), Vec::new());
}

/// Set in the re-executed test process, which runs as root in a user and mount
/// namespace of its own and so may mount tmpfs.
const REUSE_INNER: &str = "HERMIT_INODE_REUSE_TEST_INNER";

/// `hermit_test_workdir::REQUEST_ENV`. With it set, each guest runs in a fresh
/// mount namespace of its own (on an empty `/test`), where the remounts this
/// test makes from its own thread would never appear; this test uses no
/// `/test`, so the re-executed process runs without it.
const ISOLATED_WORKDIR_ENV: &str = "HERMIT_E2E_EMPTY_WORKDIR";

/// Reuse cycles per run. The host reuses the device number only when nothing
/// else takes it between the unmount and the mount, so one cycle could miss.
const REUSE_CYCLES: usize = 4;

/// How the guest first reaches the file the host gave the freed inode, which
/// is also what discards the retired number.
#[derive(Clone, Copy, Debug)]
enum FirstReach {
    /// `open(O_CREAT)` discards it, and an `fstat` on that descriptor numbers
    /// the file.
    CreatingDescriptor,
    /// The directory listing that names the new directory.
    DirectoryListing,
    /// A stat of the new directory's path.
    PathStat,
    /// `fstatat(AT_FDCWD, "", AT_EMPTY_PATH)` after `chdir` into the new
    /// directory (claude review of https://github.com/rrnewton/hermit/pull/3849,
    /// P3-1).
    WorkingDirectory,
    /// The removed file was written and never numbered, and the new
    /// directory must not inherit its mtime: it must read the same as a
    /// directory made with no reuse (the same review, P2-2).
    WrittenThenRemoved,
}

/// Mount a fresh tmpfs at `mount_point`, replacing the one there, if any.
fn mount_fresh_tmpfs(mount_point: &Path, replace: bool) {
    let target = CString::new(mount_point.as_os_str().as_bytes()).unwrap();
    if replace {
        let rc = unsafe { libc::umount2(target.as_ptr(), 0) };
        assert_eq!(rc, 0, "umount: {}", std::io::Error::last_os_error());
    }
    let tmpfs = CString::new("tmpfs").unwrap();
    let rc = unsafe {
        libc::mount(
            tmpfs.as_ptr(),
            target.as_ptr(),
            tmpfs.as_ptr(),
            0,
            std::ptr::null(),
        )
    };
    assert_eq!(rc, 0, "mount tmpfs: {}", std::io::Error::last_os_error());
}

/// Ask the test process to replace the tmpfs, and wait until it has. The
/// guest opens both FIFOs itself: Detcore refuses I/O on a descriptor it never
/// saw opened.
fn remount(request: i32, done: i32) {
    assert_eq!(unsafe { libc::write(request, b"r".as_ptr().cast(), 1) }, 1);
    let mut byte = 0_u8;
    assert_eq!(
        unsafe { libc::read(done, (&raw mut byte).cast(), 1) },
        1,
        "remount acknowledgement"
    );
}

/// Each cycle numbers the first entry of a fresh tmpfs, removes its last name,
/// has the tmpfs replaced, and reaches the new tmpfs's first entry, which the
/// host gives the same device and inode number.
// Guest code: a guest's own directory reads are what this exercises.
#[allow(clippy::disallowed_methods)]
fn reuse_guest(mount_point: &Path, fifos: &Path, reach: FirstReach) {
    // The test process holds both FIFOs open for reading and writing, so
    // neither open waits for a peer.
    let request = File::options()
        .write(true)
        .open(fifos.join("request"))
        .unwrap();
    let done = File::open(fifos.join("done")).unwrap();
    let (request, done) = (request.as_raw_fd(), done.as_raw_fd());
    for cycle in 0..REUSE_CYCLES {
        remount(request, done);
        let old = mount_point.join(format!("old-{cycle}"));
        let new = mount_point.join(format!("new-{cycle}"));
        let old_number = match reach {
            FirstReach::CreatingDescriptor => {
                drop(File::create(&old).unwrap());
                let number = ino_of_path(&old);
                std::fs::remove_file(&old).unwrap();
                number
            }
            FirstReach::DirectoryListing | FirstReach::PathStat | FirstReach::WorkingDirectory => {
                std::fs::create_dir(&old).unwrap();
                let number = ino_of_path(&old);
                std::fs::remove_dir(&old).unwrap();
                number
            }
            FirstReach::WrittenThenRemoved => {
                std::io::Write::write_all(&mut File::create(&old).unwrap(), b"x").unwrap();
                std::fs::remove_file(&old).unwrap();
                remount(request, done);
                std::fs::create_dir(&new).unwrap();
                let control = mount_point.join(format!("control-{cycle}"));
                std::fs::create_dir(&control).unwrap();
                let mtime = |path: &Path| std::fs::metadata(path).unwrap().mtime_nsec();
                let seconds = |path: &Path| std::fs::metadata(path).unwrap().mtime();
                assert_eq!(
                    (seconds(&new), mtime(&new)),
                    (seconds(&control), mtime(&control)),
                    "{reach:?}, cycle {cycle}: the new directory took the removed file's mtime"
                );
                continue;
            }
        };
        remount(request, done);
        let new_number = match reach {
            FirstReach::CreatingDescriptor => {
                let file = File::create(&new).unwrap();
                ino_of_fd(file.as_raw_fd())
            }
            FirstReach::DirectoryListing => {
                std::fs::create_dir(&new).unwrap();
                std::fs::read_dir(mount_point)
                    .unwrap()
                    .map(Result::unwrap)
                    .find(|entry| entry.file_name() == new.file_name().unwrap())
                    .expect("the listing names the new directory")
                    .ino()
            }
            FirstReach::PathStat => {
                std::fs::create_dir(&new).unwrap();
                ino_of_path(&new)
            }
            FirstReach::WorkingDirectory => {
                std::fs::create_dir(&new).unwrap();
                let previous = std::env::current_dir().unwrap();
                std::env::set_current_dir(&new).unwrap();
                let number = ino_of_working_directory();
                // Out again, or the next remount would find the mount busy.
                std::env::set_current_dir(previous).unwrap();
                number
            }
            FirstReach::WrittenThenRemoved => unreachable!(),
        };
        assert_ne!(
            new_number, old_number,
            "{reach:?}, cycle {cycle}: the new file took the removed file's number"
        );
    }
}

/// Run [`reuse_guest`] under Detcore while this thread replaces the tmpfs on
/// request, and return how many retired numbers the run saw reused.
fn run_reuse_guest(reach: FirstReach) -> u64 {
    let root = tempfile::tempdir().unwrap();
    let mount_point = root.path().join("tmpfs");
    std::fs::create_dir(&mount_point).unwrap();
    mount_fresh_tmpfs(&mount_point, false);
    let fifos = root.path();
    let mut held = Vec::new();
    for name in ["request", "done"] {
        let path = CString::new(fifos.join(name).as_os_str().as_bytes()).unwrap();
        assert_eq!(
            unsafe { libc::mkfifo(path.as_ptr(), 0o600) },
            0,
            "mkfifo {name}"
        );
        held.push(
            File::options()
                .read(true)
                .write(true)
                .open(fifos.join(name))
                .unwrap(),
        );
    }
    let (request, done) = (held[0].as_raw_fd(), held[1].as_raw_fd());

    let config = Config {
        sequentialize_threads: true,
        max_timeslice: None,
        virtualize_metadata: true,
        ..Default::default()
    };
    let (output, state) = std::thread::scope(|scope| {
        let mount_point = &mount_point;
        scope.spawn(move || {
            let mut byte = 0_u8;
            while unsafe { libc::read(request, (&raw mut byte).cast(), 1) } == 1 && byte == b'r' {
                mount_fresh_tmpfs(mount_point, true);
                assert_eq!(unsafe { libc::write(done, b"d".as_ptr().cast(), 1) }, 1);
            }
        });
        let run = detcore_testutils::test_fn_with_config::<Detcore, _>(
            || reuse_guest(mount_point, fifos, reach),
            detcore_testutils::prepare_ptrace_timer_config(config),
            true,
        );
        // Ends the remount thread.
        assert_eq!(unsafe { libc::write(request, b"q".as_ptr().cast(), 1) }, 1);
        run.unwrap()
    });
    drop(held);
    assert_eq!(
        output.status,
        ExitStatus::Exited(0),
        "{reach:?}: guest failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let target = CString::new(mount_point.as_os_str().as_bytes()).unwrap();
    unsafe { libc::umount2(target.as_ptr(), 0) };
    state.reused_inode_count()
}

/// The host reuses an inode for real, and the new file must not take the
/// removed file's number, whichever way it is first reached. The test runs
/// itself again as root in a user and mount namespace of its own, which may
/// mount tmpfs on a host that grants no privilege.
#[test]
fn a_reused_inode_gets_a_fresh_number_however_it_is_first_reached() {
    const NAME: &str =
        "inode_reuse::a_reused_inode_gets_a_fresh_number_however_it_is_first_reached";
    if std::env::var_os(REUSE_INNER).is_none() {
        let output = Command::new("unshare")
            .args(["--user", "--map-root-user", "--mount", "--"])
            .arg(std::env::current_exe().unwrap())
            .args([NAME, "--exact", "--nocapture", "--test-threads=1"])
            .env(REUSE_INNER, "1")
            .env_remove(ISOLATED_WORKDIR_ENV)
            .output()
            .expect("run unshare");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "the re-executed test failed: {stdout}{stderr}"
        );
        assert!(
            stdout.contains("inode reuse: checked 5 ways"),
            "the re-executed test did not run: {stdout}{stderr}"
        );
        return;
    }
    assert!(
        std::env::var_os(ISOLATED_WORKDIR_ENV).is_none(),
        "the guest must share this process's mount namespace"
    );
    for reach in [
        FirstReach::CreatingDescriptor,
        FirstReach::DirectoryListing,
        FirstReach::PathStat,
        FirstReach::WorkingDirectory,
        FirstReach::WrittenThenRemoved,
    ] {
        let reused = run_reuse_guest(reach);
        assert!(
            reused > 0,
            "{reach:?}: the host reused no inode in {REUSE_CYCLES} cycles, so nothing was checked"
        );
    }
    writeln!(std::io::stdout(), "inode reuse: checked 5 ways").unwrap();
}
