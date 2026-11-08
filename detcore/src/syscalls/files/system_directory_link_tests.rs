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
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::Path;

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
/// any call Detcore forwards is `reply`.
fn kvm_thread(reply: Result<i64, Errno>) -> InjectGuest<NoopTool> {
    let mut guest = InjectGuest::<NoopTool>::new(reply);
    guest.config = Config {
        directory_placeholder: Some(std::path::PathBuf::from("/placeholder")),
        ..Config::default()
    }
    .with_backend(|backend| backend.provides_process_signal_control = true);
    let tid = guest.tid().as_raw();
    guest.thread_state_mut().detpid = Some(DetPid::from_raw(tid));
    guest
}

/// Detcore's answer to `readlink(path, buffer, 256)` and to
/// `readlinkat(AT_FDCWD, path, buffer, 256)` on a [`kvm_thread`] whose kernel
/// answers `reply`, with the calls each forwarded.
fn readlinks(path: &[u8], reply: Result<i64, Errno>) -> Vec<(Result<i64, Errno>, Vec<Sysno>)> {
    let path = CString::new(path).unwrap();
    let mut buffer = [0u8; 256];
    let tool = detcore::<NoopTool>();
    let mut answers = Vec::new();
    for at in [false, true] {
        let mut guest = kvm_thread(reply);
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
