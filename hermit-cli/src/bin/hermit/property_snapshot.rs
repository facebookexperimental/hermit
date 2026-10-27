/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A frozen copy of the Android system property area for a run
//! (`detcore_model::config::PropertySnapshot`).
//!
//! init writes every property change into the shared files of
//! `/dev/__properties__`, which every process maps read-only, and bionic
//! rereads its cached values whenever their serial moves. Processes outside
//! the run (system_server rewrites cache keys routinely on an idle device)
//! therefore change, mid-run, which code a guest runs: the dynamic linker's
//! private bionic copy took 270 more branches at each of two sites of its
//! startup under a burst of property writes. The run reads this copy instead,
//! taken before the guest's first exec and shared by both runs of a
//! `--verify`.
//!
//! The copy's directory is named by a hash of its contents,
//! `hermit-properties-<hash>`, because the guest sees that path (in
//! `/proc/self/maps`, which Android's crash dumper copies into its tombstone,
//! and through `readlink`): two runs that read the same properties see the
//! same path. Runs that read the same properties therefore share one
//! directory. Each holds a shared `flock` on its `.lock` file while it runs;
//! the last one to finish, the only one that can take the lock exclusively,
//! removes the directory, and a directory nobody holds is stale and removed
//! by the next snapshot taken beside it.

use std::fs::File;
use std::io;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;

/// The Android system property area.
pub(super) const PROPERTY_AREA: &str = "/dev/__properties__";

/// The prefix of a copy directory's name: `hermit-properties-<hash>`.
const COPY_DIR_PREFIX: &str = "hermit-properties-";

/// The prefix of a directory being filled before it is renamed into place.
const FILLING_PREFIX: &str = ".hermit-properties-filling-";

/// The lock file in a copy directory (see the module documentation). A dot
/// file, so no property file can have its name.
const LOCK_NAME: &str = ".lock";

/// The longest copy path the guest's open is given: Detcore writes it to the
/// guest's scratch stack, which has room for this many bytes.
const LONGEST_COPY_PATH: usize = 511;

/// One file of the property area as one descriptor read it: its name, its
/// contents and its device and inode.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct AreaFile {
    name: std::ffi::OsString,
    contents: Vec<u8>,
    dev: u64,
    ino: u64,
}

/// What one capture of the property area found.
#[derive(Debug)]
struct Capture {
    /// Each file read, coherent on its own.
    files: Vec<AreaFile>,
    /// Each file that stopped being readable mid-capture, by the identity of
    /// its last successful read: it has no copy.
    uncopied: Vec<detcore_model::config::PropertyFile>,
    /// Whether no property was set during the capture (`SERIAL_FILE` read
    /// the same before and after), so the files are one consistent cut.
    consistent_cut: bool,
}

/// The area file init rewrites on every property set: its bytes (the area
/// header's serial among them) change exactly when a property changes.
const SERIAL_FILE: &str = "properties_serial";

/// A copy of the property area this run holds, released when the last holder
/// in the process that took it drops it.
#[derive(Debug)]
pub(super) struct PropertySnapshotDir {
    copy_dir: PathBuf,
    copied: usize,
    /// Each live file copied.
    live_files: Vec<detcore_model::config::PropertyFile>,
    /// Each live file seen but not copied.
    uncopied_files: Vec<detcore_model::config::PropertyFile>,
    /// Whether the copies are one consistent cut.
    consistent_cut: bool,
    /// The shared lock on the directory's `.lock` (close-on-exec, so no guest
    /// inherits it).
    lock: File,
    /// The process that took the snapshot. A process forked from it (the
    /// container's init, for one) holds the same values but must not release
    /// the directory while the run goes on.
    owner: u32,
}

impl PropertySnapshotDir {
    /// The Detcore configuration that makes the guest read this copy.
    pub(super) fn config(&self) -> detcore_model::config::PropertySnapshot {
        detcore_model::config::PropertySnapshot {
            guest_dir: PathBuf::from(PROPERTY_AREA),
            copy_dir: self.copy_dir.clone(),
            live_files: self.live_files.clone(),
            uncopied_files: self.uncopied_files.clone(),
            consistent_cut: self.consistent_cut,
        }
    }

    /// Whether the copies are one consistent cut, not only coherent per
    /// file.
    pub(super) fn consistent_cut(&self) -> bool {
        self.consistent_cut
    }

    /// This process's descriptors that a guest would inherit (not
    /// close-on-exec) and that are open on a live property file this copy
    /// holds: through one the guest would read the live area.
    pub(super) fn inherited_live_descriptors(&self) -> Vec<i32> {
        let Ok(entries) = std::fs::read_dir("/proc/self/fd") else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
            .filter(|fd| {
                // SAFETY: F_GETFD only reads the descriptor's flags.
                let flags = unsafe { libc::fcntl(*fd, libc::F_GETFD) };
                flags >= 0 && flags & libc::FD_CLOEXEC == 0
            })
            .filter(|fd| {
                std::fs::metadata(format!("/proc/self/fd/{fd}")).is_ok_and(|metadata| {
                    self.live_files
                        .iter()
                        .chain(&self.uncopied_files)
                        .any(|file| (file.dev, file.ino) == (metadata.dev(), metadata.ino()))
                })
            })
            .collect()
    }

    /// How many files the copy holds.
    pub(super) fn copied(&self) -> usize {
        self.copied
    }
}

impl Drop for PropertySnapshotDir {
    fn drop(&mut self) {
        if std::process::id() == self.owner {
            release(&self.copy_dir, &self.lock);
        }
    }
}

/// The directories this process holds, each with a duplicate of its lock
/// descriptor (the same lock), for `RemoveOnExit`: a run that ends by
/// unwinding (as `--timeout` does) never drops the options that hold it.
static HELD: std::sync::Mutex<Vec<(PathBuf, u32, File)>> = std::sync::Mutex::new(Vec::new());

/// Releases, when dropped, every copy directory this process still holds: on
/// return and on unwind alike. A run that ends in `_exit` (the `--timeout`
/// fallback) holds its lock until the process is gone, and the next snapshot
/// taken beside the directory removes it.
pub(super) struct RemoveOnExit;

impl Drop for RemoveOnExit {
    fn drop(&mut self) {
        release_held(|_| true);
    }
}

/// Release every copy directory this process still holds that `which`
/// selects (see `RemoveOnExit`).
fn release_held(which: impl Fn(&Path) -> bool) {
    let held = {
        let mut held = HELD.lock().unwrap_or_else(|poison| poison.into_inner());
        let (selected, kept) = std::mem::take(&mut *held)
            .into_iter()
            .partition::<Vec<_>, _>(|(copy_dir, _, _)| which(copy_dir));
        *held = kept;
        selected
    };
    for (copy_dir, owner, lock) in held {
        // Through the duplicate, the exclusive lock is refused only when
        // another run holds the directory, not by this run's own hold.
        if std::process::id() == owner {
            release(&copy_dir, &lock);
        }
    }
}

/// Give up this run's hold on `copy_dir` through `lock`: remove the directory
/// if no other run holds it (the exclusive lock is granted only then),
/// otherwise leave it to them.
fn release(copy_dir: &Path, lock: &File) {
    // Linux converts a shared lock to an exclusive one by dropping it first,
    // not atomically, so a joiner can slip in between; it then finds the lock
    // file unlinked and makes the directory again (`join`).
    if flock(lock, libc::LOCK_EX | libc::LOCK_NB).is_err() {
        return;
    }
    // A descriptor opened before another run removed and remade the
    // directory locks the old, unlinked lock file: the exclusive lock then
    // says nothing about the directory now at the path, which stays.
    let current = std::fs::symlink_metadata(copy_dir.join(LOCK_NAME));
    let locked = lock.metadata();
    let same = match (&current, &locked) {
        (Ok(current), Ok(locked)) => {
            locked.nlink() > 0 && (current.dev(), current.ino()) == (locked.dev(), locked.ino())
        }
        _ => false,
    };
    if !same {
        return;
    }
    if let Err(error) = std::fs::remove_dir_all(copy_dir)
        && error.kind() != io::ErrorKind::NotFound
    {
        let _ = writeln!(
            io::stderr(),
            "WARNING: cannot remove the property snapshot {}: {error}",
            copy_dir.display()
        );
    }
    HELD.lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .retain(|(held, _, _)| held != copy_dir);
}

fn flock(file: &File, operation: i32) -> io::Result<()> {
    // SAFETY: flock only reads the supplied descriptor and operation.
    if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// What `take` found.
#[derive(Debug)]
pub(super) enum Taken {
    /// The host has no property area: not Android.
    NoPropertyArea,
    /// The copy, to hand to Detcore.
    Snapshot(PropertySnapshotDir),
    /// The host has a property area that cannot be copied for the guest, for
    /// the reason given; the guest would read the live area.
    Refused(String),
}

/// Copy the property area `source` (`PROPERTY_AREA`) under `under`, or join
/// the copy of the same contents another run made there, for a process whose
/// effective user is `euid`.
///
/// bionic accepts a property file only if root owns it (user and group 0) and
/// neither its group nor others may write it, and it checks the file the guest
/// opens, so only a root Hermit can make copies the guest will use.
pub(super) fn take(source: &Path, under: &Path, euid: u32) -> Taken {
    if !source.is_dir() {
        return Taken::NoPropertyArea;
    }
    if euid != 0 {
        return Taken::Refused(
            "Hermit is not running as root, and the guest's C library accepts a copy of a \
             property file only when root owns it"
                .to_owned(),
        );
    }
    match take_as_root(source, under, true) {
        Ok(snapshot) => Taken::Snapshot(snapshot),
        Err(reason) => Taken::Refused(reason),
    }
}

/// `take` for a root process; `chown_root` makes the copies root's (a test,
/// not root, passes false).
fn take_as_root(
    source: &Path,
    under: &Path,
    chown_root: bool,
) -> Result<PropertySnapshotDir, String> {
    let Capture {
        files,
        uncopied,
        consistent_cut,
    } = read_coherent(source)?;
    let catalog = |live_files| Catalog {
        live_files,
        uncopied_files: uncopied.clone(),
        consistent_cut,
    };
    let live_files = files
        .iter()
        .map(|file| detcore_model::config::PropertyFile {
            dev: file.dev,
            ino: file.ino,
            name: file.name.to_string_lossy().into_owned(),
        })
        .collect::<Vec<_>>();
    if files.is_empty() {
        return Err(format!(
            "none of the files of {} could be read",
            source.display()
        ));
    }
    let copy_dir = under.join(format!("{COPY_DIR_PREFIX}{}", contents_hash(&files)));
    let longest = files.iter().map(|file| file.name.len()).max().unwrap_or(0);
    if copy_dir.as_os_str().len() + 1 + longest > LONGEST_COPY_PATH {
        return Err(format!(
            "its copy's paths under {} would be longer than the {LONGEST_COPY_PATH} bytes \
             the guest's open can be given",
            under.display()
        ));
    }
    remove_stale_copies(under);
    // Another run may be making, joining or removing the same directory; a
    // few rounds settle every interleaving (see `join`).
    for _ in 0..4 {
        if let Some(lock) = join(&copy_dir) {
            if holds(&copy_dir, &files) {
                return Ok(held(
                    copy_dir,
                    files.len(),
                    catalog(live_files.clone()),
                    lock,
                ));
            }
            // Someone changed the shared copy (only root can): this run
            // makes a private one rather than read it.
            drop(lock);
            let private = under.join(format!(
                "{}-{}",
                copy_dir.file_name().unwrap_or_default().to_string_lossy(),
                std::process::id()
            ));
            let (filling, lock) = fill(under, &files, chown_root).map_err(|error| {
                format!("its copy could not be made in {}: {error}", under.display())
            })?;
            std::fs::rename(&filling, &private).map_err(|error| {
                let _ = std::fs::remove_dir_all(&filling);
                format!("its copy could not be made in {}: {error}", under.display())
            })?;
            return Ok(held(
                private,
                files.len(),
                catalog(live_files.clone()),
                lock,
            ));
        }
        let (filling, lock) = fill(under, &files, chown_root).map_err(|error| {
            format!("its copy could not be made in {}: {error}", under.display())
        })?;
        // `rename` fails if another run put the directory in place first;
        // this run then joins it.
        if std::fs::rename(&filling, &copy_dir).is_ok() {
            return Ok(held(
                copy_dir,
                files.len(),
                catalog(live_files.clone()),
                lock,
            ));
        }
        drop(lock);
        let _ = std::fs::remove_dir_all(&filling);
    }
    Err(format!(
        "its copy in {} kept being removed while it was being joined",
        copy_dir.display()
    ))
}

/// What a snapshot knows about the live files it was taken from.
struct Catalog {
    live_files: Vec<detcore_model::config::PropertyFile>,
    uncopied_files: Vec<detcore_model::config::PropertyFile>,
    consistent_cut: bool,
}

/// Record `copy_dir` as held by this process and wrap it.
fn held(copy_dir: PathBuf, copied: usize, catalog: Catalog, lock: File) -> PropertySnapshotDir {
    let Catalog {
        live_files,
        uncopied_files,
        consistent_cut,
    } = catalog;
    let owner = std::process::id();
    if let Ok(duplicate) = lock.try_clone() {
        HELD.lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push((copy_dir.clone(), owner, duplicate));
    }
    PropertySnapshotDir {
        copy_dir,
        copied,
        live_files,
        uncopied_files,
        consistent_cut,
        lock,
        owner,
    }
}

/// The area's files. Each is read until two consecutive reads of it agree:
/// init writes a property in place (the value, then its serial, then the
/// global serial in `SERIAL_FILE`), and a new property extends one context
/// file's trie, so one read that overlaps a write can hold half of it, and
/// requiring the next read of the same file to find the same bytes makes
/// keeping such a half-written state very unlikely.
///
/// That makes each file coherent on its own, but two files read at
/// different moments can disagree: init publishes some properties in an
/// order readers rely on across files (it loads the persistent properties,
/// then sets `ro.persistent_properties.ready` in another context), so a set
/// landing between two files' reads could show the later property with the
/// earlier values. The whole capture is therefore bracketed by reads of
/// `SERIAL_FILE`: when they agree the copies are taken as one consistent cut.
/// Up to `CUT_ATTEMPTS` captures are tried; if properties keep changing, or
/// `SERIAL_FILE` cannot be read, the last capture is kept as coherent per
/// file only, and says so ([`Capture::consistent_cut`]).
///
/// The bracket is not a seqlock proof. It rests on how Android's property
/// service works: init is the area's only writer, it bumps the global serial
/// after each complete update, the serial does not wrap within a capture,
/// and the area is not re-created (a new counter generation) mid-capture.
/// Under those assumptions matching brackets allow at most one update still
/// in flight in one context file, a state the area natively holds for that
/// moment too.
///
/// Refused when a file keeps changing through `ATTEMPTS` reads. Every
/// identity (device and inode) any read of any attempt returned and that the
/// kept capture did not copy is kept as uncopied, so the guest cannot read
/// such a file live unnoticed, whichever attempt saw it.
fn read_coherent(source: &Path) -> Result<Capture, String> {
    read_coherent_with(source, read_file)
}

/// `read_coherent`, reading each file with `read`.
fn read_coherent_with(
    source: &Path,
    mut read: impl FnMut(&Path) -> Option<(Vec<u8>, u64, u64)>,
) -> Result<Capture, String> {
    const CUT_ATTEMPTS: usize = 3;
    let serial = source.join(SERIAL_FILE);
    // Every read below, the brackets' included, goes through `reads`, so
    // every identity any of them returned is in `reads.seen`.
    let mut reads = Observed {
        read: &mut read,
        seen: std::collections::BTreeMap::new(),
    };
    let mut attempt = 1;
    loop {
        // The serial file's contents only: init rewrites it in place, so its
        // device and inode never change and carry nothing about a set.
        let before = reads.read(&serial).map(|(contents, _, _)| contents);
        let files = read_each_file(source, &mut reads)?;
        let after = reads.read(&serial).map(|(contents, _, _)| contents);
        let consistent_cut = before.is_some() && before == after;
        if consistent_cut || attempt == CUT_ATTEMPTS {
            let copied = files
                .iter()
                .map(|file| (file.dev, file.ino))
                .collect::<std::collections::BTreeSet<_>>();
            let uncopied = std::mem::take(&mut reads.seen)
                .into_iter()
                .filter(|(identity, _)| !copied.contains(identity))
                .map(|((dev, ino), name)| detcore_model::config::PropertyFile { dev, ino, name })
                .collect();
            return Ok(Capture {
                files,
                uncopied,
                consistent_cut,
            });
        }
        attempt += 1;
    }
}

/// The capture's reader, recording every identity (device and inode) a read
/// returns, with the file's name, before handing back its bytes.
struct Observed<R> {
    read: R,
    seen: std::collections::BTreeMap<(u64, u64), String>,
}

impl<R: FnMut(&Path) -> Option<(Vec<u8>, u64, u64)>> Observed<R> {
    fn read(&mut self, path: &Path) -> Option<(Vec<u8>, u64, u64)> {
        let result = (self.read)(path);
        if let Some((_, dev, ino)) = &result {
            let name = path.file_name().unwrap_or_default();
            self.seen
                .entry((*dev, *ino))
                .or_insert_with(|| name.to_string_lossy().into_owned());
        }
        result
    }
}

/// One pass over the area: each readable file once two consecutive reads of
/// it agree. A file that stops being readable mid-pass is left out of the
/// pass (its identity stays in `reads.seen`).
fn read_each_file<R: FnMut(&Path) -> Option<(Vec<u8>, u64, u64)>>(
    source: &Path,
    reads: &mut Observed<R>,
) -> Result<Vec<AreaFile>, String> {
    // A file changes only while init writes a property into it, so even
    // under a burst of sets two reads of one 128 KiB file agree within a few
    // tries.
    const ATTEMPTS: usize = 50;
    let unreadable = |error: io::Error| format!("{} could not be read: {error}", source.display());
    let mut files = Vec::new();
    for entry in std::fs::read_dir(source).map_err(unreadable)? {
        let entry = entry.map_err(unreadable)?;
        if !entry.file_type().map_err(unreadable)?.is_file() {
            continue;
        }
        let path = entry.path();
        let Some(mut previous) = reads.read(&path) else {
            continue;
        };
        let mut stable = None;
        let mut vanished = false;
        for _ in 0..ATTEMPTS {
            let Some(next) = reads.read(&path) else {
                vanished = true;
                break;
            };
            if next == previous {
                stable = Some(next);
                break;
            }
            previous = next;
        }
        match stable {
            Some((contents, dev, ino)) => files.push(AreaFile {
                name: entry.file_name(),
                contents,
                dev,
                ino,
            }),
            // Its identity is in `reads.seen`, so it ends up uncopied unless
            // a later attempt copies it.
            None if vanished => {}
            None => {
                return Err(format!(
                    "{} changed between every two of {} reads, so no consistent copy of it \
                     could be taken",
                    path.display(),
                    ATTEMPTS + 1
                ));
            }
        }
    }
    files.sort();
    Ok(files)
}

/// One read of the file at `path` through one descriptor: its bytes, device
/// and inode. `None` if it cannot be opened or read: the guest, which runs
/// with this process's credentials, cannot open it either.
fn read_file(path: &Path) -> Option<(Vec<u8>, u64, u64)> {
    let mut file = File::open(path).ok()?;
    let mut contents = Vec::new();
    io::Read::read_to_end(&mut file, &mut contents).ok()?;
    let metadata = file.metadata().ok()?;
    Some((contents, metadata.dev(), metadata.ino()))
}

/// Take a shared hold on the existing copy directory `copy_dir`, if it is in
/// place and still linked once the hold is granted. A run removing it holds
/// the exclusive lock until the directory is gone, so a hold granted after
/// that finds the lock file unlinked, and the caller makes the directory
/// again.
fn join(copy_dir: &Path) -> Option<File> {
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(copy_dir.join(LOCK_NAME))
        .ok()?;
    flock(&lock, libc::LOCK_SH).ok()?;
    (lock.metadata().ok()?.nlink() > 0).then_some(lock)
}

/// Whether the copy directory `copy_dir` holds exactly `files`.
fn holds(copy_dir: &Path, files: &[AreaFile]) -> bool {
    let Ok(entries) = std::fs::read_dir(copy_dir) else {
        return false;
    };
    let names = entries
        .flatten()
        .filter(|entry| entry.file_name() != LOCK_NAME)
        .count();
    names == files.len()
        && files.iter().all(|file| {
            std::fs::read(copy_dir.join(&file.name)).is_ok_and(|copy| copy == file.contents)
        })
}

/// Fill a new directory under `under` with `files` and its lock file, held
/// shared, ready to be renamed into place.
fn fill(under: &Path, files: &[AreaFile], chown_root: bool) -> io::Result<(PathBuf, File)> {
    let filling = under.join(format!(
        "{FILLING_PREFIX}{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos())
    ));
    std::fs::DirBuilder::new().mode(0o700).create(&filling)?;
    let made = (|| {
        // Locked exclusively while it fills, so a sweep beside it leaves it.
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC)
            .open(filling.join(LOCK_NAME))?;
        flock(&lock, libc::LOCK_EX)?;
        for file in files {
            write_copy(&filling.join(&file.name), &file.contents, chown_root)?;
        }
        flock(&lock, libc::LOCK_SH)?;
        Ok(lock)
    })();
    match made {
        Ok(lock) => Ok((filling, lock)),
        Err(error) => {
            let _ = std::fs::remove_dir_all(&filling);
            Err(error)
        }
    }
}

/// A hash of every file's name and contents, which names the copy directory.
fn contents_hash(files: &[AreaFile]) -> String {
    /// The files as one stream: each name and its contents, each preceded by
    /// its length, so no two different areas give the same bytes.
    struct Stream<'a> {
        parts: Vec<std::borrow::Cow<'a, [u8]>>,
        part: usize,
        offset: usize,
    }
    impl io::Read for Stream<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            while let Some(part) = self.parts.get(self.part) {
                let rest = &part[self.offset..];
                if rest.is_empty() {
                    self.part += 1;
                    self.offset = 0;
                    continue;
                }
                let count = rest.len().min(buffer.len());
                buffer[..count].copy_from_slice(&rest[..count]);
                self.offset += count;
                return Ok(count);
            }
            Ok(0)
        }
    }
    let mut parts = Vec::new();
    for file in files {
        let name = std::os::unix::ffi::OsStrExt::as_bytes(file.name.as_os_str());
        parts.push((name.len() as u64).to_le_bytes().to_vec().into());
        parts.push(name.into());
        parts.push((file.contents.len() as u64).to_le_bytes().to_vec().into());
        parts.push(file.contents.as_slice().into());
    }
    let digest = detcore::Digest::digest_reader(Stream {
        parts,
        part: 0,
        offset: 0,
    })
    .expect("reading memory cannot fail");
    format!("{digest:x}")[..16].to_owned()
}

/// Write `contents` to the new file `target` as a file nobody may write
/// (0444), owned by root when `chown_root`.
fn write_copy(target: &Path, contents: &[u8], chown_root: bool) -> io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(target)?;
    file.write_all(contents)?;
    if chown_root {
        std::os::unix::fs::fchown(&file, Some(0), Some(0))?;
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o444))
}

/// Remove the copy directories under `under` that no run holds, and the
/// directories left half-filled by runs that died while filling them.
fn remove_stale_copies(under: &Path) {
    let Ok(entries) = std::fs::read_dir(under) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(COPY_DIR_PREFIX) && !name.starts_with(FILLING_PREFIX) {
            continue;
        }
        // A directory with no lock file is a run's first moment of filling
        // or not Hermit's; either way it is left alone.
        let Ok(lock) = File::open(entry.path().join(LOCK_NAME)) else {
            continue;
        };
        release(&entry.path(), &lock);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(files: &[(&str, &[u8])]) -> tempfile::TempDir {
        let source = tempfile::tempdir().unwrap();
        for (name, contents) in files {
            std::fs::write(source.path().join(name), contents).unwrap();
        }
        source
    }

    /// Regular files are copied byte for byte as read-only files into a
    /// directory named by their contents; a subdirectory and a file this
    /// process cannot read are not copied.
    #[test]
    fn a_property_area_copy_holds_each_readable_file_read_only() {
        let source = area(&[
            ("properties_serial", &[1, 2, 3, 4]),
            ("u:object_r:test_prop:s0", &[7; 128]),
        ]);
        std::fs::create_dir(source.path().join("subdir")).unwrap();
        let unreadable = source.path().join("u:object_r:secret_prop:s0");
        std::fs::write(&unreadable, b"secret").unwrap();
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();
        // SAFETY: `geteuid` takes no arguments and cannot fail.
        let root = unsafe { libc::geteuid() } == 0;
        let under = tempfile::tempdir().unwrap();

        let snapshot = take_as_root(source.path(), under.path(), false).unwrap();

        assert_eq!(snapshot.copied(), if root { 3 } else { 2 });
        let copy = &snapshot.config().copy_dir;
        assert!(copy.starts_with(under.path()));
        assert_eq!(
            std::fs::read(copy.join("properties_serial")).unwrap(),
            [1, 2, 3, 4]
        );
        let property = copy.join("u:object_r:test_prop:s0");
        assert_eq!(std::fs::read(&property).unwrap(), vec![7; 128]);
        assert_eq!(std::fs::metadata(&property).unwrap().mode() & 0o777, 0o444);
        assert!(!copy.join("subdir").exists());
        assert_eq!(copy.join("u:object_r:secret_prop:s0").exists(), root);
    }

    /// The read counts a reader saw, by file name.
    fn counted(reads: &std::collections::HashMap<PathBuf, u32>, name: &str) -> u32 {
        reads
            .iter()
            .find(|(path, _)| path.ends_with(name))
            .map_or(0, |(_, count)| *count)
    }

    /// A quiet area is one consistent cut: the serial file reads the same
    /// before and after, and each file is read exactly twice.
    #[test]
    fn a_quiet_area_is_captured_as_one_consistent_cut() {
        let source = area(&[
            ("properties_serial", &[0; 4]),
            ("u:object_r:test_prop:s0", &[1; 4]),
        ]);
        let mut reads = std::collections::HashMap::<PathBuf, u32>::new();
        let capture = read_coherent_with(source.path(), |path| {
            *reads.entry(path.to_owned()).or_default() += 1;
            Some((vec![1; 4], 1, 2))
        })
        .unwrap();
        assert!(capture.consistent_cut);
        assert_eq!(capture.files.len(), 2);
        assert!(capture.uncopied.is_empty());
        // Two bracket reads and two capture reads.
        assert_eq!(counted(&reads, "properties_serial"), 4);
        assert_eq!(counted(&reads, "u:object_r:test_prop:s0"), 2);
    }

    /// A file is captured once two consecutive reads of it agree, however
    /// many reads before differed. When properties keep changing across the
    /// whole capture (the serial file differs before and after it in each
    /// of the three attempts), the last capture is kept as coherent per file
    /// only.
    #[test]
    fn a_changing_area_is_captured_per_file_and_says_so() {
        let source = area(&[
            ("properties_serial", &[0; 4]),
            ("u:object_r:test_prop:s0", &[1; 4]),
        ]);
        let mut reads = std::collections::HashMap::<PathBuf, u32>::new();
        // The serial file's bytes advance every second read: two consecutive
        // reads inside a capture can agree, the bracket reads never do.
        let capture = read_coherent_with(source.path(), |path| {
            let count = reads.entry(path.to_owned()).or_default();
            *count += 1;
            let contents = if path.ends_with("properties_serial") {
                vec![(*count / 2) as u8; 4]
            } else {
                vec![1; 4]
            };
            Some((contents, 1, 2))
        })
        .unwrap();
        assert!(!capture.consistent_cut);
        assert_eq!(capture.files.len(), 2);
        // The last attempt's capture is kept: its serial reads were the 10th
        // and 11th (10 / 2 = 11 / 2 = 5).
        let serial = capture
            .files
            .iter()
            .find(|file| file.name == "properties_serial")
            .unwrap();
        assert_eq!(serial.contents, [5; 4]);
        assert!(capture.uncopied.is_empty());
        // Three attempts of: bracket, two capture reads, bracket.
        assert_eq!(counted(&reads, "properties_serial"), 12);
        assert_eq!(counted(&reads, "u:object_r:test_prop:s0"), 6);
    }

    /// A file that differs in every one of its 51 reads refuses the
    /// snapshot, naming it, after exactly that many reads.
    #[test]
    fn a_file_that_never_settles_refuses_after_its_last_read() {
        let source = area(&[
            ("properties_serial", &[0; 4]),
            ("u:object_r:test_prop:s0", &[1; 4]),
        ]);
        let mut reads = std::collections::HashMap::<PathBuf, u32>::new();
        let reason = read_coherent_with(source.path(), |path| {
            let count = reads.entry(path.to_owned()).or_default();
            *count += 1;
            let changing = path.ends_with("u:object_r:test_prop:s0");
            Some((
                if changing {
                    count.to_le_bytes().to_vec()
                } else {
                    vec![1]
                },
                1,
                2,
            ))
        })
        .unwrap_err();
        assert!(
            reason.contains("u:object_r:test_prop:s0 changed between every two of 51 reads"),
            "{reason}"
        );
        assert_eq!(counted(&reads, "u:object_r:test_prop:s0"), 51);
    }

    /// A file that stops being readable after its first read is not copied
    /// but kept by the identity that read gave, so an open that reaches it
    /// is still recognized as the live area.
    #[test]
    fn a_file_that_vanishes_mid_capture_is_kept_by_identity() {
        let source = area(&[
            ("properties_serial", &[0; 4]),
            ("u:object_r:test_prop:s0", &[1; 4]),
        ]);
        let mut seen = 0;
        let capture = read_coherent_with(source.path(), |path| {
            if path.ends_with("u:object_r:test_prop:s0") {
                seen += 1;
                return (seen == 1).then(|| (vec![1; 4], 5, 6));
            }
            Some((vec![2; 4], 1, 2))
        })
        .unwrap();
        assert_eq!(capture.files.len(), 1);
        assert_eq!(capture.files[0].name, "properties_serial");
        assert_eq!(
            capture.uncopied,
            [detcore_model::config::PropertyFile {
                dev: 5,
                ino: 6,
                name: "u:object_r:test_prop:s0".to_owned(),
            }]
        );
        let under = tempfile::tempdir().unwrap();
        let snapshot = take_as_root(source.path(), under.path(), false)
            .unwrap()
            .config();
        assert!(snapshot.uncopied_files.is_empty());
        assert!(snapshot.consistent_cut);
    }

    /// An identity seen only by an attempt that was retried is still kept:
    /// attempt 1 reads a file once and then loses it, the bracket changes,
    /// attempt 2 cannot read the file at all and its bracket agrees, yet
    /// the kept cut carries the file's identity as uncopied, so a later read
    /// of it by another name is still recognized.
    #[test]
    fn an_identity_seen_by_a_retried_attempt_is_kept() {
        let source = area(&[
            ("properties_serial", &[0; 4]),
            ("u:object_r:test_prop:s0", &[1; 4]),
        ]);
        let mut serial_reads = 0_u8;
        let mut file_reads = 0;
        let capture = read_coherent_with(source.path(), |path| {
            if path.ends_with("properties_serial") {
                serial_reads += 1;
                // Attempt 1's bracket (reads 1 and 4) differs; attempt 2's
                // (reads 5 and 8) agrees.
                let contents = if serial_reads <= 1 { 0 } else { 1 };
                return Some((vec![contents; 4], 1, 2));
            }
            file_reads += 1;
            (file_reads == 1).then(|| (vec![1; 4], 5, 6))
        })
        .unwrap();
        assert!(capture.consistent_cut);
        assert_eq!(capture.files.len(), 1);
        assert_eq!(
            capture.uncopied,
            [detcore_model::config::PropertyFile {
                dev: 5,
                ino: 6,
                name: "u:object_r:test_prop:s0".to_owned(),
            }]
        );
    }

    /// An identity only a bracket read returned is kept too: the serial
    /// file reads fine for both brackets but fails its own per-file read, so
    /// it is not copied, and its identity is uncopied rather than forgotten.
    #[test]
    fn an_identity_only_a_bracket_read_saw_is_kept() {
        let source = area(&[
            ("properties_serial", &[0; 4]),
            ("u:object_r:test_prop:s0", &[1; 4]),
        ]);
        let mut serial_reads = 0;
        let capture = read_coherent_with(source.path(), |path| {
            if path.ends_with("properties_serial") {
                serial_reads += 1;
                // Reads 1 and 3 are the brackets; read 2 is its own.
                return (serial_reads != 2).then(|| (vec![0; 4], 7, 8));
            }
            Some((vec![1; 4], 5, 6))
        })
        .unwrap();
        assert!(capture.consistent_cut);
        assert_eq!(serial_reads, 3);
        assert_eq!(capture.files.len(), 1);
        assert_eq!(capture.files[0].name, "u:object_r:test_prop:s0");
        assert_eq!(
            capture.uncopied,
            [detcore_model::config::PropertyFile {
                dev: 7,
                ino: 8,
                name: "properties_serial".to_owned(),
            }]
        );
    }

    /// Two runs that read the same properties share one directory, under the
    /// same name, and it goes when the last of them lets go; other contents
    /// get another name.
    #[test]
    fn runs_reading_the_same_properties_share_one_copy() {
        let source = area(&[("properties_serial", &[1, 2, 3, 4])]);
        let under = tempfile::tempdir().unwrap();
        let first = take_as_root(source.path(), under.path(), false).unwrap();
        let second = take_as_root(source.path(), under.path(), false).unwrap();
        let copy = first.config().copy_dir;
        assert_eq!(second.config().copy_dir, copy);
        drop(first);
        assert!(copy.join("properties_serial").exists(), "still held");
        drop(second);
        assert!(!copy.exists());

        let other = area(&[("properties_serial", &[9, 9, 9, 9])]);
        let third = take_as_root(other.path(), under.path(), false).unwrap();
        assert_ne!(third.config().copy_dir, copy);
    }

    /// A shared copy whose contents no longer match the area is not joined:
    /// the run makes a private copy with the area's contents.
    #[test]
    fn a_changed_shared_copy_is_not_joined() {
        let source = area(&[("properties_serial", &[1, 2, 3, 4])]);
        let under = tempfile::tempdir().unwrap();
        let first = take_as_root(source.path(), under.path(), false).unwrap();
        let shared = first.config().copy_dir;
        let tampered = shared.join("properties_serial");
        std::fs::set_permissions(&tampered, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::write(&tampered, [9, 9, 9, 9]).unwrap();
        let second = take_as_root(source.path(), under.path(), false).unwrap();
        let private = second.config().copy_dir;
        assert_ne!(private, shared);
        assert_eq!(
            std::fs::read(private.join("properties_serial")).unwrap(),
            [1, 2, 3, 4]
        );
        drop(second);
        assert!(!private.exists());
    }

    /// A host without the property area is not Android; a non-root Hermit
    /// cannot make copies bionic accepts and says why; a temporary directory
    /// whose copy paths would not fit the guest's open is refused.
    #[test]
    fn a_snapshot_needs_the_area_root_and_a_short_path() {
        let under = tempfile::tempdir().unwrap();
        assert!(matches!(
            take(&under.path().join("absent"), under.path(), 0),
            Taken::NoPropertyArea
        ));
        let source = area(&[("properties_serial", &[0; 4])]);
        match take(source.path(), under.path(), 2000) {
            Taken::Refused(reason) => assert!(reason.contains("not running as root"), "{reason}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        let deep = under.path().join("d".repeat(250)).join("e".repeat(250));
        std::fs::create_dir_all(&deep).unwrap();
        let reason = take_as_root(source.path(), &deep, false).unwrap_err();
        assert!(reason.contains("longer than"), "{reason}");
        assert_eq!(std::fs::read_dir(&deep).unwrap().count(), 0);
    }

    /// A copy nobody holds is stale and goes when the next snapshot is taken
    /// beside it; one a run holds stays; what `RemoveOnExit` does releases a
    /// directory only this process holds, even with its handle not dropped.
    #[test]
    fn stale_copies_go_and_held_ones_stay() {
        let under = tempfile::tempdir().unwrap();
        // A run that died left its directory, and its lock died with it.
        let stale = under
            .path()
            .join(format!("{COPY_DIR_PREFIX}0123456789abcdef"));
        std::fs::create_dir(&stale).unwrap();
        std::fs::write(stale.join(LOCK_NAME), b"").unwrap();
        std::fs::write(stale.join("properties_serial"), [0; 4]).unwrap();
        let source = area(&[("properties_serial", &[1; 4])]);
        let kept = take_as_root(source.path(), under.path(), false).unwrap();
        let kept_dir = kept.config().copy_dir;
        assert!(!stale.exists());
        assert!(kept_dir.exists());
        remove_stale_copies(under.path());
        assert!(kept_dir.exists(), "a held copy survives a sweep");

        // What `RemoveOnExit` does, for this test's directories only: this
        // process is the only holder, so the directory goes although its
        // handle was never dropped (a run that unwound past it).
        release_held(|dir| dir.starts_with(under.path()));
        assert!(!kept_dir.exists());
        drop(kept);
    }
}
