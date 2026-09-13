use std::ffi::CString;
use std::ffi::OsStr;
use std::ffi::OsString;
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
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use hermit::Backend;
use hermit::Context;
use hermit::Error;
use hermit::run_evidence::CapturedGuestStream;
use hermit::run_evidence::DispositionLimitation;
use hermit::run_evidence::GuestDisposition;
use hermit::run_evidence::GuestRunDeterminism;
use hermit::run_evidence::GuestRunResult;
use hermit::run_evidence::RunEvidenceFileIdentity;
use reverie::process::ExitStatus;

const CAPTURE_MODE: libc::mode_t = 0o600;
const MAX_STREAM_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_RESULT_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct GuestRunCapturePaths {
    pub(crate) result: PathBuf,
    pub(crate) stdout: PathBuf,
    pub(crate) stderr: PathBuf,
}

impl GuestRunCapturePaths {
    pub(crate) fn new(result: PathBuf, stdout: PathBuf, stderr: PathBuf) -> Self {
        Self {
            result,
            stdout,
            stderr,
        }
    }
}

/// Host-side owner of the exact guest output files for one ordinary run.
///
/// The parent directory and both output inodes stay open across guest
/// execution. The result sidecar is linked from an unnamed inode only after
/// all path identities and stream digests have been checked.
#[derive(Debug)]
pub(crate) struct GuestRunCaptureSession {
    parent_path: PathBuf,
    parent: File,
    parent_identity: RunEvidenceFileIdentity,
    result_name: OsString,
    stdout_name: OsString,
    stderr_name: OsString,
    stdout: File,
    stderr: File,
    stdout_identity: RunEvidenceFileIdentity,
    stderr_identity: RunEvidenceFileIdentity,
}

impl GuestRunCaptureSession {
    pub(crate) fn create(
        paths: &GuestRunCapturePaths,
        evidence_directory: &Path,
    ) -> Result<Self, Error> {
        for (description, path) in [
            ("guest run result", paths.result.as_path()),
            ("captured guest stdout", paths.stdout.as_path()),
            ("captured guest stderr", paths.stderr.as_path()),
            ("run evidence directory", evidence_directory),
        ] {
            require_normal_absolute_path(path, description)?;
        }
        let parent_path = paths
            .result
            .parent()
            .ok_or_else(|| Error::msg("guest run result has no parent"))?;
        if paths.stdout.parent() != Some(parent_path)
            || paths.stderr.parent() != Some(parent_path)
            || evidence_directory.parent() != Some(parent_path)
        {
            anyhow::bail!(
                "--run-result-json, --guest-stdout, --guest-stderr, and --run-evidence-dir must share one parent directory"
            );
        }
        let result_name = basename(&paths.result, "guest run result")?;
        let stdout_name = basename(&paths.stdout, "captured guest stdout")?;
        let stderr_name = basename(&paths.stderr, "captured guest stderr")?;
        let evidence_name = basename(evidence_directory, "run evidence directory")?;
        if result_name == stdout_name
            || result_name == stderr_name
            || stdout_name == stderr_name
            || result_name == evidence_name
            || stdout_name == evidence_name
            || stderr_name == evidence_name
        {
            anyhow::bail!(
                "--run-result-json, --guest-stdout, --guest-stderr, and --run-evidence-dir must name distinct children"
            );
        }

        let parent = open_directory_nofollow(parent_path).with_context(|| {
            format!(
                "opening non-symlink guest-capture parent {}",
                parent_path.display()
            )
        })?;
        require_child_absent(&parent, &result_name, "guest run result")?;
        let stdout = create_regular_child(&parent, &stdout_name, "captured guest stdout")?;
        let stderr = create_regular_child(&parent, &stderr_name, "captured guest stderr")?;
        let parent_identity = file_identity(&parent)?;
        let stdout_identity = regular_file_identity(&stdout, "captured guest stdout")?;
        let stderr_identity = regular_file_identity(&stderr, "captured guest stderr")?;
        if stdout_identity == stderr_identity {
            anyhow::bail!("captured guest stdout and stderr alias one inode");
        }
        parent.sync_all().with_context(|| {
            format!(
                "synchronizing guest-capture parent {}",
                parent_path.display()
            )
        })?;
        Ok(Self {
            parent_path: parent_path.to_owned(),
            parent,
            parent_identity,
            result_name,
            stdout_name,
            stderr_name,
            stdout,
            stderr,
            stdout_identity,
            stderr_identity,
        })
    }

    pub(crate) fn stdout_for_guest(&self) -> Result<File, Error> {
        self.stdout
            .try_clone()
            .context("duplicating held guest stdout descriptor")
    }

    pub(crate) fn stderr_fd_for_guest(&self) -> RawFd {
        self.stderr.as_raw_fd()
    }

    /// KVM exposes a virtual console rather than inheriting host descriptors.
    /// Preserve that backend's existing guest semantics and copy its completed
    /// virtual-console bytes into the same held files used by other backends.
    pub(crate) fn write_kvm_virtual_console(
        &self,
        stdout: &[u8],
        stderr: &[u8],
    ) -> Result<(), Error> {
        checked_size(stdout, MAX_STREAM_BYTES, "captured KVM guest stdout")?;
        checked_size(stderr, MAX_STREAM_BYTES, "captured KVM guest stderr")?;
        write_held_file(&self.stdout, stdout, "captured KVM guest stdout")?;
        write_held_file(&self.stderr, stderr, "captured KVM guest stderr")
    }

    pub(crate) fn finish(
        mut self,
        backend: Backend,
        status: ExitStatus,
        determinism: GuestRunDeterminism,
    ) -> Result<(), Error> {
        self.require_visible_identity()?;
        let stdout = read_captured_stream(
            &mut self.stdout,
            self.stdout_identity,
            "captured guest stdout",
        )?;
        let stderr = read_captured_stream(
            &mut self.stderr,
            self.stderr_identity,
            "captured guest stderr",
        )?;
        self.require_visible_identity()?;
        require_child_absent(&self.parent, &self.result_name, "guest run result")?;

        let result = GuestRunResult {
            schema_version: GuestRunResult::SCHEMA_VERSION,
            disposition: guest_disposition(backend, status)?,
            determinism,
            stdout,
            stderr,
        };
        result.validate_current().map_err(Error::msg)?;
        let mut bytes = serde_json::to_vec(&result)?;
        bytes.push(b'\n');
        checked_size(&bytes, MAX_RESULT_BYTES, "guest run result")?;

        let mut staged = create_unnamed_file_at(self.parent.as_raw_fd(), CAPTURE_MODE)
            .context("creating unnamed guest run result")?;
        write_file_contents(&mut staged, &bytes).context("writing guest run result")?;
        link_unnamed_file_at(&staged, self.parent.as_raw_fd(), &self.result_name)
            .context("publishing guest run result without replacement")?;
        let visible = open_regular_child(&self.parent, &self.result_name, "guest run result")?;
        if file_identity(&visible)? != file_identity(&staged)? {
            anyhow::bail!("guest run result path does not name its published inode");
        }
        self.parent.sync_all().with_context(|| {
            format!(
                "synchronizing guest-capture parent {}",
                self.parent_path.display()
            )
        })
    }

    fn require_visible_identity(&self) -> Result<(), Error> {
        let visible_parent = open_directory_nofollow(&self.parent_path).with_context(|| {
            format!(
                "reopening guest-capture parent {}",
                self.parent_path.display()
            )
        })?;
        if file_identity(&visible_parent)? != self.parent_identity {
            anyhow::bail!("guest-capture parent changed identity before result publication");
        }
        for (name, expected, description) in [
            (
                &self.stdout_name,
                self.stdout_identity,
                "captured guest stdout",
            ),
            (
                &self.stderr_name,
                self.stderr_identity,
                "captured guest stderr",
            ),
        ] {
            let visible = open_regular_child(&self.parent, name, description)?;
            if file_identity(&visible)? != expected {
                anyhow::bail!("{description} changed identity before result publication");
            }
        }
        Ok(())
    }
}

fn require_normal_absolute_path(path: &Path, description: &str) -> Result<(), Error> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
    {
        anyhow::bail!("{description} must be an absolute normalized path");
    }
    Ok(())
}

fn basename(path: &Path, description: &str) -> Result<OsString, Error> {
    path.file_name()
        .map(OsStr::to_owned)
        .ok_or_else(|| Error::msg(format!("{description} has no basename")))
}

fn checked_size(bytes: &[u8], maximum: u64, description: &str) -> Result<u64, Error> {
    let length = u64::try_from(bytes.len())
        .with_context(|| format!("{description} length does not fit u64"))?;
    if length > maximum {
        anyhow::bail!("{description} exceeds the {maximum}-byte limit");
    }
    Ok(length)
}

fn write_held_file(file: &File, bytes: &[u8], description: &str) -> Result<(), Error> {
    let mut file = file
        .try_clone()
        .with_context(|| format!("duplicating {description} descriptor"))?;
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

fn read_captured_stream(
    file: &mut File,
    expected_identity: RunEvidenceFileIdentity,
    description: &str,
) -> Result<CapturedGuestStream, Error> {
    file.sync_all()
        .with_context(|| format!("synchronizing {description}"))?;
    if regular_file_identity(file, description)? != expected_identity {
        anyhow::bail!("{description} held descriptor changed identity");
    }
    let initial_size = file.metadata()?.len();
    if initial_size > MAX_STREAM_BYTES {
        anyhow::bail!("{description} exceeds the {MAX_STREAM_BYTES}-byte limit");
    }
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("rewinding {description}"))?;
    let mut bytes = Vec::new();
    (&mut *file)
        .take(MAX_STREAM_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {description}"))?;
    let byte_count = checked_size(&bytes, MAX_STREAM_BYTES, description)?;
    if file.metadata()?.len() != initial_size || byte_count != initial_size {
        anyhow::bail!("{description} size changed while reading its held descriptor");
    }
    Ok(CapturedGuestStream {
        bytes: byte_count,
        sha256: detcore::Digest::new(&bytes).to_string(),
        identity: expected_identity,
    })
}

fn guest_disposition(backend: Backend, status: ExitStatus) -> Result<GuestDisposition, Error> {
    match (backend, status) {
        (Backend::Kvm, ExitStatus::Exited(code)) => Ok(GuestDisposition::ExitCodeOnly {
            code,
            limitation: DispositionLimitation::KvmExitCodeOnly,
        }),
        (Backend::Kvm, ExitStatus::Signaled(_, _)) => {
            anyhow::bail!("KVM guest capture cannot represent a signal disposition")
        }
        (_, ExitStatus::Exited(code)) => Ok(GuestDisposition::Exited { code }),
        (_, ExitStatus::Signaled(signal, core_dumped)) => Ok(GuestDisposition::Signaled {
            signal: signal as i32,
            core_dumped,
        }),
    }
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

fn open_directory_nofollow(path: &Path) -> io::Result<File> {
    let path = path_cstring(path)?;
    owned_file(unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    })
}

fn create_regular_child(parent: &File, name: &OsStr, description: &str) -> Result<File, Error> {
    let name = component_cstring(name)?;
    let file = owned_file(unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            CAPTURE_MODE,
        )
    })
    .with_context(|| format!("creating {description}; capture paths are no-clobber"))?;
    regular_file_identity(&file, description)?;
    Ok(file)
}

fn open_regular_child(parent: &File, name: &OsStr, description: &str) -> Result<File, Error> {
    let name = component_cstring(name)?;
    let file = owned_file(unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    })
    .with_context(|| format!("opening {description}"))?;
    regular_file_identity(&file, description)?;
    Ok(file)
}

fn require_child_absent(parent: &File, name: &OsStr, description: &str) -> Result<(), Error> {
    let name = component_cstring(name)?;
    let mut stat = MaybeUninit::<libc::stat>::zeroed();
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } == 0
    {
        anyhow::bail!("{description} already exists; capture paths are no-clobber");
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::NotFound {
        Ok(())
    } else {
        Err(error).with_context(|| format!("inspecting {description}"))
    }
}

fn create_unnamed_file_at(directory: RawFd, mode: libc::mode_t) -> io::Result<File> {
    owned_file(unsafe {
        libc::openat(
            directory,
            c".".as_ptr(),
            libc::O_TMPFILE | libc::O_RDWR | libc::O_CLOEXEC,
            mode,
        )
    })
}

fn link_unnamed_file_at(file: &File, directory: RawFd, destination: &OsStr) -> io::Result<()> {
    let destination = component_cstring(destination)?;
    if unsafe {
        libc::linkat(
            file.as_raw_fd(),
            c"".as_ptr(),
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

fn file_identity(file: &File) -> io::Result<RunEvidenceFileIdentity> {
    let metadata = file.metadata()?;
    use std::os::unix::fs::MetadataExt as _;
    Ok(RunEvidenceFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

fn regular_file_identity(file: &File, description: &str) -> Result<RunEvidenceFileIdentity, Error> {
    let mut stat = MaybeUninit::<libc::stat>::zeroed();
    if unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("inspecting {description}"));
    }
    // SAFETY: fstat initialized the complete structure on success.
    let stat = unsafe { stat.assume_init() };
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
        anyhow::bail!("{description} is not a regular file");
    }
    Ok(RunEvidenceFileIdentity {
        device: stat.st_dev,
        inode: stat.st_ino,
    })
}

fn write_file_contents(file: &mut File, contents: &[u8]) -> io::Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(contents)?;
    file.flush()?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::symlink;

    use super::*;

    fn capture_paths(directory: &Path) -> GuestRunCapturePaths {
        GuestRunCapturePaths::new(
            directory.join("result.json"),
            directory.join("stdout"),
            directory.join("stderr"),
        )
    }

    #[test]
    fn capture_is_no_clobber_and_refuses_aliases() {
        let directory = tempfile::tempdir().unwrap();
        let evidence = directory.path().join("evidence");
        fs::create_dir(&evidence).unwrap();
        let paths = capture_paths(directory.path());
        let _capture = GuestRunCaptureSession::create(&paths, &evidence).unwrap();
        assert!(
            GuestRunCaptureSession::create(&paths, &evidence)
                .unwrap_err()
                .to_string()
                .contains("no-clobber")
        );

        let alias_dir = tempfile::tempdir().unwrap();
        let evidence = alias_dir.path().join("evidence");
        fs::create_dir(&evidence).unwrap();
        let same = alias_dir.path().join("same");
        let aliases =
            GuestRunCapturePaths::new(same.clone(), same, alias_dir.path().join("stderr"));
        assert!(
            GuestRunCaptureSession::create(&aliases, &evidence)
                .unwrap_err()
                .to_string()
                .contains("distinct")
        );
    }

    #[test]
    fn capture_refuses_symlink_and_path_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let evidence = directory.path().join("evidence");
        fs::create_dir(&evidence).unwrap();
        let paths = capture_paths(directory.path());
        let target = directory.path().join("target");
        fs::write(&target, b"preserve").unwrap();
        symlink(&target, &paths.stdout).unwrap();
        assert!(GuestRunCaptureSession::create(&paths, &evidence).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"preserve");

        let directory = tempfile::tempdir().unwrap();
        let evidence = directory.path().join("evidence");
        fs::create_dir(&evidence).unwrap();
        let paths = capture_paths(directory.path());
        let capture = GuestRunCaptureSession::create(&paths, &evidence).unwrap();
        fs::remove_file(&paths.stderr).unwrap();
        fs::write(&paths.stderr, b"replacement").unwrap();
        let error = capture
            .finish(
                Backend::Ptrace,
                ExitStatus::Exited(0),
                GuestRunDeterminism {
                    detlog_io_buffers: true,
                    virtualize_time: true,
                },
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("changed identity"), "{error}");
        assert!(!paths.result.exists());
    }

    #[test]
    fn kvm_virtual_console_is_copied_to_held_regular_files() {
        let directory = tempfile::tempdir().unwrap();
        let evidence = directory.path().join("evidence");
        fs::create_dir(&evidence).unwrap();
        let paths = capture_paths(directory.path());
        let capture = GuestRunCaptureSession::create(&paths, &evidence).unwrap();
        capture
            .write_kvm_virtual_console(b"virtual stdout", b"virtual stderr")
            .unwrap();
        capture
            .finish(
                Backend::Kvm,
                ExitStatus::Exited(7),
                GuestRunDeterminism {
                    detlog_io_buffers: true,
                    virtualize_time: true,
                },
            )
            .unwrap();

        assert_eq!(fs::read(&paths.stdout).unwrap(), b"virtual stdout");
        assert_eq!(fs::read(&paths.stderr).unwrap(), b"virtual stderr");
        let result =
            GuestRunResult::from_current_json_slice(&fs::read(&paths.result).unwrap()).unwrap();
        assert_eq!(
            result.disposition,
            GuestDisposition::ExitCodeOnly {
                code: 7,
                limitation: DispositionLimitation::KvmExitCodeOnly,
            }
        );
        for (stream, path) in [
            (&result.stdout, &paths.stdout),
            (&result.stderr, &paths.stderr),
        ] {
            let metadata = fs::metadata(path).unwrap();
            assert_eq!(stream.identity.device, metadata.dev());
            assert_eq!(stream.identity.inode, metadata.ino());
        }
    }

    #[test]
    fn capture_refuses_invalid_disposition_before_result_publication() {
        for code in [-1, 256] {
            let directory = tempfile::tempdir().unwrap();
            let evidence = directory.path().join("evidence");
            fs::create_dir(&evidence).unwrap();
            let paths = capture_paths(directory.path());
            let capture = GuestRunCaptureSession::create(&paths, &evidence).unwrap();
            capture
                .stdout_for_guest()
                .unwrap()
                .write_all(b"out")
                .unwrap();
            let mut stderr = &capture.stderr;
            stderr.write_all(b"err").unwrap();
            let error = capture
                .finish(
                    Backend::Ptrace,
                    ExitStatus::Exited(code),
                    GuestRunDeterminism {
                        detlog_io_buffers: true,
                        virtualize_time: true,
                    },
                )
                .unwrap_err();
            assert!(error.to_string().contains("invalid Linux disposition"));
            assert!(!paths.result.exists());
            assert_eq!(fs::read(&paths.stdout).unwrap(), b"out");
            assert_eq!(fs::read(&paths.stderr).unwrap(), b"err");
        }
    }

    #[test]
    fn capture_preserves_a_preexisting_result_before_creating_streams() {
        let directory = tempfile::tempdir().unwrap();
        let evidence = directory.path().join("evidence");
        fs::create_dir(&evidence).unwrap();
        let paths = capture_paths(directory.path());
        fs::write(&paths.result, b"preserve result").unwrap();
        let error = GuestRunCaptureSession::create(&paths, &evidence).unwrap_err();
        assert!(error.to_string().contains("no-clobber"));
        assert_eq!(fs::read(&paths.result).unwrap(), b"preserve result");
        assert!(!paths.stdout.exists());
        assert!(!paths.stderr.exists());
    }
}
