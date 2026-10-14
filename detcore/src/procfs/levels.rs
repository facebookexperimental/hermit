/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Which system-wide `/proc` names exist at each `--proc` level.
//!
//! A system-wide or `/proc/sys` name exists only if this table lists it, or
//! lists a name below it; every other name is ENOENT and missing from
//! listings, on every backend. Per-process names (`self`, `thread-self`,
//! `<pid>` and everything below them, and the `net` link's targets) are not
//! decided here yet: the kernel, or the KVM executor, still answers them.
//! The table is the checklist in the dev-hermit design note
//! `ai_docs/proc-virtualization-design.md` (section 5): its V and F
//! system-wide entries, split into the two levels of its section 1b.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Component;
use std::path::Path;

use detcore_model::config::ProcLevel;

/// The names at the `minimal` level, relative to `/proc`. Each directory on
/// the way to a name exists too.
const MINIMAL: &[&str] = &[
    "cmdline",
    "cpuinfo",
    "filesystems",
    "loadavg",
    "meminfo",
    "modules",
    "mounts",
    "net",
    "stat",
    "sys/kernel/cap_last_cap",
    "sys/kernel/domainname",
    "sys/kernel/hostname",
    "sys/kernel/osrelease",
    "sys/kernel/ostype",
    "sys/kernel/overflowgid",
    "sys/kernel/overflowuid",
    "sys/kernel/pid_max",
    "sys/kernel/random/boot_id",
    "sys/kernel/random/uuid",
    "sys/kernel/version",
    "sys/vm/max_map_count",
    "sys/vm/mmap_min_addr",
    "sys/vm/overcommit_memory",
    "uptime",
    "version",
];

/// The names `standard` adds. `sys/net/core/somaxconn` and
/// `sys/net/ipv4/ip_local_port_range` are still the kernel's: they are per
/// network namespace, and Detcore's port allocator reads the host's range,
/// so the two change together (design note, slice B2a).
///
/// `sys/crypto/fips_enabled`, planned for `standard`, is not here yet: its
/// directory `sys/crypto` exists only on a kernel built with it (not on
/// every development host's), and a file in a directory the guest cannot
/// see or list is not one view. It comes with Detcore's own directory nodes (slice B3a's
/// placeholder directories). Until then it is ENOENT on every host, which
/// is what a host without the directory answers.
const STANDARD: &[&str] = &[
    "buddyinfo",
    "cgroups",
    "diskstats",
    "driver/rtc",
    "interrupts",
    "key-users",
    "locks",
    "pressure/cpu",
    "pressure/io",
    "pressure/memory",
    "schedstat",
    "softirqs",
    "swaps",
    "sys/fs/aio-max-nr",
    "sys/fs/aio-nr",
    "sys/fs/dentry-state",
    "sys/fs/file-max",
    "sys/fs/file-nr",
    "sys/fs/inode-nr",
    "sys/fs/inode-state",
    "sys/fs/pipe-max-size",
    "sys/kernel/pty/nr",
    "sys/kernel/random/entropy_avail",
    "sys/net/core/somaxconn",
    "sys/net/ipv4/ip_local_port_range",
    "tty/drivers",
    "vmstat",
    "zoneinfo",
];

/// What the table says about a path under `/proc`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcfsName {
    /// A per-process name, or one reached through a link the table does not
    /// follow: not decided here.
    Undecided,
    /// A name that exists from this level up.
    From(ProcLevel),
    /// A name in no level.
    Omitted,
}

impl ProcfsName {
    /// Whether the name is absent at `level`.
    pub(crate) fn absent_at(self, level: ProcLevel) -> bool {
        match self {
            Self::Undecided => false,
            Self::From(from) => from > level,
            Self::Omitted => true,
        }
    }
}

/// The level from which the procfs name `path` exists, for an absolute path
/// with no `..` component; `None` for a path outside `/proc`. `path` is the
/// spelling the guest gave, so a link elsewhere into `/proc` is not followed.
pub(crate) fn procfs_name(path: &Path) -> Option<ProcfsName> {
    let mut components = path.components().filter(|c| *c != Component::CurDir);
    if components.next() != Some(Component::RootDir) {
        return None;
    }
    match components.next() {
        Some(Component::Normal(name)) if name == "proc" => {}
        _ => return None,
    }
    let mut names = Vec::new();
    for component in components {
        match component {
            Component::Normal(name) => names.push(name.as_bytes()),
            // A `..` is the kernel's to resolve (see the callers).
            _ => return Some(ProcfsName::Undecided),
        }
    }
    let Some(first) = names.first() else {
        return Some(ProcfsName::From(ProcLevel::Minimal));
    };
    if *first == b"self" || *first == b"thread-self" || first.iter().all(u8::is_ascii_digit) {
        return Some(ProcfsName::Undecided);
    }
    // Below the `net` and `mounts` links is the process's own directory.
    if names.len() > 1 && (*first == b"net" || *first == b"mounts") {
        return Some(ProcfsName::Undecided);
    }
    // The name, a directory on the way to it, or a name below a file (which
    // the kernel refuses with ENOTDIR, as Linux does).
    let listed = |table: &[&str]| {
        table.iter().any(|entry| {
            names
                .iter()
                .zip(entry.split('/'))
                .all(|(name, part)| *name == part.as_bytes())
        })
    };
    Some(if listed(MINIMAL) {
        ProcfsName::From(ProcLevel::Minimal)
    } else if listed(STANDARD) {
        ProcfsName::From(ProcLevel::Standard)
    } else {
        ProcfsName::Omitted
    })
}

/// Whether the entry `name` of the directory at the canonical path
/// `directory` is absent at `level`.
pub(crate) fn absent_entry(directory: &Path, name: &[u8], level: ProcLevel) -> bool {
    if name == b"." || name == b".." {
        return false;
    }
    procfs_name(&directory.join(OsStr::from_bytes(name)))
        .is_some_and(|entry| entry.absent_at(level))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(path: &str) -> Option<ProcfsName> {
        procfs_name(Path::new(path))
    }

    #[test]
    fn levels_follow_the_checklist() {
        use ProcLevel::*;
        use ProcfsName::*;
        for (path, expected) in [
            ("/proc", From(Minimal)),
            ("/proc/", From(Minimal)),
            ("/proc/version", From(Minimal)),
            ("/proc/./version", From(Minimal)),
            ("/proc/sys", From(Minimal)),
            ("/proc/sys/kernel", From(Minimal)),
            ("/proc/sys/kernel/random", From(Minimal)),
            ("/proc/sys/kernel/random/uuid", From(Minimal)),
            ("/proc/sys/fs", From(Standard)),
            ("/proc/sys/fs/file-nr", From(Standard)),
            ("/proc/sys/kernel/random/entropy_avail", From(Standard)),
            ("/proc/sys/net", From(Standard)),
            ("/proc/interrupts", From(Standard)),
            ("/proc/tty", From(Standard)),
            ("/proc/tty/drivers", From(Standard)),
            ("/proc/version/x", From(Minimal)),
            ("/proc/kallsyms", Omitted),
            ("/proc/kcore", Omitted),
            ("/proc/sysvipc", Omitted),
            ("/proc/sysvipc/shm", Omitted),
            ("/proc/slabinfo", Omitted),
            ("/proc/bus/pci/devices", Omitted),
            ("/proc/tty/ldiscs", Omitted),
            ("/proc/sys/kernel/core_pattern", Omitted),
            ("/proc/sys/kernel/ngroups_max", Omitted),
            ("/proc/sys/kernel/random/poolsize", Omitted),
            ("/proc/sys/fs/binfmt_misc", Omitted),
            ("/proc/sys/crypto/fips_enabled", Omitted),
            ("/proc/sys/net/ipv4/tcp_rmem", Omitted),
            ("/proc/versionx", Omitted),
            ("/proc/self", Undecided),
            ("/proc/self/limits", Undecided),
            ("/proc/thread-self/status", Undecided),
            ("/proc/1/stat", Undecided),
            ("/proc/net/tcp", Undecided),
            ("/proc/mounts/x", Undecided),
            ("/proc/self/../kallsyms", Undecided),
        ] {
            assert_eq!(name(path), Some(expected), "{path}");
        }
        for path in [
            "/",
            "/procx/version",
            "/sys/kernel",
            "proc/version",
            "/tmp/proc/kcore",
        ] {
            assert_eq!(name(path), None, "{path}");
        }
        // A name that is not UTF-8 is in no level.
        assert_eq!(
            procfs_name(Path::new(OsStr::from_bytes(b"/proc/\xff"))),
            Some(Omitted)
        );
    }

    #[test]
    fn each_level_contains_the_one_before() {
        let interrupts = name("/proc/interrupts").unwrap();
        assert!(interrupts.absent_at(ProcLevel::Minimal));
        assert!(!interrupts.absent_at(ProcLevel::Standard));
        let version = name("/proc/version").unwrap();
        assert!(!version.absent_at(ProcLevel::Minimal));
        assert!(!version.absent_at(ProcLevel::Standard));
        let kallsyms = name("/proc/kallsyms").unwrap();
        assert!(kallsyms.absent_at(ProcLevel::Minimal));
        assert!(kallsyms.absent_at(ProcLevel::Standard));
        // Every rendered or fixed file is in a level.
        for path in super::super::fixed::FixedProcfsFile::ALL
            .iter()
            .map(|file| file.path())
        {
            assert!(
                !procfs_name(path).unwrap().absent_at(ProcLevel::Standard),
                "{path:?}"
            );
        }
    }

    #[test]
    fn listings_leave_out_what_lookups_refuse() {
        let proc = Path::new("/proc");
        let level = ProcLevel::Standard;
        assert!(absent_entry(proc, b"kallsyms", level));
        assert!(!absent_entry(proc, b"interrupts", level));
        assert!(absent_entry(proc, b"interrupts", ProcLevel::Minimal));
        assert!(!absent_entry(proc, b"self", level));
        assert!(!absent_entry(proc, b"42", level));
        assert!(!absent_entry(proc, b".", level));
        assert!(!absent_entry(proc, b"..", level));
        assert!(absent_entry(Path::new("/proc/sys"), b"crypto", level));
        assert!(!absent_entry(Path::new("/proc/sys"), b"fs", level));
        assert!(absent_entry(
            Path::new("/proc/sys"),
            b"fs",
            ProcLevel::Minimal
        ));
        assert!(absent_entry(
            Path::new("/proc/sys/kernel"),
            b"core_pattern",
            level
        ));
        assert!(!absent_entry(Path::new("/proc/self"), b"limits", level));
        assert!(!absent_entry(Path::new("/tmp"), b"kallsyms", level));
        assert!(absent_entry(proc, b"\xff", level));
    }
}
