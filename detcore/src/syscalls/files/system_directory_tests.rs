/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The system-wide procfs directories Detcore serves on placeholders where
//! the guest's procfs is not the kernel's (KVM): which directory a lookup
//! reaches (`Detcore::system_directory_reached`) and what a listing of it
//! holds (`Detcore::system_placeholder_snapshot`), at both `--proc` levels.
//! The tool's own lookups run on the test host's procfs, as Detcore's run in
//! the guest's mount namespace.

use std::path::Path;

use detcore_model::config::ProcLevel;
use reverie::Errno;
use reverie::Guest;
use reverie::Pid;
use reverie::Tool;

use crate::Config;
use crate::dirents::DirectoryOrder;
use crate::procfs::fd_directory::SystemDirectory;
use crate::procfs_inode;
use crate::record_or_replay::NoopTool;
use crate::syscall_hot_path_tests::InjectGuest;
use crate::tool_local::Detcore;
use crate::types::DetPid;

/// A thread of a KVM-like run: the guest's procfs is not the kernel's
/// (`provides_process_signal_control`), Detcore serves `/proc` on a
/// placeholder, and the thread's process is known.
fn kvm_thread() -> InjectGuest<NoopTool> {
    let mut guest = InjectGuest::<NoopTool>::new(Err(Errno::EACCES));
    guest.config = Config {
        directory_placeholder: Some(std::path::PathBuf::from("/placeholder")),
        ..Config::default()
    }
    .with_backend(|backend| backend.provides_process_signal_control = true);
    let tid = guest.tid().as_raw();
    guest.thread_state_mut().detpid = Some(DetPid::from_raw(tid));
    guest
}

/// A Detcore tool of a run at `level`.
fn detcore_at(level: ProcLevel) -> Detcore<NoopTool> {
    <Detcore<NoopTool> as Tool>::new(
        Pid::from_raw(4242),
        &Config {
            proc_level: level,
            ..Config::default()
        },
    )
}

/// The directory `system_directory_reached` decides `path` reaches at
/// `level`, as its path within `/proc`.
fn reached(level: ProcLevel, path: &[u8], follow: bool) -> Option<String> {
    let path = Path::new(std::ffi::OsStr::new(std::str::from_utf8(path).unwrap()));
    detcore_at(level)
        .system_directory_reached(&kvm_thread(), path, path, 0, follow)
        .expect("no lookup fails here")
        .map(|directory| directory.canonical().display().to_string())
}

/// Each system-wide directory of a level is reached by any spelling of it,
/// `..` included, which the kernel's lookup resolves, up to `/proc` itself
/// from one (procfs's root, as `/proc` is served); through a symbolic
/// link to it where the call follows the link, and not where it does not;
/// and not at a level that omits it, nor for a process's directory, a
/// file, a name the host's procfs lacks, or a path outside
/// `/proc`. A pathname of `PATH_MAX` bytes or more is the kernel's
/// ENAMETOOLONG, before any lookup; one byte shorter is looked up.
#[test]
fn a_system_directory_is_the_object_a_lookup_reaches() {
    use ProcLevel::*;
    for (path, minimal, standard) in [
        ("/proc/sys", Some("/proc/sys"), Some("/proc/sys")),
        (
            "/proc//sys/./kernel/",
            Some("/proc/sys/kernel"),
            Some("/proc/sys/kernel"),
        ),
        (
            "/proc/sys/kernel/../vm",
            Some("/proc/sys/vm"),
            Some("/proc/sys/vm"),
        ),
        (
            "/proc/sys/kernel/random",
            Some("/proc/sys/kernel/random"),
            Some("/proc/sys/kernel/random"),
        ),
        ("/proc/sys/fs", None, Some("/proc/sys/fs")),
        ("/proc/sys/net/ipv4", None, Some("/proc/sys/net/ipv4")),
        ("/proc/tty", None, Some("/proc/tty")),
        ("/proc", Some("/proc"), Some("/proc")),
        ("/proc/sys/..", Some("/proc"), Some("/proc")),
        ("/proc/tty/..", Some("/proc"), Some("/proc")),
        ("/proc/self", None, None),
        ("/proc/sys/kernel/random/uuid", None, None),
        ("/proc/sys/no-such-directory", None, None),
        ("/tmp", None, None),
    ] {
        assert_eq!(
            reached(Minimal, path.as_bytes(), true).as_deref(),
            minimal,
            "{path}"
        );
        assert_eq!(
            reached(Standard, path.as_bytes(), true).as_deref(),
            standard,
            "{path}"
        );
    }
    let links = tempfile::tempdir().unwrap();
    let link = links.path().join("link");
    std::os::unix::fs::symlink("/proc/sys/kernel", &link).unwrap();
    let link = link.to_str().unwrap().as_bytes().to_vec();
    assert_eq!(
        reached(Minimal, &link, true).as_deref(),
        Some("/proc/sys/kernel")
    );
    assert_eq!(reached(Minimal, &link, false), None);
    let below = [link.as_slice(), b"/random"].concat();
    assert_eq!(
        reached(Minimal, &below, false).as_deref(),
        Some("/proc/sys/kernel/random")
    );
    let longest = [b"/proc/sys".as_slice(), &b"/.".repeat(2043)].concat();
    assert_eq!(longest.len(), libc::PATH_MAX as usize - 1);
    assert_eq!(
        reached(Minimal, &longest, true).as_deref(),
        Some("/proc/sys")
    );
    let overlong = [longest.as_slice(), b"/"].concat();
    assert_eq!(reached(Minimal, &overlong, true), None);
}

/// A listing of a system-wide directory holds the level's names from the
/// table, each with its type, as ptrace's listing of the kernel's directory
/// does (`Detcore::modeled_directory_snapshot`), less a name the host's
/// procfs lacks; each entry is numbered by its path's key on KVM's procfs
/// device, in name order.
#[test]
fn a_system_directory_lists_the_levels_names() {
    let names = |level: ProcLevel, path: &str| {
        let directory = SystemDirectory::named(Path::new(path), level).expect("a system directory");
        let (entries, _, order) = detcore_at(level)
            .system_placeholder_snapshot(&kvm_thread(), &directory)
            .unwrap();
        assert!(matches!(order, DirectoryOrder::NameBytes));
        let within = directory.within();
        entries
            .into_iter()
            .map(|entry| {
                let name = String::from_utf8(entry.name).unwrap();
                let path = match name.as_str() {
                    "." => within.clone(),
                    ".." => within.parent().unwrap().to_path_buf(),
                    name => within.join(name),
                };
                assert_eq!(
                    entry.ino,
                    procfs_inode::path_inode(procfs_inode::KVM_SYNTHETIC_PROC_DEVICE, &path).ino,
                    "{name}"
                );
                format!("{name}/{}", entry.ty)
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names(ProcLevel::Minimal, "/proc/sys"),
        ["./4", "../4", "kernel/4", "vm/4"]
    );
    assert_eq!(
        names(ProcLevel::Standard, "/proc/sys"),
        ["./4", "../4", "kernel/4", "vm/4", "fs/4", "net/4"]
    );
    assert_eq!(
        names(ProcLevel::Minimal, "/proc/sys/kernel/random"),
        ["./4", "../4", "boot_id/8", "uuid/8"]
    );
    assert_eq!(
        names(ProcLevel::Standard, "/proc/sys/kernel/random"),
        ["./4", "../4", "boot_id/8", "uuid/8", "entropy_avail/8"]
    );
    assert_eq!(
        names(ProcLevel::Standard, "/proc/sys/net/ipv4"),
        ["./4", "../4", "ip_local_port_range/8"]
    );
    let kernel = names(ProcLevel::Minimal, "/proc/sys/kernel");
    assert_eq!(kernel.len(), 12, "{kernel:?}");
    assert!(kernel.contains(&"random/4".to_string()), "{kernel:?}");
    assert!(!kernel.contains(&"pty/4".to_string()), "{kernel:?}");
    assert!(names(ProcLevel::Standard, "/proc/sys/kernel").contains(&"pty/4".to_string()));
}
