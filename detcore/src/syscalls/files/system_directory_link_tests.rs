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

/// Linux's `readlink` of `path`, as the host answers it, settled: the
/// link's length or the errno, and how many attempts it took. An ELOOP is
/// asked again, up to 8 times, for this native reference only (Detcore is
/// asked once): Linux keeps a lookup's link count across the restart of its
/// RCU walk (`set_nameidata` once, then `path_lookupat` again; fs/namei.c
/// v6.12), which a mount anywhere on the host forces, so a native lookup
/// near `MAXSYMLINKS` can fail with ELOOP below the budget, though it never
/// succeeds past it.
fn native_readlink(path: &[u8]) -> (Result<i64, Errno>, usize) {
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
        if answer != Err(Errno::ELOOP) || attempts == 8 {
            return (answer, attempts);
        }
    }
}

/// [`native_readlink`] of `path`, which must be `expected`: a lookup
/// below the budget that is still ELOOP after 8 attempts fails the test as
/// "native unsettled", never taken for the answer.
fn assert_native(path: &[u8], expected: Result<i64, Errno>, label: &str) {
    let (answer, attempts) = native_readlink(path);
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
    for path in [
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
