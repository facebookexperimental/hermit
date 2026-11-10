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
    ///
    /// Its JSON key is `snapshot_content`, not `content`: the identity is
    /// flattened into records (`HostInputRecord`) that may carry a `content`
    /// key of their own (a hash of a host file's bytes), and two fields of
    /// one name in one record make serde refuse it as a duplicate field. A
    /// renamed key costs the positional bincode encoding nothing.
    #[serde(default, rename = "snapshot_content")]
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
    /// The kinds of file behind the guest's standard descriptors when the
    /// run began ([`StdioKinds`]). Not part of [`Self::digest`], which follows
    /// the files alone. Absent where the run did not look.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdio: Option<StdioKinds>,
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

/// The kind of file behind one of the guest's standard descriptors. A guest
/// can tell them apart (`fstat`, `isatty`, and calls whose result depends on
/// the kind: `fsync` fails with `EINVAL` on a pipe and succeeds on a file, and
/// `lseek` fails with `ESPIPE` on a pipe), so a replay whose stdio kinds
/// differ from its recording's may run differently where the guest checks.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StdioKind {
    /// A regular file.
    File,
    /// A pipe or FIFO.
    Pipe,
    /// A socket.
    Socket,
    /// A terminal.
    Tty,
    /// A character device that is not a terminal, such as `/dev/null`.
    CharDevice,
    /// A block device.
    BlockDevice,
    /// A directory.
    Directory,
    /// No open descriptor.
    Closed,
    /// A file Hermit inspected and found to be of none of the kinds above:
    /// an anonymous inode, a namespace, or a symbolic link an `O_PATH`
    /// descriptor names.
    Other,
    /// Not read: Hermit could not tell the kind without an operation on the
    /// file's filesystem that it does not make (a file on a mount outside
    /// [`LOCAL_KERNEL_FILESYSTEMS`], such as FUSE, NFS or overlay), or the
    /// facts it reads were unavailable. Never compared: a descriptor whose
    /// kind is unknown in either run is not named as changed
    /// ([`StdioKinds::changes`]).
    Unknown,
}

/// The filesystems on which Detcore reads the type of a file behind one of
/// the guest's standard descriptors ([`StdioKind::of_guest_descriptor`]):
/// local, in-kernel filesystems whose `getattr` answers from the host
/// itself, so the read waits on no server and makes no request another
/// process could see. Network filesystems (NFS, CIFS, 9p, Ceph), FUSE,
/// whose server may be a guest process, and overlay, whose `getattr` goes to
/// its layers, are left out, and so is every type not listed here.
pub const LOCAL_KERNEL_FILESYSTEMS: [&str; 11] = [
    "ext2", "ext3", "ext4", "xfs", "btrfs", "tmpfs", "ramfs", "devtmpfs", "devpts", "proc", "sysfs",
];

/// The mount ID and filesystem type of each row of `mountinfo`, the text of
/// a process's `/proc/<pid>/mountinfo`. Mount IDs are unique across mount
/// namespaces while the mounts exist, so rows read from several namespaces
/// can be searched together.
pub fn mount_types(mountinfo: &str) -> Vec<(u64, String)> {
    mountinfo
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let id = fields.next()?.parse::<u64>().ok()?;
            // The filesystem type follows the `-` that ends the optional
            // fields.
            let fstype = fields.skip_while(|field| *field != "-").nth(1)?;
            Some((id, fstype.to_owned()))
        })
        .collect()
}

/// The mounts of the namespace hermit-cli was launched in, read before it
/// creates its container ([`mount_types`]): a descriptor the guest inherits
/// from outside the container names one of these mounts in its `fdinfo`,
/// which the container's own mountinfo, whose copies have new IDs, does not
/// list (`crate::config::Config::launch_namespace_mounts`). Only its size
/// shows in `Debug`, because a run logs its configuration at its start, and
/// mount IDs differ from run to run.
#[derive(Clone, Default, Eq, PartialEq)]
pub struct LaunchNamespaceMounts(pub Vec<(u64, String)>);

impl std::fmt::Debug for LaunchNamespaceMounts {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LaunchNamespaceMounts")
            .field("mounts", &self.0.len())
            .finish()
    }
}

/// Whether `maps`, the text of a process's `/proc/<pid>/maps`, maps the file
/// `exe` (the target of its `/proc/<pid>/exe` link) with inode number `ino`.
/// Detcore knows a process as the root it expects when its image is the
/// file the root's exec ran, and both texts come from procfs, which names
/// the file and its inode without asking the file's filesystem. Inode 0 is
/// an anonymous mapping's, never a file's.
pub fn maps_name_image(maps: &str, exe: &[u8], ino: u64) -> bool {
    ino != 0
        && maps.lines().any(|line| {
            // address, perms, offset, dev, inode, then the pathname, which may
            // hold spaces.
            let mut rest = line;
            let mut fields = Vec::with_capacity(5);
            for _ in 0..5 {
                rest = rest.trim_start();
                let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                fields.push(&rest[..end]);
                rest = &rest[end..];
            }
            fields[4].parse::<u64>().is_ok_and(|inode| inode == ino)
                && rest.trim_start().as_bytes() == exe
        })
}

impl StdioKind {
    /// The kind of an open descriptor whose `st_mode` is `mode`, with
    /// `terminal` whether it is a terminal (`isatty`).
    pub fn from_mode(mode: libc::mode_t, terminal: bool) -> Self {
        match mode & libc::S_IFMT {
            libc::S_IFREG => Self::File,
            libc::S_IFIFO => Self::Pipe,
            libc::S_IFSOCK => Self::Socket,
            libc::S_IFCHR if terminal => Self::Tty,
            libc::S_IFCHR => Self::CharDevice,
            libc::S_IFBLK => Self::BlockDevice,
            libc::S_IFDIR => Self::Directory,
            _ => Self::Other,
        }
    }

    /// Whether the character device numbered `rdev` belongs to a terminal
    /// driver, by `drivers`, the kernel's list of them (the text of
    /// `/proc/tty/drivers`: one driver per line, ending with its major
    /// number, its minor number or range, and its type). Detcore classifies
    /// a guest's descriptor without opening it ([`Self::of_guest_descriptor`]),
    /// so it cannot ask `isatty`.
    pub fn is_terminal_device(rdev: u64, drivers: &str) -> bool {
        // glibc's `gnu_dev_major` and `gnu_dev_minor`.
        let major = ((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff);
        let minor = (rdev & 0xff) | ((rdev >> 12) & !0xff);
        drivers.lines().any(|line| {
            let mut fields = line.split_whitespace().rev();
            let (Some(_kind), Some(minors), Some(driver_major)) =
                (fields.next(), fields.next(), fields.next())
            else {
                return false;
            };
            let (first, last) = minors.split_once('-').unwrap_or((minors, minors));
            driver_major.parse() == Ok(major)
                && first.parse().is_ok_and(|first: u64| first <= minor)
                && last.parse().is_ok_and(|last: u64| minor <= last)
        })
    }

    /// The kind of one of a guest's descriptors, from what Detcore can read
    /// about it without opening it and without an operation on its file's
    /// filesystem that could wait on a server. `link` is the target of its
    /// `/proc/<pid>/fd/<n>` link: a name that is no path is one only the
    /// kernel gives, to a pipe (`pipe:[ino]`), a socket (`socket:[ino]`) or
    /// another pseudo-filesystem object (`anon_inode:...`, `<type>:[ino]`),
    /// and tells the kind alone. A path never does, since the guest's own
    /// mounts decide what is at it (a regular file bound over `/dev/null` has
    /// the same link): `fdinfo` (the text of `/proc/<pid>/fdinfo/<n>`) names
    /// the file's mount, and `mounts` (mount IDs and filesystem types,
    /// [`mount_types`], of the guest's namespace, the coordinator's, and the
    /// one Hermit was launched in) its type. On one of
    /// [`LOCAL_KERNEL_FILESYSTEMS`], `stat` reads the file's own type through
    /// the link (`st_mode` and `st_rdev`), and that decides: a character
    /// device is a terminal when a driver in `drivers` (the text of
    /// `/proc/tty/drivers`) owns its device number. Anything else is
    /// [`Self::Unknown`], unread: a mount outside that list, no mount ID, a
    /// mount in neither namespace, a failed `stat`, or a character device
    /// without the driver list.
    pub fn of_guest_descriptor(
        link: &[u8],
        fdinfo: Option<&str>,
        mounts: &[(u64, String)],
        drivers: Option<&str>,
        stat: impl FnOnce() -> Option<(libc::mode_t, u64)>,
    ) -> Self {
        if !link.starts_with(b"/") {
            return if link.starts_with(b"pipe:[") {
                Self::Pipe
            } else if link.starts_with(b"socket:[") {
                Self::Socket
            } else {
                Self::Other
            };
        }
        let local = fdinfo.is_some_and(|fdinfo| {
            let Some(mount) = fdinfo
                .lines()
                .find_map(|line| line.strip_prefix("mnt_id:"))
                .and_then(|id| id.trim().parse::<u64>().ok())
            else {
                return false;
            };
            mounts.iter().any(|(id, fstype)| {
                *id == mount && LOCAL_KERNEL_FILESYSTEMS.contains(&fstype.as_str())
            })
        });
        if !local {
            return Self::Unknown;
        }
        let Some((mode, rdev)) = stat() else {
            return Self::Unknown;
        };
        if mode & libc::S_IFMT == libc::S_IFCHR {
            return match drivers {
                Some(drivers) => Self::from_mode(mode, Self::is_terminal_device(rdev, drivers)),
                None => Self::Unknown,
            };
        }
        Self::from_mode(mode, false)
    }

    fn describe(self) -> &'static str {
        match self {
            Self::File => "a regular file",
            Self::Pipe => "a pipe",
            Self::Socket => "a socket",
            Self::Tty => "a terminal",
            Self::CharDevice => "a character device",
            Self::BlockDevice => "a block device",
            Self::Directory => "a directory",
            Self::Closed => "closed",
            Self::Other => "a file of another kind (such as an anonymous inode)",
            Self::Unknown => "of a kind not read",
        }
    }
}

/// The kinds of file behind the guest's stdin, stdout and stderr when a run
/// began.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StdioKinds {
    pub stdin: StdioKind,
    pub stdout: StdioKind,
    pub stderr: StdioKind,
}

impl StdioKinds {
    /// The descriptors whose kind differs between `recorded` and `now`, in
    /// descriptor order. A descriptor whose kind either run did not read
    /// ([`StdioKind::Unknown`]) is never named: nothing says it changed.
    pub fn changes(recorded: &Self, now: &Self) -> Vec<StdioChange> {
        [
            (0, recorded.stdin, now.stdin),
            (1, recorded.stdout, now.stdout),
            (2, recorded.stderr, now.stderr),
        ]
        .into_iter()
        .filter(|(_, recorded, now)| {
            recorded != now && *recorded != StdioKind::Unknown && *now != StdioKind::Unknown
        })
        .map(|(fd, recorded, now)| StdioChange { fd, recorded, now })
        .collect()
    }
}

/// A standard descriptor whose kind differs between a recorded run and the
/// run that reproduces it ([`StdioKinds::changes`]).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StdioChange {
    /// 0, 1 or 2.
    pub fd: u8,
    pub recorded: StdioKind,
    pub now: StdioKind,
}

impl std::fmt::Display for StdioChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self.fd {
            0 => "stdin",
            1 => "stdout",
            _ => "stderr",
        };
        write!(
            f,
            "{name} (fd {}) was {} in the recording and is {} now; the run may diverge where the guest checks it (fstat, isatty, fsync, lseek)",
            self.fd,
            self.recorded.describe(),
            self.now.describe()
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
            stdio: None,
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

    /// A descriptor's kind follows its mode, and a character device is a
    /// terminal only when it says so. A replay names each descriptor whose
    /// kind changed, in descriptor order. A set without stdio kinds is
    /// written as before, and one with them round-trips.
    /// A guest descriptor's kind comes from its link's name only where that
    /// is no path, with no `stat`. A path is stat'ed only when its mount is
    /// one of the local, in-kernel filesystems (here by injected mountinfo
    /// texts), and the file's own type decides, whatever the path says: a
    /// regular file bound over `/dev/null` or `/dev/pts/3` is `file`, the
    /// real devices are `char_device` and `tty`. A file on NFS, CIFS, 9p,
    /// Ceph, FUSE, overlay or a type not listed, a `/dev/null` on overlay
    /// among them, is `unknown`, unstat'ed; so are a mount in neither
    /// namespace, a missing mount ID, a failed `stat`, and a character device
    /// without the driver list.
    #[test]
    fn a_guest_descriptor_is_statted_only_on_a_local_kernel_filesystem() {
        let guest_mounts = "\
22 1 0:21 / /proc rw,nosuid shared:5 - proc proc rw
23 29 0:5 / /dev rw,nosuid shared:2 - devtmpfs devtmpfs rw,size=4096k
24 23 0:22 / /dev/pts rw,nosuid,noexec shared:3 - devpts devpts rw,gid=5,mode=620
40 29 0:45 / /mnt/remote rw,nosuid,nodev shared:30 - fuse.sshfs host:/ rw,user_id=0
41 29 0:46 / /mnt/plain rw - fuse fuse rw
42 29 8:1 / /mnt/disk rw - fuseblk /dev/sda1 rw
43 29 0:47 / /mnt/nfs rw shared:31 - nfs4 server:/export rw,vers=4.2
44 29 0:48 / /mnt/smb rw - cifs //server/share rw
45 29 0:49 / /mnt/9p rw - 9p hostshare rw,trans=virtio
46 29 0:50 / / rw - overlay overlay rw,lowerdir=/l,upperdir=/u,workdir=/w
47 29 0:51 / /mnt/ceph rw - ceph mon:/ rw
48 29 0:52 / /sys/fs/fuse/connections rw - fusectl fusectl rw
49 29 0:53 / /dev/shm rw - tmpfs tmpfs rw
";
        let host_mounts = "29 1 8:2 / / rw,relatime shared:1 - ext4 /dev/sda2 rw\n";
        let mut mounts = mount_types(guest_mounts);
        mounts.extend(mount_types(host_mounts));
        assert_eq!(mounts.len(), 14);
        assert!(mounts.contains(&(46, "overlay".to_owned())));
        let fdinfo = |id: u64| format!("pos:\t0\nflags:\t0100002\nmnt_id:\t{id}\nino:\t5\n");
        let no_stat =
            || -> Option<(libc::mode_t, u64)> { panic!("stat'ed a file it must not reach") };
        let classify = |link: &str, fdinfo: Option<&str>| {
            StdioKind::of_guest_descriptor(link.as_bytes(), fdinfo, &mounts, None, no_stat)
        };
        for (link, kind) in [
            ("pipe:[1234]", StdioKind::Pipe),
            ("socket:[99]", StdioKind::Socket),
            ("anon_inode:[eventfd]", StdioKind::Other),
            ("net:[4026531840]", StdioKind::Other),
        ] {
            assert_eq!(classify(link, None), kind, "{link}");
        }
        // Not stat'ed: a mount outside the list, whatever the path, a
        // container's /dev on overlay among them.
        for (link, mount) in [
            ("/dev/null", 46),
            ("/dev/pts/3", 46),
            ("/dev/console", 46),
            ("/mnt/remote/out.log", 40),
        ] {
            assert_eq!(
                classify(link, Some(&fdinfo(mount))),
                StdioKind::Unknown,
                "{link}"
            );
        }
        for id in 40..=48 {
            assert_eq!(
                classify("/mnt/remote/out.log", Some(&fdinfo(id))),
                StdioKind::Unknown,
                "mount {id}"
            );
        }
        // On a local, in-kernel filesystem, one stat decides.
        let pty = (136 << 8) | 3;
        let null = (1 << 8) | 3;
        let drivers = "pty_slave            /dev/pts      136 0-1048575 pty:slave\n";
        let stat_once =
            |link: &str, mount: u64, mode: libc::mode_t, rdev: u64, drivers: Option<&str>| {
                let mut stats = 0;
                let kind = StdioKind::of_guest_descriptor(
                    link.as_bytes(),
                    Some(&fdinfo(mount)),
                    &mounts,
                    drivers,
                    || {
                        stats += 1;
                        Some((mode, rdev))
                    },
                );
                assert_eq!(stats, 1, "{link}");
                kind
            };
        let file = libc::S_IFREG | 0o644;
        let chr = libc::S_IFCHR | 0o666;
        for (link, mount, mode, rdev, kind) in [
            ("/home/user/out.log", 29, file, 0, StdioKind::File),
            ("/dev/shm/out", 49, file, 0, StdioKind::File),
            ("/home/user/fifo", 29, libc::S_IFIFO, 0, StdioKind::Pipe),
            // The real devices.
            ("/dev/null", 23, chr, null, StdioKind::CharDevice),
            ("/dev/pts/3", 24, chr, pty, StdioKind::Tty),
            // Regular files bound over device paths, on ext4 and tmpfs.
            ("/dev/null", 29, file, 0, StdioKind::File),
            ("/dev/pts/3", 49, file, 0, StdioKind::File),
            // A device node at another path is what it is.
            ("/home/user/tty", 29, chr, pty, StdioKind::Tty),
        ] {
            assert_eq!(
                stat_once(link, mount, mode, rdev, Some(drivers)),
                kind,
                "{link} on {mount}"
            );
        }
        assert_eq!(
            stat_once("/dev/pts/3", 24, chr, pty, None),
            StdioKind::Unknown
        );
        assert_eq!(
            stat_once("/dev/null", 23, chr, null, None),
            StdioKind::Unknown
        );
        // No mount ID, a mount neither namespace lists, or a failed stat.
        assert_eq!(classify("/home/user/out.log", None), StdioKind::Unknown);
        assert_eq!(
            classify("/memfd:input (deleted)", Some(&fdinfo(7))),
            StdioKind::Unknown
        );
        assert_eq!(
            StdioKind::of_guest_descriptor(b"/x", Some(&fdinfo(29)), &mounts, None, || None),
            StdioKind::Unknown
        );
    }

    /// A descriptor whose kind either run did not read is never named as
    /// changed, whichever run it is, while two kinds both read still are,
    /// `other` among them; `unknown` keeps its name on the wire.
    #[test]
    fn an_unknown_stdio_kind_is_never_compared() {
        let read = StdioKinds {
            stdin: StdioKind::CharDevice,
            stdout: StdioKind::File,
            stderr: StdioKind::Other,
        };
        let unread = StdioKinds {
            stdin: StdioKind::Unknown,
            stdout: StdioKind::Unknown,
            stderr: StdioKind::Unknown,
        };
        assert!(StdioKinds::changes(&read, &unread).is_empty());
        assert!(StdioKinds::changes(&unread, &read).is_empty());
        assert!(StdioKinds::changes(&unread, &unread).is_empty());
        let now = StdioKinds {
            stdin: StdioKind::Unknown,
            stdout: StdioKind::Pipe,
            stderr: StdioKind::File,
        };
        assert_eq!(
            StdioKinds::changes(&read, &now),
            vec![
                StdioChange {
                    fd: 1,
                    recorded: StdioKind::File,
                    now: StdioKind::Pipe,
                },
                StdioChange {
                    fd: 2,
                    recorded: StdioKind::Other,
                    now: StdioKind::File,
                },
            ]
        );
        assert_eq!(
            serde_json::to_string(&StdioKind::Unknown).unwrap(),
            "\"unknown\""
        );
        assert_eq!(
            serde_json::from_str::<StdioKind>("\"other\"").unwrap(),
            StdioKind::Other
        );
    }

    /// A process is the expected root when its maps map the file its `exe`
    /// link names with the inode the root's exec ran, whatever spaces the
    /// path holds; another inode, another path, or no mapping is not it.
    #[test]
    fn the_root_is_known_by_its_mapped_image() {
        let maps = "\
55d0c8a00000-55d0c8a02000 r--p 00000000 00:2a 100483                     /usr/bin/echo
55d0c8a02000-55d0c8a06000 r-xp 00002000 00:2a 100483                     /usr/bin/echo
7f1e2c000000-7f1e2c022000 r--p 00000000 00:2a 160034633                  /usr/lib64/ld-linux-x86-64.so.2
7ffc1a000000-7ffc1a021000 rw-p 00000000 00:00 0                          [stack]
7f1e2d000000-7f1e2d001000 r-xp 00000000 00:2a 77                         /opt/my tools/run me (deleted)
";
        assert!(maps_name_image(maps, b"/usr/bin/echo", 100483));
        assert!(!maps_name_image(maps, b"/usr/bin/echo", 100484));
        assert!(!maps_name_image(maps, b"/usr/bin/true", 100483));
        assert!(maps_name_image(maps, b"/opt/my tools/run me (deleted)", 77));
        assert!(!maps_name_image(maps, b"[stack]", 0));
        assert!(!maps_name_image("", b"/usr/bin/echo", 100483));
    }

    /// A character device is a terminal when a terminal driver in the
    /// kernel's list owns its major number and its minor number: a pty slave,
    /// a serial port and the console are; /dev/null and a minor number past
    /// a driver's range are not.
    #[test]
    fn terminal_devices_follow_the_kernels_driver_list() {
        let drivers = "\
/dev/tty             /dev/tty        5       0 system:/dev/tty
/dev/console         /dev/console    5       1 system:console
/dev/vc/0            /dev/vc/0       4       0 system:vtmaster
serial               /dev/ttyS       4 64-95 serial
pty_slave            /dev/pts      136 0-1048575 pty:slave
";
        let device = |major: u64, minor: u64| {
            ((major & 0xfff) << 8)
                | ((major & !0xfff) << 32)
                | (minor & 0xff)
                | ((minor & !0xff) << 12)
        };
        for (major, minor, terminal) in [
            (136, 3, true),
            (136, 1_000_000, true),
            (4, 64, true),
            (4, 95, true),
            (4, 96, false),
            (5, 1, true),
            (1, 3, false),
        ] {
            assert_eq!(
                StdioKind::is_terminal_device(device(major, minor), drivers),
                terminal,
                "{major}:{minor}"
            );
        }
        assert!(!StdioKind::is_terminal_device(device(136, 3), ""));
        // A symbolic link an `O_PATH` descriptor names is no regular file.
        assert_eq!(StdioKind::from_mode(libc::S_IFLNK, false), StdioKind::Other);
    }

    #[test]
    fn stdio_kinds_name_each_changed_descriptor() {
        assert_eq!(
            StdioKind::from_mode(libc::S_IFREG | 0o644, false),
            StdioKind::File
        );
        assert_eq!(StdioKind::from_mode(libc::S_IFIFO, false), StdioKind::Pipe);
        assert_eq!(StdioKind::from_mode(libc::S_IFCHR, true), StdioKind::Tty);
        assert_eq!(
            StdioKind::from_mode(libc::S_IFCHR, false),
            StdioKind::CharDevice
        );
        let recorded = StdioKinds {
            stdin: StdioKind::CharDevice,
            stdout: StdioKind::Pipe,
            stderr: StdioKind::File,
        };
        let now = StdioKinds {
            stdin: StdioKind::Closed,
            stderr: StdioKind::Pipe,
            ..recorded
        };
        assert!(StdioKinds::changes(&recorded, &recorded).is_empty());
        let changes = StdioKinds::changes(&recorded, &now);
        assert_eq!(
            changes,
            vec![
                StdioChange {
                    fd: 0,
                    recorded: StdioKind::CharDevice,
                    now: StdioKind::Closed,
                },
                StdioChange {
                    fd: 2,
                    recorded: StdioKind::File,
                    now: StdioKind::Pipe,
                },
            ]
        );
        assert_eq!(
            changes[1].to_string(),
            "stderr (fd 2) was a regular file in the recording and is a pipe now; the run may \
             diverge where the guest checks it (fstat, isatty, fsync, lseek)"
        );
        let without = set(&[]);
        assert!(!serde_json::to_string(&without).unwrap().contains("stdio"));
        let with = HostInputSet {
            stdio: Some(now),
            ..set(&[])
        };
        let json = serde_json::to_string(&with).unwrap();
        assert!(json.contains(r#""stderr":"pipe""#), "{json}");
        assert_eq!(serde_json::from_str::<HostInputSet>(&json).unwrap(), with);
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

    /// A snapshot identity flattened into a host-input record, or a set
    /// entry, that also carries its own `content` (the hash of a file's
    /// bytes the run's end read) writes two distinct keys and reads back
    /// whole. With both under `content`, serde refused every such record as
    /// a duplicate field, and every replay of a recording with host inputs
    /// with it.
    #[test]
    fn a_flattened_identity_does_not_clash_with_a_records_own_content() {
        let snapshot = HostFileIdentity::of_snapshot(131072, 0x0123_4567_89ab_cdef);
        let plain: HostFileIdentity =
            serde_json::from_str(r#"{"dev":33,"ino":7,"size":100,"mtime_sec":1,"mtime_nsec":2}"#)
                .unwrap();
        let keys = |json: &str| -> serde_json::Map<String, serde_json::Value> {
            serde_json::from_str(json).unwrap()
        };
        for (identity, content) in [
            (snapshot, Some("ab".repeat(32))),
            (snapshot, None),
            (plain, Some("cd".repeat(32))),
            (plain, None),
        ] {
            let record = HostInputRecord {
                path: "/dev/__properties__/properties_serial".to_owned(),
                dtid: 3,
                syscall: 9,
                identity,
                content: content.clone(),
                exec: false,
                kernel_object: false,
            };
            let json = serde_json::to_string(&record).unwrap();
            let fields = keys(&json);
            assert_eq!(fields.contains_key("content"), content.is_some(), "{json}");
            assert!(fields.contains_key("snapshot_content"), "{json}");
            assert_eq!(
                serde_json::from_str::<HostInputRecord>(&json).unwrap(),
                record,
                "{json}"
            );
            // A set entry is strict (`deny_unknown_fields` beside the
            // flattened identity): it reads both keys too.
            let entry = HostInput {
                path: record.path.clone(),
                identity,
                content,
            };
            let json = serde_json::to_string(&entry).unwrap();
            assert_eq!(
                serde_json::from_str::<HostInput>(&json).unwrap(),
                entry,
                "{json}"
            );
            let unknown = json.replacen('{', r#"{"other":1,"#, 1);
            assert!(serde_json::from_str::<HostInput>(&unknown).is_err());
        }
        // The snapshot's hash is keyed as written, as a number.
        let record = HostInputRecord {
            path: "/dev/__properties__/properties_serial".to_owned(),
            dtid: 3,
            syscall: 9,
            identity: snapshot,
            content: None,
            exec: false,
            kernel_object: false,
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(
            json.contains(r#""snapshot_content":81985529216486895"#),
            "{json}"
        );
    }
}
