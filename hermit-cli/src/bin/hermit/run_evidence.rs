use std::ffi::CString;
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;

use hermit::Backend;
use hermit::Context;
use hermit::Error;
use hermit::canonical_verdict::RecordEnvelopeReport;
use hermit::run_evidence::CanonicalInfoEvidence;
use hermit::run_evidence::CanonicalInfoPolicy;
use hermit::run_evidence::DispositionLimitation;
use hermit::run_evidence::GuestDisposition;
use hermit::run_evidence::RUN_EVIDENCE_INFO_ARTIFACT;
use hermit::run_evidence::RUN_EVIDENCE_MANIFEST;
use hermit::run_evidence::RUN_EVIDENCE_MANIFEST_MODE;
use hermit::run_evidence::RUN_EVIDENCE_SCHEMA_VERSION;
use hermit::run_evidence::RunEvidenceBackend;
use hermit::run_evidence::RunEvidenceNoResultReason;
use hermit::run_evidence::RunEvidenceOutcome;
use hermit::run_evidence::RunEvidenceReport;
use reverie::process::ExitStatus;
use uuid::Uuid;

#[cfg(test)]
use super::tracing::LatchedWriter;
use super::tracing::WriteErrorLatch;

const ARTIFACT_MODE: libc::mode_t = 0o600;
const MANIFEST_UNPUBLISHED_MODE: libc::mode_t = 0o000;
const MANIFEST_PUBLISHED_MODE: libc::mode_t = RUN_EVIDENCE_MANIFEST_MODE;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DirectorySyncPoint {
    ParentClaim,
    ArtifactPublished,
    ManifestPreflight,
    ManifestBeforeLink,
    AnonymousPublication,
}

#[cfg(test)]
enum InjectedSyncFailure {
    Once {
        point: DirectorySyncPoint,
        fired: Arc<std::sync::atomic::AtomicBool>,
    },
    PersistentManifestPreparation,
}

#[derive(Default)]
struct DirectorySync {
    #[cfg(test)]
    injected_failure: Option<InjectedSyncFailure>,
}

impl DirectorySync {
    fn sync(&self, point: DirectorySyncPoint, directory: &File) -> io::Result<()> {
        #[cfg(not(test))]
        let _ = point;
        #[cfg(test)]
        if let Some(failure) = &self.injected_failure {
            let should_fail = match failure {
                InjectedSyncFailure::Once {
                    point: fail_point,
                    fired,
                } => point == *fail_point && !fired.swap(true, std::sync::atomic::Ordering::SeqCst),
                InjectedSyncFailure::PersistentManifestPreparation => matches!(
                    point,
                    DirectorySyncPoint::ManifestPreflight | DirectorySyncPoint::ManifestBeforeLink
                ),
            };
            if should_fail {
                return Err(io::Error::other(format!(
                    "injected {point:?} directory fsync failure"
                )));
            }
        }
        directory.sync_all()
    }

    #[cfg(test)]
    fn fail_once(point: DirectorySyncPoint) -> Self {
        Self {
            injected_failure: Some(InjectedSyncFailure::Once {
                point,
                fired: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            }),
        }
    }

    #[cfg(test)]
    fn fail_manifest_persistently() -> Self {
        Self {
            injected_failure: Some(InjectedSyncFailure::PersistentManifestPreparation),
        }
    }
}

pub(crate) struct RunEvidenceSession {
    invocation_id: Uuid,
    backend: Backend,
    raw_log: Arc<File>,
    artifact: File,
    manifest: File,
    write_error: WriteErrorLatch,
    directory_sync: DirectorySync,
    // Drop the exclusive-lock description last, after every mutable file handle.
    directory: File,
}

impl RunEvidenceSession {
    /// Atomically claim a destination that did not exist before this invocation.
    pub(crate) fn create(directory: &Path, backend: Backend) -> Result<Self, Error> {
        Self::create_with_directory_sync(directory, backend, DirectorySync::default())
    }

    fn create_with_directory_sync(
        directory: &Path,
        backend: Backend,
        directory_sync: DirectorySync,
    ) -> Result<Self, Error> {
        let invocation_id = Uuid::new_v4();
        let raw_log = tempfile::tempfile().with_context(|| {
            format!(
                "cannot create private run-evidence capture for {}",
                directory.display()
            )
        })?;
        let write_error = WriteErrorLatch::new().with_context(|| {
            format!(
                "cannot create process-shared run-evidence error latch for {}",
                directory.display()
            )
        })?;
        let claimed = create_claimed_directory(directory, invocation_id, &directory_sync)
            .with_context(|| {
                format!(
                    "cannot create --run-evidence-dir {}: the destination must not already exist",
                    directory.display()
                )
            })?;
        let artifact =
            create_unnamed_file_at(claimed.as_raw_fd(), ARTIFACT_MODE).with_context(|| {
                format!(
                    "cannot create unnamed run-evidence artifact in {}",
                    directory.display()
                )
            })?;
        let manifest = create_unnamed_file_at(claimed.as_raw_fd(), MANIFEST_UNPUBLISHED_MODE)
            .with_context(|| {
                format!(
                    "cannot create unnamed run-evidence manifest in {}",
                    directory.display()
                )
            })?;
        // Opening O_TMPFILE alone does not establish that AT_EMPTY_PATH/no-replace
        // publication works. Exercise the exact primitive before guest launch.
        verify_anonymous_publication(&claimed, invocation_id, &directory_sync)
            .context("preflighting anonymous run-evidence publication before launch")?;
        Ok(Self {
            directory: claimed,
            invocation_id,
            backend,
            raw_log: Arc::new(raw_log),
            artifact,
            manifest,
            write_error,
            directory_sync,
        })
    }

    pub(crate) fn log_handle(&self) -> Arc<File> {
        Arc::clone(&self.raw_log)
    }

    pub(crate) fn write_error_latch(&self) -> WriteErrorLatch {
        self.write_error.clone()
    }

    /// Publish a terminal candidate after the guest result and log artifact are final.
    /// Successful typed inspection, including checked file/directory synchronization,
    /// certifies durable Complete. Publication itself has no post-link rollback.
    pub(crate) fn finish(mut self, run_result: Result<&ExitStatus, &Error>) -> Result<(), Error> {
        let observed_disposition = run_result
            .ok()
            .and_then(|status| guest_disposition(self.backend, *status));
        let unsupported = !matches!(
            self.backend,
            Backend::Ptrace | Backend::Liteinst | Backend::Kvm
        );

        let (outcome, canonical_info) = if unsupported {
            (
                RunEvidenceOutcome::NoResult {
                    reason: RunEvidenceNoResultReason::UnsupportedBackend,
                    observed_disposition,
                },
                CanonicalInfoEvidence::no_result(),
            )
        } else if run_result.is_err() {
            (
                RunEvidenceOutcome::NoResult {
                    reason: RunEvidenceNoResultReason::RunFailed,
                    observed_disposition: None,
                },
                CanonicalInfoEvidence::no_result(),
            )
        } else if self.write_error.failed() {
            (
                RunEvidenceOutcome::NoResult {
                    reason: RunEvidenceNoResultReason::MissingCanonicalInfo,
                    observed_disposition,
                },
                CanonicalInfoEvidence::no_result(),
            )
        } else if let Some(disposition) = observed_disposition {
            match self.publish_canonical_info() {
                Ok(canonical_info) => {
                    (RunEvidenceOutcome::Complete { disposition }, canonical_info)
                }
                Err(reason) => (
                    RunEvidenceOutcome::NoResult {
                        reason,
                        observed_disposition,
                    },
                    CanonicalInfoEvidence::no_result(),
                ),
            }
        } else {
            (
                RunEvidenceOutcome::NoResult {
                    reason: RunEvidenceNoResultReason::UnsupportedDisposition,
                    observed_disposition: None,
                },
                CanonicalInfoEvidence::no_result(),
            )
        };

        let report = RunEvidenceReport {
            schema_version: RUN_EVIDENCE_SCHEMA_VERSION,
            invocation_id: self.invocation_id,
            backend: report_backend(self.backend),
            attempt: 1,
            outcome,
            canonical_info,
        };
        self.publish_manifest(&report)
    }

    fn read_private_log(&self) -> Result<Vec<u8>, RunEvidenceNoResultReason> {
        if self.write_error.failed() {
            return Err(RunEvidenceNoResultReason::MissingCanonicalInfo);
        }
        let mut file = self
            .raw_log
            .try_clone()
            .map_err(|_| RunEvidenceNoResultReason::MissingCanonicalInfo)?;
        file.seek(SeekFrom::Start(0))
            .map_err(|_| RunEvidenceNoResultReason::MissingCanonicalInfo)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|_| RunEvidenceNoResultReason::MissingCanonicalInfo)?;
        if self.write_error.failed() {
            return Err(RunEvidenceNoResultReason::MissingCanonicalInfo);
        }
        Ok(bytes)
    }

    fn publish_canonical_info(
        &mut self,
    ) -> Result<CanonicalInfoEvidence, RunEvidenceNoResultReason> {
        let bytes = self.read_private_log()?;
        if bytes.is_empty() {
            return Err(RunEvidenceNoResultReason::ZeroCanonicalInfo);
        }
        if std::str::from_utf8(&bytes)
            .ok()
            .is_some_and(detcore::logdiff::log_was_truncated)
        {
            return Err(RunEvidenceNoResultReason::TruncatedCanonicalInfo);
        }
        let message_count = detcore::logdiff::write_bitwise_info_v1_bytes(
            &bytes,
            "private run-evidence log",
            &mut std::io::sink(),
        )
        .map_err(|_| RunEvidenceNoResultReason::MalformedCanonicalInfo)?
            as u64;
        if message_count == 0 {
            return Err(RunEvidenceNoResultReason::ZeroCanonicalInfo);
        }

        let digest = detcore::Digest::new(&bytes).to_string();
        write_file_contents(&mut self.artifact, &bytes)
            .map_err(|_| RunEvidenceNoResultReason::ArtifactWriteFailed)?;
        let published = read_file_contents(&mut self.artifact)
            .map_err(|_| RunEvidenceNoResultReason::ArtifactWriteFailed)?;
        if published.len() != bytes.len() || detcore::Digest::new(&published).to_string() != digest
        {
            return Err(RunEvidenceNoResultReason::ArtifactWriteFailed);
        }
        link_unnamed_file_at(
            &self.artifact,
            self.directory.as_raw_fd(),
            OsStr::new(RUN_EVIDENCE_INFO_ARTIFACT),
        )
        .map_err(|_| RunEvidenceNoResultReason::ArtifactWriteFailed)?;
        self.directory_sync
            .sync(DirectorySyncPoint::ArtifactPublished, &self.directory)
            .map_err(|_| RunEvidenceNoResultReason::ArtifactWriteFailed)?;

        Ok(CanonicalInfoEvidence {
            policy: CanonicalInfoPolicy::BitwiseInfoV1,
            record_envelope: RecordEnvelopeReport::AllRecordsV1,
            message_count,
            byte_count: bytes.len() as u64,
            sha256: Some(digest),
            artifact: RUN_EVIDENCE_INFO_ARTIFACT.to_string(),
        })
    }

    fn publish_manifest(&mut self, report: &RunEvidenceReport) -> Result<(), Error> {
        let mut contents = serde_json::to_vec(report)?;
        contents.push(b'\n');
        write_file_contents(&mut self.manifest, &contents)
            .context("writing the held run-evidence manifest inode")?;

        // A persistent inability to synchronize this directory is discovered
        // before a complete manifest gets a name.
        self.directory_sync
            .sync(DirectorySyncPoint::ManifestPreflight, &self.directory)
            .context("preflighting the run-evidence directory before manifest publication")?;

        let prepared = read_file_contents(&mut self.manifest)
            .context("reading back the held run-evidence manifest inode")?;
        if prepared != contents {
            return Err(Error::msg("held run-evidence manifest readback differs"));
        }
        // Activation and its inode sync happen while the manifest has no name.
        // An error cannot expose a candidate that cleanup would have to revoke.
        set_file_mode(&self.manifest, MANIFEST_PUBLISHED_MODE)
            .context("activating the unnamed run-evidence manifest")?;
        self.manifest
            .sync_all()
            .context("synchronizing the activated unnamed run-evidence manifest")?;
        self.directory_sync
            .sync(DirectorySyncPoint::ManifestBeforeLink, &self.directory)
            .context("synchronizing the run-evidence directory before manifest publication")?;

        // COMMIT: this no-replace link is the final fallible producer operation.
        // No sync, write, deactivation or rollback follows success. The inspector
        // holds the directory lock and checks the final directory sync itself.
        link_unnamed_file_at(
            &self.manifest,
            self.directory.as_raw_fd(),
            OsStr::new(RUN_EVIDENCE_MANIFEST),
        )
        .context("linking the exact held run-evidence manifest inode without replacement")
    }
}

fn write_file_contents(file: &mut File, contents: &[u8]) -> io::Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(contents)?;
    file.flush()?;
    file.sync_all()
}

fn read_file_contents(file: &mut File) -> io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(0))?;
    let mut contents = Vec::new();
    file.read_to_end(&mut contents)?;
    Ok(contents)
}

fn path_cstring(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

fn component_cstring(component: &OsStr) -> io::Result<CString> {
    CString::new(component.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path component contains NUL"))
}

fn owned_file(fd: RawFd) -> io::Result<File> {
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: a nonnegative open/openat return is one newly owned fd.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn open_directory(path: &Path) -> io::Result<File> {
    let path = path_cstring(path)?;
    owned_file(unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    })
}

fn open_directory_at(parent: RawFd, component: &OsStr) -> io::Result<File> {
    let component = component_cstring(component)?;
    owned_file(unsafe {
        libc::openat(
            parent,
            component.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    })
}

fn create_unnamed_file_at(directory: RawFd, mode: libc::mode_t) -> io::Result<File> {
    let dot = c".";
    owned_file(unsafe {
        libc::openat(
            directory,
            dot.as_ptr(),
            libc::O_TMPFILE | libc::O_RDWR | libc::O_CLOEXEC,
            mode,
        )
    })
}

fn link_unnamed_file_at(file: &File, directory: RawFd, destination: &OsStr) -> io::Result<()> {
    let empty = c"";
    let destination = component_cstring(destination)?;
    if unsafe {
        libc::linkat(
            file.as_raw_fd(),
            empty.as_ptr(),
            directory,
            destination.as_ptr(),
            libc::AT_EMPTY_PATH,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn set_file_mode(file: &File, mode: libc::mode_t) -> io::Result<()> {
    if unsafe { libc::fchmod(file.as_raw_fd(), mode) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn unlink_file_at(parent: RawFd, component: &OsStr) -> io::Result<()> {
    let component = component_cstring(component)?;
    if unsafe { libc::unlinkat(parent, component.as_ptr(), 0) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn remove_directory_at(parent: RawFd, component: &OsStr) -> io::Result<()> {
    let component = component_cstring(component)?;
    if unsafe { libc::unlinkat(parent, component.as_ptr(), libc::AT_REMOVEDIR) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn rename_noreplace_at(parent: RawFd, source: &OsStr, destination: &OsStr) -> io::Result<()> {
    let source = component_cstring(source)?;
    let destination = component_cstring(destination)?;
    if unsafe {
        libc::renameat2(
            parent,
            source.as_ptr(),
            parent,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn file_identity(file: &File) -> io::Result<(libc::dev_t, libc::ino_t)> {
    let mut stat = MaybeUninit::<libc::stat>::zeroed();
    if unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fstat initialized the complete structure on success.
    let stat = unsafe { stat.assume_init() };
    Ok((stat.st_dev, stat.st_ino))
}

/// Claim NEW_PATH by atomically renaming an already-open private directory.
///
/// The returned fd, not the pathname, is the authority for every later child
/// operation. A descriptor-relative identity check after rename refuses a
/// replaced staging object; a later rename or replacement cannot redirect
/// publication because no later operation resolves NEW_PATH.
fn create_claimed_directory(
    path: &Path,
    invocation_id: Uuid,
    directory_sync: &DirectorySync,
) -> io::Result<File> {
    let destination = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "destination has no basename")
    })?;
    let parent_path = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = open_directory(parent_path)?;
    let staging_name = format!(".run-evidence-claim-{invocation_id}");
    let staging = OsStr::new(&staging_name);
    let staging_c = component_cstring(staging)?;
    if unsafe { libc::mkdirat(parent.as_raw_fd(), staging_c.as_ptr(), 0o700) } != 0 {
        return Err(io::Error::last_os_error());
    }

    let claimed = match open_directory_at(parent.as_raw_fd(), staging) {
        Ok(claimed) => claimed,
        Err(error) => {
            return match remove_directory_at(parent.as_raw_fd(), staging) {
                Ok(()) => Err(error),
                Err(cleanup) => Err(io::Error::other(format!(
                    "{error}; staging directory cleanup failed: {cleanup}"
                ))),
            };
        }
    };
    // Acquire the lock before this directory becomes visible. The retained open
    // description, including inherited fork copies, owns it until publication ends.
    let claim = lock_exclusive(&claimed)
        .and_then(|()| rename_noreplace_at(parent.as_raw_fd(), staging, destination));
    if let Err(error) = claim {
        return match remove_directory_at(parent.as_raw_fd(), staging) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(io::Error::other(format!(
                "{error}; staging directory cleanup failed: {cleanup}"
            ))),
        };
    }

    let visible = open_directory_at(parent.as_raw_fd(), destination)?;
    if file_identity(&visible)? != file_identity(&claimed)? {
        return Err(io::Error::other(
            "NEW_PATH no longer names the directory inode this invocation claimed",
        ));
    }
    directory_sync.sync(DirectorySyncPoint::ParentClaim, &parent)?;
    Ok(claimed)
}

fn lock_exclusive(directory: &File) -> io::Result<()> {
    if unsafe { libc::flock(directory.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn verify_anonymous_publication(
    directory: &File,
    invocation_id: Uuid,
    directory_sync: &DirectorySync,
) -> io::Result<()> {
    let probe = create_unnamed_file_at(directory.as_raw_fd(), MANIFEST_UNPUBLISHED_MODE)?;
    let name = format!(".anonymous-publication-probe-{invocation_id}");
    let name = OsStr::new(&name);
    link_unnamed_file_at(&probe, directory.as_raw_fd(), name)?;
    // This inode is mode000 just like a prepared manifest before activation;
    // fstatat checks identity without requiring read permission or a proc path.
    let check = (|| {
        let name_c = component_cstring(name)?;
        let mut stat = MaybeUninit::<libc::stat>::zeroed();
        if unsafe {
            libc::fstatat(
                directory.as_raw_fd(),
                name_c.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fstatat initialized the structure.
        let stat = unsafe { stat.assume_init() };
        if (stat.st_dev, stat.st_ino) != file_identity(&probe)?
            || stat.st_mode & libc::S_IFMT != libc::S_IFREG
        {
            return Err(io::Error::other(
                "anonymous publication changed inode identity",
            ));
        }
        Ok(())
    })();
    // A failed identity observation gives no authority to remove the name.
    // Leave it intact and refuse creation; never clean up a known replacement.
    check?;
    let check = match link_unnamed_file_at(&probe, directory.as_raw_fd(), name) {
        Err(error) if error.raw_os_error() == Some(libc::EEXIST) => Ok(()),
        Err(error) => Err(error),
        Ok(()) => Err(io::Error::other(
            "anonymous publication replaced an existing name",
        )),
    };
    let cleanup = unlink_file_at(directory.as_raw_fd(), name);
    match (check, cleanup) {
        (Ok(()), Ok(())) => {}
        (Err(error), Ok(())) | (Ok(()), Err(error)) => return Err(error),
        (Err(error), Err(cleanup)) => {
            return Err(io::Error::other(format!(
                "{error}; anonymous publication probe cleanup failed: {cleanup}"
            )));
        }
    }
    directory_sync.sync(DirectorySyncPoint::AnonymousPublication, directory)
}

fn report_backend(backend: Backend) -> RunEvidenceBackend {
    match backend {
        Backend::Ptrace => RunEvidenceBackend::Ptrace,
        Backend::Dbt => RunEvidenceBackend::Dbt,
        Backend::Liteinst => RunEvidenceBackend::Liteinst,
        Backend::Sabre => RunEvidenceBackend::Sabre,
        Backend::Kvm => RunEvidenceBackend::Kvm,
        Backend::E9patch => RunEvidenceBackend::E9patch,
    }
}

fn guest_disposition(backend: Backend, status: ExitStatus) -> Option<GuestDisposition> {
    // ExitStatus already describes an observed Linux result. Never clamp an
    // impossible value into plausible evidence; the reader enforces the same range.
    if matches!(status, ExitStatus::Exited(code) if !(0..=255).contains(&code)) {
        return None;
    }
    match (backend, status) {
        (Backend::Kvm, ExitStatus::Exited(code)) => Some(GuestDisposition::ExitCodeOnly {
            code,
            limitation: DispositionLimitation::KvmExitCodeOnly,
        }),
        (Backend::Kvm, ExitStatus::Signaled(_, _)) => None,
        (_, ExitStatus::Exited(code)) => Some(GuestDisposition::Exited { code }),
        (_, ExitStatus::Signaled(signal, core_dumped)) => Some(GuestDisposition::Signaled {
            signal: signal as i32,
            core_dumped,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::MetadataExt;
    use std::sync::Barrier;
    use std::thread;

    use hermit::run_evidence::RunEvidenceInspection;
    use hermit::run_evidence::RunEvidenceInspectionFailure;
    use hermit::run_evidence::inspect_run_evidence;

    use super::*;

    const VALID_LOG: &[u8] = b"Apr 09 06:08:01.100  INFO hermit_test: evidence record\n";

    struct FailAfterN<W> {
        inner: W,
        remaining: usize,
    }

    impl<W: Write> Write for FailAfterN<W> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(io::Error::other("injected capture write failure"));
            }
            let take = self.remaining.min(buf.len());
            let written = self.inner.write(&buf[..take])?;
            self.remaining -= written;
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.inner.flush()
        }
    }

    fn write_valid_log(session: &RunEvidenceSession) {
        (&*session.raw_log).write_all(VALID_LOG).unwrap();
    }

    #[test]
    fn destination_creation_is_concurrent_and_no_clobber() {
        let parent = tempfile::tempdir().unwrap();
        let destination = Arc::new(parent.path().join("one-run"));
        let barrier = Arc::new(Barrier::new(3));
        let mut joins = Vec::new();
        for _ in 0..2 {
            let destination = Arc::clone(&destination);
            let barrier = Arc::clone(&barrier);
            joins.push(thread::spawn(move || {
                barrier.wait();
                RunEvidenceSession::create(&destination, Backend::Ptrace).is_ok()
            }));
        }
        barrier.wait();
        let winners = joins
            .into_iter()
            .map(|join| join.join().unwrap())
            .filter(|won| *won)
            .count();
        assert_eq!(winners, 1, "exactly one invocation may claim a path");
    }

    #[test]
    fn parent_directory_sync_failure_refuses_before_launch_artifacts() {
        let parent = tempfile::tempdir().unwrap();
        let destination = parent.path().join("claim-fsync");
        let result = RunEvidenceSession::create_with_directory_sync(
            &destination,
            Backend::Ptrace,
            DirectorySync::fail_once(DirectorySyncPoint::ParentClaim),
        );
        let error = result.err().expect("parent sync injection must fail");
        assert!(format!("{error:#}").contains("ParentClaim"));
        assert!(destination.is_dir());
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
    }

    #[test]
    fn every_artifact_is_no_clobber_and_manifest_is_last() {
        let parent = tempfile::tempdir().unwrap();
        let destination = parent.path().join("one-run");
        let session = RunEvidenceSession::create(&destination, Backend::Ptrace).unwrap();
        write_valid_log(&session);
        assert!(!destination.join(RUN_EVIDENCE_MANIFEST).exists());
        fs::write(
            destination.join(RUN_EVIDENCE_INFO_ARTIFACT),
            b"do-not-replace",
        )
        .unwrap();

        let status = ExitStatus::Exited(0);
        session.finish(Ok(&status)).unwrap();
        assert_eq!(
            fs::read(destination.join(RUN_EVIDENCE_INFO_ARTIFACT)).unwrap(),
            b"do-not-replace"
        );
        assert_eq!(
            inspect_run_evidence(&destination),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::ReportedNoResult(
                RunEvidenceNoResultReason::ArtifactWriteFailed
            ))
        );
    }

    #[test]
    fn staging_name_replacement_cannot_change_the_linked_manifest_inode() {
        let parent = tempfile::tempdir().unwrap();
        let destination = parent.path().join("staging-replacement");
        let session = RunEvidenceSession::create(&destination, Backend::Ptrace).unwrap();
        write_valid_log(&session);

        let attacker = destination.join(".manifest.json.pending");
        fs::write(&attacker, b"attacker-controlled staging name").unwrap();
        let attacker_identity = fs::metadata(&attacker).unwrap();

        let status = ExitStatus::Exited(0);
        session.finish(Ok(&status)).unwrap();
        assert!(matches!(
            inspect_run_evidence(&destination),
            RunEvidenceInspection::Complete(_)
        ));
        let after = fs::metadata(&attacker).unwrap();
        assert_eq!(
            (attacker_identity.dev(), attacker_identity.ino()),
            (after.dev(), after.ino())
        );
        assert_eq!(
            fs::read(&attacker).unwrap(),
            b"attacker-controlled staging name"
        );
    }

    #[test]
    fn valid_prefix_followed_by_capture_error_is_no_result() {
        let mut canonical = Vec::new();
        assert_eq!(
            detcore::logdiff::write_bitwise_info_v1_bytes(
                VALID_LOG,
                "valid prefix",
                &mut canonical,
            )
            .unwrap(),
            1
        );
        let parent = tempfile::tempdir().unwrap();
        let destination = parent.path().join("write-error");
        let session = RunEvidenceSession::create(&destination, Backend::Ptrace).unwrap();
        let file = session.raw_log.try_clone().unwrap();
        let fail_after = FailAfterN {
            inner: file,
            remaining: VALID_LOG.len(),
        };
        let mut writer = LatchedWriter::new(fail_after, session.write_error.clone());
        writer.write_all(VALID_LOG).unwrap();
        assert!(writer.write_all(b"lost record").is_err());
        drop(writer);

        let status = ExitStatus::Exited(0);
        session.finish(Ok(&status)).unwrap();
        assert_eq!(
            inspect_run_evidence(&destination),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::ReportedNoResult(
                RunEvidenceNoResultReason::MissingCanonicalInfo
            ))
        );
        assert!(
            !destination.join(RUN_EVIDENCE_INFO_ARTIFACT).exists(),
            "a syntactically valid prefix must not be published as complete"
        );
    }

    #[test]
    fn renamed_destination_cannot_redirect_publication() {
        let parent = tempfile::tempdir().unwrap();
        let destination = parent.path().join("claimed");
        let original = parent.path().join("original-claimed-directory");
        let session = RunEvidenceSession::create(&destination, Backend::Ptrace).unwrap();
        write_valid_log(&session);

        fs::rename(&destination, &original).unwrap();
        fs::create_dir(&destination).unwrap();

        let status = ExitStatus::Exited(0);
        session.finish(Ok(&status)).unwrap();
        assert_eq!(
            fs::read_dir(&destination).unwrap().count(),
            0,
            "replacement path received evidence"
        );
        assert!(matches!(
            inspect_run_evidence(&original),
            RunEvidenceInspection::Complete(_)
        ));
    }

    #[test]
    fn directory_sync_failures_never_leave_complete_evidence() {
        let parent = tempfile::tempdir().unwrap();

        let artifact_failure = parent.path().join("artifact-fsync");
        let mut session = RunEvidenceSession::create(&artifact_failure, Backend::Ptrace).unwrap();
        write_valid_log(&session);
        session.directory_sync = DirectorySync::fail_once(DirectorySyncPoint::ArtifactPublished);
        let status = ExitStatus::Exited(0);
        session.finish(Ok(&status)).unwrap();
        assert_eq!(
            inspect_run_evidence(&artifact_failure),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::ReportedNoResult(
                RunEvidenceNoResultReason::ArtifactWriteFailed
            ))
        );

        let manifest_failure = parent.path().join("manifest-fsync");
        let mut session = RunEvidenceSession::create(&manifest_failure, Backend::Ptrace).unwrap();
        write_valid_log(&session);
        session.directory_sync = DirectorySync::fail_once(DirectorySyncPoint::ManifestBeforeLink);
        assert!(session.finish(Ok(&status)).is_err());
        assert_eq!(
            inspect_run_evidence(&manifest_failure),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::MissingManifest)
        );
    }

    #[test]
    fn persistent_manifest_sync_failure_remains_fail_closed() {
        let parent = tempfile::tempdir().unwrap();
        let destination = parent.path().join("persistent-manifest-fsync");
        let mut session = RunEvidenceSession::create(&destination, Backend::Ptrace).unwrap();
        write_valid_log(&session);
        session.directory_sync = DirectorySync::fail_manifest_persistently();
        let status = ExitStatus::Exited(0);
        let error = session.finish(Ok(&status)).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("preflighting the run-evidence directory before manifest publication")
        );
        assert_eq!(
            inspect_run_evidence(&destination),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::MissingManifest),
            "persistent fsync failure left a readable manifest"
        );
    }

    #[test]
    fn unpublished_manifest_mode_is_never_complete() {
        let parent = tempfile::tempdir().unwrap();
        let destination = parent.path().join("unpublished-mode");
        let mut session = RunEvidenceSession::create(&destination, Backend::Ptrace).unwrap();
        write_valid_log(&session);
        let canonical_info = session.publish_canonical_info().unwrap();
        let report = RunEvidenceReport {
            schema_version: RUN_EVIDENCE_SCHEMA_VERSION,
            invocation_id: session.invocation_id,
            backend: RunEvidenceBackend::Ptrace,
            attempt: 1,
            outcome: RunEvidenceOutcome::Complete {
                disposition: GuestDisposition::Exited { code: 0 },
            },
            canonical_info,
        };
        let mut contents = serde_json::to_vec(&report).unwrap();
        contents.push(b'\n');
        write_file_contents(&mut session.manifest, &contents).unwrap();
        link_unnamed_file_at(
            &session.manifest,
            session.directory.as_raw_fd(),
            OsStr::new(RUN_EVIDENCE_MANIFEST),
        )
        .unwrap();
        drop(session); // Release the live-publisher lock; still test mode000 itself.
        assert_eq!(
            inspect_run_evidence(&destination),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::MissingManifest)
        );
    }

    #[test]
    fn kvm_disposition_states_its_exit_code_only_limit() {
        assert_eq!(
            guest_disposition(Backend::Kvm, ExitStatus::Exited(23)),
            Some(GuestDisposition::ExitCodeOnly {
                code: 23,
                limitation: DispositionLimitation::KvmExitCodeOnly,
            })
        );

        let parent = tempfile::tempdir().unwrap();
        let destination = parent.path().join("unexpected-signal");
        let session = RunEvidenceSession::create(&destination, Backend::Kvm).unwrap();
        write_valid_log(&session);
        let status = ExitStatus::Signaled(reverie::process::Signal::SIGTERM, false);
        session.finish(Ok(&status)).unwrap();
        assert_eq!(
            inspect_run_evidence(&destination),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::ReportedNoResult(
                RunEvidenceNoResultReason::UnsupportedDisposition
            ))
        );
    }

    #[test]
    fn anonymous_prelaunch_probe_preserves_no_replace_and_checks_sync() {
        let parent = tempfile::tempdir().unwrap();
        let held = open_directory(parent.path()).unwrap();
        let invocation_id = Uuid::from_u128(7);
        verify_anonymous_publication(&held, invocation_id, &DirectorySync::default()).unwrap();
        assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
        let sentinel = parent
            .path()
            .join(format!(".anonymous-publication-probe-{invocation_id}"));
        fs::write(&sentinel, b"preexisting probe name").unwrap();
        let original = fs::metadata(&sentinel).unwrap();
        let error = verify_anonymous_publication(&held, invocation_id, &DirectorySync::default())
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EEXIST));
        let after = fs::metadata(&sentinel).unwrap();
        assert_eq!((after.dev(), after.ino()), (original.dev(), original.ino()));
        assert_eq!(fs::read(&sentinel).unwrap(), b"preexisting probe name");
        let destination = parent.path().join("failed-prelaunch-probe");
        let result = RunEvidenceSession::create_with_directory_sync(
            &destination,
            Backend::Ptrace,
            DirectorySync::fail_once(DirectorySyncPoint::AnonymousPublication),
        );
        let error = result
            .err()
            .expect("actual prelaunch publication check must run");
        assert!(format!("{error:#}").contains("AnonymousPublication"));
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
        assert_eq!(
            inspect_run_evidence(&destination),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::MissingManifest)
        );
    }

    #[test]
    fn final_manifest_link_never_replaces_an_existing_inode() {
        use std::os::unix::fs::PermissionsExt;
        let parent = tempfile::tempdir().unwrap();
        let destination = parent.path().join("manifest-collision");
        let session = RunEvidenceSession::create(&destination, Backend::Ptrace).unwrap();
        write_valid_log(&session);
        let path = destination.join(RUN_EVIDENCE_MANIFEST);
        fs::write(&path, b"preexisting manifest").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(MANIFEST_PUBLISHED_MODE)).unwrap();
        let original = fs::metadata(&path).unwrap();
        assert!(session.finish(Ok(&ExitStatus::Exited(0))).is_err());
        let after = fs::metadata(&path).unwrap();
        assert_eq!((after.dev(), after.ino()), (original.dev(), original.ino()));
        assert_eq!(fs::read(&path).unwrap(), b"preexisting manifest");
        assert_eq!(
            inspect_run_evidence(&destination),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::MalformedManifest)
        );
    }

    struct NativeChild(std::process::Child);

    impl NativeChild {
        fn wait(&mut self) -> std::process::ExitStatus {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                if let Some(status) = self.0.try_wait().unwrap() {
                    return status;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "native control child exceeded 10 seconds"
                );
                thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }

    impl Drop for NativeChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn native_control_command(mode: &str, path: &Path) -> std::process::Command {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .arg("--exact")
            .arg(thread::current().name().expect("named libtest thread"))
            .arg("--nocapture")
            .env("HERMIT_RUN_EVIDENCE_NATIVE_CONTROL", mode)
            .env("HERMIT_RUN_EVIDENCE_NATIVE_PATH", path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit());
        command
    }

    fn prepared_report(session: &mut RunEvidenceSession) -> RunEvidenceReport {
        write_valid_log(session);
        let canonical_info = session.publish_canonical_info().unwrap();
        RunEvidenceReport {
            schema_version: RUN_EVIDENCE_SCHEMA_VERSION,
            invocation_id: session.invocation_id,
            backend: RunEvidenceBackend::Ptrace,
            attempt: 1,
            outcome: RunEvidenceOutcome::Complete {
                disposition: GuestDisposition::Exited { code: 23 },
            },
            canonical_info,
        }
    }

    #[test]
    fn publication_lock_and_death_boundaries() {
        use std::os::fd::OwnedFd;
        use std::os::unix::net::UnixStream;
        use std::os::unix::process::ExitStatusExt;
        if let Ok(mode) = std::env::var("HERMIT_RUN_EVIDENCE_NATIVE_CONTROL") {
            let destination = std::env::var_os("HERMIT_RUN_EVIDENCE_NATIVE_PATH").unwrap();
            let mut session =
                RunEvidenceSession::create(Path::new(&destination), Backend::Ptrace).unwrap();
            let report = prepared_report(&mut session);
            if mode == "after-link" {
                session.publish_manifest(&report).unwrap();
            } else {
                assert_eq!(mode, "before-link");
            }
            let fd = unsafe { libc::dup(libc::STDIN_FILENO) };
            assert!(fd >= 0);
            // SAFETY: dup returned a separately owned descriptor for the passed socket.
            let mut rendezvous = unsafe { UnixStream::from_raw_fd(fd) };
            rendezvous
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .unwrap();
            rendezvous.write_all(b"R").unwrap();
            let mut command = [0];
            rendezvous.read_exact(&mut command).unwrap();
            assert_eq!(command, *b"X");
            drop(session);
            return;
        }
        let parent = tempfile::tempdir().unwrap();
        for (mode, kill) in [
            ("after-link", true),
            ("before-link", true),
            ("after-link", false),
        ] {
            let destination = parent.path().join(format!("{mode}-{kill}"));
            let (mut rendezvous, child_socket) = UnixStream::pair().unwrap();
            rendezvous
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .unwrap();
            let child_fd: OwnedFd = child_socket.into();
            let mut child = NativeChild(
                native_control_command(mode, &destination)
                    .stdin(std::process::Stdio::from(child_fd))
                    .spawn()
                    .unwrap(),
            );
            let mut ready = [0];
            rendezvous.read_exact(&mut ready).unwrap();
            assert_eq!(ready, *b"R");
            assert_eq!(
                destination.join(RUN_EVIDENCE_MANIFEST).exists(),
                mode == "after-link"
            );
            assert_eq!(
                inspect_run_evidence(&destination),
                RunEvidenceInspection::NoResult(
                    RunEvidenceInspectionFailure::PublicationInProgress
                )
            );

            // The lock follows the held directory inode, not its original pathname.
            let renamed = parent.path().join(format!("moved-{mode}-{kill}"));
            fs::rename(&destination, &renamed).unwrap();
            fs::create_dir(&destination).unwrap();
            assert_eq!(
                inspect_run_evidence(&renamed),
                RunEvidenceInspection::NoResult(
                    RunEvidenceInspectionFailure::PublicationInProgress
                )
            );
            assert_eq!(
                inspect_run_evidence(&destination),
                RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::MissingManifest)
            );
            // An unrelated producer completes while this publisher is still paused.
            let other = parent.path().join(format!("other-{mode}-{kill}"));
            let other_session = RunEvidenceSession::create(&other, Backend::Ptrace).unwrap();
            write_valid_log(&other_session);
            other_session.finish(Ok(&ExitStatus::Exited(0))).unwrap();
            assert!(matches!(
                inspect_run_evidence(&other),
                RunEvidenceInspection::Complete(_)
            ));
            if kill {
                child.0.kill().unwrap();
                assert_eq!(child.wait().signal(), Some(libc::SIGKILL));
            } else {
                rendezvous.write_all(b"X").unwrap();
                assert!(child.wait().success());
            }
            let inspected = inspect_run_evidence(&renamed);
            if mode == "before-link" {
                assert_eq!(
                    inspected,
                    RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::MissingManifest)
                );
            } else {
                let RunEvidenceInspection::Complete(report) = inspected else {
                    panic!(
                        "prepared publication was not independently synchronized: {inspected:?}"
                    );
                };
                assert_eq!(
                    report.outcome,
                    RunEvidenceOutcome::Complete {
                        disposition: GuestDisposition::Exited { code: 23 }
                    }
                );
                assert_eq!(report.canonical_info.message_count, 1);
                assert_eq!(report.canonical_info.byte_count, VALID_LOG.len() as u64);
                assert_eq!(
                    report.canonical_info.sha256,
                    Some(detcore::Digest::new(VALID_LOG).to_string())
                );
                assert_eq!(
                    fs::read(renamed.join(RUN_EVIDENCE_INFO_ARTIFACT)).unwrap(),
                    VALID_LOG
                );
            }
        }
    }

    #[test]
    fn observed_linux_exit_neighbors_are_not_clamped_untrusted_values() {
        use std::os::unix::process::ExitStatusExt;
        if let Ok(mode) = std::env::var("HERMIT_RUN_EVIDENCE_NATIVE_CONTROL") {
            let code: i32 = mode.parse().unwrap();
            unsafe { libc::_exit(code) }
        }
        let parent = tempfile::tempdir().unwrap();
        for (argument, observed) in [(-1, 255), (0, 0), (23, 23), (255, 255), (256, 0)] {
            let mut child = NativeChild(
                native_control_command(&argument.to_string(), parent.path())
                    .spawn()
                    .unwrap(),
            );
            let status = child.wait();
            assert_eq!(status.code(), Some(observed));
            let status = ExitStatus::from_raw(status.into_raw());
            assert_eq!(status, ExitStatus::Exited(observed));
            for backend in [Backend::Ptrace, Backend::Liteinst, Backend::Kvm] {
                let destination = parent.path().join(format!("{backend:?}-{argument}"));
                let session = RunEvidenceSession::create(&destination, backend).unwrap();
                write_valid_log(&session);
                session.finish(Ok(&status)).unwrap();
                let RunEvidenceInspection::Complete(report) = inspect_run_evidence(&destination)
                else {
                    panic!("valid observed Linux status refused");
                };
                assert_eq!(
                    report.outcome,
                    RunEvidenceOutcome::Complete {
                        disposition: guest_disposition(backend, status).unwrap()
                    }
                );
            }
        }
        for code in [i32::MIN, -1, 256, i32::MAX] {
            for backend in [Backend::Ptrace, Backend::Liteinst, Backend::Kvm] {
                assert_eq!(guest_disposition(backend, ExitStatus::Exited(code)), None);
                let destination = parent.path().join(format!("invalid-{backend:?}-{code}"));
                let session = RunEvidenceSession::create(&destination, backend).unwrap();
                write_valid_log(&session);
                session.finish(Ok(&ExitStatus::Exited(code))).unwrap();
                assert_eq!(
                    inspect_run_evidence(&destination),
                    RunEvidenceInspection::NoResult(
                        RunEvidenceInspectionFailure::ReportedNoResult(
                            RunEvidenceNoResultReason::UnsupportedDisposition
                        )
                    )
                );
                assert!(!destination.join(RUN_EVIDENCE_INFO_ARTIFACT).exists());
            }
        }
    }

    struct RawCaptureWriter(RawFd);

    impl Write for RawCaptureWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let written = unsafe { libc::write(self.0, bytes.as_ptr().cast(), bytes.len()) };
            if written < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(written as usize)
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn capture_error_in_forked_writer_is_not_complete() {
        if std::env::var_os("HERMIT_RUN_EVIDENCE_NATIVE_CONTROL").is_none() {
            let parent = tempfile::tempdir().unwrap();
            let destination = parent.path().join("forked-writer");
            let mut child = NativeChild(
                native_control_command("forked-writer", &destination)
                    .spawn()
                    .unwrap(),
            );
            assert!(child.wait().success());
            assert_eq!(
                inspect_run_evidence(&destination),
                RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::ReportedNoResult(
                    RunEvidenceNoResultReason::MissingCanonicalInfo
                ))
            );
            return;
        }
        // The fixture is created in this executed test subprocess. Other parallel
        // tests cannot inherit its capture descriptors or publication lock.
        let destination = std::env::var_os("HERMIT_RUN_EVIDENCE_NATIVE_PATH").unwrap();
        let session = RunEvidenceSession::create(Path::new(&destination), Backend::Ptrace).unwrap();
        let raw_fd = session.raw_log.as_raw_fd();
        let mut writer = LatchedWriter::new(RawCaptureWriter(raw_fd), session.write_error.clone());
        writer.write_all(VALID_LOG).unwrap();
        let full = fs::OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .unwrap();
        let full_fd = full.as_raw_fd();
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            // No allocation, formatter, unwinding, Rust destruction or inherited
            // mutex use in the fork child: dup2/actual failed write + _exit. The
            // parent's capture descriptor still names the original valid prefix.
            let redirected = unsafe { libc::dup2(full_fd, raw_fd) };
            let failed = writer.write(b"lost record");
            let okay = redirected == raw_fd
                && failed.is_err_and(|error| error.raw_os_error() == Some(libc::ENOSPC));
            unsafe { libc::_exit(if okay { 23 } else { 24 }) }
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut status = 0;
        loop {
            let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if waited == pid {
                break;
            }
            if waited < 0 || std::time::Instant::now() >= deadline {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                    libc::waitpid(pid, &mut status, 0);
                }
                panic!("forked capture writer did not finish within five seconds");
            }
            thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 23);
        let write_failed = session.write_error.failed();
        drop(writer);
        let mut raw = session.raw_log.try_clone().unwrap();
        assert_eq!(read_file_contents(&mut raw).unwrap(), VALID_LOG);
        session.finish(Ok(&ExitStatus::Exited(0))).unwrap();
        assert_eq!(
            inspect_run_evidence(Path::new(&destination)),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::ReportedNoResult(
                RunEvidenceNoResultReason::MissingCanonicalInfo
            )),
            "a fork-local latch must not certify the surviving valid prefix"
        );
        assert!(
            write_failed,
            "actual child write error was lost across fork"
        );
        assert!(
            !Path::new(&destination)
                .join(RUN_EVIDENCE_INFO_ARTIFACT)
                .exists()
        );
    }
}
