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

    fn require_different_spelling(&self, other: &Path, other_name: &str) -> Result<(), Error> {
        if [&self.result, &self.stdout, &self.stderr]
            .into_iter()
            .any(|path| path == other)
        {
            return Err(collision_error(other, other_name));
        }
        Ok(())
    }

    pub(crate) fn require_distinct_from(
        &self,
        other: Option<&Path>,
        other_name: &str,
    ) -> Result<(), Error> {
        let Some(other) = other else {
            return Ok(());
        };
        self.require_distinct_from_in(other, other_name, Path::new("."), false)
    }

    /// A summary is written inside the completed container. Refuse collisions
    /// already visible on the host, but leave unresolved guest-only paths for
    /// the mandatory check against inherited capture handles in that namespace.
    pub(crate) fn check_host_summary(&self, other: Option<&Path>) -> Result<(), Error> {
        let Some(other) = other else {
            return Ok(());
        };
        self.require_distinct_from_in(other, "--summary-json", Path::new("."), true)
    }

    fn require_distinct_from_in(
        &self,
        other: &Path,
        other_name: &str,
        cwd: &Path,
        defer_unresolved: bool,
    ) -> Result<(), Error> {
        self.require_different_spelling(other, other_name)?;
        let destination = match OutputDestination::resolve(other, cwd) {
            Ok(destination) => destination,
            Err(_) if defer_unresolved => return Ok(()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("resolving {other_name} path {}", other.display()));
            }
        };
        for path in [&self.result, &self.stdout, &self.stderr] {
            let capture = OutputDestination::resolve(path, cwd)
                .with_context(|| format!("resolving guest capture path {}", path.display()))?;
            if capture.aliases(&destination) {
                return Err(collision_error(other, other_name));
            }
        }
        Ok(())
    }
}

/// Identify the destination that an ordinary followed-path writer would use,
/// including a not-yet-created leaf. Parent traversal is left to the kernel:
/// cancelling `..` lexically would be wrong when a preceding component is a
/// symlink. Terminal symlinks also matter before their capture target exists.
struct OutputDestination {
    parent: RunEvidenceFileIdentity,
    name: OsString,
    file: Option<RunEvidenceFileIdentity>,
}

impl OutputDestination {
    fn resolve(path: &Path, cwd: &Path) -> io::Result<Self> {
        let mut path = cwd.join(path);
        for followed in 0..=40 {
            // Follow the actual complete path first. In particular, procfs
            // descriptor links select an open object; read_link's displayed
            // pathname may be deleted or shadowed in this namespace.
            let followed_file = match std::fs::metadata(&path) {
                Ok(metadata) => Some(metadata_identity(&metadata)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            };
            let last = path
                .as_os_str()
                .as_bytes()
                .rsplit(|byte| *byte == b'/')
                .next()
                .unwrap_or_default();
            if last.is_empty() || last == b"." || last == b".." {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "output destination has no file basename",
                ));
            }
            let parent_path = path.parent().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "output destination has no parent",
                )
            })?;
            let parent = std::fs::metadata(parent_path)?;
            if !parent.is_dir() {
                return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
            }
            let file = if followed_file.is_some() {
                followed_file
            } else {
                match std::fs::symlink_metadata(&path) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        if followed == 40 {
                            return Err(io::Error::from_raw_os_error(libc::ELOOP));
                        }
                        path = parent_path.join(std::fs::read_link(&path)?);
                        continue;
                    }
                    Ok(metadata) => Some(metadata_identity(&metadata)),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                    Err(error) => return Err(error),
                }
            };
            return Ok(Self {
                parent: metadata_identity(&parent),
                name: OsStr::from_bytes(last).to_owned(),
                file,
            });
        }
        unreachable!("terminal symlink traversal is bounded")
    }

    fn aliases(&self, other: &Self) -> bool {
        (self.parent == other.parent && self.name == other.name)
            || self.file.is_some_and(|file| Some(file) == other.file)
    }
}

fn collision_error(other: &Path, other_name: &str) -> Error {
    Error::msg(format!(
        "--run-result-json, --guest-stdout, and --guest-stderr must not reuse the {other_name} path {}",
        other.display()
    ))
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
    result: Option<File>,
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
            result: None,
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

    /// Use inherited identities, not host pathnames: the completed container
    /// may hide those paths or expose the same directory at a different name.
    pub(crate) fn require_distinct_from(
        &self,
        other: &Path,
        other_name: &str,
    ) -> Result<(), Error> {
        let destination = OutputDestination::resolve(other, Path::new(".")).with_context(|| {
            format!(
                "resolving {other_name} path {} in the writer namespace",
                other.display()
            )
        })?;
        let same_child = destination.parent == self.parent_identity
            && [&self.result_name, &self.stdout_name, &self.stderr_name]
                .contains(&&destination.name);
        let result_identity = self.result.as_ref().map(file_identity).transpose()?;
        let same_file = destination.file.is_some_and(|file| {
            file == self.stdout_identity
                || file == self.stderr_identity
                || Some(file) == result_identity
        });
        if same_child || same_file {
            return Err(collision_error(other, other_name));
        }
        Ok(())
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
        &mut self,
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
        })?;
        // Keep the published result inode alongside both stream handles until
        // the outer engagement writer has passed its final collision check.
        self.result = Some(staged);
        Ok(())
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
    Ok(metadata_identity(&file.metadata()?))
}

fn metadata_identity(metadata: &std::fs::Metadata) -> RunEvidenceFileIdentity {
    use std::os::unix::fs::MetadataExt as _;
    RunEvidenceFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
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
    fn capture_collision_resolution_preserves_path_semantics() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let capture_dir = root.join("capture");
        fs::create_dir_all(capture_dir.join("sub")).unwrap();
        fs::create_dir(root.join("other")).unwrap();
        let paths = capture_paths(&capture_dir);
        symlink(&capture_dir, root.join("alias")).unwrap();
        symlink(capture_dir.join("sub"), root.join("shortcut")).unwrap();
        symlink("capture/stdout", root.join("dangling")).unwrap();
        symlink("dangling", root.join("chain")).unwrap();
        for path in [
            "capture/stdout",
            "capture/./stdout",
            "capture/sub/../stdout",
            "alias/stdout",
            "shortcut/../stdout",
            "dangling",
            "chain",
        ] {
            let error = paths
                .require_distinct_from_in(Path::new(path), "--summary-json", root, false)
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("must not reuse the --summary-json path"),
                "{path}: {error:#}"
            );
        }
        for path in ["capture/distinct", "other/stdout", "shortcut/../../stdout"] {
            paths
                .require_distinct_from_in(Path::new(path), "--summary-json", root, false)
                .unwrap();
        }
        // The kernel must traverse `sub/..`: treating a missing component as
        // lexical cancellation would certify a path that Linux cannot open.
        assert!(
            paths
                .require_distinct_from_in(
                    Path::new("missing/../distinct"),
                    "--summary-json",
                    root,
                    false
                )
                .is_err()
        );
        symlink("loop", root.join("loop")).unwrap();
        let error = paths
            .require_distinct_from_in(Path::new("loop"), "--summary-json", root, false)
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().raw_os_error(),
            Some(libc::ELOOP)
        );
        assert!(!paths.result.exists());
        assert!(!paths.stdout.exists());
        assert!(!paths.stderr.exists());
    }

    #[test]
    fn capture_held_identity_refuses_summary_aliases() {
        let directory = tempfile::tempdir().unwrap();
        let capture_dir = directory.path().join("capture");
        fs::create_dir(&capture_dir).unwrap();
        let evidence = capture_dir.join("evidence");
        fs::create_dir(&evidence).unwrap();
        let paths = capture_paths(&capture_dir);
        let mut capture = GuestRunCaptureSession::create(&paths, &evidence).unwrap();
        capture
            .stdout_for_guest()
            .unwrap()
            .write_all(b"out")
            .unwrap();
        (&capture.stderr).write_all(b"err").unwrap();
        let distinct = directory.path().join("distinct");
        fs::write(&distinct, b"independent").unwrap();
        capture
            .require_distinct_from(&distinct, "--summary-json")
            .unwrap();
        for (index, target) in [&paths.stdout, &paths.stderr].into_iter().enumerate() {
            for symbolic in [false, true] {
                let alias = directory.path().join(format!("alias-{index}-{symbolic}"));
                if symbolic {
                    symlink(target, &alias).unwrap();
                } else {
                    fs::hard_link(target, &alias).unwrap();
                }
                assert!(
                    capture
                        .require_distinct_from(&alias, "--summary-json")
                        .unwrap_err()
                        .to_string()
                        .contains("must not reuse")
                );
            }
        }
        let result_alias = directory.path().join("future-result");
        symlink(&paths.result, &result_alias).unwrap();
        assert!(
            capture
                .require_distinct_from(&result_alias, "--summary-json")
                .is_err()
        );
        let unavailable = directory.path().join("guest-only/summary");
        paths.check_host_summary(Some(&unavailable)).unwrap();
        assert!(
            capture
                .require_distinct_from(&unavailable, "--summary-json")
                .is_err()
        );

        // Model an alternate visible name without a privileged mount: the
        // admission helper must use inherited identities, not reopen host names.
        let moved = directory.path().join("moved");
        fs::rename(&capture_dir, &moved).unwrap();
        assert!(!capture_dir.exists());
        assert!(
            capture
                .require_distinct_from(&moved.join("stdout"), "--summary-json")
                .unwrap_err()
                .to_string()
                .contains("must not reuse")
        );
        capture
            .require_distinct_from(&distinct, "--summary-json")
            .unwrap();
        fs::rename(&moved, &capture_dir).unwrap();
        capture
            .finish(
                Backend::Ptrace,
                ExitStatus::Exited(0),
                GuestRunDeterminism {
                    detlog_io_buffers: true,
                    virtualize_time: true,
                },
            )
            .unwrap();
        let published_alias = directory.path().join("published-result");
        fs::hard_link(&paths.result, &published_alias).unwrap();
        assert!(
            capture
                .require_distinct_from(&published_alias, "--backend-engagement-json")
                .unwrap_err()
                .to_string()
                .contains("must not reuse")
        );
        assert_eq!(fs::read(&paths.stdout).unwrap(), b"out");
        assert_eq!(fs::read(&paths.stderr).unwrap(), b"err");
        assert_eq!(fs::read(&distinct).unwrap(), b"independent");
        let result =
            GuestRunResult::from_current_json_slice(&fs::read(&paths.result).unwrap()).unwrap();
        assert_eq!(result.stdout.bytes, 3);
        assert_eq!(result.stderr.bytes, 3);

        for file in [
            &capture.stdout,
            &capture.stderr,
            capture.result.as_ref().unwrap(),
        ] {
            let descriptor = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
            assert!(
                capture
                    .require_distinct_from(&descriptor, "--summary-json")
                    .unwrap_err()
                    .to_string()
                    .contains("must not reuse")
            );
        }
        let independent = File::open(&distinct).unwrap();
        let independent_descriptor =
            PathBuf::from(format!("/proc/self/fd/{}", independent.as_raw_fd()));
        capture
            .require_distinct_from(&independent_descriptor, "--summary-json")
            .unwrap();
        // The held stdout still selects its original inode after its former
        // name is replaced. Procfs's " (deleted)" display is not a new path.
        fs::remove_file(&paths.stdout).unwrap();
        fs::write(&paths.stdout, b"replacement").unwrap();
        let descriptor = PathBuf::from(format!("/proc/self/fd/{}", capture.stdout.as_raw_fd()));
        let displayed = fs::read_link(&descriptor).unwrap();
        assert!(displayed.as_os_str().as_bytes().ends_with(b" (deleted)"));
        assert_ne!(
            fs::metadata(&descriptor).unwrap().ino(),
            fs::metadata(&paths.stdout).unwrap().ino()
        );
        assert!(
            capture
                .require_distinct_from(&descriptor, "--summary-json")
                .unwrap_err()
                .to_string()
                .contains("must not reuse")
        );
        assert_eq!(fs::read(&descriptor).unwrap(), b"out");
        assert_eq!(fs::read(&paths.stdout).unwrap(), b"replacement");
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
        let mut capture = GuestRunCaptureSession::create(&paths, &evidence).unwrap();
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
        let mut capture = GuestRunCaptureSession::create(&paths, &evidence).unwrap();
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
            let mut capture = GuestRunCaptureSession::create(&paths, &evidence).unwrap();
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
