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
//!
//! A run that writes a record meant to be reproduced later (a
//! `--record-preemptions-to` schedule, or a `--summary-json` summary) also
//! keeps a [`HostInputSet`]: each host file the guest opened, with the
//! identity its first open found, and a digest of the set. A run that replays
//! such a schedule compares the files it opens against the set and reports
//! each one that changed ([`HostInputChange`]): Hermit does not make a
//! changing filesystem deterministic, so a replay that reads different host
//! files can diverge, and it then says why.

use std::path::Path;

use serde::Deserialize;
use serde::Serialize;

/// The host's identity for a file at the moment it was opened or captured.
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
    /// For a file Hermit copied before the run and the guest reads in its
    /// place (a frozen snapshot, see
    /// [`crate::config::Config::property_snapshot`]), a hash of the copy's
    /// contents. The copy's own device, inode and modification time are new
    /// in every run, so they are zero and this identifies the file instead:
    /// two runs whose snapshots differ in content record different
    /// identities. `None` for every other file. Always encoded, because the
    /// identity also travels in positional (bincode) RPC messages; a JSON
    /// record written before the field existed reads as `None`.
    #[serde(default)]
    pub content: Option<u64>,
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
            content: None,
        }
    }

    /// The identity of a snapshot copy of `size` bytes whose contents hash to
    /// `content` (see [`Self::content`]).
    pub fn of_snapshot(size: i64, content: u64) -> Self {
        Self {
            dev: 0,
            ino: 0,
            size,
            mtime_sec: 0,
            mtime_nsec: 0,
            content: Some(content),
        }
    }
}

/// A private copy of a captured host input. The launcher supplies this only
/// for a copy it owns: `physical` authenticates the object the guest opened,
/// while `origin` names the object whose bytes were captured. Aliases of the
/// copy have the same provenance; an unrelated object at its usual guest path
/// does not. This is host-only state, never a guest or record-format payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostInputSnapshot {
    pub physical: HostFileIdentity,
    pub origin: HostFileIdentity,
    /// SHA-256 of the captured bytes, checked before origin attribution is
    /// retained at shutdown, even if a rewrite preserved size and mtime.
    pub content: String,
}

impl std::fmt::Display for HostFileIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(content) = self.content {
            return write!(f, "snapshot size {} content {:016x}", self.size, content);
        }
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
/// the open reached (or the captured origin of an authenticated launcher-owned
/// snapshot), and the open itself, as the thread (`dtid`) and that
/// thread's syscall number (`syscall`, the `finish syscall #N` of the run's
/// log) that made it. A path that is not UTF-8 is recorded lossily.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HostInputRecord {
    pub path: String,
    pub dtid: u64,
    pub syscall: u64,
    #[serde(flatten)]
    pub identity: HostFileIdentity,
    /// The content of the file the open found, as [`HostInput::content`],
    /// read when the run had ended; absent where Detcore could not read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Whether the kernel opened the file for an exec, as the new image or
    /// its program interpreter, rather than the guest by an open call. Such a
    /// record has no open in the log, so the verification pattern walk
    /// leaves it out; a [`HostInputSet`] keeps it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exec: bool,
    /// Whether the open reached a kernel object: by the descriptor's own
    /// path ([`is_kernel_object`]), whatever the guest spelled (`/etc/mtab`
    /// names `/proc/self/mounts`), or by the file's type, a FIFO, a socket or
    /// a device wherever it is, which holds no bytes to compare and whose
    /// times move with every write to it. A [`HostInputSet`] leaves it out,
    /// as it leaves out a path spelled under `/proc`, `/sys` or `/dev`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub kernel_object: bool,
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

/// One host file in a [`HostInputSet`]: its path and the identity the run's
/// first open of that path found.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostInput {
    pub path: String,
    #[serde(flatten)]
    pub identity: HostFileIdentity,
    /// The SHA-256 (lowercase hex) of what the guest can read of the file:
    /// a regular file's bytes, or the sorted names in a directory, without
    /// those the guest itself made or removed there ([`guest_rebound`]).
    /// Read when the run has ended, after every guest process exited, never
    /// while the guest runs (Linux raises an inotify event the guest could
    /// see for every open and read of a file), and only where the file still
    /// has the identity that open found (a directory, the same device and
    /// inode); present for a regular file of at most
    /// [`HOST_INPUT_CONTENT_LIMIT`] bytes or a directory on a backend whose
    /// tool can name the guest's descriptor. The guest sees a host file's
    /// bytes and size, and a directory's names, not their host inodes or
    /// times (Hermit virtualizes those, and makes some directories, such as
    /// its private `/tmp`, afresh for each run), so where both runs have it,
    /// the content decides whether the file changed ([`host_input_changed`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// The largest file whose bytes Detcore hashes for a [`HostInput`]: a larger
/// one is compared by its identity alone, so that a guest that opens big data
/// files does not pay to read them twice.
pub const HOST_INPUT_CONTENT_LIMIT: u64 = 32 << 20;

/// Whether `now` shows a different file than `recorded`, for the same path:
/// other bytes, where both runs hashed them, and otherwise another identity.
/// A file rewritten with the same bytes (as a package manager's `ldconfig`
/// does) is therefore not a change where its content was hashed.
pub fn host_input_changed(recorded: &HostInput, now: &HostInput) -> bool {
    match (&recorded.content, &now.content) {
        (Some(recorded), Some(now)) => recorded != now,
        _ => recorded.identity != now.identity,
    }
}

/// The host files one run's guest opened, for a later run that reproduces it
/// to compare against: one [`HostInput`] per path, sorted by path, and a
/// digest of the set, so that two runs can be compared by one string. The
/// digest is the SHA-256 (lowercase hex) of one line per file: its path and
/// the SHA-256 of its bytes where they were hashed, otherwise its identity.
/// So it agrees with [`host_input_changed`]: two runs that read the same
/// bytes at the same paths get the same digest whatever the files' inodes
/// and times. A path is left out when it is not absolute, names a kernel
/// object ([`is_kernel_object`], also by the descriptor's own path,
/// [`HostInputRecord::kernel_object`]), or was rebound by the guest itself
/// ([`guest_rebound`]): those can change between runs without the host's
/// files changing.
#[derive(Clone, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostInputSet {
    pub digest: String,
    pub files: Vec<HostInput>,
}

impl HostInputSet {
    /// The entry this set holds for `path`. A set read from a file is
    /// searched as its files lie; [`Self::normalized`] sorts it first.
    pub fn file(&self, path: &str) -> Option<&HostInput> {
        self.files
            .binary_search_by(|file| file.path.as_str().cmp(path))
            .ok()
            .map(|index| &self.files[index])
    }

    /// This set with its files sorted by path and one entry per path (the
    /// first), as a run writes it, whatever a file it was read from held.
    pub fn normalized(mut self) -> Self {
        self.files.sort_by(|a, b| a.path.cmp(&b.path));
        self.files
            .dedup_by(|later, earlier| later.path == earlier.path);
        self
    }
}

/// A host file that differs between a recorded run and the run that
/// reproduces it ([`host_input_changed`]): other bytes, or, where either run
/// could not hash them, another identity (replaced, or modified in size or
/// time). The reproduction may read other bytes than the recorded run did,
/// and diverge where it does.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostInputChange {
    pub path: String,
    pub recorded: HostFileIdentity,
    pub now: HostFileIdentity,
}

impl std::fmt::Display for HostInputChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "host input {} changed since the recording ({} then, {} now); the run may diverge where the guest reads it",
            self.path, self.recorded, self.now
        )
    }
}

/// The marker that begins the stderr line a reproducing run prints for each
/// [`HostInputChange`]. A harness greps for it, as for
/// `HERMIT_SKID_OVERSHOOT`; keep it stable.
pub const HOST_INPUT_CHANGED_SINCE_RECORDING_MARKER: &str =
    "HERMIT_HOST_INPUT_CHANGED_SINCE_RECORDING";

/// Whether `path` names a kernel object under `/proc`, `/sys` or `/dev`
/// rather than a host input file: its metadata is not file content, and
/// Hermit virtualizes those trees separately.
pub fn is_kernel_object(path: &str) -> bool {
    ["/proc", "/sys", "/dev"].iter().any(|root| {
        path == *root
            || path
                .strip_prefix(root)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// Whether the guest may have rebound `path`, or a directory above it, in its
/// file namespace: one of `mutated` ([`HostMutationRecord`] paths) is `path`
/// or an ancestor of it. Detcore reports `/`, the ancestor of every absolute
/// path, for a change whose path it could not establish. A path that is not
/// absolute cannot be matched against those changes at all, so it counts as
/// rebound too.
pub fn guest_rebound<'a>(mutated: impl IntoIterator<Item = &'a String>, path: &str) -> bool {
    let path = Path::new(path);
    !path.is_absolute()
        || mutated
            .into_iter()
            .any(|rebound| path.starts_with(Path::new(rebound)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(ino: u64, size: i64) -> HostFileIdentity {
        HostFileIdentity {
            dev: 33,
            ino,
            size,
            mtime_sec: 1_700_000_000,
            mtime_nsec: 5,
            content: None,
        }
    }

    fn set(files: &[(&str, HostFileIdentity)]) -> HostInputSet {
        HostInputSet {
            digest: String::new(),
            files: files
                .iter()
                .map(|(path, identity)| HostInput {
                    path: (*path).to_owned(),
                    identity: *identity,
                    content: None,
                })
                .collect(),
        }
    }

    #[test]
    fn a_rebound_ancestor_or_a_relative_path_counts_as_rebound() {
        let mutated = vec!["/work".to_owned()];
        assert!(guest_rebound(&mutated, "/work/out.txt"));
        assert!(guest_rebound(&mutated, "/work"));
        assert!(!guest_rebound(&mutated, "/workspace/in.txt"));
        assert!(guest_rebound(&mutated, "relative.txt"));
        assert!(guest_rebound(&["/".to_owned()], "/etc/ld.so.cache"));
    }

    #[test]
    fn a_set_finds_each_file_by_its_path() {
        let recorded = set(&[
            ("/etc/ld.so.cache", identity(9, 36887)),
            ("/lib64/libc.so.6", identity(10, 2_000_000)),
        ]);
        assert_eq!(
            recorded.file("/etc/ld.so.cache").map(|file| file.identity),
            Some(identity(9, 36887))
        );
        assert_eq!(
            recorded.file("/lib64/libc.so.6").map(|file| file.identity),
            Some(identity(10, 2_000_000))
        );
        assert_eq!(recorded.file("/etc"), None);
    }

    #[test]
    fn hashed_content_decides_over_identity() {
        let file = |ino: u64, content: Option<&str>| HostInput {
            path: "/etc/ld.so.cache".to_owned(),
            identity: identity(ino, 36887),
            content: content.map(str::to_owned),
        };
        // Rewritten with the same bytes: a new inode, no change.
        assert!(!host_input_changed(
            &file(9, Some("aa")),
            &file(11, Some("aa"))
        ));
        // Same inode and size, other bytes: a change.
        assert!(host_input_changed(
            &file(9, Some("aa")),
            &file(9, Some("bb"))
        ));
        // Either side unhashed: the identity decides.
        assert!(host_input_changed(&file(9, None), &file(11, Some("aa"))));
        assert!(!host_input_changed(&file(9, Some("aa")), &file(9, None)));
    }

    #[test]
    fn the_change_line_names_the_path_and_both_identities() {
        let change = HostInputChange {
            path: "/etc/ld.so.cache".to_owned(),
            recorded: identity(9, 36887),
            now: identity(11, 36839),
        };
        let line = change.to_string();
        assert!(line.starts_with("host input /etc/ld.so.cache changed since the recording"));
        assert!(line.contains("size 36887"), "{line}");
        assert!(line.contains("size 36839"), "{line}");
    }

    /// A record written before `content` existed reads with none; every
    /// identity, with or without contents, round-trips through JSON and
    /// through the positional bincode encoding the RPCs use; a snapshot's
    /// identity names its contents.
    #[test]
    fn host_file_identities_without_and_with_contents() {
        let old = r#"{"dev":33,"ino":7,"size":100,"mtime_sec":1,"mtime_nsec":2}"#;
        let identity: HostFileIdentity = serde_json::from_str(old).unwrap();
        assert_eq!(identity.content, None);
        let snapshot = HostFileIdentity::of_snapshot(131072, 0x0123_4567_89ab_cdef);
        for value in [identity, snapshot] {
            let json = serde_json::to_string(&value).unwrap();
            assert_eq!(
                serde_json::from_str::<HostFileIdentity>(&json).unwrap(),
                value
            );
            let config = bincode::config::legacy();
            let bytes = bincode::serde::encode_to_vec(value, config).unwrap();
            let (decoded, read): (HostFileIdentity, usize) =
                bincode::serde::decode_from_slice(&bytes, config).unwrap();
            assert_eq!((decoded, read), (value, bytes.len()));
        }
        assert_eq!(
            snapshot.to_string(),
            "snapshot size 131072 content 0123456789abcdef"
        );
        assert_ne!(snapshot, HostFileIdentity::of_snapshot(131072, 1));
    }
}
