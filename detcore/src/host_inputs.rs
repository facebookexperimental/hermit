/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Writes a run's host-input records to its `host_input_log` when the run
//! ends (see `detcore_model::host_input`), sums them up as the run's
//! [`HostInputSet`] for its summary and its preemption record, and compares
//! the opens of a schedule replay against the recorded run's set
//! ([`ReplayHostInputs`]).
//!
//! The records are an observation for the verification layer, not Detcore
//! state: Detcore's openat handler reports each one to the global state
//! (`tool_global::record_host_input`), which keeps them for the run, and
//! nothing reads them back, no scheduling decision consults them, and nothing
//! the guest sees depends on them. They are written once, after the run, so
//! the log stays empty while any guest runs; a run that does not end normally
//! writes nothing, which leaves its divergence unexplained rather than
//! explained by partial evidence.
//!
//! Nothing here opens or reads a guest's file while any guest process runs.
//! Linux raises an inotify or fanotify event for every open, read and close
//! of a file, whoever makes it (`fsnotify_open`, `fsnotify_access` and
//! `fsnotify_close`, fs/namei.c, fs/read_write.c and fs/file_table.c), so a
//! guest watching the file's directory would see Hermit's reads among its
//! own. While the guest runs, Detcore takes only what raises no event: a
//! file's status, and the kernel's path for the guest's descriptor (a
//! `readlink` of `/proc/<tid>/fd/<n>`). The bytes, and a directory's names,
//! are read when the run has ended ([`content_at_end`]), and only where the
//! file still has the identity the guest's open found.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashSet;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;

use detcore_model::host_input::HOST_INPUT_CONTENT_LIMIT;
use detcore_model::host_input::HostFileIdentity;
use detcore_model::host_input::HostInput;
use detcore_model::host_input::HostInputChange;
use detcore_model::host_input::HostInputLogEnd;
use detcore_model::host_input::HostInputRecord;
use detcore_model::host_input::HostInputSet;
use detcore_model::host_input::HostMutationRecord;
use detcore_model::host_input::guest_rebound;
use detcore_model::host_input::host_input_changed;
use detcore_model::host_input::is_kernel_object;
use sha2::Digest;
use sha2::Sha256;
use tracing::warn;

/// Write `records` to `log`, one JSON line each, then one
/// [`HostMutationRecord`] line for each path in `mutated`, followed by a
/// [`HostInputLogEnd`] line naming how many lines precede it, so that a
/// reader can tell a complete log from a cut one.
pub(crate) fn write(log: &Path, records: &[HostInputRecord], mutated: &BTreeSet<String>) {
    let mut text = Vec::new();
    let end = HostInputLogEnd {
        records: (records.len() + mutated.len()) as u64,
    };
    let encoded = records
        .iter()
        .map(serde_json::to_vec)
        .chain(mutated.iter().map(|path| {
            serde_json::to_vec(&HostMutationRecord {
                mutated: path.clone(),
            })
        }))
        .chain(std::iter::once(serde_json::to_vec(&end)))
        .try_for_each(|line| {
            text.extend(line?);
            text.push(b'\n');
            Ok::<(), serde_json::Error>(())
        });
    let written = encoded
        .map_err(std::io::Error::other)
        .and_then(|()| std::fs::write(log, &text));
    if let Err(error) = written {
        warn!(
            "[detcore] cannot write the host-input log {}: {}",
            log.display(),
            error
        );
    }
}

/// The run's [`HostInputSet`]: for each path the guest opened, in path order,
/// the identity and bytes its first open found, leaving out kernel objects
/// and paths the guest rebound itself (`mutated`, or a relative path), and
/// the set's digest (`HostInputSet`).
pub(crate) fn input_set(records: &[HostInputRecord], mutated: &BTreeSet<String>) -> HostInputSet {
    let mut first: BTreeMap<&str, &HostInputRecord> = BTreeMap::new();
    for record in records {
        first.entry(record.path.as_str()).or_insert(record);
    }
    let files: Vec<HostInput> = first
        .into_iter()
        .filter(|(path, record)| {
            // An authenticated snapshot identity (`of_snapshot`, a content
            // hash Hermit itself computed for its frozen copy) is retained
            // even though its guest path lies under `/dev`: it names the
            // snapshot's bytes, not a device. Actual devices and kernel
            // objects carry no such identity and stay excluded.
            let snapshot = record.identity.content.is_some();
            (snapshot || (!record.kernel_object && !is_kernel_object(path)))
                && !guest_rebound(mutated, path)
        })
        .map(|(path, record)| HostInput {
            path: path.to_owned(),
            identity: record.identity,
            content: record.content.clone(),
        })
        .collect();
    // One line per file: its path (which holds no NUL), then its bytes'
    // digest where hashed, else its identity.
    let mut hasher = Sha256::new();
    for file in &files {
        hasher.update(file.path.as_bytes());
        hasher.update(b"\0");
        match &file.content {
            Some(content) => {
                hasher.update(b"content ");
                hasher.update(content.as_bytes());
            }
            None => {
                hasher.update(b"identity ");
                hasher.update(serde_json::to_vec(&file.identity).expect("an identity serializes"));
            }
        }
        hasher.update(b"\n");
    }
    HostInputSet {
        digest: hex(&hasher.finalize()),
        files,
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// What a run's end reads for the first open of `path` (the guest's path)
/// that found `identity`, through `source` (the kernel's path for the
/// guest's descriptor, or for an exec's image, at that open): the SHA-256 of
/// a regular file's bytes, when it still has that identity and holds at most
/// [`HOST_INPUT_CONTENT_LIMIT`] bytes, or of the names in a directory that is
/// still the same directory (device and inode; its size and times change
/// with every name made in it) as [`directory_digest`] lists them; `None`
/// otherwise, and for any other kind of file, which is never opened. Called
/// only when every guest process has exited (see the module documentation).
/// Raw syscalls only, as for [`read_digest`].
pub(crate) fn content_at_end(
    source: &Path,
    path: &str,
    identity: HostFileIdentity,
    mutated: &BTreeSet<String>,
) -> Option<String> {
    let stat = crate::util::raw_stat(source).ok()?;
    match stat.st_mode & libc::S_IFMT {
        libc::S_IFREG => {
            if HostFileIdentity::from_stat(&stat) != identity
                || u64::try_from(identity.size).map_or(true, |size| size > HOST_INPUT_CONTENT_LIMIT)
            {
                return None;
            }
            let file = crate::util::RawFd::open(source, libc::O_RDONLY | libc::O_NONBLOCK).ok()?;
            // The file opened is the one whose status was compared.
            (HostFileIdentity::from_stat(&file.stat().ok()?) == identity)
                .then(|| read_digest(&file))
                .flatten()
        }
        libc::S_IFDIR => {
            if (stat.st_dev, stat.st_ino) != (identity.dev, identity.ino) {
                return None;
            }
            let directory =
                crate::util::RawFd::open(source, libc::O_RDONLY | libc::O_DIRECTORY).ok()?;
            let opened = directory.stat().ok()?;
            ((opened.st_dev, opened.st_ino) == (identity.dev, identity.ino))
                .then(|| directory_digest(&directory, path, mutated))
                .flatten()
        }
        _ => None,
    }
}

/// The SHA-256 of the file open at `file`, read to its end but no further
/// than [`HOST_INPUT_CONTENT_LIMIT`] bytes, or `None` when it cannot be read
/// or is longer than that. Raw syscalls only: in-guest Detcore must not call
/// libc's wrappers, which the guest may define (`crate::util::raw_syscall`).
fn read_digest(file: &crate::util::RawFd) -> Option<String> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1 << 16];
    let mut read = 0_u64;
    loop {
        let count = file.read_at(&mut buffer, read).ok()?;
        if count == 0 {
            break;
        }
        read += count as u64;
        if read > HOST_INPUT_CONTENT_LIMIT {
            return None;
        }
        hasher.update(&buffer[..count]);
    }
    Some(hex(&hasher.finalize()))
}

/// The SHA-256 of the names in the directory open at `directory`, which the
/// guest names `path`: sorted, each followed by a NUL, after `directory\0`,
/// without `.` and `..` and without each name whose path the guest itself
/// rebound (`mutated`, [`guest_rebound`]), so that what the guest made or
/// removed in it (in its private `/tmp`, say) does not count. `None` when
/// the names are longer in all than [`HOST_INPUT_CONTENT_LIMIT`] bytes or
/// cannot be read. A directory's identity is not compared: Hermit makes some
/// afresh for each run (its private `/tmp`), and the guest sees only the
/// names in one, not its host inode or times.
fn directory_digest(
    directory: &crate::util::RawFd,
    path: &str,
    mutated: &BTreeSet<String>,
) -> Option<String> {
    let mut names = Vec::new();
    let mut total = 0_u64;
    let too_long = crate::util::find_in_open_directory(directory, |name| {
        total += name.len() as u64 + 1;
        if total > HOST_INPUT_CONTENT_LIMIT {
            return Some(());
        }
        let child = Path::new(path).join(name);
        if !guest_rebound(mutated, &child.to_string_lossy()) {
            names.push(name.as_bytes().to_vec());
        }
        None
    })
    .ok()?;
    if too_long.is_some() {
        return None;
    }
    names.sort();
    let mut hasher = Sha256::new();
    hasher.update(b"directory\0");
    for name in &names {
        hasher.update(name);
        hasher.update(b"\0");
    }
    Some(hex(&hasher.finalize()))
}

/// A file mapped in a process, as a `/proc/<pid>/maps` line shows it: its
/// name, the inode number of the object mapped, and the mapping's address
/// range as the line spells it, which names the mapping's
/// `/proc/<pid>/map_files` link.
#[derive(Debug, PartialEq)]
struct MappedFile {
    path: PathBuf,
    ino: u64,
    range: String,
}

impl MappedFile {
    /// Whether `stat`, taken through [`Self::path`], is of the object mapped.
    ///
    /// Where the tool may follow the mapping's `map_files` link (`mapped`, a
    /// stat through it; Linux allows that only to a holder of
    /// CAP_CHECKPOINT_RESTORE or CAP_SYS_ADMIN in the initial user
    /// namespace), the two have the same device and inode, which keeps the
    /// filesystem and, on btrfs, the subvolume.
    ///
    /// Otherwise the inode number is all that names the object mapped: a
    /// maps line shows its superblock's device (`show_map_vma`), while btrfs
    /// reports a subvolume's own device in a stat (`btrfs_getattr`), so the
    /// devices differ for the same file. The mapping holds the mapped inode,
    /// so a file that replaced it at the path has another number on that
    /// filesystem; what the number cannot rule out is another filesystem or
    /// subvolume mounted over the path with an inode of the same number. On
    /// overlayfs a maps line names the underlying file, whose number a stat
    /// through the overlay need not report, so the object may not be
    /// identified there.
    fn identifies(&self, stat: &libc::stat, mapped: Option<&libc::stat>) -> bool {
        match mapped {
            Some(mapped) => (mapped.st_dev, mapped.st_ino) == (stat.st_dev, stat.st_ino),
            None => stat.st_ino == self.ino,
        }
    }
}

/// The file mapped at `address` in a process whose `/proc/<pid>/maps` is
/// `maps`: the mapping's pathname field, when it names a file (absolute, and
/// not deleted since it was mapped), and its inode field. Linux shows the
/// name from the reader's root, and the inode of the file mapped
/// (`show_map_vma`, fs/proc/task_mmu.c).
fn mapped_file(maps: &[u8], address: u64) -> Option<MappedFile> {
    maps.split(|byte| *byte == b'\n').find_map(|line| {
        let line = std::str::from_utf8(line).ok()?;
        // address perms offset dev inode pathname: the pathname is the rest of
        // the line after the fifth field, and may hold spaces.
        let mut fields = line.splitn(6, ' ');
        let range = fields.next()?;
        let (start, end) = range.split_once('-')?;
        let (start, end) = (
            u64::from_str_radix(start, 16).ok()?,
            u64::from_str_radix(end, 16).ok()?,
        );
        if !(start..end).contains(&address) {
            return None;
        }
        let ino = fields.nth(3)?.parse().ok()?;
        let path = fields.next()?.trim_start();
        (path.starts_with('/') && !path.ends_with(" (deleted)")).then(|| MappedFile {
            path: PathBuf::from(path),
            ino,
            range: range.to_owned(),
        })
    })
}

impl<T: crate::record_or_replay::RecordOrReplay> crate::Detcore<T> {
    /// Report the files the kernel opened for the exec this image came from
    /// to the global state's host-input records ([`HostInputRecord::exec`]),
    /// each with its path in the process's root, its identity now, at the
    /// exec, and where the tool finds it for the run's end to read its bytes
    /// ([`content_at_end`], which reads only a file that still has that
    /// identity):
    /// - the image, through the kernel's own `exe` link; for a `#!` script,
    ///   the interpreter the kernel finally ran, not the script;
    /// - its program interpreter, if it has one: the file mapped where the
    ///   auxiliary vector's `AT_BASE` says the kernel loaded it, by the name
    ///   `/proc/<tid>/maps` shows ([`mapped_file`]), when a stat through that
    ///   name finds the object mapped: the device and inode a stat through
    ///   the mapping's `map_files` link finds, where the tool may follow it,
    ///   and otherwise the inode number the maps line names
    ///   ([`MappedFile::identifies`]). Otherwise the mapped object cannot be
    ///   identified and nothing is recorded for it, rather than a file that
    ///   replaced it at that name since (or, on overlayfs, an overlay file
    ///   that does not report the underlying number).
    ///
    /// Both identities are the exec's: a file replaced while the run goes on
    /// keeps the identity the process ran with, as a file the guest opens
    /// keeps the one its open found. Where the tool may not follow
    /// `map_files`, only the interpreter's inode number is compared, so an
    /// inode of the same number that another filesystem or btrfs subvolume
    /// exposes at the name cannot be told apart. Nothing is opened but the thread's own
    /// procfs files (the `auxv` and `maps` files, read as
    /// `Self::check_brk_model` reads `auxv`), and the image and its
    /// interpreter only have their status taken (see the module
    /// documentation). Nothing where the tool cannot see the thread's procfs
    /// directory. Raw syscalls only, as for [`read_digest`].
    pub(crate) async fn record_exec_host_inputs<G: reverie::Guest<Self>>(&self, guest: &mut G) {
        let Some(task) = self.guest_task_directory(guest) else {
            return;
        };
        let Ok(root) = crate::util::raw_read_link(&task.join("root")) else {
            return;
        };
        let exe = task.join("exe");
        let Ok(source) = crate::util::raw_read_link(&exe) else {
            return;
        };
        let Some(image) = source
            .strip_prefix(&root)
            .ok()
            .map(|image| Path::new("/").join(image))
        else {
            return;
        };
        let Ok(stat) = crate::util::raw_stat(&exe) else {
            return;
        };
        let mut files = vec![(image, source, stat)];
        if let Some(mapped) = crate::util::raw_read_file(&task.join("auxv"))
            .ok()
            .and_then(|auxv| crate::exec_record::auxv_entry(&auxv, libc::AT_BASE))
            .filter(|base| *base != 0)
            .and_then(|base| {
                mapped_file(&crate::util::raw_read_file(&task.join("maps")).ok()?, base)
            })
            && let Ok(stat) = crate::util::raw_stat(&mapped.path)
            && mapped.identifies(
                &stat,
                crate::util::raw_stat(&task.join("map_files").join(&mapped.range))
                    .ok()
                    .as_ref(),
            )
            && let Ok(relative) = mapped.path.strip_prefix(&root)
        {
            files.push((Path::new("/").join(relative), mapped.path.clone(), stat));
        }
        for (path, source, stat) in files {
            crate::tool_global::record_host_input(
                guest,
                path.to_string_lossy().into_owned(),
                HostFileIdentity::from_stat(&stat),
                source.to_str().map(str::to_owned),
                stat.st_mode & libc::S_IFMT == libc::S_IFREG,
                true,
                false,
            )
            .await;
        }
    }
}

/// A schedule replay's comparison of the host files its guest opens against
/// the recorded run's [`HostInputSet`]. Each path is compared once, for its
/// first open in this run; a path the recorded set does not hold, a kernel
/// object, or one the guest has rebound by then is not compared. A change
/// the open's status already proves is reported at the open
/// ([`Self::observe`]): another identity where the recording has no content,
/// another size of a regular file. Any other is decided by the content the
/// run's end reads ([`Self::finish`]), as [`host_input_changed`] decides.
#[derive(Debug)]
pub(crate) struct ReplayHostInputs {
    recorded: HostInputSet,
    opened: HashSet<String>,
    /// First opens left for [`Self::finish`]: the path and the identity the
    /// open found.
    pending: Vec<(String, HostFileIdentity)>,
    changes: Vec<HostInputChange>,
}

impl ReplayHostInputs {
    pub(crate) fn new(recorded: HostInputSet) -> Self {
        let recorded = recorded.normalized();
        Self {
            recorded,
            opened: HashSet::new(),
            pending: Vec::new(),
            changes: Vec::new(),
        }
    }

    /// Compare an open of `path` that found `identity` (`regular` for a
    /// regular file; `kernel_object` when the descriptor's own path is a
    /// kernel object), with `mutated` the paths the guest has rebound so
    /// far. Returns the change when this first open of the path already
    /// shows a different file than the recording: where the recording holds
    /// no content its identity decides, and a regular file of another size
    /// holds other bytes whatever either run could read. Otherwise the path
    /// waits for [`Self::finish`].
    pub(crate) fn observe(
        &mut self,
        path: &str,
        identity: HostFileIdentity,
        regular: bool,
        kernel_object: bool,
        mutated: &BTreeSet<String>,
    ) -> Option<&HostInputChange> {
        // A recorded authenticated snapshot identity is compared even
        // though its path lies under `/dev` (see `input_set`); an actual
        // device has no recorded entry and stays skipped here.
        let snapshot = identity.content.is_some()
            || self
                .recorded
                .file(path)
                .is_some_and(|file| file.identity.content.is_some());
        if self.opened.contains(path) {
            return None;
        }
        // The descriptor-path `kernel_object` bit is a path heuristic: a
        // regular snapshot copy under `/dev/shm` (a permitted `TMPDIR`)
        // gets it from its copy path, yet its authenticated content hash
        // (`of_snapshot`) is stronger evidence than the path. Mirror
        // `input_set`: a snapshot identity is compared even when the bit
        // is set; an actual device has no snapshot identity and stays
        // excluded by either check.
        if (!snapshot && (kernel_object || is_kernel_object(path))) || guest_rebound(mutated, path)
        {
            return None;
        }
        self.opened.insert(path.to_owned());
        let recorded = self.recorded.file(path)?;
        let proven = match recorded.content {
            None => recorded.identity != identity,
            Some(_) => regular && recorded.identity.size != identity.size,
        };
        if !proven {
            self.pending.push((path.to_owned(), identity));
            return None;
        }
        self.changes.push(HostInputChange {
            path: path.to_owned(),
            recorded: recorded.identity,
            now: identity,
        });
        self.changes.last()
    }

    /// Decide the opens [`Self::observe`] left, once the run has ended, with
    /// `content` the content the run's end read for a path's first open
    /// ([`content_at_end`]) and `mutated` every path the guest rebound in
    /// the run, which the run's own set leaves out too. Returns the changes
    /// this finds, in the order of their first opens, after those reported
    /// at the opens.
    pub(crate) fn finish(
        &mut self,
        mut content: impl FnMut(&str) -> Option<String>,
        mutated: &BTreeSet<String>,
    ) -> &[HostInputChange] {
        let found = self.changes.len();
        for (path, identity) in std::mem::take(&mut self.pending) {
            if guest_rebound(mutated, &path) {
                continue;
            }
            let Some(recorded) = self.recorded.file(&path) else {
                continue;
            };
            let now = HostInput {
                content: content(&path),
                path,
                identity,
            };
            if host_input_changed(recorded, &now) {
                self.changes.push(HostInputChange {
                    path: now.path,
                    recorded: recorded.identity,
                    now: identity,
                });
            }
        }
        &self.changes[found..]
    }

    /// A captured input's private copy could not be checked at shutdown.
    /// Do not let its stable origin hide that loss of evidence: the existing
    /// identity-only fallback must compare the physical object instead. Only
    /// the first open awaiting settlement is changed; changes already found
    /// by `observe` remain reported.
    pub(crate) fn restore_snapshot_identity(&mut self, path: &str, physical: HostFileIdentity) {
        if let Some((_, identity)) = self.pending.iter_mut().find(|(pending, _)| pending == path) {
            *identity = physical;
        }
    }

    /// The changes found so far: those reported at their opens, in the order
    /// of those opens, then those [`Self::finish`] found.
    pub(crate) fn changes(&self) -> &[HostInputChange] {
        &self.changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(ino: u64) -> HostFileIdentity {
        HostFileIdentity {
            dev: 33,
            ino,
            size: 100 + ino as i64,
            mtime_sec: 1_700_000_000,
            mtime_nsec: 0,
            content: None,
        }
    }

    fn open(path: &str, ino: u64, syscall: u64) -> HostInputRecord {
        HostInputRecord {
            path: path.to_owned(),
            dtid: 3,
            syscall,
            identity: identity(ino),
            content: None,
            exec: false,
            kernel_object: false,
        }
    }

    #[test]
    fn the_set_keeps_first_opens_of_host_files_in_path_order() {
        let records = [
            open("/etc/ld.so.cache", 9, 6),
            open("/proc/self/maps", 1, 7),
            open("/lib64/libc.so.6", 10, 8),
            open("/etc/ld.so.cache", 11, 9),
            open("/work/out", 12, 10),
            open("relative", 13, 11),
            // `/etc/mtab` names `/proc/self/mounts`.
            HostInputRecord {
                kernel_object: true,
                ..open("/etc/mtab", 14, 12)
            },
        ];
        let mutated = BTreeSet::from(["/work".to_owned()]);
        let set = input_set(&records, &mutated);
        let files: Vec<(&str, u64)> = set
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.identity.ino))
            .collect();
        assert_eq!(files, [("/etc/ld.so.cache", 9), ("/lib64/libc.so.6", 10)]);
        assert_eq!(set.digest.len(), 64);
        assert!(set.digest.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    /// An authenticated snapshot identity survives `input_set` and replay
    /// comparison despite its `/dev` guest path: unchanged bytes report no
    /// change, changed bytes report one, and a real device stays excluded.
    #[test]
    fn a_snapshot_identity_survives_the_set_and_replay_comparison() {
        let none = BTreeSet::new();
        let snapshot = |hash: u64| HostInputRecord {
            identity: HostFileIdentity::of_snapshot(64, hash),
            ..open("/dev/__properties__/properties_serial", 1, 6)
        };
        let device = HostInputRecord {
            kernel_object: true,
            ..open("/dev/null", 2, 7)
        };
        let recorded = input_set(&[snapshot(0xaa), device.clone()], &none);
        assert_eq!(recorded.files.len(), 1);
        assert_eq!(
            recorded.files[0].path,
            "/dev/__properties__/properties_serial"
        );
        assert_eq!(
            recorded.files[0].identity,
            HostFileIdentity::of_snapshot(64, 0xaa)
        );
        // Different snapshot bytes give a different persisted digest.
        let changed_set = input_set(&[snapshot(0xbb), device.clone()], &none);
        assert_ne!(recorded.digest, changed_set.digest);
        // Unchanged: no change at the open or at the end.
        let mut replay = ReplayHostInputs::new(recorded.clone());
        assert_eq!(
            replay.observe(
                "/dev/__properties__/properties_serial",
                HostFileIdentity::of_snapshot(64, 0xaa),
                true,
                false,
                &none,
            ),
            None
        );
        assert_eq!(replay.finish(|_| None, &none), []);
        assert!(replay.changes().is_empty());
        // The `/dev/null` control is neither in the set nor compared.
        assert_eq!(
            replay.observe("/dev/null", identity(2), false, true, &none),
            None
        );
        // Changed: reported at the open with both hashes.
        let mut replay = ReplayHostInputs::new(recorded.clone());
        let change = replay
            .observe(
                "/dev/__properties__/properties_serial",
                HostFileIdentity::of_snapshot(64, 0xbb),
                true,
                false,
                &none,
            )
            .expect("a changed snapshot hash is reported");
        assert_eq!(change.recorded.content, Some(0xaa));
        assert_eq!(change.now.content, Some(0xbb));
        // The producer case: a regular copy under `/dev/shm` (a permitted
        // `TMPDIR` below `/dev`) carries `kernel_object=true` from its
        // descriptor path, in the persisted record and at replay. The
        // authenticated snapshot identity must survive both, while an
        // actual device with the bit set stays excluded.
        let shm_snapshot = HostInputRecord {
            kernel_object: true,
            ..snapshot(0xaa)
        };
        let recorded = input_set(&[shm_snapshot, device.clone()], &none);
        assert_eq!(recorded.files.len(), 1, "true-bit snapshot is retained");
        assert_eq!(
            recorded.files[0].identity,
            HostFileIdentity::of_snapshot(64, 0xaa)
        );
        let mut replay = ReplayHostInputs::new(recorded.clone());
        assert_eq!(
            replay.observe(
                "/dev/__properties__/properties_serial",
                HostFileIdentity::of_snapshot(64, 0xaa),
                true,
                true,
                &none,
            ),
            None,
            "unchanged true-bit snapshot reports no change"
        );
        assert_eq!(replay.finish(|_| None, &none), []);
        assert!(replay.changes().is_empty());
        let mut replay = ReplayHostInputs::new(recorded);
        let change = replay
            .observe(
                "/dev/__properties__/properties_serial",
                HostFileIdentity::of_snapshot(64, 0xbb),
                true,
                true,
                &none,
            )
            .expect("a changed true-bit snapshot hash is reported");
        assert_eq!(change.recorded.content, Some(0xaa));
        assert_eq!(change.now.content, Some(0xbb));
    }

    #[test]
    fn the_digest_follows_the_files_alone() {
        let none = BTreeSet::new();
        let first = input_set(&[open("/a", 1, 1), open("/b", 2, 2)], &none);
        let reordered = input_set(&[open("/b", 2, 5), open("/a", 1, 6)], &none);
        let changed = input_set(&[open("/a", 1, 1), open("/b", 3, 2)], &none);
        assert_eq!(first, reordered);
        assert_ne!(first.digest, changed.digest);
        assert_ne!(first.digest, input_set(&[], &none).digest);
        // Where the bytes were hashed they decide, as `host_input_changed`
        // decides: a new inode with the same bytes keeps the digest, other
        // bytes under the same inode change it.
        let hashed = |ino: u64, content: &str| HostInputRecord {
            content: Some(content.to_owned()),
            ..open("/b", ino, 2)
        };
        let before = input_set(&[open("/a", 1, 1), hashed(2, "aa")], &none);
        let rewritten = input_set(&[open("/a", 1, 1), hashed(3, "aa")], &none);
        let edited = input_set(&[open("/a", 1, 1), hashed(2, "bb")], &none);
        assert_eq!(before.digest, rewritten.digest);
        assert_ne!(before.digest, edited.digest);
    }

    #[test]
    fn a_replay_reports_each_changed_file_once_at_its_first_open() {
        let none = BTreeSet::new();
        let recorded = input_set(
            &[open("/etc/ld.so.cache", 9, 6), open("/lib", 10, 7)],
            &none,
        );
        let mut replay = ReplayHostInputs::new(recorded);
        assert_eq!(
            replay.observe("/etc/ld.so.cache", identity(11), true, false, &none),
            Some(&HostInputChange {
                path: "/etc/ld.so.cache".to_owned(),
                recorded: identity(9),
                now: identity(11),
            })
        );
        // A later open of the same path is not compared again.
        assert_eq!(
            replay.observe("/etc/ld.so.cache", identity(12), true, false, &none),
            None
        );
        // An unchanged file, one the recording did not open, and a kernel
        // object are not reported.
        assert_eq!(
            replay.observe("/lib", identity(10), false, false, &none),
            None
        );
        assert_eq!(
            replay.observe("/new", identity(13), true, false, &none),
            None
        );
        assert_eq!(
            replay.observe("/proc/self/maps", identity(14), true, false, &none),
            None
        );
        // Unhashed in the recording: nothing is left for the run's end.
        assert_eq!(replay.finish(|_| Some("aa".to_owned()), &none), []);
        assert_eq!(replay.changes().len(), 1);
    }

    /// Where the recording hashed a file, an open whose status cannot prove
    /// a change leaves it to the content the run's end reads: a new inode
    /// with the same bytes is no change, the same inode with other bytes is
    /// one. A regular file of another size is reported at its open.
    #[test]
    fn a_replay_compares_hashed_content_before_identity() {
        let none = BTreeSet::new();
        let hashed = |path: &str, ino: u64, content: &str| HostInputRecord {
            content: Some(content.to_owned()),
            ..open(path, ino, 6)
        };
        let recorded = input_set(
            &[
                hashed("/etc/ld.so.cache", 9, "aa"),
                hashed("/etc/hosts", 10, "cc"),
                hashed("/etc/passwd", 11, "dd"),
                hashed("/etc", 12, "ee"),
            ],
            &none,
        );
        let mut replay = ReplayHostInputs::new(recorded);
        let resized = HostFileIdentity {
            size: 1,
            ..identity(11)
        };
        // Rewritten under a new inode, edited in place, resized.
        assert_eq!(
            replay.observe(
                "/etc/ld.so.cache",
                HostFileIdentity {
                    ino: 13,
                    ..identity(9)
                },
                true,
                false,
                &none
            ),
            None
        );
        assert_eq!(
            replay.observe("/etc/hosts", identity(10), true, false, &none),
            None
        );
        assert!(
            replay
                .observe("/etc/passwd", resized, true, false, &none)
                .is_some()
        );
        // A directory's size says nothing about its names.
        let directory = HostFileIdentity {
            size: 1,
            ..identity(14)
        };
        assert_eq!(replay.observe("/etc", directory, false, false, &none), None);
        let content = |path: &str| {
            match path {
                "/etc/ld.so.cache" => Some("aa"),
                "/etc/hosts" => Some("ff"),
                "/etc" => Some("ee"),
                _ => None,
            }
            .map(str::to_owned)
        };
        let found: Vec<&str> = replay
            .finish(content, &none)
            .iter()
            .map(|change| change.path.as_str())
            .collect();
        assert_eq!(found, ["/etc/hosts"]);
        assert_eq!(replay.changes().len(), 2);

        // Unread at the end (changed meanwhile, or too large): the identity
        // decides. A recorded set out of path order (a hand-edited file) is
        // searched sorted, and an open whose descriptor is a kernel object
        // is not compared.
        let mut unsorted = input_set(
            &[
                hashed("/a", 1, "aa"),
                hashed("/b", 2, "bb"),
                hashed("/c", 3, "cc"),
            ],
            &none,
        );
        unsorted.files.reverse();
        let mut replay = ReplayHostInputs::new(unsorted);
        let moved = HostFileIdentity {
            ino: 5,
            ..identity(1)
        };
        assert_eq!(replay.observe("/a", moved, true, false, &none), None);
        assert_eq!(replay.observe("/b", identity(6), true, true, &none), None);
        assert_eq!(replay.observe("/c", identity(3), true, false, &none), None);
        let found: Vec<&str> = replay
            .finish(|_| None, &none)
            .iter()
            .map(|change| change.path.as_str())
            .collect();
        assert_eq!(found, ["/a"]);
    }

    /// The run's end reads a regular file's bytes only while it has the
    /// identity the open found and no more than the limit, and a directory's
    /// names (not those the guest made or removed itself), whatever its size
    /// and times.
    #[test]
    fn the_end_reads_an_unchanged_file_and_a_directory_by_its_names() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("F");
        std::fs::write(&path, "input\n").unwrap();
        let none = BTreeSet::new();
        let found = HostFileIdentity::from_stat(&crate::util::raw_stat(&path).unwrap());
        // `printf 'input\n' | sha256sum`
        let expected = "7d3f9b6284c6f36e77b425cac882e8fbbcc97a4727ec20790853076d0f463453";
        assert_eq!(
            content_at_end(&path, "/F", found, &none).as_deref(),
            Some(expected)
        );
        // Another identity than the open found: not read.
        let other = HostFileIdentity {
            mtime_nsec: found.mtime_nsec ^ 1,
            ..found
        };
        assert_eq!(content_at_end(&path, "/F", other, &none), None);
        std::fs::write(&path, "other\n").unwrap();
        let rewritten = HostFileIdentity::from_stat(&crate::util::raw_stat(&path).unwrap());
        assert_ne!(
            content_at_end(&path, "/F", rewritten, &none).as_deref(),
            Some(expected)
        );
        let large = HostFileIdentity {
            size: HOST_INPUT_CONTENT_LIMIT as i64 + 1,
            ..rewritten
        };
        assert_eq!(content_at_end(&path, "/F", large, &none), None);
        assert_eq!(
            content_at_end(&directory.path().join("missing"), "/missing", found, &none),
            None
        );

        let listed = tempfile::tempdir().unwrap();
        std::fs::write(listed.path().join("b"), "").unwrap();
        std::fs::write(listed.path().join("a"), "").unwrap();
        let at =
            |listed: &Path| HostFileIdentity::from_stat(&crate::util::raw_stat(listed).unwrap());
        let names = content_at_end(listed.path(), "/tmp", at(listed.path()), &none)
            .expect("a directory's names");
        // A name the guest made itself does not count, so neither does the
        // directory's new size and times.
        std::fs::write(listed.path().join("made"), "").unwrap();
        let made = BTreeSet::from(["/tmp/made".to_owned()]);
        assert_eq!(
            content_at_end(listed.path(), "/tmp", at(listed.path()), &made).as_deref(),
            Some(names.as_str())
        );
        // One the host made does.
        assert_ne!(
            content_at_end(listed.path(), "/tmp", at(listed.path()), &none).as_deref(),
            Some(names.as_str())
        );
        // The same names elsewhere give the same digest; another directory
        // than the open found is not read.
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::write(elsewhere.path().join("a"), "").unwrap();
        std::fs::write(elsewhere.path().join("b"), "").unwrap();
        assert_eq!(
            content_at_end(elsewhere.path(), "/tmp", at(elsewhere.path()), &none).as_deref(),
            Some(names.as_str())
        );
        assert_eq!(
            content_at_end(elsewhere.path(), "/tmp", at(listed.path()), &none),
            None
        );
    }

    #[test]
    fn a_replay_does_not_report_a_file_the_guest_rebound_first() {
        let recorded = input_set(&[open("/work/in", 9, 6)], &BTreeSet::new());
        let mut replay = ReplayHostInputs::new(recorded);
        let mutated = BTreeSet::from(["/work".to_owned()]);
        assert_eq!(
            replay.observe("/work/in", identity(11), true, false, &mutated),
            None
        );
        assert!(replay.changes().is_empty());
        // Nor one rebound after its first open, which the run's end leaves
        // out as the run's own set does.
        let mut hashed = open("/work/in", 9, 6);
        hashed.content = Some("aa".to_owned());
        let mut replay = ReplayHostInputs::new(input_set(&[hashed], &BTreeSet::new()));
        let none = BTreeSet::new();
        assert_eq!(
            replay.observe("/work/in", identity(9), true, false, &none),
            None
        );
        assert_eq!(replay.finish(|_| Some("bb".to_owned()), &mutated), []);
    }

    /// The program interpreter is the file mapped at the auxiliary vector's
    /// `AT_BASE`: for this test binary, which is dynamically linked, the
    /// dynamic loader, under the name the mapping shows, and a stat through
    /// that name identifies the object mapped. A mapping of a deleted file,
    /// or of no file, names none. Another inode at the name is not the object
    /// mapped, and nor is an inode of the same number on another device where
    /// the mapping's `map_files` link can be followed.
    #[test]
    fn the_interpreter_is_the_file_mapped_at_the_base_the_auxv_names() {
        let auxv = crate::util::raw_read_file(Path::new("/proc/self/auxv")).unwrap();
        let base = crate::exec_record::auxv_entry(&auxv, libc::AT_BASE).unwrap();
        assert_ne!(base, 0, "the test binary is dynamically linked");
        let maps = crate::util::raw_read_file(Path::new("/proc/self/maps")).unwrap();
        let mapped = mapped_file(&maps, base).expect("a file is mapped at AT_BASE");
        let interpreter = &mapped.path;
        assert!(interpreter.is_absolute(), "{}", interpreter.display());
        // The dynamic loader: `ld-linux-<arch>.so.<n>` or `ld-musl-<arch>.so.1`.
        assert!(
            interpreter
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("ld-")),
            "{} is not a dynamic loader",
            interpreter.display()
        );
        // A stat through the name finds the object mapped, except where the
        // loader is on overlayfs, whose maps lines name the underlying file.
        let stat = crate::util::raw_stat(interpreter).unwrap();
        let mut fs = std::mem::MaybeUninit::<libc::statfs>::uninit();
        let name = std::ffi::CString::new(interpreter.as_os_str().as_bytes()).unwrap();
        // SAFETY: `name` is NUL-terminated and `fs` is written before it is read.
        assert_eq!(unsafe { libc::statfs(name.as_ptr(), fs.as_mut_ptr()) }, 0);
        // SAFETY: statfs returned 0, so it filled `fs`.
        let overlay =
            unsafe { fs.assume_init() }.f_type as u64 == libc::OVERLAYFS_SUPER_MAGIC as u64;
        let link = Path::new("/proc/self/map_files").join(&mapped.range);
        let through_link = crate::util::raw_stat(&link).ok();
        assert!(
            mapped.identifies(&stat, through_link.as_ref()) || overlay,
            "{mapped:?}"
        );
        assert!(mapped.identifies(&stat, None) || overlay, "{mapped:?}");
        // Another inode at the name is not the object mapped, with or without
        // the link.
        let mut replaced = stat;
        replaced.st_ino ^= 1;
        assert!(!mapped.identifies(&replaced, None));
        assert!(!mapped.identifies(&replaced, Some(&stat)));
        // An inode of the same number on another device (another filesystem,
        // or another btrfs subvolume, mounted over the name) is the object
        // mapped only by its number: the link's device tells it apart where
        // the tool may follow the link.
        let mut covering = stat;
        covering.st_dev ^= 1;
        assert!(!mapped.identifies(&covering, Some(&stat)));
        assert!(mapped.identifies(&covering, None));

        let maps = b"00400000-00401000 r-xp 00000000 08:01 12 /usr/bin/a b\n\
            7f0000000000-7f0000001000 r--p 00000000 08:01 13 /lib/ld (deleted)\n\
            7f0000002000-7f0000003000 r--p 00000000 00:00 0 \n\
            7f0000004000-7f0000005000 r--p 00000000 103:1a 4096 /lib/ld-x.so\n";
        assert_eq!(
            mapped_file(maps, 0x400800),
            Some(MappedFile {
                path: PathBuf::from("/usr/bin/a b"),
                ino: 12,
                range: "00400000-00401000".to_owned(),
            })
        );
        assert_eq!(
            mapped_file(maps, 0x7f0000004000),
            Some(MappedFile {
                path: PathBuf::from("/lib/ld-x.so"),
                ino: 4096,
                range: "7f0000004000-7f0000005000".to_owned(),
            })
        );
        assert_eq!(mapped_file(maps, 0x7f0000000000), None);
        assert_eq!(mapped_file(maps, 0x7f0000002000), None);
        assert_eq!(mapped_file(maps, 0x500000), None);
    }
}
