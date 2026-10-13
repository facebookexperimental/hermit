/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! System-wide procfs files that describe the kernel and its fixed limits
//! (owner decision, Oct 13; <https://github.com/rrnewton/hermit/issues/4266>).
//! Each holds one value from the [`MachineDescription`], the same on every
//! backend and host, which Detcore renders with no kernel open. Before, the
//! host-process backends passed the host's bytes through unchanged and the
//! KVM executor made up its own, which disagreed with `uname`.

use std::path::Path;

use serde::Deserialize;
use serde::Serialize;

use crate::machine::MachineDescription;

/// A procfs file whose contents are a fact of the [`MachineDescription`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum FixedProcfsFile {
    /// `/proc/version`: the kernel's banner.
    Version,
    /// `/proc/cmdline`: the kernel's command line.
    Cmdline,
    Osrelease,
    Ostype,
    /// `kernel/version`.
    KernelVersion,
    BootId,
    EntropyAvail,
    OverflowUid,
    OverflowGid,
    PidMax,
    CapLastCap,
    OvercommitMemory,
    MaxMapCount,
    MmapMinAddr,
}

impl FixedProcfsFile {
    /// Every fixed file.
    pub(crate) const ALL: [Self; 14] = [
        Self::Version,
        Self::Cmdline,
        Self::Osrelease,
        Self::Ostype,
        Self::KernelVersion,
        Self::BootId,
        Self::EntropyAvail,
        Self::OverflowUid,
        Self::OverflowGid,
        Self::PidMax,
        Self::CapLastCap,
        Self::OvercommitMemory,
        Self::MaxMapCount,
        Self::MmapMinAddr,
    ];

    /// The file's path, NUL-terminated.
    pub(crate) fn c_path(self) -> &'static std::ffi::CStr {
        match self {
            Self::Version => c"/proc/version",
            Self::Cmdline => c"/proc/cmdline",
            Self::Osrelease => c"/proc/sys/kernel/osrelease",
            Self::Ostype => c"/proc/sys/kernel/ostype",
            Self::KernelVersion => c"/proc/sys/kernel/version",
            Self::BootId => c"/proc/sys/kernel/random/boot_id",
            Self::EntropyAvail => c"/proc/sys/kernel/random/entropy_avail",
            Self::OverflowUid => c"/proc/sys/kernel/overflowuid",
            Self::OverflowGid => c"/proc/sys/kernel/overflowgid",
            Self::PidMax => c"/proc/sys/kernel/pid_max",
            Self::CapLastCap => c"/proc/sys/kernel/cap_last_cap",
            Self::OvercommitMemory => c"/proc/sys/vm/overcommit_memory",
            Self::MaxMapCount => c"/proc/sys/vm/max_map_count",
            Self::MmapMinAddr => c"/proc/sys/vm/mmap_min_addr",
        }
    }

    /// The file's path.
    pub(crate) fn path(self) -> &'static Path {
        Path::new(
            self.c_path()
                .to_str()
                .expect("fixed procfs paths are ASCII"),
        )
    }

    pub(crate) fn from_path(path: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|file| file.c_path().to_bytes() == path.as_bytes())
    }

    /// The permission bits Linux gives the file: read-only for the kernel's
    /// descriptions, writable by its owner (global root) for the tunables.
    /// Detcore refuses every write, as Linux refuses a process that is not
    /// global root. A guest that is global root would otherwise change the
    /// host's setting, so it is refused too: a known difference.
    pub(crate) fn permissions(self) -> u32 {
        match self {
            Self::Version
            | Self::Cmdline
            | Self::Osrelease
            | Self::Ostype
            | Self::KernelVersion
            | Self::BootId
            | Self::EntropyAvail
            | Self::CapLastCap => 0o444,
            Self::OverflowUid
            | Self::OverflowGid
            | Self::PidMax
            | Self::OvercommitMemory
            | Self::MaxMapCount
            | Self::MmapMinAddr => 0o644,
        }
    }

    /// Whether the file is an integer sysctl, whose reads at a nonzero
    /// position Linux answers with end of file. The others are strings
    /// (`proc_dostring`, `proc_do_uuid`) or `seq_file` files, which serve the
    /// rest of their contents.
    pub(crate) fn is_integer(self) -> bool {
        !matches!(
            self,
            Self::Version
                | Self::Cmdline
                | Self::Osrelease
                | Self::Ostype
                | Self::KernelVersion
                | Self::BootId
        )
    }

    /// Whether the file is a `seq_file` (`/proc/version`, `/proc/cmdline`),
    /// which seeks only from its start or position (`seq_lseek`), rather
    /// than a sysctl file.
    pub(crate) fn is_seq_file(self) -> bool {
        matches!(self, Self::Version | Self::Cmdline)
    }

    /// The file's contents on `machine`.
    pub(crate) fn contents(self, machine: &MachineDescription) -> Vec<u8> {
        let kernel = &machine.kernel;
        let limits = &machine.limits;
        let text = match self {
            Self::Version => kernel.banner(),
            Self::Cmdline => kernel.command_line(),
            Self::Osrelease => kernel.release.clone(),
            Self::Ostype => kernel.sysname.clone(),
            Self::KernelVersion => kernel.version(),
            Self::BootId => kernel.boot_id.clone(),
            Self::EntropyAvail => limits.entropy_avail.to_string(),
            Self::OverflowUid => limits.overflow_uid.to_string(),
            Self::OverflowGid => limits.overflow_gid.to_string(),
            Self::PidMax => limits.pid_max.to_string(),
            Self::CapLastCap => limits.cap_last_cap.to_string(),
            Self::OvercommitMemory => limits.overcommit_memory.to_string(),
            Self::MaxMapCount => limits.max_map_count.to_string(),
            Self::MmapMinAddr => limits.mmap_min_addr.to_string(),
        };
        format!("{text}\n").into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use chrono::Utc;

    use super::*;

    fn machine() -> MachineDescription {
        MachineDescription::new(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap())
    }

    fn contents(file: FixedProcfsFile) -> String {
        String::from_utf8(file.contents(&machine())).unwrap()
    }

    /// The table, byte for byte: changing a value is a decision, recorded
    /// here and in `MachineDescription`.
    #[test]
    fn fixed_procfs_files_have_their_documented_values() {
        use FixedProcfsFile::*;
        let expected = [
            (
                Version,
                "Linux version 5.2.0 #1 SMP Thu Jan 01 00:00:00 UTC 2026\n",
            ),
            (Cmdline, "BOOT_IMAGE=/vmlinuz-5.2.0 quiet\n"),
            (Osrelease, "5.2.0\n"),
            (Ostype, "Linux\n"),
            (KernelVersion, "#1 SMP Thu Jan 01 00:00:00 UTC 2026\n"),
            (BootId, "6865726d-6974-4000-8000-000000000000\n"),
            (EntropyAvail, "256\n"),
            (OverflowUid, "65534\n"),
            (OverflowGid, "65534\n"),
            (PidMax, "4194304\n"),
            (CapLastCap, "40\n"),
            (OvercommitMemory, "0\n"),
            (MaxMapCount, "65530\n"),
            (MmapMinAddr, "65536\n"),
        ];
        assert_eq!(expected.len(), FixedProcfsFile::ALL.len());
        for (file, text) in expected {
            assert_eq!(contents(file), text, "{file:?}");
        }
    }

    /// `/proc/version`, `kernel/osrelease`, `kernel/ostype` and
    /// `kernel/version` describe the kernel `uname` reports, from the same
    /// description.
    #[test]
    fn procfs_agrees_with_uname() {
        let machine = machine();
        let kernel = &machine.kernel;
        let version = contents(FixedProcfsFile::Version);
        assert!(version.starts_with(&format!("{} version {} ", kernel.sysname, kernel.release)));
        assert!(version.trim_end().ends_with(&kernel.version()));
        assert_eq!(
            contents(FixedProcfsFile::Osrelease).trim_end(),
            kernel.release
        );
        assert_eq!(contents(FixedProcfsFile::Ostype).trim_end(), kernel.sysname);
        assert_eq!(
            contents(FixedProcfsFile::KernelVersion).trim_end(),
            kernel.version()
        );
    }

    /// Opened by Detcore, a fixed file reads its contents on the run's
    /// machine, whatever the template it starts from.
    #[test]
    fn a_rendered_fixed_file_reads_its_contents() {
        for fixed in FixedProcfsFile::ALL {
            let mut file = super::super::ProcfsFile::from_path(fixed.path())
                .unwrap()
                .into_rendered();
            let template = file.rendering_template().to_vec();
            file.initialize(
                template,
                super::super::ProcfsSnapshotContext {
                    machine: machine(),
                    ..Default::default()
                },
            );
            assert_eq!(
                file.take(usize::MAX),
                Some(fixed.contents(&machine())),
                "{fixed:?}"
            );
        }
    }

    /// Linux formats an integer sysctl only for a read from position 0, so a
    /// read that copied part of it is followed by end of file; a string
    /// sysctl and a `seq_file` file serve the rest.
    #[test]
    fn integer_files_end_after_a_partial_read() {
        let open = |fixed: FixedProcfsFile| {
            let mut file = super::super::ProcfsFile::from_path(fixed.path())
                .unwrap()
                .into_rendered();
            let template = file.rendering_template().to_vec();
            file.initialize(
                template,
                super::super::ProcfsSnapshotContext {
                    machine: machine(),
                    ..Default::default()
                },
            );
            file
        };
        let mut pid_max = open(FixedProcfsFile::PidMax);
        assert_eq!(pid_max.take(2), Some(b"41".to_vec()));
        assert_eq!(pid_max.take(64), Some(Vec::new()));
        assert_eq!(pid_max.take_at(0, 64), Some(b"4194304\n".to_vec()));

        let mut osrelease = open(FixedProcfsFile::Osrelease);
        assert_eq!(osrelease.take(2), Some(b"5.".to_vec()));
        assert_eq!(osrelease.take(64), Some(b"2.0\n".to_vec()));

        let mut version = open(FixedProcfsFile::Version);
        assert_eq!(version.take(6), Some(b"Linux ".to_vec()));
        assert_eq!(version.take(7), Some(b"version".to_vec()));

        for fixed in FixedProcfsFile::ALL {
            let integer = contents(fixed).trim_end().parse::<u64>().is_ok();
            assert_eq!(fixed.is_integer(), integer, "{fixed:?}");
        }
    }

    #[test]
    fn fixed_procfs_files_are_found_by_path_alone() {
        for file in FixedProcfsFile::ALL {
            assert_eq!(
                FixedProcfsFile::from_path(file.path().to_str().unwrap()),
                Some(file)
            );
        }
        assert_eq!(
            FixedProcfsFile::from_path("/proc/sys/kernel/hostname"),
            None
        );
        assert_eq!(FixedProcfsFile::from_path("/proc/self/cmdline"), None);
    }
}
