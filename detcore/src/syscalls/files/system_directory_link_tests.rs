/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `readlink` and `readlinkat` of the system-wide procfs directories where
//! the guest's procfs is not the kernel's (KVM):
//! `Detcore::answer_system_directory_link`.

use std::ffi::CString;
use std::os::fd::AsRawFd;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::Path;

use nix::fcntl::OFlag;
use reverie::Errno;
use reverie::Guest;
use reverie::syscalls;
use reverie::syscalls::AddrMut;
use reverie::syscalls::PathPtr;
use reverie::syscalls::Sysno;

use crate::Config;
use crate::record_or_replay::NoopTool;
use crate::syscall_hot_path_tests::InjectGuest;
use crate::syscall_hot_path_tests::detcore;
use crate::types::DetPid;

/// A thread of a KVM-like run: the guest's procfs is not the kernel's
/// (`provides_process_signal_control`), Detcore serves `/proc` on a
/// placeholder, and the thread's process is known. The kernel's answer to
/// any call Detcore forwards is `reply`. With `proc_fd`, the thread also
/// holds that descriptor open on `/proc`, served on a placeholder.
fn kvm_thread(reply: Result<i64, Errno>, proc_fd: Option<RawFd>) -> InjectGuest<NoopTool> {
    let mut guest = InjectGuest::<NoopTool>::new(reply);
    guest.config = Config {
        directory_placeholder: Some(std::path::PathBuf::from("/placeholder")),
        ..Config::default()
    }
    .with_backend(|backend| backend.provides_process_signal_control = true);
    let tid = guest.tid().as_raw();
    let thread = guest.thread_state_mut();
    thread.detpid = Some(DetPid::from_raw(tid));
    if let Some(fd) = proc_fd {
        thread
            .add_fd(
                fd,
                OFlag::O_RDONLY | OFlag::O_DIRECTORY,
                crate::fd::FdType::Regular,
                None,
            )
            .unwrap();
        thread
            .with_detfd(fd, |detfd| {
                detfd.set_path(std::path::PathBuf::from("/proc"));
                detfd.set_placeholder(crate::fd::PlaceholderState {
                    directory: crate::procfs::fd_directory::PlaceholderDirectory::Root,
                    tag: None,
                    position: 0,
                    path_only: false,
                });
            })
            .unwrap();
    }
    guest
}

/// Detcore's answer to `readlink(path, buffer, 256)` and to
/// `readlinkat(AT_FDCWD, path, buffer, 256)` on a [`kvm_thread`] whose kernel
/// answers `reply`, with the calls each forwarded.
fn readlinks(path: &[u8], reply: Result<i64, Errno>) -> Vec<(Result<i64, Errno>, Vec<Sysno>)> {
    readlinks_holding(path, reply, None)
}

/// [`readlinks`] by a thread that also holds `proc_fd` open on `/proc`
/// ([`kvm_thread`]).
fn readlinks_holding(
    path: &[u8],
    reply: Result<i64, Errno>,
    proc_fd: Option<RawFd>,
) -> Vec<(Result<i64, Errno>, Vec<Sysno>)> {
    let path = CString::new(path).unwrap();
    let mut buffer = [0u8; 256];
    let tool = detcore::<NoopTool>();
    let mut answers = Vec::new();
    for at in [false, true] {
        let mut guest = kvm_thread(reply, proc_fd);
        let path_ptr = PathPtr::from_ptr(path.as_ptr());
        let buf = AddrMut::from_raw(buffer.as_mut_ptr() as usize);
        let result = futures::executor::block_on(async {
            if at {
                tool.handle_readlinkat(
                    &mut guest,
                    syscalls::Readlinkat::new()
                        .with_dirfd(libc::AT_FDCWD)
                        .with_path(path_ptr)
                        .with_buf(buf)
                        .with_buf_len(buffer.len()),
                )
                .await
            } else {
                tool.handle_readlink(
                    &mut guest,
                    syscalls::Readlink::new()
                        .with_path(path_ptr)
                        .with_buf(buf)
                        .with_bufsize(buffer.len()),
                )
                .await
            }
        });
        let forwarded = guest
            .injected()
            .into_iter()
            .map(|(sysno, _)| sysno)
            .collect();
        answers.push((
            result.map_err(|error| error.into_errno().unwrap()),
            forwarded,
        ));
    }
    answers
}

/// A system-wide procfs directory of the default level is no link: Detcore
/// answers both calls with EINVAL itself, as Linux does, and forwards
/// nothing to KVM's executor, which refuses every procfs object with
/// EACCES. So is a spelling with redundant separators and `.` components,
/// up to the longest pathname Linux takes (`PATH_MAX - 1` bytes).
#[test]
fn a_system_procfs_directory_is_no_link_on_kvm() {
    let longest = [b"/proc/sys".as_slice(), &b"/.".repeat(2043)].concat();
    assert_eq!(longest.len(), libc::PATH_MAX as usize - 1);
    for path in [
        b"/proc/sys".as_slice(),
        b"/proc/sys/",
        b"/proc//sys/./kernel",
        b"/proc/sys/kernel/random",
        &longest,
    ] {
        for (result, forwarded) in readlinks(path, Err(Errno::EACCES)) {
            assert_eq!(result, Err(Errno::EINVAL), "{}", path.len());
            assert!(forwarded.is_empty(), "{}: {forwarded:?}", path.len());
        }
    }
}

/// A pathname of `PATH_MAX` bytes or more fails with ENAMETOOLONG before
/// Linux looks any name up (`getname_flags`), so Detcore leaves it to the
/// kernel, even where it spells a system-wide procfs directory.
#[test]
fn an_overlong_system_directory_spelling_is_left_to_the_kernel() {
    let overlong = [b"/proc/sys".as_slice(), &b"/.".repeat(2044)].concat();
    assert!(overlong.len() >= libc::PATH_MAX as usize);
    for (result, forwarded) in readlinks(&overlong, Err(Errno::ENAMETOOLONG)) {
        assert_eq!(result, Err(Errno::ENAMETOOLONG));
        assert_eq!(forwarded.len(), 1, "{forwarded:?}");
    }
}

/// How many times [`native_readlink`] asks an ELOOP. The attempts run back
/// to back, so their number is the window a burst of host mounts must
/// outlast: 8 did not on a busy host, where up to 34 lookups in a row were
/// ELOOP (56 under induced mount churn); 10000 take under 0.1 s.
const SETTLE_ATTEMPTS: usize = 10000;

/// Linux's `readlink` of `path`, as the host answers it, settled: the
/// link's length or the errno, and how many attempts it took. With `settle`,
/// an ELOOP is asked again, up to [`SETTLE_ATTEMPTS`] times, for this
/// native reference only (Detcore is asked once): Linux keeps a lookup's
/// link count across the restart of its RCU walk (`set_nameidata` once,
/// then `path_lookupat` again; fs/namei.c v6.12), which a mount anywhere on
/// the host forces, so a native lookup near `MAXSYMLINKS` can fail with
/// ELOOP below the budget, though it never succeeds past it.
fn native_readlink(path: &[u8], settle: bool) -> (Result<i64, Errno>, usize) {
    let path = CString::new(path).unwrap();
    let mut buffer = [0u8; 256];
    let mut attempts = 0;
    loop {
        attempts += 1;
        // SAFETY: a NUL-terminated path and a buffer of the length given.
        let length =
            unsafe { libc::readlink(path.as_ptr(), buffer.as_mut_ptr().cast(), buffer.len()) };
        let answer = if length < 0 {
            Err(Errno::last())
        } else {
            Ok(length as i64)
        };
        if answer != Err(Errno::ELOOP) || !settle || attempts == SETTLE_ATTEMPTS {
            return (answer, attempts);
        }
    }
}

/// [`native_readlink`] of `path`, which must be `expected`: a lookup
/// below the budget that is still ELOOP after [`SETTLE_ATTEMPTS`] attempts
/// fails the test as "native unsettled", never taken for the answer. A
/// lookup whose `expected` answer is ELOOP is asked once: a restarted walk
/// can add an ELOOP, never take one away.
fn assert_native(path: &[u8], expected: Result<i64, Errno>, label: &str) {
    let (answer, attempts) = native_readlink(path, expected != Err(Errno::ELOOP));
    assert!(
        !(answer == Err(Errno::ELOOP) && expected != answer),
        "native unsettled: {label}, ELOOP on all {attempts} attempts"
    );
    assert_eq!(answer, expected, "the host, {label} ({attempts} attempts)");
}

/// `dir/chain0 -> chain1 -> ... -> chain38 -> /proc/sys`: 39 links.
fn make_chain(dir: &Path) {
    for index in 0..38 {
        std::os::unix::fs::symlink(
            format!("chain{}", index + 1),
            dir.join(format!("chain{index}")),
        )
        .unwrap();
    }
    std::os::unix::fs::symlink("/proc/sys", dir.join("chain38")).unwrap();
}

/// One `MAXSYMLINKS` budget (40) covers the whole lookup: the links
/// Detcore resolves itself through the caller's descriptor `R` on `/proc`
/// (`/proc/self` and `/proc/self/fd/R`, 2), and the links of the rest of the
/// walk, which Detcore walks and counts itself, here a chain of symbolic
/// links whose last names `/proc/sys`, spelled with a final `/` so that the
/// lookup follows the last link too. 39 and 40 links reach `/proc/sys`, no
/// link, so EINVAL; 41 is ELOOP (`reserve_stack`, fs/namei.c). So too
/// through a pathname of `PATH_MAX - 1` bytes, to which Detcore's walk adds
/// nothing. The host answers each spelling first, with `R` a real
/// descriptor on `/proc`, so that native Linux decides every answer.
#[test]
fn one_link_budget_covers_descriptor_links_and_the_rest() {
    let native_proc = std::fs::File::open("/proc").unwrap();
    // The same number for the host's descriptor and the guest's placeholder,
    // so that the spellings are the same length.
    let r = native_proc.as_raw_fd();
    let fixture = tempfile::tempdir().unwrap();
    let fixture = crate::util::canonicalize(fixture.path()).unwrap();
    make_chain(&fixture);
    let through = format!("/proc/self/fd/{r}/..");
    // A directory below the fixture that makes `through` + it + "/chain0/"
    // PATH_MAX - 1 bytes long.
    let mut deep = fixture.clone();
    let room =
        libc::PATH_MAX as usize - 1 - through.len() - fixture.as_os_str().len() - "/chain0/".len();
    // Names of at most 255 bytes (NAME_MAX), each with its "/".
    let count = room.div_ceil(256);
    let letters = room - count;
    for index in 0..count {
        deep.push("d".repeat(letters / count + usize::from(index < letters % count)));
        std::fs::create_dir(&deep).unwrap();
    }
    make_chain(&deep);
    let spelled = |prefix: &str, dir: &Path, first: usize| {
        let mut path = prefix.as_bytes().to_vec();
        path.extend_from_slice(dir.as_os_str().as_bytes());
        path.extend_from_slice(format!("/chain{first}/").as_bytes());
        path
    };
    for (label, path) in [
        ("39 links", spelled("", &fixture, 0)),
        ("39 links through R", spelled(&through, &fixture, 2)),
        ("40 links through R", spelled(&through, &fixture, 1)),
        ("41 links through R", spelled(&through, &fixture, 0)),
        ("39 links through R, deep", spelled(&through, &deep, 2)),
        ("40 links through R, deep", spelled(&through, &deep, 1)),
        ("41 links through R, deep", spelled(&through, &deep, 0)),
    ] {
        if label.ends_with("deep") {
            assert_eq!(path.len(), libc::PATH_MAX as usize - 1, "{label}");
        }
        let expected = if label.starts_with("41") {
            Err(Errno::ELOOP)
        } else {
            Err(Errno::EINVAL)
        };
        assert_native(&path, expected, label);
        for (result, forwarded) in readlinks_holding(&path, Err(Errno::EACCES), Some(r)) {
            assert_eq!(result, expected, "{label}");
            assert!(forwarded.is_empty(), "{label}: {forwarded:?}");
        }
    }
}

/// Detcore's walk takes each step as Linux does, by the object reached, not
/// by the spelling: a fixture's own `proc/self/cwd`, a symbolic link to
/// `/proc/sys`, reaches `/proc/sys/kernel`; `..` after a symbolic link is
/// the parent of the directory the link reaches; and procfs's `self` and
/// `thread-self`, recognised as links in procfs's root, are left again by
/// `..`. Each is EINVAL natively, and so on KVM.
#[test]
fn the_walk_takes_each_step_by_the_object_reached() {
    let fixture = tempfile::tempdir().unwrap();
    let fixture = crate::util::canonicalize(fixture.path()).unwrap();
    std::fs::create_dir_all(fixture.join("proc/self")).unwrap();
    std::os::unix::fs::symlink("/proc/sys", fixture.join("proc/self/cwd")).unwrap();
    std::os::unix::fs::symlink("/proc/sys/kernel/random", fixture.join("to-random")).unwrap();
    // A sticky world-writable directory whose owner owns the link: Linux's
    // `fs.protected_symlinks` lets anyone follow it (`may_follow_link`).
    std::fs::create_dir(fixture.join("sticky")).unwrap();
    std::fs::set_permissions(
        fixture.join("sticky"),
        std::os::unix::fs::PermissionsExt::from_mode(0o1777),
    )
    .unwrap();
    std::os::unix::fs::symlink("/proc/sys", fixture.join("sticky/to-sys")).unwrap();
    for path in [
        fixture.join("sticky/to-sys/"),
        fixture.join("proc/self/cwd/kernel"),
        fixture.join("to-random/../random"),
        Path::new("/proc/self/../sys").to_path_buf(),
        Path::new("/proc/thread-self/../../../sys").to_path_buf(),
    ] {
        let path = path.as_os_str().as_bytes();
        assert_native(path, Err(Errno::EINVAL), &String::from_utf8_lossy(path));
        for (result, forwarded) in readlinks(path, Err(Errno::EACCES)) {
            assert_eq!(
                result,
                Err(Errno::EINVAL),
                "{}",
                String::from_utf8_lossy(path)
            );
            assert!(forwarded.is_empty(), "{forwarded:?}");
        }
    }
}

/// Set in the child that [`a_nosymfollow_mount_refuses_to_follow_its_links`]
/// runs in its own user and mount namespaces.
const NOSYMFOLLOW_CHILD: &str = "DETCORE_SYSTEM_DIRECTORY_NOSYMFOLLOW_CHILD";

/// A mount with `nosymfollow` holding `to-sys -> /proc/sys`: Linux reads the
/// link itself (9 bytes, from the kernel) but refuses to follow it, ELOOP,
/// whether a name follows it or a final `/` or `/.` does (`pick_link`,
/// `MNT_NOSYMFOLLOW`, fs/namei.c v6.12), and Detcore's walk answers ELOOP
/// as well. The test runs again in a child of its own, in new user and
/// mount namespaces, with the fixture bound over itself and remounted
/// `nosymfollow`.
#[test]
fn a_nosymfollow_mount_refuses_to_follow_its_links() {
    if let Some(mount) = std::env::var_os(NOSYMFOLLOW_CHILD) {
        let mount = std::path::PathBuf::from(mount);
        let spell = |rest: &str| {
            let mut path = mount.as_os_str().as_bytes().to_vec();
            path.extend_from_slice(rest.as_bytes());
            path
        };
        assert_native(&spell("/to-sys"), Ok(9), "to-sys");
        for (result, forwarded) in readlinks(&spell("/to-sys"), Ok(9)) {
            assert_eq!(result, Ok(9), "to-sys");
            assert_eq!(forwarded.len(), 1, "{forwarded:?}");
        }
        for rest in ["/to-sys/kernel", "/to-sys/", "/to-sys/."] {
            assert_native(&spell(rest), Err(Errno::ELOOP), rest);
            for (result, forwarded) in readlinks(&spell(rest), Err(Errno::EACCES)) {
                assert_eq!(result, Err(Errno::ELOOP), "{rest}");
                assert!(forwarded.is_empty(), "{rest}: {forwarded:?}");
            }
        }
        return;
    }
    let fixture = tempfile::tempdir().unwrap();
    let mount = crate::util::canonicalize(fixture.path()).unwrap();
    std::os::unix::fs::symlink("/proc/sys", mount.join("to-sys")).unwrap();
    let target = CString::new(mount.as_os_str().as_bytes()).unwrap();
    // The flags of the mount the fixture is on that a user namespace may not
    // drop (`MNT_LOCK_*`), kept by the remount.
    let mut vfs = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: a NUL-terminated path and a writable structure.
    assert_eq!(
        unsafe { libc::statvfs(target.as_ptr(), vfs.as_mut_ptr()) },
        0
    );
    // SAFETY: the call succeeded and initialized the structure.
    let kept = unsafe { vfs.assume_init() }.f_flag;
    let locked = [
        (libc::ST_RDONLY, libc::MS_RDONLY),
        (libc::ST_NOSUID, libc::MS_NOSUID),
        (libc::ST_NODEV, libc::MS_NODEV),
        (libc::ST_NOEXEC, libc::MS_NOEXEC),
        (libc::ST_NOATIME, libc::MS_NOATIME),
        (libc::ST_NODIRATIME, libc::MS_NODIRATIME),
        (libc::ST_RELATIME, libc::MS_RELATIME),
    ]
    .into_iter()
    .filter(|(st, _)| kept & st != 0)
    .fold(0, |flags, (_, ms)| flags | ms);
    let name = module_path!()
        .split_once("::")
        .map_or(module_path!(), |(_, rest)| rest)
        .to_owned()
        + "::a_nosymfollow_mount_refuses_to_follow_its_links";
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child
        .args([name.as_str(), "--exact", "--nocapture", "--test-threads=1"])
        .env(NOSYMFOLLOW_CHILD, &mount);
    // SAFETY: the hook makes only raw system calls on memory prepared before
    // the fork.
    unsafe {
        child.pre_exec(move || {
            let check = |result: libc::c_int| {
                if result == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            };
            check(libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS))?;
            check(libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            ))?;
            check(libc::mount(
                target.as_ptr(),
                target.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND,
                std::ptr::null(),
            ))?;
            check(libc::mount(
                std::ptr::null(),
                target.as_ptr(),
                std::ptr::null(),
                libc::MS_REMOUNT | libc::MS_BIND | libc::MS_NOSYMFOLLOW | locked,
                std::ptr::null(),
            ))
        });
    }
    let output = child
        .output()
        .expect("the child in its own namespaces must start");
    assert!(
        output.status.success(),
        "the child failed: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "the child ran the test: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

/// Which stage of [`a_protected_symlink_with_projected_owners_is_the_kernels`]
/// a child runs: "outer" builds the fixture, "inner" checks it.
const PROJECTED_OWNERS_STAGE: &str = "DETCORE_PROJECTED_OWNERS_STAGE";

/// The fixture directory of [`a_protected_symlink_with_projected_owners_is_the_kernels`].
const PROJECTED_OWNERS_FIXTURE: &str = "DETCORE_PROJECTED_OWNERS_FIXTURE";

/// This test's own name, for a child that runs only it.
fn projected_owners_test_name() -> String {
    module_path!()
        .split_once("::")
        .map_or(module_path!(), |(_, rest)| rest)
        .to_owned()
        + "::a_protected_symlink_with_projected_owners_is_the_kernels"
}

/// A command that runs this test again as `stage`.
fn projected_owners_stage(stage: &str, fixture: &Path) -> std::process::Command {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child
        .args([
            projected_owners_test_name().as_str(),
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(PROJECTED_OWNERS_STAGE, stage)
        .env(PROJECTED_OWNERS_FIXTURE, fixture);
    child
}

/// `child`'s output, which must be a pass of this one test.
fn assert_stage_passed(output: std::process::Output, stage: &str) {
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "the {stage} stage failed: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A link under `fs.protected_symlinks` in a sticky world-writable directory
/// whose owner (uid 1) does not own the link (uid 2), both outside the map
/// of the user namespace that looks at them, where each reads as the
/// overflow uid: Linux compares the owners as it maps them and refuses to
/// follow `x/` (EACCES, `may_follow_link`). Detcore decides from no owner
/// the tool reads, which can be such projections: it leaves every trailing
/// link in a sticky world-writable directory to the kernel's lookup, so
/// `x/` is the kernel's EACCES; `x/.`, which follows `x` before its last
/// name and is no trailing link, is EINVAL; and `x` reads its 9 bytes.
///
/// The outer stage, in a user namespace that maps uids 0 to 2 (as root, or
/// with `newuidmap` and the caller's subordinate range) and a mount
/// namespace of its own, builds the fixture on a tmpfs; the inner stage,
/// in a nested user namespace without a map, checks it.
#[test]
fn a_protected_symlink_with_projected_owners_is_the_kernels() {
    let stage = std::env::var(PROJECTED_OWNERS_STAGE).ok();
    if stage.as_deref() == Some("inner") {
        let fixture = std::path::PathBuf::from(std::env::var_os(PROJECTED_OWNERS_FIXTURE).unwrap());
        let sticky = fixture.join("sticky");
        let link = sticky.join("x");
        let link_stat = std::fs::symlink_metadata(&link).unwrap();
        let sticky_stat = std::fs::symlink_metadata(&sticky).unwrap();
        assert_eq!(
            (
                std::os::unix::fs::MetadataExt::uid(&sticky_stat),
                std::os::unix::fs::MetadataExt::uid(&link_stat)
            ),
            (65534, 65534),
            "both owners read as the overflow uid"
        );
        let spell = |rest: &str| {
            let mut path = link.as_os_str().as_bytes().to_vec();
            path.extend_from_slice(rest.as_bytes());
            path
        };
        assert_native(&spell(""), Ok(9), "x");
        for (result, forwarded) in readlinks(&spell(""), Ok(9)) {
            assert_eq!(result, Ok(9), "x");
            assert_eq!(forwarded.len(), 1, "{forwarded:?}");
        }
        assert_native(&spell("/"), Err(Errno::EACCES), "x/");
        for (result, forwarded) in readlinks(&spell("/"), Err(Errno::EACCES)) {
            assert_eq!(result, Err(Errno::EACCES), "x/");
            assert_eq!(forwarded.len(), 1, "x/ is the kernel's: {forwarded:?}");
        }
        assert_native(&spell("/."), Err(Errno::EINVAL), "x/.");
        for (result, forwarded) in readlinks(&spell("/."), Err(Errno::EACCES)) {
            assert_eq!(result, Err(Errno::EINVAL), "x/.");
            assert!(forwarded.is_empty(), "x/.: {forwarded:?}");
        }
        return;
    }
    if stage.as_deref() == Some("mapping") {
        // Wait for the map, then run again: an exec as uid 0 of the user
        // namespace gives the outer stage its capabilities there.
        let fixture = std::path::PathBuf::from(std::env::var_os(PROJECTED_OWNERS_FIXTURE).unwrap());
        let mut go = String::new();
        std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut go).unwrap();
        let error = projected_owners_stage("outer", &fixture).exec();
        panic!("exec of the outer stage: {error}");
    }
    if stage.as_deref() == Some("outer") {
        let fixture = std::path::PathBuf::from(std::env::var_os(PROJECTED_OWNERS_FIXTURE).unwrap());
        let fixture_c = CString::new(fixture.as_os_str().as_bytes()).unwrap();
        // SAFETY: NUL-terminated strings and null data.
        unsafe {
            assert_eq!(
                libc::mount(
                    std::ptr::null(),
                    c"/".as_ptr(),
                    std::ptr::null(),
                    libc::MS_REC | libc::MS_PRIVATE,
                    std::ptr::null(),
                ),
                0,
                "{}",
                std::io::Error::last_os_error()
            );
            assert_eq!(
                libc::mount(
                    c"tmpfs".as_ptr(),
                    fixture_c.as_ptr(),
                    c"tmpfs".as_ptr(),
                    0,
                    c"mode=0755".as_ptr().cast(),
                ),
                0,
                "{}",
                std::io::Error::last_os_error()
            );
        }
        let sticky = fixture.join("sticky");
        std::fs::create_dir(&sticky).unwrap();
        std::os::unix::fs::chown(&sticky, Some(1), Some(0)).unwrap();
        std::fs::set_permissions(
            &sticky,
            std::os::unix::fs::PermissionsExt::from_mode(0o1777),
        )
        .unwrap();
        std::os::unix::fs::symlink("/proc/sys", sticky.join("x")).unwrap();
        std::os::unix::fs::lchown(sticky.join("x"), Some(2), Some(0)).unwrap();
        let mut inner = projected_owners_stage("inner", &fixture);
        // SAFETY: the hook makes one raw system call.
        unsafe {
            inner.pre_exec(|| {
                if libc::unshare(libc::CLONE_NEWUSER) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
        assert_stage_passed(inner.output().unwrap(), "inner");
        return;
    }
    let mut enabled = String::new();
    std::io::Read::read_to_string(
        &mut std::fs::File::open("/proc/sys/fs/protected_symlinks").unwrap(),
        &mut enabled,
    )
    .unwrap();
    assert_eq!(enabled.trim(), "1", "the test needs fs.protected_symlinks");
    let fixture = tempfile::tempdir().unwrap();
    let fixture = crate::util::canonicalize(fixture.path()).unwrap();
    // SAFETY: no arguments.
    let root = unsafe { libc::geteuid() } == 0;
    let mut outer = projected_owners_stage("mapping", &fixture);
    outer
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let flags = if root {
        libc::CLONE_NEWNS
    } else {
        libc::CLONE_NEWUSER | libc::CLONE_NEWNS
    };
    // SAFETY: the hook makes one raw system call.
    unsafe {
        outer.pre_exec(move || {
            if libc::unshare(flags) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    let mut child = outer.spawn().unwrap();
    if !root {
        // Map uids and gids 0 to 2: 0 to the caller, 1 and 2 to the first of
        // its subordinate range, as newuidmap allows an unprivileged user.
        // SAFETY: no arguments.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let user = std::env::var("USER").unwrap_or_default();
        let first = |file: &str| {
            std::fs::read_to_string(file)
                .unwrap_or_default()
                .lines()
                .filter_map(|line| {
                    let mut fields = line.split(':');
                    let name = fields.next()?;
                    let start = fields.next()?;
                    (name == user || name == uid.to_string()).then(|| start.to_owned())
                })
                .next()
                .unwrap_or_else(|| panic!("no subordinate range for {user} in {file}"))
        };
        let pid = child.id().to_string();
        for (helper, own, file) in [
            ("newuidmap", uid, "/etc/subuid"),
            ("newgidmap", gid, "/etc/subgid"),
        ] {
            let status = std::process::Command::new(helper)
                .args([&pid, "0", &own.to_string(), "1", "1", &first(file), "2"])
                .status()
                .unwrap_or_else(|error| panic!("{helper}: {error}"));
            assert!(status.success(), "{helper} failed: {status:?}");
        }
    }
    std::io::Write::write_all(child.stdin.as_mut().unwrap(), b"go\n").unwrap();
    assert_stage_passed(child.wait_with_output().unwrap(), "outer");
}

/// An errno other than ENOENT from a security-module probe (here EMFILE,
/// as from a full descriptor table) says nothing about the modules: the
/// lookup that met it leaves the follow to the kernel, and nothing is kept,
/// so that a later probe that finds SELinux active is honored.
#[test]
fn a_failed_security_module_probe_is_not_kept() {
    use super::rendered_procfs::security_module_answer;
    use super::rendered_procfs::security_modules_allow_the_walk;
    let cache = std::sync::OnceLock::new();
    let full_table = |path: &[u8]| {
        if path == b"/proc/self/attr" {
            Ok(())
        } else {
            Err(Errno::EMFILE)
        }
    };
    let answer = security_module_answer(&cache, &full_table);
    assert_eq!(answer, None);
    assert!(
        !security_modules_allow_the_walk(answer),
        "the kernel decides"
    );
    assert_eq!(cache.get(), None, "nothing is kept");
    let selinux = |path: &[u8]| {
        if path == b"/proc/self/attr" || path == b"/proc/self/attr/selinux" {
            Ok(())
        } else {
            Err(Errno::ENOENT)
        }
    };
    assert_eq!(security_module_answer(&cache, &selinux), Some(true));
    assert_eq!(cache.get(), Some(&true));
    assert!(!security_modules_allow_the_walk(Some(true)));
    // An unreadable /proc/self/attr is as unknown, and none at all (ENOENT
    // for every module) is a definite answer that is kept.
    let unknown = std::sync::OnceLock::new();
    assert_eq!(
        security_module_answer(&unknown, &|_| Err(Errno::EACCES)),
        None
    );
    assert_eq!(unknown.get(), None);
    let none = std::sync::OnceLock::new();
    let absent = |path: &[u8]| {
        if path == b"/proc/self/attr" {
            Ok(())
        } else {
            Err(Errno::ENOENT)
        }
    };
    assert_eq!(security_module_answer(&none, &absent), Some(false));
    assert_eq!(none.get(), Some(&false));
    assert!(security_modules_allow_the_walk(Some(false)));
}

/// Set in the child that [`a_bind_over_proc_sys_keeps_its_links`] runs in
/// its own user and mount namespaces.
const BIND_CHILD: &str = "DETCORE_SYSTEM_DIRECTORY_BIND_CHILD";

/// A user mount over `/proc/sys` holding `kernel -> /dev/null`: Detcore
/// decides by the object the lookup reaches, not by the spelling, so both
/// calls on `/proc/sys/kernel` reach the kernel, which reads the link's 9
/// bytes, and so does `/proc/sys`, now the fixture's own directory. The test
/// runs again in a child of its own with the fixture bound over `/proc/sys`
/// in new user and mount namespaces, which the child's lookups and Detcore's
/// share, as they share the guest's on KVM.
#[test]
fn a_bind_over_proc_sys_keeps_its_links() {
    if std::env::var_os(BIND_CHILD).is_some() {
        assert_eq!(
            std::fs::read_link("/proc/sys/kernel").unwrap(),
            Path::new("/dev/null"),
            "the fixture is bound over /proc/sys"
        );
        for path in [b"/proc/sys/kernel".as_slice(), b"/proc/sys"] {
            for (result, forwarded) in readlinks(path, Ok(9)) {
                assert_eq!(result, Ok(9), "{}", String::from_utf8_lossy(path));
                assert_eq!(forwarded.len(), 1, "{forwarded:?}");
            }
        }
        return;
    }
    let fixture = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink("/dev/null", fixture.path().join("kernel")).unwrap();
    let source = CString::new(fixture.path().as_os_str().as_bytes()).unwrap();
    let name = module_path!()
        .split_once("::")
        .map_or(module_path!(), |(_, rest)| rest)
        .to_owned()
        + "::a_bind_over_proc_sys_keeps_its_links";
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child
        .args([name.as_str(), "--exact", "--nocapture", "--test-threads=1"])
        .env(BIND_CHILD, "1");
    // SAFETY: the hook makes only raw system calls on memory prepared before
    // the fork.
    unsafe {
        child.pre_exec(move || {
            let check = |result: libc::c_int| {
                if result == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            };
            check(libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS))?;
            check(libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            ))?;
            check(libc::mount(
                source.as_ptr(),
                c"/proc/sys".as_ptr(),
                std::ptr::null(),
                libc::MS_BIND,
                std::ptr::null(),
            ))
        });
    }
    let output = child
        .output()
        .expect("the child in its own namespaces must start");
    assert!(
        output.status.success(),
        "the child failed: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "the child ran the test: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}
