// Copyright (c) Meta Platforms, Inc. and affiliates.
// This source code is licensed under the BSD-style license found in LICENSE.

//! Captured runs publish a bounded summary before their authoritative result.
//! The library still serializes and writes its temporary summary without a
//! bound. This module bounds the later read and final publication only.
//!
//! Captured summary publication deliberately requests creation mode 0600 (the
//! process umask may further restrict it). It creates a new inode and does not
//! preserve the previous inode's mode, owner, or extended attributes. Uncaptured
//! summary writing is unchanged: direct fs::write retains an existing inode and
//! follows its ordinary creation permissions and umask for an absent file.
//! Cleanup's identity check followed by unlink shares the publication checks'
//! non-atomic limit against concurrent namespace changes.

use std::ffi::CString;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::fs::File;
use std::fs::Metadata;
use std::fs::OpenOptions;
use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;

use hermit::Context;
use hermit::Error;
use hermit::run_evidence::RunEvidenceFileIdentity;

use super::guest_capture::GuestRunCaptureSession;

const MAX_SUMMARY_BYTES: usize = 1024 * 1024;

/// This file is opened after stdin reservation, before the container fork.
/// Its name is unlinked immediately and is never passed to the guest.
pub(crate) fn private_output(directory: &Path) -> Result<File, Error> {
    let file = tempfile::tempfile_in(directory).context("creating private captured-run summary")?;
    require_output_descriptor(&file)?;
    Ok(file)
}

/// Run the library with a controller-local proc-fd path, then publish in that
/// same namespace before returning success to the parent's capture.finish.
pub(crate) fn with_published_summary<T>(
    output: Option<&File>,
    destination: Option<&Path>,
    capture: Option<&GuestRunCaptureSession>,
    run: impl FnOnce(&Option<PathBuf>) -> Result<T, Error>,
) -> Result<T, Error> {
    with_published_summary_using(
        output,
        destination,
        capture,
        MAX_SUMMARY_BYTES,
        &mut NativeIo,
        run,
    )
}

fn with_published_summary_using<T>(
    output: Option<&File>,
    destination: Option<&Path>,
    capture: Option<&GuestRunCaptureSession>,
    maximum: usize,
    io: &mut impl SummaryIo,
    run: impl FnOnce(&Option<PathBuf>) -> Result<T, Error>,
) -> Result<T, Error> {
    let (Some(output), Some(destination), Some(capture)) = (output, destination, capture) else {
        if output.is_some() || (destination.is_some() && capture.is_some()) {
            anyhow::bail!("captured-run summary storage is missing or inconsistent");
        }
        return run(&destination.map(Path::to_owned));
    };
    // Keep the original final destination for every collision and identity
    // check. A host staging pathname cannot stand in for this check.
    capture.require_distinct_from(destination, "--summary-json")?;
    let writer = writer_path(output)?;
    let destination = PreparedDestination::prepare(destination, capture)?;
    let result = run(&Some(writer))?;
    let bytes = read_output(output, maximum, io)?;
    destination.replace(&bytes, capture, io)?;
    Ok(result)
}

fn identity(metadata: &Metadata) -> RunEvidenceFileIdentity {
    RunEvidenceFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

fn require_output_descriptor(file: &File) -> Result<RunEvidenceFileIdentity, Error> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        anyhow::bail!("private captured-run summary is not a regular file");
    }
    // SAFETY: both fcntl commands inspect the live borrowed descriptor.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error()).context("inspecting summary access mode");
    }
    if flags & libc::O_ACCMODE != libc::O_RDWR {
        anyhow::bail!("private captured-run summary must be readable and writable");
    }
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error()).context("inspecting summary descriptor flags");
    }
    if flags & libc::FD_CLOEXEC == 0 {
        anyhow::bail!("private captured-run summary must have CLOEXEC");
    }
    Ok(identity(&metadata))
}

fn writer_path(file: &File) -> Result<PathBuf, Error> {
    let path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
    require_writer_path(file, &path)?;
    Ok(path)
}

fn require_writer_path(file: &File, path: &Path) -> Result<(), Error> {
    let expected = require_output_descriptor(file)?;
    let visible = fs::metadata(path).context("resolving controller summary descriptor")?;
    if !visible.is_file() || identity(&visible) != expected {
        anyhow::bail!("controller summary descriptor path changed identity");
    }
    Ok(())
}

/// These are the production I/O boundaries. Native controls inject actual
/// partial writes and failures here without duplicating publication logic.
trait SummaryIo {
    fn read(&mut self, file: &mut &File, buffer: &mut [u8]) -> io::Result<usize> {
        file.read(buffer)
    }
    fn write(&mut self, file: &mut File, bytes: &[u8]) -> io::Result<usize> {
        file.write(bytes)
    }
    fn flush(&mut self, file: &mut &File) -> io::Result<()> {
        file.flush()
    }
    fn sync(&mut self, file: &File) -> io::Result<()> {
        file.sync_all()
    }
    fn rename(&mut self, parent: &File, from: &OsStr, to: &OsStr) -> io::Result<()> {
        rename_child(parent, from, to)
    }
}

struct NativeIo;
impl SummaryIo for NativeIo {}

fn read_output(file: &File, maximum: usize, io: &mut impl SummaryIo) -> Result<Vec<u8>, Error> {
    let mut reader = file;
    io.flush(&mut reader)
        .context("flushing private captured-run summary")?;
    io.sync(file)
        .context("synchronizing private captured-run summary")?;
    let expected = require_output_descriptor(file)?;
    let initial_size = file.metadata()?.len();
    reader
        .seek(SeekFrom::Start(0))
        .context("rewinding private captured-run summary")?;
    let capacity = maximum
        .checked_add(1)
        .ok_or_else(|| Error::msg("summary limit overflow"))?;
    // No read_to_end growth: one fixed maximum-plus-one payload buffer.
    let mut bytes = vec![0; capacity];
    let mut count = 0;
    while count < bytes.len() {
        match io.read(&mut reader, &mut bytes[count..]) {
            Ok(0) => break,
            Ok(read) => count += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error).context("reading private captured-run summary"),
        }
    }
    if count > maximum {
        anyhow::bail!("captured-run summary exceeds the {maximum}-byte publication limit");
    }
    let after = file.metadata()?;
    if !after.is_file()
        || identity(&after) != expected
        || after.len() != initial_size
        || count as u64 != initial_size
    {
        anyhow::bail!("private captured-run summary changed identity or size while reading");
    }
    bytes.truncate(count);
    Ok(bytes)
}

struct PreparedDestination {
    path: PathBuf,
    parent_path: PathBuf,
    parent: File,
    parent_identity: RunEvidenceFileIdentity,
    name: OsString,
    expected: Option<RunEvidenceFileIdentity>,
    // Holding the placeholder prevents inode reuse from hiding replacement.
    placeholder: Option<File>,
}

impl PreparedDestination {
    fn prepare(path: &Path, capture: &GuestRunCaptureSession) -> Result<Self, Error> {
        let leaf = path
            .as_os_str()
            .as_bytes()
            .rsplit(|byte| *byte == b'/')
            .next()
            .unwrap_or_default();
        if leaf.is_empty() || leaf == b"." || leaf == b".." {
            anyhow::bail!("captured-run summary must name a regular file");
        }
        let path = std::env::current_dir()?.join(path);
        let parent_path = path
            .parent()
            .ok_or_else(|| Error::msg("summary has no parent"))?
            .to_owned();
        // The unchanged strict capture collision check requires this parent
        // to exist. Do not widen that admission by creating it here.
        let parent = open_directory(&parent_path)?;
        let parent_identity = identity(&parent.metadata()?);
        let name = path
            .file_name()
            .ok_or_else(|| Error::msg("summary has no filename"))?
            .to_owned();
        let initial = open_child(&parent, &name)?;
        let expected = regular_child_identity(initial.as_ref())?;
        let mut prepared = Self {
            path,
            parent_path,
            parent,
            parent_identity,
            name,
            expected,
            placeholder: initial,
        };
        // Invalidate an old successful summary before entering the backend,
        // without truncating an existing inode or following a leaf symlink.
        let placeholder = prepared.replace(&[], capture, &mut NativeIo)?;
        prepared.expected = Some(identity(&placeholder.metadata()?));
        prepared.placeholder = Some(placeholder);
        Ok(prepared)
    }

    fn check(&self, capture: &GuestRunCaptureSession) -> Result<(), Error> {
        capture.require_distinct_from(&self.path, "--summary-json")?;
        let visible_parent = open_directory(&self.parent_path)?;
        if identity(&visible_parent.metadata()?) != self.parent_identity {
            anyhow::bail!("captured-run summary parent changed identity");
        }
        let child = open_child(&self.parent, &self.name)?;
        if regular_child_identity(child.as_ref())? != self.expected {
            anyhow::bail!("captured-run summary destination changed identity");
        }
        Ok(())
    }

    fn replace(
        &self,
        bytes: &[u8],
        capture: &GuestRunCaptureSession,
        io: &mut impl SummaryIo,
    ) -> Result<File, Error> {
        self.check(capture)?;
        let mut staged = StagedChild::create(&self.parent, capture)?;
        let mut remaining = bytes;
        while !remaining.is_empty() {
            match io.write(&mut staged.file, remaining) {
                Ok(0) => {
                    return Err(io::Error::from(io::ErrorKind::WriteZero))
                        .context("writing captured-run summary");
                }
                Ok(written) => remaining = &remaining[written..],
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error).context("writing captured-run summary"),
            }
        }
        io.flush(&mut &staged.file)
            .context("flushing captured-run summary publication")?;
        io.sync(&staged.file)
            .context("synchronizing captured-run summary publication")?;
        staged.check()?;
        self.check(capture)?;
        // Checks plus rename are not an atomic compare-and-replace against an
        // arbitrary concurrent rename. Observed leaf/parent changes refuse.
        io.rename(&self.parent, &staged.name, &self.name)
            .context("renaming captured-run summary")?;
        let visible = open_child(&self.parent, &self.name)?;
        if regular_child_identity(visible.as_ref())? != Some(staged.identity) {
            anyhow::bail!("captured-run summary does not name its published inode");
        }
        let visible_parent = open_directory(&self.parent_path)?;
        if identity(&visible_parent.metadata()?) != self.parent_identity {
            anyhow::bail!("captured-run summary parent changed during publication");
        }
        // A sync error after rename can leave visible bytes. Return the error;
        // do not claim success or try to roll back someone else's namespace.
        io.sync(&self.parent)
            .context("synchronizing captured-run summary directory")?;
        Ok(staged.file.try_clone()?)
    }
}

fn open_directory(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(path)
}

fn cstring(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "summary pathname contains NUL"))
}

fn open_child(parent: &File, name: &OsStr) -> io::Result<Option<File>> {
    let name = cstring(name)?;
    // O_PATH does not block on a FIFO and does not require read permission.
    // O_NOFOLLOW leaves a symlink identifiable rather than following its target.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd >= 0 {
        // SAFETY: successful openat returned a newly owned descriptor.
        Ok(Some(unsafe { File::from_raw_fd(fd) }))
    } else {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(error)
        }
    }
}

fn regular_child_identity(file: Option<&File>) -> Result<Option<RunEvidenceFileIdentity>, Error> {
    file.map(|file| {
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            anyhow::bail!("captured-run summary destination is not a regular file");
        }
        Ok(identity(&metadata))
    })
    .transpose()
}

fn rename_child(parent: &File, from: &OsStr, to: &OsStr) -> io::Result<()> {
    let from = cstring(from)?;
    let to = cstring(to)?;
    let rc = unsafe {
        libc::renameat(
            parent.as_raw_fd(),
            from.as_ptr(),
            parent.as_raw_fd(),
            to.as_ptr(),
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

struct StagedChild<'a> {
    parent: &'a File,
    file: File,
    name: OsString,
    identity: RunEvidenceFileIdentity,
}

impl<'a> StagedChild<'a> {
    fn create(parent: &'a File, capture: &GuestRunCaptureSession) -> Result<Self, Error> {
        let directory = PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd()));
        if identity(&fs::metadata(&directory)?) != identity(&parent.metadata()?) {
            anyhow::bail!("summary publication directory descriptor changed identity");
        }
        let temporary = tempfile::Builder::new()
            .prefix(".hermit-summary-")
            .make_in(directory, |path| create_staging_file(path, capture))?;
        // Take responsibility for cleanup: TempPath's unconditional unlink
        // must not remove a replacement introduced by another writer.
        let (file, path) = temporary.keep()?;
        let staged = Self {
            parent,
            identity: identity(&file.metadata()?),
            file,
            name: path
                .file_name()
                .ok_or_else(|| Error::msg("publication file has no name"))?
                .to_owned(),
        };
        staged.check()?;
        Ok(staged)
    }

    fn check(&self) -> Result<(), Error> {
        if regular_child_identity(open_child(self.parent, &self.name)?.as_ref())?
            != Some(self.identity)
        {
            anyhow::bail!("summary publication staging file changed identity");
        }
        Ok(())
    }
}

fn create_staging_file(path: &Path, capture: &GuestRunCaptureSession) -> io::Result<File> {
    // The future result is absent. create_new alone would not stop a random
    // publication name from taking it before capture.finish.
    capture
        .require_distinct_from(path, "summary publication staging")
        .map_err(io::Error::other)?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
}

impl Drop for StagedChild<'_> {
    fn drop(&mut self) {
        // No cleanup after successful rename (the old name is absent). Never
        // intentionally unlink a name observed to refer to somebody else's inode.
        // The identity check and unlink are not atomic against concurrent rename.
        if self.check().is_ok()
            && let Ok(name) = cstring(&self.name)
        {
            unsafe {
                libc::unlinkat(self.parent.as_raw_fd(), name.as_ptr(), 0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use hermit::Backend;
    use hermit::run_evidence::GuestRunDeterminism;
    use reverie::process::ExitStatus;

    use super::*;
    use crate::guest_capture::GuestRunCapturePaths;

    struct Fixture {
        directory: tempfile::TempDir,
        paths: GuestRunCapturePaths,
        capture: GuestRunCaptureSession,
        output: File,
        summary: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let capture_dir = directory.path().join("capture");
            let summary_dir = directory.path().join("summaries");
            fs::create_dir(&capture_dir).unwrap();
            fs::create_dir(&summary_dir).unwrap();
            let evidence = capture_dir.join("evidence");
            fs::create_dir(&evidence).unwrap();
            let paths = GuestRunCapturePaths::new(
                capture_dir.join("result.json"),
                capture_dir.join("stdout"),
                capture_dir.join("stderr"),
            );
            let capture = GuestRunCaptureSession::create(&paths, &evidence).unwrap();
            let output = private_output(directory.path()).unwrap();
            let summary = summary_dir.join("summary.json");
            Self {
                directory,
                paths,
                capture,
                output,
                summary,
            }
        }

        fn complete(
            &mut self,
            bytes: &[u8],
            maximum: usize,
            io: &mut impl SummaryIo,
        ) -> Result<ExitStatus, Error> {
            let status = with_published_summary_using(
                Some(&self.output),
                Some(&self.summary),
                Some(&self.capture),
                maximum,
                io,
                |path| {
                    assert_eq!(fs::read(&self.summary)?, b"");
                    fs::write(path.as_ref().unwrap(), bytes)?;
                    self.capture
                        .write_kvm_virtual_console(b"guest stdout\n", b"guest stderr\n")?;
                    Ok(ExitStatus::Exited(7))
                },
            )?;
            // This is the actual capture publication API, after the same
            // fallible summary boundary used by run_in_container.
            self.capture.finish(
                Backend::Kvm,
                status,
                GuestRunDeterminism {
                    detlog_io_buffers: true,
                    virtualize_time: true,
                },
            )?;
            Ok(status)
        }

        fn assert_no_result(&self) {
            assert!(!self.paths.result.exists());
            assert_eq!(fs::read(&self.paths.stdout).unwrap(), b"guest stdout\n");
            assert_eq!(fs::read(&self.paths.stderr).unwrap(), b"guest stderr\n");
        }
    }

    #[test]
    fn absent_and_existing_regular_summaries_publish_exact_bytes() {
        for existing in [false, true] {
            let mut fixture = Fixture::new();
            let old_alias = fixture.directory.path().join("old-inode");
            if existing {
                fs::write(&fixture.summary, b"old successful summary\n").unwrap();
                fs::hard_link(&fixture.summary, &old_alias).unwrap();
            }
            let bytes = b"{\"answer\":42}\n";
            assert_eq!(
                fixture
                    .complete(bytes, MAX_SUMMARY_BYTES, &mut NativeIo)
                    .unwrap(),
                ExitStatus::Exited(7)
            );
            assert_eq!(fs::read(&fixture.summary).unwrap(), bytes);
            assert!(fixture.paths.result.is_file());
            assert_eq!(fs::read(&fixture.paths.stdout).unwrap(), b"guest stdout\n");
            assert_eq!(fs::read(&fixture.paths.stderr).unwrap(), b"guest stderr\n");
            if existing {
                assert_eq!(fs::read(old_alias).unwrap(), b"old successful summary\n");
            }
            assert_eq!(
                fs::read_dir(fixture.summary.parent().unwrap())
                    .unwrap()
                    .count(),
                1
            );
        }
    }

    #[test]
    fn initial_symlink_and_nonregular_summaries_refuse_before_run() {
        for kind in ["symlink", "directory", "fifo"] {
            let fixture = Fixture::new();
            let victim = fixture.directory.path().join("victim");
            fs::write(&victim, b"unchanged victim\n").unwrap();
            match kind {
                "symlink" => symlink(&victim, &fixture.summary).unwrap(),
                "directory" => fs::create_dir(&fixture.summary).unwrap(),
                "fifo" => {
                    let path = cstring(fixture.summary.as_os_str()).unwrap();
                    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
                }
                _ => unreachable!(),
            }
            let mut entered = false;
            let error = with_published_summary(
                Some(&fixture.output),
                Some(&fixture.summary),
                Some(&fixture.capture),
                |_| {
                    entered = true;
                    Ok(())
                },
            )
            .unwrap_err();
            assert!(
                error.to_string().contains("not a regular file"),
                "{error:#}"
            );
            assert!(!entered);
            assert_eq!(fs::read(victim).unwrap(), b"unchanged victim\n");
            assert!(!fixture.paths.result.exists());
        }
    }

    #[test]
    fn summary_placeholder_invalidates_stale_success_before_backend_error() {
        let fixture = Fixture::new();
        fs::write(&fixture.summary, b"old successful summary\n").unwrap();
        let error = with_published_summary(
            Some(&fixture.output),
            Some(&fixture.summary),
            Some(&fixture.capture),
            |_| -> Result<(), Error> {
                assert_eq!(fs::read(&fixture.summary)?, b"");
                anyhow::bail!("constructed backend error");
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("constructed backend error"));
        assert_eq!(fs::read(&fixture.summary).unwrap(), b"");
        assert!(!fixture.paths.result.exists());
    }

    #[test]
    fn missing_summary_parent_remains_a_refusal_before_run() {
        let fixture = Fixture::new();
        let missing = fixture.directory.path().join("missing/summary.json");
        let mut entered = false;
        assert!(
            with_published_summary(
                Some(&fixture.output),
                Some(&missing),
                Some(&fixture.capture),
                |_| {
                    entered = true;
                    Ok(())
                }
            )
            .is_err()
        );
        assert!(!entered);
        assert!(!missing.parent().unwrap().exists());
        assert!(!fixture.paths.result.exists());
    }

    #[test]
    fn post_prepare_symlink_regular_and_parent_replacements_refuse() {
        for kind in ["symlink", "regular", "parent"] {
            let fixture = Fixture::new();
            let victim = fixture.directory.path().join("victim");
            fs::write(&victim, b"unchanged victim\n").unwrap();
            let error = with_published_summary(
                Some(&fixture.output),
                Some(&fixture.summary),
                Some(&fixture.capture),
                |path| {
                    fs::write(path.as_ref().unwrap(), b"new summary\n")?;
                    match kind {
                        "symlink" => {
                            fs::remove_file(&fixture.summary)?;
                            symlink(&victim, &fixture.summary)?;
                        }
                        "regular" => {
                            let replacement = fixture.directory.path().join("replacement");
                            fs::write(&replacement, b"another writer\n")?;
                            fs::rename(replacement, &fixture.summary)?;
                        }
                        "parent" => {
                            let parent = fixture.summary.parent().unwrap();
                            fs::rename(parent, fixture.directory.path().join("old-parent"))?;
                            fs::create_dir(parent)?;
                            fs::write(&fixture.summary, b"another writer\n")?;
                        }
                        _ => unreachable!(),
                    }
                    Ok(())
                },
            )
            .unwrap_err();
            assert!(
                error.to_string().contains(if kind == "symlink" {
                    "not a regular file"
                } else {
                    "changed identity"
                }),
                "{error:#}"
            );
            assert_eq!(fs::read(&victim).unwrap(), b"unchanged victim\n");
            assert_eq!(
                fs::read(&fixture.summary).unwrap(),
                if kind == "symlink" {
                    b"unchanged victim\n".as_slice()
                } else {
                    b"another writer\n".as_slice()
                }
            );
            assert!(!fixture.paths.result.exists());
        }
    }

    #[derive(Default)]
    struct CountingReader {
        requested: Vec<usize>,
        returned: usize,
    }
    impl SummaryIo for CountingReader {
        fn read(&mut self, file: &mut &File, buffer: &mut [u8]) -> io::Result<usize> {
            self.requested.push(buffer.len());
            let read = file.read(buffer)?;
            self.returned += read;
            Ok(read)
        }
    }

    #[test]
    fn exact_limit_publishes_and_maximum_plus_one_refuses_before_capture_result() {
        for maximum in [4, MAX_SUMMARY_BYTES] {
            for excess in [false, true] {
                let mut fixture = Fixture::new();
                let mut bytes = vec![b'x'; maximum + usize::from(excess)];
                *bytes.last_mut().unwrap() = b'\n';
                let mut io = CountingReader::default();
                let result = fixture.complete(&bytes, maximum, &mut io);
                assert_eq!(io.requested.first(), Some(&(maximum + 1)));
                assert!(
                    io.requested
                        .iter()
                        .all(|requested| *requested <= maximum + 1)
                );
                assert_eq!(io.returned, bytes.len());
                if excess {
                    assert!(
                        result
                            .unwrap_err()
                            .to_string()
                            .contains("publication limit")
                    );
                    assert_eq!(fs::read(&fixture.summary).unwrap(), b"");
                    fixture.assert_no_result();
                } else {
                    assert_eq!(result.unwrap(), ExitStatus::Exited(7));
                    assert_eq!(fs::read(&fixture.summary).unwrap(), bytes);
                    assert!(fixture.paths.result.is_file());
                }
            }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Failure {
        Read,
        OutputFlush,
        OutputSync,
        PartialWrite,
        PublicationFlush,
        PublicationSync,
        Rename,
        VisibleIdentity,
        DirectorySync,
    }
    struct FaultIo {
        failure: Failure,
        flushes: usize,
        syncs: usize,
        writes: usize,
        destination: PathBuf,
    }
    impl FaultIo {
        fn error() -> io::Error {
            io::Error::other("injected summary I/O error")
        }
    }
    impl SummaryIo for FaultIo {
        fn read(&mut self, file: &mut &File, buffer: &mut [u8]) -> io::Result<usize> {
            if self.failure == Failure::Read {
                Err(Self::error())
            } else {
                file.read(buffer)
            }
        }
        fn write(&mut self, file: &mut File, bytes: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            if self.failure == Failure::PartialWrite {
                if self.writes == 1 {
                    file.write(&bytes[..1])
                } else {
                    Err(Self::error())
                }
            } else {
                file.write(bytes)
            }
        }
        fn flush(&mut self, file: &mut &File) -> io::Result<()> {
            self.flushes += 1;
            if (self.failure == Failure::OutputFlush && self.flushes == 1)
                || (self.failure == Failure::PublicationFlush && self.flushes == 2)
            {
                Err(Self::error())
            } else {
                file.flush()
            }
        }
        fn sync(&mut self, file: &File) -> io::Result<()> {
            self.syncs += 1;
            if (self.failure == Failure::OutputSync && self.syncs == 1)
                || (self.failure == Failure::PublicationSync && self.syncs == 2)
                || (self.failure == Failure::DirectorySync && file.metadata()?.is_dir())
            {
                Err(Self::error())
            } else {
                file.sync_all()
            }
        }
        fn rename(&mut self, parent: &File, from: &OsStr, to: &OsStr) -> io::Result<()> {
            if self.failure == Failure::Rename {
                return Err(Self::error());
            }
            rename_child(parent, from, to)?;
            if self.failure == Failure::VisibleIdentity {
                fs::remove_file(&self.destination)?;
                fs::write(&self.destination, b"another writer\n")?;
            }
            Ok(())
        }
    }

    #[test]
    fn publication_io_errors_propagate_and_prevent_capture_result() {
        for failure in [
            Failure::Read,
            Failure::OutputFlush,
            Failure::OutputSync,
            Failure::PartialWrite,
            Failure::PublicationFlush,
            Failure::PublicationSync,
            Failure::Rename,
            Failure::VisibleIdentity,
            Failure::DirectorySync,
        ] {
            let mut fixture = Fixture::new();
            let mut io = FaultIo {
                failure,
                flushes: 0,
                syncs: 0,
                writes: 0,
                destination: fixture.summary.clone(),
            };
            let error = fixture
                .complete(b"accepted summary\n", MAX_SUMMARY_BYTES, &mut io)
                .unwrap_err();
            if failure == Failure::VisibleIdentity {
                assert!(
                    error.to_string().contains("published inode"),
                    "{failure:?}: {error:#}"
                );
            } else {
                assert!(
                    format!("{error:#}").contains("injected summary I/O error"),
                    "{failure:?}: {error:#}"
                );
            }
            if failure == Failure::PartialWrite {
                assert_eq!(io.writes, 2);
            }
            let expected: &[u8] = match failure {
                Failure::VisibleIdentity => b"another writer\n",
                Failure::DirectorySync => b"accepted summary\n",
                _ => b"",
            };
            assert_eq!(fs::read(&fixture.summary).unwrap(), expected, "{failure:?}");
            assert_eq!(
                fs::read_dir(fixture.summary.parent().unwrap())
                    .unwrap()
                    .count(),
                1,
                "{failure:?}"
            );
            fixture.assert_no_result();
        }
    }

    #[test]
    fn private_output_requires_unlinked_writable_regular_cloexec_descriptor() {
        let directory = tempfile::tempdir().unwrap();
        let output = private_output(directory.path()).unwrap();
        assert_eq!(output.metadata().unwrap().nlink(), 0);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
        let path = writer_path(&output).unwrap();
        fs::write(path, b"controller bytes\n").unwrap();
        assert_eq!(
            read_output(&output, 64, &mut NativeIo).unwrap(),
            b"controller bytes\n"
        );
        let ordinary = directory.path().join("ordinary");
        fs::write(&ordinary, b"unmodified\n").unwrap();
        assert!(
            require_output_descriptor(&File::open(&ordinary).unwrap())
                .unwrap_err()
                .to_string()
                .contains("readable and writable")
        );
        assert!(
            require_output_descriptor(&File::open(directory.path()).unwrap())
                .unwrap_err()
                .to_string()
                .contains("not a regular file")
        );
        assert!(
            require_writer_path(&output, &ordinary)
                .unwrap_err()
                .to_string()
                .contains("changed identity")
        );
        assert!(require_writer_path(&output, &directory.path().join("missing-proc-fd")).is_err());
        assert_eq!(
            unsafe { libc::fcntl(output.as_raw_fd(), libc::F_SETFD, 0) },
            0
        );
        assert!(
            writer_path(&output)
                .unwrap_err()
                .to_string()
                .contains("CLOEXEC")
        );
        assert_eq!(fs::read(ordinary).unwrap(), b"unmodified\n");
    }

    #[test]
    fn changing_private_output_size_during_read_refuses() {
        struct ChangeSize;
        impl SummaryIo for ChangeSize {
            fn read(&mut self, file: &mut &File, buffer: &mut [u8]) -> io::Result<usize> {
                let count = file.read(buffer)?;
                if count != 0 {
                    file.set_len(1)?;
                }
                Ok(count)
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let output = private_output(directory.path()).unwrap();
        fs::write(writer_path(&output).unwrap(), b"summary\n").unwrap();
        assert!(
            read_output(&output, 32, &mut ChangeSize)
                .unwrap_err()
                .to_string()
                .contains("changed identity or size")
        );
    }

    #[test]
    fn final_capture_collision_is_rechecked_after_completed_run() {
        let fixture = Fixture::new();
        let before = fs::read(&fixture.paths.stdout).unwrap();
        let error = with_published_summary(
            Some(&fixture.output),
            Some(&fixture.summary),
            Some(&fixture.capture),
            |path| {
                fs::write(path.as_ref().unwrap(), b"summary\n")?;
                fs::remove_file(&fixture.summary)?;
                fs::hard_link(&fixture.paths.stdout, &fixture.summary)?;
                Ok(())
            },
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("must not reuse the --summary-json path")
        );
        assert_eq!(fs::read(&fixture.paths.stdout).unwrap(), before);
        assert!(!fixture.paths.result.exists());
    }

    #[test]
    fn publication_staging_cannot_create_the_absent_capture_result() {
        let fixture = Fixture::new();
        assert!(!fixture.paths.result.exists());
        assert!(
            create_staging_file(&fixture.paths.result, &fixture.capture)
                .unwrap_err()
                .to_string()
                .contains("must not reuse")
        );
        assert!(!fixture.paths.result.exists());
        let other = fixture.directory.path().join("distinct-staging");
        let mut file = create_staging_file(&other, &fixture.capture).unwrap();
        file.write_all(b"distinct\n").unwrap();
        assert_eq!(fs::read(other).unwrap(), b"distinct\n");
    }

    #[test]
    fn uncaptured_run_preserves_original_summary_path_and_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ordinary.json");
        let bytes = vec![b'x'; MAX_SUMMARY_BYTES + 2];
        let error =
            with_published_summary(None, Some(&path), None, |actual| -> Result<(), Error> {
                assert_eq!(actual.as_deref(), Some(path.as_path()));
                fs::write(actual.as_ref().unwrap(), &bytes)?;
                anyhow::bail!("ordinary failure");
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "ordinary failure");
        assert_eq!(fs::read(path).unwrap(), bytes);
        with_published_summary(None, None, None, |actual| {
            assert!(actual.is_none());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn summary_staging_stays_outside_prepared_source_enumeration() {
        fn source_files(root: &Path) -> Vec<(Vec<u8>, Vec<u8>)> {
            let output = std::process::Command::new("git")
                .args([
                    "ls-files",
                    "--cached",
                    "--others",
                    "--exclude-standard",
                    "-z",
                ])
                .current_dir(root)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            output
                .stdout
                .split(|byte| *byte == 0)
                .filter(|name| !name.is_empty())
                .map(|name| {
                    (
                        name.to_vec(),
                        fs::read(root.join(OsStr::from_bytes(name))).unwrap(),
                    )
                })
                .collect()
        }
        struct ObserveSource<'a> {
            root: &'a Path,
            before: &'a [(Vec<u8>, Vec<u8>)],
            observed: bool,
        }
        impl SummaryIo for ObserveSource<'_> {
            fn write(&mut self, file: &mut File, bytes: &[u8]) -> io::Result<usize> {
                assert_eq!(source_files(self.root), self.before);
                assert!(
                    fs::read_dir(self.root.join("ignored/artifacts"))
                        .unwrap()
                        .any(|entry| entry
                            .unwrap()
                            .file_name()
                            .as_bytes()
                            .starts_with(b".hermit-summary-"))
                );
                self.observed = true;
                file.write(bytes)
            }
        }
        let fixture = Fixture::new();
        let root = fixture.directory.path();
        let init = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(init.status.success());
        fs::write(
            root.join(".gitignore"),
            include_bytes!("../../../../.gitignore"),
        )
        .unwrap();
        let destination = root.join("ignored/artifacts/summary.json");
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        let before = source_files(root);
        let mut io = ObserveSource {
            root,
            before: &before,
            observed: false,
        };
        with_published_summary_using(
            Some(&fixture.output),
            Some(&destination),
            Some(&fixture.capture),
            64,
            &mut io,
            |path| {
                assert_eq!(source_files(root), before);
                fs::write(path.as_ref().unwrap(), b"summary\n")?;
                Ok(())
            },
        )
        .unwrap();
        assert!(io.observed);
        assert_eq!(source_files(root), before);
        let leaked = root.join(".hermit-verify-summary-leaked");
        fs::write(&leaked, b"detect this leak\n").unwrap();
        assert!(
            source_files(root)
                .iter()
                .any(|(name, bytes)| name == b".hermit-verify-summary-leaked"
                    && bytes == b"detect this leak\n")
        );
        fs::remove_file(leaked).unwrap();
        assert_eq!(source_files(root), before);
    }

    /// Requires real mount/user/PID namespaces. Select separately from the
    /// native file controls; this is not a Detcore guest or a parity result.
    #[test]
    fn container_summary_uses_held_output_and_controller_cwd() {
        use reverie::process::Mount;

        let fixture = Fixture::new();
        let root = fixture.directory.path();
        let hidden = root.join("private-output");
        let controller = root.join("controller");
        let guest = root.join("guest-workdir");
        for path in [&hidden, &controller, &guest] {
            fs::create_dir(path).unwrap();
        }
        let marker = hidden.join("host-only");
        fs::write(&marker, b"host marker").unwrap();
        let output = private_output(&hidden).unwrap();
        let held = identity(&output.metadata().unwrap());
        let parent_pid = std::process::id();
        let mut container = crate::container::default_container(false);
        container.mount(Mount::tmpfs(&hidden));
        let status = crate::container::with_container(&mut container, || {
            assert_ne!(std::process::id(), parent_pid);
            assert!(
                !marker.exists(),
                "the child still sees the host temporary directory"
            );
            std::env::set_current_dir(&controller)?;
            with_published_summary(
                Some(&output),
                Some(Path::new("summary.json")),
                Some(&fixture.capture),
                |path| {
                    assert_eq!(identity(&fs::metadata(path.as_ref().unwrap())?), held);
                    assert_eq!(output.metadata()?.nlink(), 0);
                    let exec = std::process::Command::new("/bin/sh")
                        .args([
                            "-c",
                            "test ! -e /proc/self/fd/\"$1\"",
                            "summary-fd-control",
                            &output.as_raw_fd().to_string(),
                        ])
                        .current_dir(&guest)
                        .output()?;
                    assert!(exec.status.success(), "summary descriptor survived exec");
                    fs::write(path.as_ref().unwrap(), b"controller summary\n")?;
                    Ok(7)
                },
            )
        })
        .unwrap();
        assert_eq!(status, 7);
        assert_eq!(
            fs::read(controller.join("summary.json")).unwrap(),
            b"controller summary\n"
        );
        assert!(!guest.join("summary.json").exists());
        assert_eq!(fs::read(marker).unwrap(), b"host marker");
        assert_eq!(identity(&output.metadata().unwrap()), held);
    }

    /// The same spelling is independent on the host and aliases a capture
    /// stream after the actual bind mount. Refuse before entering the run.
    #[test]
    fn container_summary_collision_refuses_after_mount_resolution_changes() {
        use reverie::process::Mount;

        let fixture = Fixture::new();
        let alias = fixture.directory.path().join("controller-view");
        fs::create_dir(&alias).unwrap();
        let destination = alias.join("stdout");
        fs::write(&destination, b"distinct host file\n").unwrap();
        fixture
            .capture
            .require_distinct_from(&destination, "--summary-json")
            .unwrap();
        let mut container = crate::container::default_container(false);
        container.mount(Mount::bind(fixture.paths.stdout.parent().unwrap(), &alias));
        let error = crate::container::with_container(&mut container, || {
            with_published_summary(
                Some(&fixture.output),
                Some(&destination),
                Some(&fixture.capture),
                |_| -> Result<(), Error> {
                    panic!("summary collision entered the run");
                },
            )
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("must not reuse the --summary-json path"));
        assert_eq!(fs::read(&destination).unwrap(), b"distinct host file\n");
        assert_eq!(fs::read(&fixture.paths.stdout).unwrap(), b"");
        assert!(!fixture.paths.result.exists());
    }
}
