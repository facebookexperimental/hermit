/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The machine a guest runs on, as Hermit describes it: one description that
//! every guest-visible source of machine facts reads, so that they tell the
//! same story on every backend and every host
//! (<https://github.com/rrnewton/hermit/issues/4266>).
//!
//! Today `uname`, `sysinfo` and the fixed procfs files (`procfs::fixed`)
//! read it. The virtual CPU (CPUID, `/proc/cpuinfo`, the CPU files under
//! `/sys`, `sched_getaffinity`) still has its own sources, which this
//! description is to absorb; the design lists every source that must agree.
//!
//! The values are fixed. They may come only from Hermit's configuration
//! (today: the epoch and the memory size), never from the host.

use chrono::DateTime;
use chrono::Utc;

/// The machine a guest sees.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MachineDescription {
    /// The running kernel.
    pub kernel: Kernel,
    /// The kernel's fixed limits and settings.
    pub limits: KernelLimits,
    /// The number of CPUs: 1. Detcore runs a guest's threads one at a time,
    /// as on one CPU, and `/proc/stat`, the CPU files under `/sys` and
    /// `sched_getaffinity` already report one; the tables that have a column
    /// or block per CPU (`/proc/interrupts`, `/proc/softirqs`,
    /// `/proc/schedstat`, `/proc/zoneinfo`) render this many.
    pub cpus: u32,
    /// The memory size in bytes, which `/proc/meminfo` (in kB, rounded
    /// down) and `sysinfo` report: the configured `--memory` (1 GB by
    /// default). Linux counts memory in whole pages, so its two reports
    /// agree exactly; here they differ by the size modulo 1024 (512 bytes
    /// for the default), as they did before this description.
    pub memory: u64,
}

/// `--memory`'s default, 1 GB (`bytesize` reads "1GB" as 10^9 bytes).
const DEFAULT_MEMORY: u64 = 1_000_000_000;

impl Default for MachineDescription {
    fn default() -> Self {
        Self {
            kernel: Kernel::default(),
            limits: KernelLimits::default(),
            cpus: 1,
            memory: DEFAULT_MEMORY,
        }
    }
}

impl MachineDescription {
    /// The machine of a run whose configured epoch is `epoch`.
    pub fn new(epoch: DateTime<Utc>) -> Self {
        Self {
            kernel: Kernel {
                built: epoch,
                ..Kernel::default()
            },
            ..Self::default()
        }
    }

    /// The contents of the fixed procfs file at `path` on this machine, if
    /// `path` names one (`procfs::fixed`).
    pub fn procfs_file(&self, path: &str) -> Option<Vec<u8>> {
        crate::procfs::FixedProcfsFile::from_path(path).map(|file| file.contents(self))
    }

    /// The machine of a run with configuration `config`.
    pub fn of(config: &crate::config::Config) -> Self {
        Self {
            memory: config.memory,
            ..Self::new(config.epoch)
        }
    }
}

/// The running kernel.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Kernel {
    /// `uname().sysname` and `kernel/ostype`.
    pub sysname: String,
    /// `uname().release` and `kernel/osrelease`: 5.2.0. It must pass glibc's
    /// ABI-tag check for the libraries a guest loads; whether to advertise a
    /// newer release is an open question in the design.
    pub release: String,
    /// When the kernel was built, which dates its version: the epoch.
    pub built: DateTime<Utc>,
    /// The options after the boot image on `/proc/cmdline`: `quiet`.
    pub boot_options: String,
    /// `kernel/random/boot_id`: one UUID for every run, version 4 in form;
    /// its first bytes spell `hermit` in hex.
    pub boot_id: String,
}

impl Default for Kernel {
    fn default() -> Self {
        Self {
            sysname: "Linux".to_owned(),
            release: "5.2.0".to_owned(),
            built: DateTime::<Utc>::default(),
            boot_options: "quiet".to_owned(),
            boot_id: "6865726d-6974-4000-8000-000000000000".to_owned(),
        }
    }
}

impl Kernel {
    /// `uname().version` and `kernel/version`: a build dated when it was
    /// built. Formatted in UTC: `Local` initializes libc TLS, which is
    /// unavailable while a DynamoRIO application thread is executing a
    /// client callback.
    pub fn version(&self) -> String {
        format!("#1 SMP {}", self.built.format("%a %b %d %T %Z %Y"))
    }

    /// `/proc/version`: sysname, release and version, in the form
    /// `Linux version <release> <version>` that gVisor also uses. Linux adds
    /// the builder and compiler, which describe no kernel a guest can see.
    pub fn banner(&self) -> String {
        format!(
            "{} version {} {}",
            self.sysname,
            self.release,
            self.version()
        )
    }

    /// `/proc/cmdline`: a boot image named after the release.
    pub fn command_line(&self) -> String {
        format!("BOOT_IMAGE=/vmlinuz-{} {}", self.release, self.boot_options)
    }
}

/// The kernel's fixed limits and settings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KernelLimits {
    /// `kernel/random/entropy_avail`: 256, which Linux 5.18 and later always
    /// report.
    pub entropy_avail: u32,
    /// `kernel/overflowuid`: 65534, Linux's default, the ID an unmapped user
    /// shows as.
    pub overflow_uid: u32,
    /// `kernel/overflowgid`: 65534, as `overflow_uid`, for groups.
    pub overflow_gid: u32,
    /// `kernel/pid_max`: 4194304 (2^22, `PID_MAX_LIMIT`), the value systemd
    /// sets on 64-bit machines. Detcore's virtual PIDs stay far below it.
    pub pid_max: u32,
    /// `kernel/cap_last_cap`: 40, `CAP_CHECKPOINT_RESTORE`, the last
    /// capability of the kernels Hermit runs on. `capget` and
    /// `PR_CAPBSET_READ` reach the host kernel, so this agrees with the
    /// capability sets the guest is shown.
    pub cap_last_cap: u32,
    /// `vm/overcommit_memory`: 0, Linux's heuristic default.
    pub overcommit_memory: u32,
    /// `vm/max_map_count`: 65530, Linux's default (`DEFAULT_MAX_MAP_COUNT`).
    pub max_map_count: u32,
    /// `vm/mmap_min_addr`: 65536, the default of the common distributions.
    pub mmap_min_addr: u64,
    /// `fs/file-max`: `LONG_MAX`, what Linux 5.0 and later default to.
    pub file_max: u64,
    /// `fs/aio-max-nr`: 65536, Linux's default. (Before, the guest read 0,
    /// which says that no asynchronous I/O context can be set up.)
    pub aio_max_nr: u64,
    /// `fs/pipe-max-size`: the capacity Detcore pins every pipe at
    /// (`DETERMINISTIC_PIPE_CAPACITY_BYTES`), which is also the most that
    /// `F_SETPIPE_SZ` allows.
    pub pipe_max_size: u32,
}

impl Default for KernelLimits {
    fn default() -> Self {
        Self {
            entropy_avail: 256,
            overflow_uid: 65534,
            overflow_gid: 65534,
            pid_max: 1 << 22,
            cap_last_cap: 40,
            overcommit_memory: 0,
            max_map_count: 65530,
            mmap_min_addr: 65536,
            file_max: i64::MAX as u64,
            aio_max_nr: 65536,
            pipe_max_size: crate::syscalls::DETERMINISTIC_PIPE_CAPACITY_BYTES as u32,
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    /// The kernel's strings agree with each other as Linux's do.
    #[test]
    fn the_kernel_describes_itself_consistently() {
        let machine = MachineDescription::new(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap());
        let kernel = &machine.kernel;
        assert_eq!(kernel.version(), "#1 SMP Thu Jan 01 00:00:00 UTC 2026");
        assert_eq!(
            kernel.banner(),
            format!("Linux version 5.2.0 {}", kernel.version())
        );
        assert_eq!(kernel.command_line(), "BOOT_IMAGE=/vmlinuz-5.2.0 quiet");
    }
}
