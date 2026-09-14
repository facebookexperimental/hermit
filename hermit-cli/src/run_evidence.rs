//! Typed evidence for one ordinary `hermit run` invocation.
//!
//! This is deliberately distinct from [`crate::canonical_verdict`]: verification
//! compares two runs, while this report binds one run's disposition to one
//! complete canonical-INFO input. A consumer must still compare two validated
//! reports before making a determinism claim.

use std::ffi::CString;
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::io::Read;
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

use crate::canonical_verdict::RecordEnvelopeReport;

pub const RUN_EVIDENCE_SCHEMA_VERSION: u32 = 1;
pub const RUN_EVIDENCE_MANIFEST: &str = "manifest.json";
pub const RUN_EVIDENCE_INFO_ARTIFACT: &str = "canonical-info-v1.log";
pub const RUN_EVIDENCE_MANIFEST_MAX_BYTES: u64 = 1024 * 1024;
pub const RUN_EVIDENCE_INFO_MAX_BYTES: u64 = 1024 * 1024 * 1024;
/// The producer prepares this mode while the manifest is still unnamed.
/// A readable manifest is a terminal candidate, not a durability certificate:
/// only the locked, synchronizing inspector may return `Complete`.
pub const RUN_EVIDENCE_MANIFEST_MODE: u32 = 0o400;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunEvidenceBackend {
    Ptrace,
    Dbt,
    Liteinst,
    Sabre,
    Kvm,
    E9patch,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CanonicalInfoPolicy {
    BitwiseInfoV1,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DispositionLimitation {
    /// Reverie KVM currently returns one integer and does not distinguish an
    /// ordinary exit from signal death or report a core-dump bit.
    KvmExitCodeOnly,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum GuestDisposition {
    Exited {
        code: i32,
    },
    Signaled {
        signal: i32,
        core_dumped: bool,
    },
    ExitCodeOnly {
        code: i32,
        limitation: DispositionLimitation,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunEvidenceNoResultReason {
    RunFailed,
    UnsupportedBackend,
    MissingCanonicalInfo,
    ZeroCanonicalInfo,
    TruncatedCanonicalInfo,
    MalformedCanonicalInfo,
    ArtifactWriteFailed,
    UnsupportedDisposition,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RunEvidenceOutcome {
    Complete {
        disposition: GuestDisposition,
    },
    NoResult {
        reason: RunEvidenceNoResultReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        observed_disposition: Option<GuestDisposition>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CanonicalInfoEvidence {
    pub policy: CanonicalInfoPolicy,
    pub record_envelope: RecordEnvelopeReport,
    pub message_count: u64,
    pub byte_count: u64,
    pub sha256: Option<String>,
    pub artifact: String,
}

impl CanonicalInfoEvidence {
    pub fn no_result() -> Self {
        Self {
            policy: CanonicalInfoPolicy::BitwiseInfoV1,
            record_envelope: RecordEnvelopeReport::AllRecordsV1,
            message_count: 0,
            byte_count: 0,
            sha256: None,
            artifact: RUN_EVIDENCE_INFO_ARTIFACT.to_string(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunEvidenceReport {
    pub schema_version: u32,
    pub invocation_id: Uuid,
    pub backend: RunEvidenceBackend,
    pub attempt: u32,
    pub outcome: RunEvidenceOutcome,
    pub canonical_info: CanonicalInfoEvidence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunEvidenceInspectionFailure {
    MissingManifest,
    ManifestTooLarge,
    PublicationInProgress,
    PublicationLockFailed,
    ArtifactSyncFailed,
    ManifestSyncFailed,
    DirectorySyncFailed,
    IdentityChanged,
    MalformedManifest,
    UnsupportedSchema,
    InvalidManifest,
    ReportedNoResult(RunEvidenceNoResultReason),
    MissingArtifact,
    ArtifactTooLarge,
    ArtifactSizeMismatch,
    DigestMismatch,
    TruncatedCanonicalInfo,
    MalformedCanonicalInfo,
    ZeroCanonicalInfo,
    MessageCountMismatch,
}

impl std::fmt::Display for RunEvidenceInspectionFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunEvidenceFileIdentity {
    pub device: u64,
    pub inode: u64,
}

/// Digest and exact inode identity of one harness-owned guest stream.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedGuestStream {
    pub bytes: u64,
    pub sha256: String,
    pub identity: RunEvidenceFileIdentity,
}

/// Determinism settings bound by one ordinary-run result.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GuestRunDeterminism {
    pub detlog_io_buffers: bool,
    pub virtualize_time: bool,
}

/// Typed terminal result for one harness-managed ordinary execution.
///
/// The named stdout/stderr files are separate from Hermit's own diagnostic
/// descriptors. A consumer must also validate the companion run evidence and
/// exact stream bytes before accepting this result as a complete observation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GuestRunResult {
    pub schema_version: u32,
    #[serde(deserialize_with = "deserialize_guest_run_disposition")]
    pub disposition: GuestDisposition,
    pub determinism: GuestRunDeterminism,
    pub stdout: CapturedGuestStream,
    pub stderr: CapturedGuestStream,
}

fn deserialize_guest_run_disposition<'de, D>(deserializer: D) -> Result<GuestDisposition, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // Keep the existing manifest reader's representation compatible while
    // rejecting contradictory or unknown fields in the new guest result.
    #[derive(Deserialize)]
    #[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
    enum StrictDisposition {
        Exited {
            code: i32,
        },
        Signaled {
            signal: i32,
            core_dumped: bool,
        },
        ExitCodeOnly {
            code: i32,
            limitation: DispositionLimitation,
        },
    }

    Ok(match StrictDisposition::deserialize(deserializer)? {
        StrictDisposition::Exited { code } => GuestDisposition::Exited { code },
        StrictDisposition::Signaled {
            signal,
            core_dumped,
        } => GuestDisposition::Signaled {
            signal,
            core_dumped,
        },
        StrictDisposition::ExitCodeOnly { code, limitation } => {
            GuestDisposition::ExitCodeOnly { code, limitation }
        }
    })
}

impl GuestRunResult {
    pub const SCHEMA_VERSION: u32 = 1;

    pub fn from_current_json_slice(bytes: &[u8]) -> Result<Self, String> {
        // Deserialize the original bytes so duplicate fields cannot collapse
        // through an intermediate serde_json::Value.
        let result: Self = serde_json::from_slice(bytes)
            .map_err(|error| format!("invalid guest run result: {error}"))?;
        result.validate_current()?;
        Ok(result)
    }

    pub fn validate_current(&self) -> Result<(), String> {
        if self.schema_version != Self::SCHEMA_VERSION {
            return Err(format!(
                "unsupported guest run result schema {}; expected {}",
                self.schema_version,
                Self::SCHEMA_VERSION
            ));
        }
        if !disposition_numbers_are_valid(self.disposition) {
            return Err("guest run result has an invalid Linux disposition".into());
        }
        for (name, stream) in [("stdout", &self.stdout), ("stderr", &self.stderr)] {
            let valid_sha = stream.sha256.len() == 64
                && stream
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
            if !valid_sha {
                return Err(format!(
                    "guest run result {name} sha256 is not lowercase hex"
                ));
            }
            if stream.identity.inode == 0 {
                return Err(format!("guest run result {name} inode must be nonzero"));
            }
        }
        if self.stdout.identity == self.stderr.identity {
            return Err("guest run result stdout and stderr must name distinct inodes".into());
        }
        Ok(())
    }
}

/// A synchronized report and the exact bytes inspected while its publication
/// lock and artifact inode were held. Later path changes do not change this
/// snapshot; callers must compare these bytes rather than reopen the artifact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedRunEvidence {
    pub report: RunEvidenceReport,
    pub canonical_info: Vec<u8>,
    pub artifact_identity: RunEvidenceFileIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunEvidenceInspection {
    Complete(RunEvidenceReport),
    NoResult(RunEvidenceInspectionFailure),
}

fn backend_supports_evidence(backend: RunEvidenceBackend) -> bool {
    matches!(
        backend,
        RunEvidenceBackend::Ptrace | RunEvidenceBackend::Liteinst | RunEvidenceBackend::Kvm
    )
}

fn disposition_numbers_are_valid(disposition: GuestDisposition) -> bool {
    // These are Linux wait dispositions, not arbitrary i32 syscall arguments.
    // The backends normalize an exit argument before producing ExitStatus.
    match disposition {
        GuestDisposition::Exited { code } | GuestDisposition::ExitCodeOnly { code, .. } => {
            (0..=255).contains(&code)
        }
        // Linux supports signal numbers 1 through 64, including realtime signals.
        GuestDisposition::Signaled { signal, .. } => (1..=64).contains(&signal),
    }
}

fn disposition_matches_backend(backend: RunEvidenceBackend, disposition: GuestDisposition) -> bool {
    if !disposition_numbers_are_valid(disposition) {
        return false;
    }
    match backend {
        RunEvidenceBackend::Kvm => matches!(
            disposition,
            GuestDisposition::ExitCodeOnly {
                limitation: DispositionLimitation::KvmExitCodeOnly,
                ..
            }
        ),
        RunEvidenceBackend::Ptrace | RunEvidenceBackend::Liteinst => matches!(
            disposition,
            GuestDisposition::Exited { .. } | GuestDisposition::Signaled { .. }
        ),
        RunEvidenceBackend::Dbt | RunEvidenceBackend::Sabre | RunEvidenceBackend::E9patch => false,
    }
}

fn static_manifest_fields_are_valid(report: &RunEvidenceReport) -> bool {
    report.schema_version == RUN_EVIDENCE_SCHEMA_VERSION
        && !report.invocation_id.is_nil()
        && report.attempt == 1
        && report.canonical_info.policy == CanonicalInfoPolicy::BitwiseInfoV1
        && report.canonical_info.record_envelope == RecordEnvelopeReport::AllRecordsV1
        && report.canonical_info.artifact == RUN_EVIDENCE_INFO_ARTIFACT
        && match report.outcome {
            RunEvidenceOutcome::Complete { disposition } => {
                backend_supports_evidence(report.backend)
                    && disposition_matches_backend(report.backend, disposition)
            }
            RunEvidenceOutcome::NoResult {
                reason,
                observed_disposition,
            } => {
                observed_disposition.is_none_or(|disposition| {
                    disposition_matches_backend(report.backend, disposition)
                }) && (reason == RunEvidenceNoResultReason::UnsupportedBackend)
                    != backend_supports_evidence(report.backend)
            }
        }
}
fn component_cstring(component: &OsStr) -> io::Result<CString> {
    CString::new(component.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

fn owned_file(fd: RawFd) -> io::Result<File> {
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: a nonnegative open/openat return is one newly owned fd.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn open_evidence_directory(path: &Path) -> io::Result<File> {
    let path = component_cstring(path.as_os_str())?;
    owned_file(unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    })
}

struct HeldChild {
    file: File,
    bytes: Vec<u8>,
    device: libc::dev_t,
    inode: libc::ino_t,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegularChildReadFailure {
    Unavailable,
    TooLarge,
    SizeChanged,
}

fn read_regular_child(
    directory: &File,
    name: &OsStr,
    required_mode: Option<u32>,
    maximum_bytes: u64,
) -> Result<HeldChild, RegularChildReadFailure> {
    let name = component_cstring(name).map_err(|_| RegularChildReadFailure::Unavailable)?;
    let mut file = owned_file(unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    })
    .map_err(|_| RegularChildReadFailure::Unavailable)?;
    let mut stat = MaybeUninit::<libc::stat>::zeroed();
    if unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(RegularChildReadFailure::Unavailable);
    }
    // SAFETY: fstat initialized the complete structure on success.
    let stat = unsafe { stat.assume_init() };
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG
        || required_mode.is_some_and(|mode| stat.st_mode & 0o777 != mode)
    {
        return Err(RegularChildReadFailure::Unavailable);
    }
    let initial_size =
        u64::try_from(stat.st_size).map_err(|_| RegularChildReadFailure::Unavailable)?;
    if initial_size > maximum_bytes {
        return Err(RegularChildReadFailure::TooLarge);
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(maximum_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| RegularChildReadFailure::Unavailable)?;
    if u64::try_from(bytes.len()).map_err(|_| RegularChildReadFailure::TooLarge)? > maximum_bytes {
        return Err(RegularChildReadFailure::TooLarge);
    }
    let metadata = file
        .metadata()
        .map_err(|_| RegularChildReadFailure::Unavailable)?;
    if metadata.len() != initial_size || metadata.len() != bytes.len() as u64 {
        return Err(RegularChildReadFailure::SizeChanged);
    }
    Ok(HeldChild {
        file,
        bytes,
        device: stat.st_dev,
        inode: stat.st_ino,
    })
}

fn child_identity_is_current(
    directory: &File,
    name: &str,
    held: &HeldChild,
    required_mode: Option<u32>,
) -> io::Result<bool> {
    let name = component_cstring(OsStr::new(name))?;
    let mut stat = MaybeUninit::<libc::stat>::zeroed();
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful fstatat initialized the structure; symlinks were not followed.
    let stat = unsafe { stat.assume_init() };
    Ok(stat.st_mode & libc::S_IFMT == libc::S_IFREG
        && required_mode.is_none_or(|mode| stat.st_mode & 0o777 == mode)
        && (stat.st_dev, stat.st_ino) == (held.device, held.inode))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InspectionSyncPoint {
    Artifact,
    Manifest,
    Directory,
}

/// Read and independently validate an ordinary-run sidecar.
///
/// Every unreadable or incomplete state is a typed no-result. In particular,
/// this function never returns `Complete` merely because the manifest says so:
/// it re-reads the artifact, verifies its length and SHA-256 digest, and runs
/// the fixed `BitwiseInfoV1` parser over the exact bytes. A nonblocking shared
/// lock excludes a live publisher. The held files and directory are synchronized
/// before `Complete`: publisher success alone only publishes a terminal candidate.
/// Lock release after publisher death is not a substitute for these checked syncs.
pub fn inspect_run_evidence(directory: &Path) -> RunEvidenceInspection {
    let directory = match open_evidence_directory(directory) {
        Ok(directory) => directory,
        Err(_) => {
            return RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::MissingManifest);
        }
    };
    inspect_run_evidence_directory(&directory)
}

fn inspect_run_evidence_directory(directory: &File) -> RunEvidenceInspection {
    inspect_with_sync(directory, &mut |_, file| file.sync_all())
}

/// Load a complete report and its exact canonical bytes using the same held
/// inode, publication lock, parser and checked syncs as [`inspect_run_evidence`].
pub fn load_run_evidence(
    directory: &Path,
) -> Result<ValidatedRunEvidence, RunEvidenceInspectionFailure> {
    let directory = open_evidence_directory(directory)
        .map_err(|_| RunEvidenceInspectionFailure::MissingManifest)?;
    load_with_sync(&directory, &mut |_, file| file.sync_all())
}

fn inspect_with_sync(
    directory: &File,
    synchronize: &mut impl FnMut(InspectionSyncPoint, &File) -> io::Result<()>,
) -> RunEvidenceInspection {
    match load_with_sync(directory, synchronize) {
        Ok(evidence) => RunEvidenceInspection::Complete(evidence.report),
        Err(failure) => RunEvidenceInspection::NoResult(failure),
    }
}

fn load_with_sync(
    directory: &File,
    synchronize: &mut impl FnMut(InspectionSyncPoint, &File) -> io::Result<()>,
) -> Result<ValidatedRunEvidence, RunEvidenceInspectionFailure> {
    // Open a separate description of the same held inode. try_clone would share
    // flock ownership with the caller and could accidentally convert its lock.
    let locked_directory = match owned_file(unsafe {
        libc::openat(
            directory.as_raw_fd(),
            c".".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    }) {
        Ok(directory) => directory,
        Err(_) => {
            return Err(RunEvidenceInspectionFailure::PublicationLockFailed);
        }
    };
    if unsafe { libc::flock(locked_directory.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } != 0 {
        let error = io::Error::last_os_error();
        return Err(if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            RunEvidenceInspectionFailure::PublicationInProgress
        } else {
            RunEvidenceInspectionFailure::PublicationLockFailed
        });
    }
    // The independently owned description releases this shared lock on every
    // return, after all held-file validation/synchronization has completed.
    let directory = &locked_directory;
    let manifest = match read_regular_child(
        directory,
        OsStr::new(RUN_EVIDENCE_MANIFEST),
        Some(RUN_EVIDENCE_MANIFEST_MODE),
        RUN_EVIDENCE_MANIFEST_MAX_BYTES,
    ) {
        Ok(manifest) => manifest,
        Err(failure) => {
            return Err(match failure {
                RegularChildReadFailure::Unavailable => {
                    RunEvidenceInspectionFailure::MissingManifest
                }
                RegularChildReadFailure::TooLarge => RunEvidenceInspectionFailure::ManifestTooLarge,
                RegularChildReadFailure::SizeChanged => {
                    RunEvidenceInspectionFailure::MalformedManifest
                }
            });
        }
    };
    let report: RunEvidenceReport = match serde_json::from_slice(&manifest.bytes) {
        Ok(report) => report,
        Err(_) => {
            return Err(RunEvidenceInspectionFailure::MalformedManifest);
        }
    };
    if report.schema_version != RUN_EVIDENCE_SCHEMA_VERSION {
        return Err(RunEvidenceInspectionFailure::UnsupportedSchema);
    }
    if !static_manifest_fields_are_valid(&report) {
        return Err(RunEvidenceInspectionFailure::InvalidManifest);
    }
    match report.outcome {
        RunEvidenceOutcome::NoResult { reason, .. } => {
            if report.canonical_info.message_count != 0
                || report.canonical_info.byte_count != 0
                || report.canonical_info.sha256.is_some()
            {
                return Err(RunEvidenceInspectionFailure::InvalidManifest);
            }
            return Err(RunEvidenceInspectionFailure::ReportedNoResult(reason));
        }
        RunEvidenceOutcome::Complete { .. } => {}
    }

    if report.canonical_info.message_count == 0 {
        return Err(RunEvidenceInspectionFailure::ZeroCanonicalInfo);
    }
    let Some(expected_digest) = report.canonical_info.sha256.as_deref() else {
        return Err(RunEvidenceInspectionFailure::InvalidManifest);
    };
    if report.canonical_info.byte_count > RUN_EVIDENCE_INFO_MAX_BYTES {
        return Err(RunEvidenceInspectionFailure::ArtifactTooLarge);
    }
    let artifact = match read_regular_child(
        directory,
        OsStr::new(RUN_EVIDENCE_INFO_ARTIFACT),
        None,
        RUN_EVIDENCE_INFO_MAX_BYTES,
    ) {
        Ok(artifact) => artifact,
        Err(failure) => {
            return Err(match failure {
                RegularChildReadFailure::Unavailable => {
                    RunEvidenceInspectionFailure::MissingArtifact
                }
                RegularChildReadFailure::TooLarge => RunEvidenceInspectionFailure::ArtifactTooLarge,
                RegularChildReadFailure::SizeChanged => {
                    RunEvidenceInspectionFailure::ArtifactSizeMismatch
                }
            });
        }
    };
    if artifact.bytes.len() as u64 != report.canonical_info.byte_count {
        return Err(RunEvidenceInspectionFailure::ArtifactSizeMismatch);
    }
    if detcore::Digest::new(&artifact.bytes).to_string() != expected_digest {
        return Err(RunEvidenceInspectionFailure::DigestMismatch);
    }
    if std::str::from_utf8(&artifact.bytes)
        .ok()
        .is_some_and(detcore::logdiff::log_was_truncated)
    {
        return Err(RunEvidenceInspectionFailure::TruncatedCanonicalInfo);
    }
    let mut canonical = Vec::new();
    let count = match detcore::logdiff::write_bitwise_info_v1_bytes(
        &artifact.bytes,
        RUN_EVIDENCE_INFO_ARTIFACT,
        &mut canonical,
    ) {
        Ok(count) => count as u64,
        Err(_) => {
            return Err(RunEvidenceInspectionFailure::MalformedCanonicalInfo);
        }
    };
    if count == 0 {
        return Err(RunEvidenceInspectionFailure::ZeroCanonicalInfo);
    }
    if count != report.canonical_info.message_count {
        return Err(RunEvidenceInspectionFailure::MessageCountMismatch);
    }
    for (point, file, failure) in [
        (
            InspectionSyncPoint::Artifact,
            &artifact.file,
            RunEvidenceInspectionFailure::ArtifactSyncFailed,
        ),
        (
            InspectionSyncPoint::Manifest,
            &manifest.file,
            RunEvidenceInspectionFailure::ManifestSyncFailed,
        ),
        (
            InspectionSyncPoint::Directory,
            directory,
            RunEvidenceInspectionFailure::DirectorySyncFailed,
        ),
    ] {
        if synchronize(point, file).is_err() {
            return Err(failure);
        }
    }
    if !child_identity_is_current(
        directory,
        RUN_EVIDENCE_MANIFEST,
        &manifest,
        Some(RUN_EVIDENCE_MANIFEST_MODE),
    )
    .unwrap_or(false)
        || !child_identity_is_current(directory, RUN_EVIDENCE_INFO_ARTIFACT, &artifact, None)
            .unwrap_or(false)
    {
        return Err(RunEvidenceInspectionFailure::IdentityChanged);
    }
    Ok(ValidatedRunEvidence {
        report,
        canonical_info: artifact.bytes,
        artifact_identity: RunEvidenceFileIdentity {
            device: artifact.device,
            inode: artifact.inode,
        },
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn valid_log() -> Vec<u8> {
        b"Apr 09 06:08:01.100  INFO hermit_test: first evidence record\n\
Apr 09 06:08:02.100  INFO hermit_test: second evidence record\n"
            .to_vec()
    }

    fn write_complete_fixture(directory: &Path, artifact: &[u8], digest: String, count: u64) {
        fs::write(directory.join(RUN_EVIDENCE_INFO_ARTIFACT), artifact).unwrap();
        let report = RunEvidenceReport {
            schema_version: RUN_EVIDENCE_SCHEMA_VERSION,
            invocation_id: Uuid::from_u128(1),
            backend: RunEvidenceBackend::Ptrace,
            attempt: 1,
            outcome: RunEvidenceOutcome::Complete {
                disposition: GuestDisposition::Exited { code: 0 },
            },
            canonical_info: CanonicalInfoEvidence {
                policy: CanonicalInfoPolicy::BitwiseInfoV1,
                record_envelope: RecordEnvelopeReport::AllRecordsV1,
                message_count: count,
                byte_count: artifact.len() as u64,
                sha256: Some(digest),
                artifact: RUN_EVIDENCE_INFO_ARTIFACT.to_string(),
            },
        };
        let manifest = directory.join(RUN_EVIDENCE_MANIFEST);
        fs::write(&manifest, serde_json::to_vec(&report).unwrap()).unwrap();
        let mut permissions = fs::metadata(&manifest).unwrap().permissions();
        permissions.set_mode(RUN_EVIDENCE_MANIFEST_MODE);
        fs::set_permissions(manifest, permissions).unwrap();
    }

    #[test]
    fn complete_report_requires_the_bound_artifact() {
        let directory = tempfile::tempdir().unwrap();
        let log = valid_log();
        write_complete_fixture(
            directory.path(),
            &log,
            detcore::Digest::new(&log).to_string(),
            2,
        );
        assert!(matches!(
            inspect_run_evidence(directory.path()),
            RunEvidenceInspection::Complete(_)
        ));
    }

    #[test]
    fn missing_zero_truncated_malformed_and_digest_mismatch_are_no_result() {
        let missing = tempfile::tempdir().unwrap();
        assert_eq!(
            inspect_run_evidence(missing.path()),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::MissingManifest)
        );

        let zero = tempfile::tempdir().unwrap();
        write_complete_fixture(zero.path(), b"", detcore::Digest::new(b"").to_string(), 0);
        assert_eq!(
            inspect_run_evidence(zero.path()),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::ZeroCanonicalInfo)
        );

        let truncated = tempfile::tempdir().unwrap();
        let truncated_log = format!(
            "{}{}\n",
            String::from_utf8(valid_log()).unwrap(),
            detcore::logdiff::TRUNCATION_MARKER
        );
        write_complete_fixture(
            truncated.path(),
            truncated_log.as_bytes(),
            detcore::Digest::new(truncated_log.as_bytes()).to_string(),
            2,
        );
        assert_eq!(
            inspect_run_evidence(truncated.path()),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::TruncatedCanonicalInfo)
        );

        let malformed = tempfile::tempdir().unwrap();
        let invalid_utf8 = [0x80];
        write_complete_fixture(
            malformed.path(),
            &invalid_utf8,
            detcore::Digest::new(&invalid_utf8).to_string(),
            1,
        );
        assert_eq!(
            inspect_run_evidence(malformed.path()),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::MalformedCanonicalInfo)
        );

        let mismatch = tempfile::tempdir().unwrap();
        let log = valid_log();
        write_complete_fixture(
            mismatch.path(),
            &log,
            detcore::Digest::new(b"different").to_string(),
            2,
        );
        assert_eq!(
            inspect_run_evidence(mismatch.path()),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::DigestMismatch)
        );
    }
    #[test]
    fn inspector_pins_one_directory_identity() {
        let parent = tempfile::tempdir().unwrap();
        let requested = parent.path().join("requested");
        fs::create_dir(&requested).unwrap();
        let log = valid_log();
        write_complete_fixture(&requested, &log, detcore::Digest::new(&log).to_string(), 2);
        let held = open_evidence_directory(&requested).unwrap();

        let original = parent.path().join("original");
        fs::rename(&requested, &original).unwrap();
        fs::create_dir(&requested).unwrap();

        assert!(matches!(
            inspect_run_evidence_directory(&held),
            RunEvidenceInspection::Complete(_)
        ));
        assert_eq!(
            inspect_run_evidence(&requested),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::MissingManifest)
        );
    }

    #[test]
    fn inspector_refuses_symlinked_directory_and_children() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let source = parent.path().join("source");
        fs::create_dir(&source).unwrap();
        let log = valid_log();
        write_complete_fixture(&source, &log, detcore::Digest::new(&log).to_string(), 2);

        let directory_link = parent.path().join("directory-link");
        symlink(&source, &directory_link).unwrap();
        assert_eq!(
            inspect_run_evidence(&directory_link),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::MissingManifest)
        );

        let manifest_link_case = parent.path().join("manifest-link");
        fs::create_dir(&manifest_link_case).unwrap();
        symlink(
            source.join(RUN_EVIDENCE_MANIFEST),
            manifest_link_case.join(RUN_EVIDENCE_MANIFEST),
        )
        .unwrap();
        assert_eq!(
            inspect_run_evidence(&manifest_link_case),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::MissingManifest)
        );

        let artifact_link_case = parent.path().join("artifact-link");
        fs::create_dir(&artifact_link_case).unwrap();
        fs::copy(
            source.join(RUN_EVIDENCE_MANIFEST),
            artifact_link_case.join(RUN_EVIDENCE_MANIFEST),
        )
        .unwrap();
        symlink(
            source.join(RUN_EVIDENCE_INFO_ARTIFACT),
            artifact_link_case.join(RUN_EVIDENCE_INFO_ARTIFACT),
        )
        .unwrap();
        assert_eq!(
            inspect_run_evidence(&artifact_link_case),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::MissingArtifact)
        );
    }

    fn replace_manifest(directory: &Path, change: impl FnOnce(&mut RunEvidenceReport)) {
        let path = directory.join(RUN_EVIDENCE_MANIFEST);
        let mut report: RunEvidenceReport =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        change(&mut report);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&path, serde_json::to_vec(&report).unwrap()).unwrap();
        fs::set_permissions(
            &path,
            fs::Permissions::from_mode(RUN_EVIDENCE_MANIFEST_MODE),
        )
        .unwrap();
    }

    #[test]
    fn linux_disposition_ranges_apply_to_complete_and_observed_no_result() {
        let directory = tempfile::tempdir().unwrap();
        let log = valid_log();
        write_complete_fixture(
            directory.path(),
            &log,
            detcore::Digest::new(&log).to_string(),
            2,
        );
        let template: RunEvidenceReport = serde_json::from_slice(
            &fs::read(directory.path().join(RUN_EVIDENCE_MANIFEST)).unwrap(),
        )
        .unwrap();
        for backend in [
            RunEvidenceBackend::Ptrace,
            RunEvidenceBackend::Liteinst,
            RunEvidenceBackend::Kvm,
        ] {
            for code in [i32::MIN, -1, 0, 23, 128, 255, 256, i32::MAX] {
                let disposition = if backend == RunEvidenceBackend::Kvm {
                    GuestDisposition::ExitCodeOnly {
                        code,
                        limitation: DispositionLimitation::KvmExitCodeOnly,
                    }
                } else {
                    GuestDisposition::Exited { code }
                };
                for no_result in [false, true] {
                    replace_manifest(directory.path(), |report| {
                        *report = template.clone();
                        report.backend = backend;
                        report.outcome = if no_result {
                            report.canonical_info = CanonicalInfoEvidence::no_result();
                            RunEvidenceOutcome::NoResult {
                                reason: RunEvidenceNoResultReason::RunFailed,
                                observed_disposition: Some(disposition),
                            }
                        } else {
                            RunEvidenceOutcome::Complete { disposition }
                        };
                    });
                    let inspected = inspect_run_evidence(directory.path());
                    if !(0..=255).contains(&code) {
                        assert_eq!(
                            inspected,
                            RunEvidenceInspection::NoResult(
                                RunEvidenceInspectionFailure::InvalidManifest
                            ),
                            "{backend:?} {code} no_result={no_result}"
                        );
                    } else if no_result {
                        assert_eq!(
                            inspected,
                            RunEvidenceInspection::NoResult(
                                RunEvidenceInspectionFailure::ReportedNoResult(
                                    RunEvidenceNoResultReason::RunFailed
                                )
                            )
                        );
                    } else {
                        let RunEvidenceInspection::Complete(report) = inspected else {
                            panic!(
                                "valid Linux disposition refused: {backend:?} {code}: {inspected:?}"
                            );
                        };
                        assert_eq!(report.outcome, RunEvidenceOutcome::Complete { disposition });
                    }
                }
            }
        }
        for signal in [i32::MIN, -1, 0, 1, 31, 32, 33, 34, 64, 65, i32::MAX] {
            for core_dumped in [false, true] {
                for no_result in [false, true] {
                    replace_manifest(directory.path(), |report| {
                        *report = template.clone();
                        let disposition = GuestDisposition::Signaled {
                            signal,
                            core_dumped,
                        };
                        report.outcome = if no_result {
                            report.canonical_info = CanonicalInfoEvidence::no_result();
                            RunEvidenceOutcome::NoResult {
                                reason: RunEvidenceNoResultReason::RunFailed,
                                observed_disposition: Some(disposition),
                            }
                        } else {
                            RunEvidenceOutcome::Complete { disposition }
                        };
                    });
                    let inspected = inspect_run_evidence(directory.path());
                    if !(1..=64).contains(&signal) {
                        assert_eq!(
                            inspected,
                            RunEvidenceInspection::NoResult(
                                RunEvidenceInspectionFailure::InvalidManifest
                            ),
                            "signal={signal}, core={core_dumped}, no_result={no_result}"
                        );
                    } else if no_result {
                        assert_eq!(
                            inspected,
                            RunEvidenceInspection::NoResult(
                                RunEvidenceInspectionFailure::ReportedNoResult(
                                    RunEvidenceNoResultReason::RunFailed
                                )
                            )
                        );
                    } else {
                        assert!(matches!(inspected, RunEvidenceInspection::Complete(_)));
                    }
                }
            }
        }
        for (backend, disposition) in [
            (
                RunEvidenceBackend::Kvm,
                GuestDisposition::Exited { code: 0 },
            ),
            (
                RunEvidenceBackend::Ptrace,
                GuestDisposition::ExitCodeOnly {
                    code: 0,
                    limitation: DispositionLimitation::KvmExitCodeOnly,
                },
            ),
        ] {
            replace_manifest(directory.path(), |report| {
                *report = template.clone();
                report.backend = backend;
                report.outcome = RunEvidenceOutcome::Complete { disposition };
            });
            assert_eq!(
                inspect_run_evidence(directory.path()),
                RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::InvalidManifest)
            );
        }
    }

    #[test]
    fn every_postpublication_sync_is_checked_before_complete() {
        let directory = tempfile::tempdir().unwrap();
        let log = valid_log();
        write_complete_fixture(
            directory.path(),
            &log,
            detcore::Digest::new(&log).to_string(),
            2,
        );
        let held = open_evidence_directory(directory.path()).unwrap();
        let points = [
            InspectionSyncPoint::Artifact,
            InspectionSyncPoint::Manifest,
            InspectionSyncPoint::Directory,
        ];
        for (index, failure) in [
            RunEvidenceInspectionFailure::ArtifactSyncFailed,
            RunEvidenceInspectionFailure::ManifestSyncFailed,
            RunEvidenceInspectionFailure::DirectorySyncFailed,
        ]
        .into_iter()
        .enumerate()
        {
            // A persistent failure is never promoted by an earlier successful read.
            for _ in 0..2 {
                let mut called = Vec::new();
                let inspected = inspect_with_sync(&held, &mut |point, file| {
                    called.push(point);
                    if point == points[index] {
                        return Err(io::Error::from_raw_os_error(libc::EIO));
                    }
                    file.sync_all()
                });
                assert_eq!(inspected, RunEvidenceInspection::NoResult(failure));
                assert_eq!(called, points[..=index]);
                assert!(
                    directory.path().join(RUN_EVIDENCE_MANIFEST).exists(),
                    "this is a post-publication failure, not missing setup"
                );
            }
        }
        let mut called = Vec::new();
        assert!(matches!(
            inspect_with_sync(&held, &mut |point, file| {
                called.push(point);
                file.sync_all()
            }),
            RunEvidenceInspection::Complete(_)
        ));
        assert_eq!(called, points);
        assert!(matches!(
            inspect_run_evidence(directory.path()),
            RunEvidenceInspection::Complete(_)
        ));
    }

    #[test]
    fn reader_lock_refuses_live_publication_without_locking_other_directories() {
        let directory = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let log = valid_log();
        for path in [directory.path(), other.path()] {
            write_complete_fixture(path, &log, detcore::Digest::new(&log).to_string(), 2);
        }
        let publisher = open_evidence_directory(directory.path()).unwrap();
        assert_eq!(
            unsafe { libc::flock(publisher.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        for _ in 0..2 {
            assert_eq!(
                inspect_run_evidence(directory.path()),
                RunEvidenceInspection::NoResult(
                    RunEvidenceInspectionFailure::PublicationInProgress
                )
            );
            assert!(matches!(
                inspect_run_evidence(other.path()),
                RunEvidenceInspection::Complete(_)
            ));
        }
        // The inspector must not downgrade the publisher's flock by cloning its OFD.
        assert_eq!(
            inspect_run_evidence_directory(&publisher),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::PublicationInProgress)
        );
        // A concurrent fork can retain this open description after our drop.
        // Keep a duplicate so that case is deterministic, and explicitly end
        // this fixture's publication before checking post-publication inspection.
        let inherited = publisher.try_clone().unwrap();
        assert_eq!(
            unsafe { libc::flock(publisher.as_raw_fd(), libc::LOCK_UN) },
            0
        );
        drop(publisher);
        assert!(matches!(
            inspect_run_evidence(directory.path()),
            RunEvidenceInspection::Complete(_)
        ));
        drop(inherited);
    }

    #[test]
    fn reader_refuses_replaced_file_identities_after_sync() {
        let log = valid_log();
        for name in [RUN_EVIDENCE_MANIFEST, RUN_EVIDENCE_INFO_ARTIFACT] {
            let directory = tempfile::tempdir().unwrap();
            write_complete_fixture(
                directory.path(),
                &log,
                detcore::Digest::new(&log).to_string(),
                2,
            );
            let held = open_evidence_directory(directory.path()).unwrap();
            let mut replaced = false;
            let inspected = inspect_with_sync(&held, &mut |point, file| {
                file.sync_all()?;
                if point == InspectionSyncPoint::Directory {
                    let original = directory.path().join(name);
                    let saved = directory.path().join("old-inode");
                    fs::rename(&original, &saved)?;
                    fs::copy(&saved, &original)?;
                    replaced = true;
                }
                Ok(())
            });
            assert!(replaced);
            assert_eq!(
                inspected,
                RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::IdentityChanged)
            );
            // The bytes were identical; a fresh independently synchronized snapshot works.
            assert!(matches!(
                inspect_run_evidence(directory.path()),
                RunEvidenceInspection::Complete(_)
            ));
        }
    }
    fn guest_result_fixture(disposition: GuestDisposition) -> GuestRunResult {
        GuestRunResult {
            schema_version: GuestRunResult::SCHEMA_VERSION,
            disposition,
            determinism: GuestRunDeterminism {
                detlog_io_buffers: true,
                virtualize_time: true,
            },
            stdout: CapturedGuestStream {
                bytes: 3,
                sha256: detcore::Digest::new(b"out").to_string(),
                identity: RunEvidenceFileIdentity {
                    device: 7,
                    inode: 1,
                },
            },
            stderr: CapturedGuestStream {
                bytes: 3,
                sha256: detcore::Digest::new(b"err").to_string(),
                identity: RunEvidenceFileIdentity {
                    device: 7,
                    inode: 2,
                },
            },
        }
    }

    #[test]
    fn guest_result_preserves_valid_linux_dispositions_and_refuses_invalid_neighbors() {
        for code in [0, 23, 255] {
            for disposition in [
                GuestDisposition::Exited { code },
                GuestDisposition::ExitCodeOnly {
                    code,
                    limitation: DispositionLimitation::KvmExitCodeOnly,
                },
            ] {
                let expected = guest_result_fixture(disposition);
                assert_eq!(
                    GuestRunResult::from_current_json_slice(
                        &serde_json::to_vec(&expected).unwrap()
                    ),
                    Ok(expected),
                );
            }
        }
        for signal in [1, 9, 64] {
            for core_dumped in [false, true] {
                let expected = guest_result_fixture(GuestDisposition::Signaled {
                    signal,
                    core_dumped,
                });
                assert_eq!(
                    GuestRunResult::from_current_json_slice(
                        &serde_json::to_vec(&expected).unwrap()
                    ),
                    Ok(expected),
                );
            }
        }
        for code in [-1, 256, i32::MAX, i32::MIN] {
            for disposition in [
                GuestDisposition::Exited { code },
                GuestDisposition::ExitCodeOnly {
                    code,
                    limitation: DispositionLimitation::KvmExitCodeOnly,
                },
            ] {
                let bytes = serde_json::to_vec(&guest_result_fixture(disposition)).unwrap();
                assert!(
                    GuestRunResult::from_current_json_slice(&bytes)
                        .unwrap_err()
                        .contains("invalid Linux disposition")
                );
            }
        }
        for signal in [-1, 0, 65, i32::MAX] {
            let bytes = serde_json::to_vec(&guest_result_fixture(GuestDisposition::Signaled {
                signal,
                core_dumped: false,
            }))
            .unwrap();
            assert!(
                GuestRunResult::from_current_json_slice(&bytes)
                    .unwrap_err()
                    .contains("invalid Linux disposition")
            );
        }
    }

    #[test]
    fn guest_result_parses_original_bytes_and_refuses_duplicate_fields() {
        let expected = guest_result_fixture(GuestDisposition::Exited { code: 23 });
        let original = serde_json::to_string(&expected).unwrap();
        assert_eq!(
            GuestRunResult::from_current_json_slice(original.as_bytes()),
            Ok(expected)
        );
        for (field, value, conflicting) in [
            ("schema_version", "1", "2"),
            ("code", "23", "0"),
            ("detlog_io_buffers", "true", "false"),
            ("bytes", "3", "4"),
            ("inode", "1", "3"),
        ] {
            for duplicate in [value, conflicting] {
                let needle = format!("\"{field}\":{value}");
                let replacement = format!("{needle},\"{field}\":{duplicate}");
                let malformed = original.replacen(&needle, &replacement, 1);
                assert_ne!(malformed, original, "fixture did not replace {field}");
                let error =
                    GuestRunResult::from_current_json_slice(malformed.as_bytes()).unwrap_err();
                assert!(error.contains("duplicate field"), "{field}: {error}");
            }
        }
    }

    #[test]
    fn guest_result_refuses_missing_unknown_and_aliased_stream_metadata() {
        let expected = guest_result_fixture(GuestDisposition::Exited { code: 23 });
        let original = serde_json::to_value(&expected).unwrap();
        for field in [
            "schema_version",
            "disposition",
            "determinism",
            "stdout",
            "stderr",
        ] {
            let mut malformed = original.clone();
            assert!(malformed.as_object_mut().unwrap().remove(field).is_some());
            assert!(
                GuestRunResult::from_current_json_slice(&serde_json::to_vec(&malformed).unwrap())
                    .unwrap_err()
                    .contains("missing field")
            );
        }
        for nested in [None, Some("stdout"), Some("determinism")] {
            let mut malformed = original.clone();
            let object = match nested {
                None => &mut malformed,
                Some(name) => &mut malformed[name],
            };
            assert!(
                object
                    .as_object_mut()
                    .unwrap()
                    .insert("unknown".into(), serde_json::json!(true))
                    .is_none()
            );
            assert!(
                GuestRunResult::from_current_json_slice(&serde_json::to_vec(&malformed).unwrap())
                    .unwrap_err()
                    .contains("unknown field")
            );
        }
        let mut aliased = expected.clone();
        aliased.stderr.identity = aliased.stdout.identity;
        assert!(
            GuestRunResult::from_current_json_slice(&serde_json::to_vec(&aliased).unwrap())
                .unwrap_err()
                .contains("distinct inodes")
        );
        let mut invalid_digest = expected.clone();
        invalid_digest.stdout.sha256 = "not-a-digest".into();
        assert!(
            GuestRunResult::from_current_json_slice(&serde_json::to_vec(&invalid_digest).unwrap())
                .unwrap_err()
                .contains("lowercase hex")
        );
        let mut invalid_inode = expected;
        invalid_inode.stderr.identity.inode = 0;
        assert!(
            GuestRunResult::from_current_json_slice(&serde_json::to_vec(&invalid_inode).unwrap())
                .unwrap_err()
                .contains("inode must be nonzero")
        );
    }

    #[test]
    fn guest_result_refuses_fields_from_other_disposition_variants() {
        for (disposition, extra_field, extra_value) in [
            (GuestDisposition::Exited { code: 23 }, "signal", 9),
            (
                GuestDisposition::Signaled {
                    signal: 9,
                    core_dumped: false,
                },
                "code",
                23,
            ),
            (
                GuestDisposition::ExitCodeOnly {
                    code: 23,
                    limitation: DispositionLimitation::KvmExitCodeOnly,
                },
                "signal",
                9,
            ),
        ] {
            let expected = guest_result_fixture(disposition);
            let mut value = serde_json::to_value(&expected).unwrap();
            assert_eq!(
                GuestRunResult::from_current_json_slice(&serde_json::to_vec(&value).unwrap()),
                Ok(expected)
            );
            assert!(
                value["disposition"]
                    .as_object_mut()
                    .unwrap()
                    .insert(extra_field.into(), serde_json::json!(extra_value))
                    .is_none()
            );
            let error =
                GuestRunResult::from_current_json_slice(&serde_json::to_vec(&value).unwrap())
                    .unwrap_err();
            assert!(error.contains("unknown field"), "{disposition:?}: {error}");
            assert!(error.contains(extra_field), "{disposition:?}: {error}");
        }
    }

    #[test]
    fn loader_returns_the_exact_synchronized_bytes_and_original_inode() {
        use std::os::unix::fs::MetadataExt;

        let directory = tempfile::tempdir().unwrap();
        let log = valid_log();
        write_complete_fixture(
            directory.path(),
            &log,
            detcore::Digest::new(&log).to_string(),
            2,
        );
        let artifact = directory.path().join(RUN_EVIDENCE_INFO_ARTIFACT);
        let original = fs::metadata(&artifact).unwrap();
        let loaded = load_run_evidence(directory.path()).unwrap();
        assert_eq!(loaded.canonical_info, log);
        assert_eq!(
            loaded.artifact_identity,
            RunEvidenceFileIdentity {
                device: original.dev(),
                inode: original.ino(),
            }
        );
        assert_eq!(
            loaded.report.canonical_info.sha256.as_deref(),
            Some(
                detcore::Digest::new(&loaded.canonical_info)
                    .to_string()
                    .as_str()
            )
        );
        fs::rename(&artifact, directory.path().join("held-original")).unwrap();
        fs::write(&artifact, b"replacement bytes\n").unwrap();
        assert_eq!(
            loaded.canonical_info, log,
            "the returned snapshot must not reopen a path"
        );
        assert_ne!(
            fs::metadata(&artifact).unwrap().ino(),
            loaded.artifact_identity.inode
        );
        assert!(load_run_evidence(directory.path()).is_err());
    }

    #[test]
    fn loader_refuses_live_publication_and_each_failed_sync() {
        let directory = tempfile::tempdir().unwrap();
        let log = valid_log();
        write_complete_fixture(
            directory.path(),
            &log,
            detcore::Digest::new(&log).to_string(),
            2,
        );
        let held = open_evidence_directory(directory.path()).unwrap();
        assert_eq!(
            unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        assert_eq!(
            load_run_evidence(directory.path()),
            Err(RunEvidenceInspectionFailure::PublicationInProgress)
        );
        // A concurrent fork can retain this open description after our drop.
        // Keep a duplicate so that case is deterministic, and explicitly end
        // this fixture's publication before checking post-publication inspection.
        let inherited = held.try_clone().unwrap();
        assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_UN) }, 0);
        drop(held);
        let held = open_evidence_directory(directory.path()).unwrap();
        for (failed, expected) in [
            (
                InspectionSyncPoint::Artifact,
                RunEvidenceInspectionFailure::ArtifactSyncFailed,
            ),
            (
                InspectionSyncPoint::Manifest,
                RunEvidenceInspectionFailure::ManifestSyncFailed,
            ),
            (
                InspectionSyncPoint::Directory,
                RunEvidenceInspectionFailure::DirectorySyncFailed,
            ),
        ] {
            let mut reached = false;
            let loaded = load_with_sync(&held, &mut |point, file| {
                if point == failed {
                    reached = true;
                    Err(io::Error::from_raw_os_error(libc::EIO))
                } else {
                    file.sync_all()
                }
            });
            assert!(reached, "sync point {failed:?} was not reached: {loaded:?}");
            assert_eq!(loaded, Err(expected));
        }
        let mut synchronized = Vec::new();
        let loaded = load_with_sync(&held, &mut |point, file| {
            synchronized.push(point);
            file.sync_all()
        })
        .unwrap();
        assert_eq!(
            synchronized,
            [
                InspectionSyncPoint::Artifact,
                InspectionSyncPoint::Manifest,
                InspectionSyncPoint::Directory
            ]
        );
        assert_eq!(loaded.canonical_info, log);
        drop(inherited);
    }

    #[test]
    fn bounded_loader_refuses_oversize_manifest_and_artifact_inputs() {
        let directory = tempfile::tempdir().unwrap();
        let log = valid_log();
        write_complete_fixture(
            directory.path(),
            &log,
            detcore::Digest::new(&log).to_string(),
            2,
        );
        let held = open_evidence_directory(directory.path()).unwrap();
        // Exercise the actual fstat-before-read guard with a small limit, so
        // this control does not need a gigabyte file or weaker native FSIZE bound.
        assert!(matches!(
            read_regular_child(
                &held,
                OsStr::new(RUN_EVIDENCE_INFO_ARTIFACT),
                None,
                log.len() as u64 - 1
            ),
            Err(RegularChildReadFailure::TooLarge)
        ));
        assert_eq!(
            read_regular_child(
                &held,
                OsStr::new(RUN_EVIDENCE_INFO_ARTIFACT),
                None,
                log.len() as u64
            )
            .unwrap()
            .bytes,
            log
        );
        let manifest = directory.path().join(RUN_EVIDENCE_MANIFEST);
        let mut report: RunEvidenceReport =
            serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
        report.canonical_info.byte_count = RUN_EVIDENCE_INFO_MAX_BYTES + 1;
        fs::set_permissions(&manifest, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&manifest, serde_json::to_vec(&report).unwrap()).unwrap();
        fs::set_permissions(
            &manifest,
            fs::Permissions::from_mode(RUN_EVIDENCE_MANIFEST_MODE),
        )
        .unwrap();
        assert_eq!(
            load_run_evidence(directory.path()),
            Err(RunEvidenceInspectionFailure::ArtifactTooLarge)
        );
        fs::set_permissions(&manifest, fs::Permissions::from_mode(0o600)).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&manifest)
            .unwrap()
            .set_len(RUN_EVIDENCE_MANIFEST_MAX_BYTES + 1)
            .unwrap();
        fs::set_permissions(
            &manifest,
            fs::Permissions::from_mode(RUN_EVIDENCE_MANIFEST_MODE),
        )
        .unwrap();
        assert_eq!(
            load_run_evidence(directory.path()),
            Err(RunEvidenceInspectionFailure::ManifestTooLarge)
        );
    }
}
