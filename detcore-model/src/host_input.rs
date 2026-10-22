/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The host identity of a file a guest opened.
//!
//! Hermit maps each host inode it sees to a deterministic one in the order it
//! first sees them, so a host file that is REPLACED while a run is going (a
//! package manager rewriting `/etc/ld.so.cache` between the opens of two
//! guest processes, for example) gives that run one more inode than the other
//! run, and every later inode number differs. `hermit run --verify` then
//! reports a divergence that the guest did not cause. Each verification run
//! therefore records one [`HostInputRecord`] per host file the guest opens.
//! Detcore reports each open to its global state
//! ([`crate::config::Config::record_host_inputs`]), which writes the run's
//! records to a side file ([`crate::config::Config::host_input_log`]) when the
//! run ends, never part of the compared log. On a divergence the two runs'
//! records are compared to name such a change.

use serde::Deserialize;
use serde::Serialize;

/// The host's identity for a file at the moment the guest opened it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct HostFileIdentity {
    /// The host `st_dev`.
    pub dev: u64,
    /// The host `st_ino`.
    pub ino: u64,
    /// The host `st_size`.
    pub size: i64,
    /// The host `st_mtime`, seconds.
    pub mtime_sec: i64,
    /// The host `st_mtime`, nanoseconds.
    pub mtime_nsec: i64,
}

impl HostFileIdentity {
    /// The identity in a host `struct stat`.
    pub fn from_stat(stat: &libc::stat) -> Self {
        Self {
            dev: stat.st_dev,
            ino: stat.st_ino,
            size: stat.st_size,
            mtime_sec: stat.st_mtime,
            mtime_nsec: stat.st_mtime_nsec,
        }
    }
}

impl std::fmt::Display for HostFileIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "dev:ino {}:{}:{} size {} mtime {}.{:09}",
            libc::major(self.dev),
            libc::minor(self.dev),
            self.ino,
            self.size,
            self.mtime_sec,
            self.mtime_nsec
        )
    }
}

/// One host file a guest opened: the path it named, made absolute against its
/// working directory or directory descriptor, the host identity of the file
/// the open reached, and the open itself, as the thread (`dtid`) and that
/// thread's syscall number (`syscall`, the `finish syscall #N` of the run's
/// log) that made it. A path that is not UTF-8 is recorded lossily.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HostInputRecord {
    pub path: String,
    pub dtid: u64,
    pub syscall: u64,
    #[serde(flatten)]
    pub identity: HostFileIdentity,
}

/// A path the guest itself rebound in its file namespace during the run:
/// renamed to or from, linked, unlinked, made as a symlink, node or directory,
/// removed as a directory, or opened to create (see
/// [`crate::config::Config::record_host_inputs`]). Made absolute as an open's
/// path is. A host file whose path, or a directory above it, the guest
/// rebound may have been replaced by the guest, so such a change is never
/// named as a host input change.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostMutationRecord {
    pub mutated: String,
}

/// The last line of a complete host-input log: how many records, of both
/// kinds, precede it. A log without it, or with another count, is incomplete.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostInputLogEnd {
    pub records: u64,
}
