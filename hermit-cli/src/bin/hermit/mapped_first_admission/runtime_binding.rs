//! Retained private-static package DATA. No source qualification or Ready.
//! The unsafe entry caller still owns its exact artifact/ABI obligations.
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;

use reverie_loader::artifact::TOOL_REGION;
use reverie_loader::handoff::Extent;
use serde::Deserialize;
use sha2::Digest;
use sha2::Sha256;

use super::super::verify_inputs::MappedPackageMeta;
use super::super::verify_inputs::RetainedPackageFileMeta;
use super::runtime_support::ObservedImage;
use super::runtime_support::PAGE;
use super::runtime_support::ProposedPlacement;
use super::runtime_support::checked_image;
use super::runtime_support::exact_rx;
use super::runtime_support::rpc;

const SCHEMA: &str = "hermit.private-static-runtime-data.v1";
const ELF_RESOURCE: &str = "detcore_private_runtime.elf";
const DATA_RESOURCE: &str = "detcore_private_runtime.package.json";
const MAX_IMAGE: u64 = 512 * 1024 * 1024;
const MAX_DATA: u64 = 1024 * 1024;

// These values describe bytes. Deserializing them never creates a source owner,
// initialized admission, executable qualification, or permission to enter.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PackageData {
    schema: String,
    image: ImageData,
    entry: u64,
    terminal_int3: u64,
    fallback_entry: u64,
    completion_syscall: u64,
    observed: ObservedData,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageData {
    bytes: u64,
    sha256: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ObservedData {
    layout_address: u64,
    reference_address: u64,
    handler_address: u64,
    handler_bytes: u64,
    handler_sha256: String,
    dev_null_address: u64,
    sites: [[u64; 4]; 4],
    ordinary_post_ips: Vec<u64>,
}
fn digest(value: &str) -> io::Result<[u8; 32]> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err(rpc(
            "exec-stop-package-invalid: SHA256 must be lowercase 64-digit hex",
        ));
    }
    let mut result = [0; 32];
    for (i, byte) in result.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[i * 2..i * 2 + 2], 16).map_err(rpc)?;
    }
    Ok(result)
}
fn parse_data(bytes: &[u8]) -> io::Result<PackageData> {
    if bytes.is_empty() || bytes.len() as u64 > MAX_DATA {
        return Err(rpc("exec-stop-package-invalid: descriptor bound"));
    }
    // Existing serde JSON parser, with its normal recursion limit. No new wire
    // codec, ELF parser, unknown-field fallback or compatibility defaults.
    let data: PackageData = serde_json::from_slice(bytes).map_err(rpc)?;
    if data.schema != SCHEMA || data.image.bytes == 0 || data.image.bytes > MAX_IMAGE {
        return Err(rpc("exec-stop-package-invalid: schema or image bound"));
    }
    digest(&data.image.sha256)?;
    digest(&data.observed.handler_sha256)?;
    if data.observed.handler_bytes == 0
        || data.observed.handler_bytes > 65536
        || data.observed.ordinary_post_ips.is_empty()
        || data.observed.ordinary_post_ips.len() > 4096
        || !data
            .observed
            .ordinary_post_ips
            .windows(2)
            .all(|pair| pair[0] < pair[1])
    {
        return Err(rpc("exec-stop-package-invalid: observed bounds/order"));
    }
    Ok(data)
}
fn package_paths() -> io::Result<(PathBuf, PathBuf)> {
    let image = hermit_resources::resource(ELF_RESOURCE)?
        .ok_or_else(|| rpc("exec-stop-package-absent: private runtime resources unavailable"))?;
    let data = hermit_resources::resource(DATA_RESOURCE)?
        .ok_or_else(|| rpc("exec-stop-package-absent: private runtime resources unavailable"))?;
    Ok((image, data))
}
pub(crate) fn declared() -> bool {
    // Availability only; open authenticates both resources before use. A package
    // descriptor is not a Ready bit and cannot discharge unsafe entry contracts.
    package_paths().is_ok_and(|(image, data)| image.is_file() && data.is_file())
}
struct RetainedFile {
    file: File,
    bytes: Vec<u8>,
    state: [u64; 8],
}
fn state(file: &File) -> io::Result<[u64; 8]> {
    let m = file.metadata()?;
    if !m.is_file() {
        return Err(rpc("mapped package input is not a regular file"));
    }
    // The container maps the invoking effective UID to zero. Authenticate
    // ownership in each namespace instead of comparing the two numeric UIDs.
    // SAFETY: geteuid has no pointer arguments and only reads caller identity.
    if m.uid() != unsafe { libc::geteuid() } {
        return Err(rpc(
            "mapped package input is not owned by the invoking effective user",
        ));
    }
    Ok([
        m.dev(),
        m.ino(),
        m.len(),
        m.mode() as u64,
        m.mtime() as u64,
        m.mtime_nsec() as u64,
        m.ctime() as u64,
        m.ctime_nsec() as u64,
    ])
}
impl RetainedFile {
    fn open(path: &Path, limit: u64) -> io::Result<Self> {
        // Nonblocking open lets non-regular resource types reach the metadata
        // refusal instead of blocking before the owned-regular-file check.
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(path)
            .map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    rpc("exec-stop-package-absent: required private runtime resource missing")
                } else {
                    error
                }
            })?;
        let before = state(&file)?;
        // Check actual open-file length before allocating, for BOTH resources.
        if before[2] == 0 || before[2] > limit {
            return Err(rpc("exec-stop-package-invalid: input file bound"));
        }
        let length = usize::try_from(before[2]).map_err(rpc)?;
        let mut bytes = vec![0; length];
        file.read_exact_at(&mut bytes, 0)?;
        if state(&file)? != before {
            return Err(rpc(
                "mapped package actual file identity changed during read",
            ));
        }
        Ok(Self {
            file,
            bytes,
            state: before,
        })
    }
    fn pair_meta(&self) -> RetainedPackageFileMeta {
        RetainedPackageFileMeta {
            state: self.state,
            sha256: format!("{:x}", Sha256::digest(&self.bytes)),
        }
    }
    fn adopt_pair(file: File, expected: RetainedPackageFileMeta, limit: u64) -> io::Result<Self> {
        // The exporter duplicates the original read-only OFD. Never reopen a
        // source path or substitute a sealed copy for that physical identity.
        // SAFETY: F_GETFL only queries an owned, live descriptor.
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if flags & libc::O_ACCMODE != libc::O_RDONLY {
            return Err(rpc("retained mapped package descriptor is not read-only"));
        }
        // Keep the owner safe even if a direct caller did not set CLOEXEC.
        // SAFETY: descriptor-only flag operations on the owned file.
        let descriptor_flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
        if descriptor_flags < 0
            || unsafe {
                libc::fcntl(
                    file.as_raw_fd(),
                    libc::F_SETFD,
                    descriptor_flags | libc::FD_CLOEXEC,
                )
            } < 0
        {
            return Err(io::Error::last_os_error());
        }
        let expected_digest = digest(&expected.sha256)?;
        let before = state(&file)?;
        if before != expected.state || before[2] == 0 || before[2] > limit {
            return Err(rpc(
                "retained mapped package identity/bound differs from holder",
            ));
        }
        let mut bytes = vec![0; usize::try_from(before[2]).map_err(rpc)?];
        file.read_exact_at(&mut bytes, 0)?;
        if state(&file)? != before || <[u8; 32]>::from(Sha256::digest(&bytes)) != expected_digest {
            return Err(rpc("retained mapped package bytes differ from holder"));
        }
        Ok(Self {
            file,
            bytes,
            state: before,
        })
    }
    fn verify(&self) -> io::Result<()> {
        let actual = state(&self.file)?;
        if actual != self.state {
            return Err(rpc("retained mapped package identity changed"));
        }
        let mut bytes = vec![0; self.bytes.len()];
        self.file.read_exact_at(&mut bytes, 0)?;
        if bytes != self.bytes || state(&self.file)? != self.state {
            return Err(rpc("retained mapped package bytes changed"));
        }
        Ok(())
    }
}
pub(crate) struct RetainedImage {
    image: RetainedFile,
    descriptor: RetainedFile,
    observed: ObservedImage,
    entry: u64,
    terminal_int3: u64,
    #[cfg(feature = "loader-rcb-observation")]
    fallback_entry: u64,
    #[cfg(feature = "loader-rcb-observation")]
    completion_syscall: u64,
}
impl std::fmt::Debug for RetainedImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RetainedImage(<retained package DATA>)")
    }
}
impl RetainedImage {
    pub(crate) fn open() -> io::Result<Self> {
        let (image, descriptor) = package_paths()?;
        Self::open_paths(&image, &descriptor)
    }
    /// Required, test-scoped paired resource input. This reuses the actual
    /// retained package parser and OFDs without changing process-wide discovery.
    /// The caller's package gate owns source/entry binding; these remain DATA.
    #[cfg(test)]
    pub(crate) fn open_test_install(install: &Path) -> io::Result<Self> {
        let resources = install.join("rsrcs");
        Self::open_paths(
            &resources.join(ELF_RESOURCE),
            &resources.join(DATA_RESOURCE),
        )
    }
    fn open_paths(image_path: &Path, descriptor_path: &Path) -> io::Result<Self> {
        let descriptor = RetainedFile::open(descriptor_path, MAX_DATA)?;
        let data = parse_data(&descriptor.bytes)?;
        let image = RetainedFile::open(image_path, MAX_IMAGE)?;
        Self::from_retained(image, descriptor, data)
    }
    /// Export the SAME original regular files for the existing pair holder.
    /// Metadata and bytes remain DATA; this does not qualify entry or source.
    pub(crate) fn export_pair_inputs(&self) -> io::Result<(MappedPackageMeta, [OwnedFd; 2])> {
        self.verify()?;
        let meta = MappedPackageMeta {
            image: self.image.pair_meta(),
            descriptor: self.descriptor.pair_meta(),
        };
        let files = [
            self.image.file.try_clone()?.into(),
            self.descriptor.file.try_clone()?.into(),
        ];
        self.verify()?;
        Ok((meta, files))
    }
    /// Adopt original inherited OFDs, checked against the sealed holder record.
    /// All package parsing/ELF/layout checks are the same as ordinary opening.
    pub(crate) fn adopt_pair_inputs(
        meta: MappedPackageMeta,
        files: [OwnedFd; 2],
    ) -> io::Result<Self> {
        let [image, descriptor] = files;
        let descriptor = RetainedFile::adopt_pair(descriptor.into(), meta.descriptor, MAX_DATA)?;
        let data = parse_data(&descriptor.bytes)?;
        let image = RetainedFile::adopt_pair(image.into(), meta.image, MAX_IMAGE)?;
        Self::from_retained(image, descriptor, data)
    }
    /// Only descriptor-retirement custody, not a second image owner.
    pub(crate) fn pair_fds(&self) -> [BorrowedFd<'_>; 2] {
        [self.image.file.as_fd(), self.descriptor.file.as_fd()]
    }
    fn from_retained(
        image: RetainedFile,
        descriptor: RetainedFile,
        data: PackageData,
    ) -> io::Result<Self> {
        let image_sha256 = digest(&data.image.sha256)?;
        if image.state[2] != data.image.bytes
            || <[u8; 32]>::from(Sha256::digest(&image.bytes)) != image_sha256
        {
            return Err(rpc(
                "exec-stop-package-invalid: actual image length/SHA256 differs",
            ));
        }
        let observed = ObservedImage {
            image_sha256,
            layout_address: data.observed.layout_address,
            reference_address: data.observed.reference_address,
            handler_address: data.observed.handler_address,
            handler_bytes: usize::try_from(data.observed.handler_bytes).map_err(rpc)?,
            handler_sha256: digest(&data.observed.handler_sha256)?,
            dev_null_address: data.observed.dev_null_address,
            sites: data.observed.sites,
            ordinary_post_ips: data.observed.ordinary_post_ips,
        };
        // Preserve the existing loader and exact layout/reference predicates.
        // Remaining handler/site/role checks still run unchanged in wire build.
        let (validated, _) = checked_image(&image.bytes, &observed).map_err(rpc)?;
        if validated.entry() != data.entry
            || exact_rx(&validated, data.terminal_int3, 1).map_err(rpc)? != [0xcc]
            || data.fallback_entry >= data.completion_syscall
            || exact_rx(&validated, data.fallback_entry, 1).is_err()
            || exact_rx(&validated, data.completion_syscall, 4).map_err(rpc)?
                != [0x0f, 0x05, 0x0f, 0x0b]
        {
            return Err(rpc(
                "exec-stop-package-invalid: entry/terminal/observer coordinates",
            ));
        }
        descriptor.verify()?;
        image.verify()?;
        Ok(Self {
            image,
            descriptor,
            observed,
            entry: data.entry,
            terminal_int3: data.terminal_int3,
            #[cfg(feature = "loader-rcb-observation")]
            fallback_entry: data.fallback_entry,
            #[cfg(feature = "loader-rcb-observation")]
            completion_syscall: data.completion_syscall,
        })
    }
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.image.bytes
    }
    pub(crate) fn observed(&self) -> &ObservedImage {
        &self.observed
    }
    pub(crate) fn entry(&self) -> u64 {
        self.entry
    }
    // These are measured package DATA. The unsafe observer separately checks
    // the actual retained stop/bytes/roles; normal handover needs no observer.
    #[cfg(feature = "loader-rcb-observation")]
    pub(crate) fn rcb_observation_coordinates(
        &self,
    ) -> reverie_inguest::launcher::exec_stop::physical::RcbObservationCoordinates {
        reverie_inguest::launcher::exec_stop::physical::RcbObservationCoordinates {
            fallback_entry: self.fallback_entry,
            completion_syscall: self.completion_syscall,
        }
    }
    pub(crate) fn terminal_int3(&self) -> u64 {
        self.terminal_int3
    }
    pub(crate) fn placement(&self) -> io::Result<ProposedPlacement> {
        placement()
    }
    pub(crate) fn verify(&self) -> io::Result<()> {
        // Both exact opened OFDs/byte snapshots survive the complete container
        // and cleanup lifetime. Do not reopen either path after namespace entry.
        self.descriptor.verify()?;
        self.image.verify()?;
        self.descriptor.verify()
    }
}

fn placement() -> io::Result<ProposedPlacement> {
    // Existing shared free band DATA_FIRST=1MiB/PAGE, TOOL_BAND.0=65535.
    // These are fallible proposed roles, not observed mappings or initialized
    // arena ownership. Actual copied-input and complete shared validation, then
    // the unchanged no-replace injector, check every role and planned map.
    let first = TOOL_REGION
        .start
        .checked_add(1024 * 1024)
        .ok_or_else(|| rpc("role start overflow"))?;
    let last = TOOL_REGION
        .start
        .checked_add(65535 * PAGE)
        .ok_or_else(|| rpc("free band overflow"))?;
    let mut cursor = first;
    let mut roles = [Extent { start: 0, end: 0 }; 5];
    // Stack, TLS, bootstrap signal stack, handoff, scalar state; guards included.
    for (role, pages) in roles.iter_mut().zip([256_u64, 64, 256, 256, 256]) {
        let bytes = pages
            .checked_mul(PAGE)
            .ok_or_else(|| rpc("role bytes overflow"))?;
        let end = cursor
            .checked_add(bytes)
            .ok_or_else(|| rpc("role end overflow"))?;
        if end > last {
            return Err(rpc("proposed roles exceed shared free band"));
        }
        *role = Extent { start: cursor, end };
        cursor = end;
    }
    let filter_scratch = roles[4]
        .start
        .checked_add(8 * PAGE)
        .ok_or_else(|| rpc("filter staging overflow"))?;
    Ok(ProposedPlacement {
        roles,
        free_band: Extent {
            start: first,
            end: last,
        },
        filter_scratch,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn data() -> serde_json::Value {
        // Numeric schema DATA only, not a genuine runtime or source positive.
        json!({"schema":SCHEMA,"image":{"bytes":4096,"sha256":"11".repeat(32)},
            "entry":1,"terminal_int3":2,"fallback_entry":3,"completion_syscall":4,
            "observed":{"layout_address":5,"reference_address":6,"handler_address":7,
                "handler_bytes":8,"handler_sha256":"22".repeat(32),"dev_null_address":9,
                "sites":[[10,12,158,1],[14,16,218,2],[18,20,7,3],[22,24,2,4]],
                "ordinary_post_ips":[12,16,20,24]}})
    }
    fn parse(value: &serde_json::Value) -> io::Result<PackageData> {
        parse_data(&serde_json::to_vec(value).unwrap())
    }
    #[test]
    fn package_data_is_explicit_not_readiness() {
        let value = parse(&data()).unwrap();
        assert_eq!(value.image.bytes, 4096);
        assert_eq!(digest(&value.image.sha256).unwrap(), [0x11; 32]);
        assert_eq!(value.observed.sites.len(), 4);
    }
    #[test]
    fn package_missing_unknown_duplicate_and_old_schema_refuse() {
        let mut missing = data();
        missing.as_object_mut().unwrap().remove("fallback_entry");
        assert!(parse(&missing).is_err());
        let mut extra = data();
        extra["ready"] = json!(true);
        assert!(parse(&extra).is_err());
        let mut old = data();
        old["schema"] = json!("hermit.private-static-runtime-data.v0");
        assert!(parse(&old).is_err());
        let encoded = serde_json::to_string(&data()).unwrap();
        let duplicate = format!("{{\"entry\":1,{}", &encoded[1..]);
        assert!(parse_data(duplicate.as_bytes()).is_err());
        let mut nested = data();
        nested["image"]["authority"] = json!(true);
        assert!(parse(&nested).is_err());
    }
    #[test]
    fn package_digest_bounds_and_site_order_refuse() {
        for hash in ["00".repeat(31), "AA".repeat(32), "z0".repeat(32)] {
            let mut bad = data();
            bad["image"]["sha256"] = json!(hash);
            assert!(parse(&bad).is_err());
        }
        let mut bad = data();
        bad["image"]["bytes"] = json!(MAX_IMAGE + 1);
        assert!(parse(&bad).is_err());
        let mut bad = data();
        bad["observed"]["ordinary_post_ips"] = json!([12, 12]);
        assert!(parse(&bad).is_err());
        let mut bad = data();
        bad["observed"]["ordinary_post_ips"] = json!([]);
        assert!(parse(&bad).is_err());
        assert!(parse_data(&vec![b' '; MAX_DATA as usize + 1]).is_err());
    }
    #[test]
    fn pair_file_adoption_keeps_original_identity_offset_and_both_consumers() {
        use std::io::Seek;
        use std::io::SeekFrom;
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        std::fs::create_dir(&source).unwrap();
        let path = source.join("image");
        std::fs::write(&path, b"original file DATA").unwrap();
        let retained = RetainedFile::open(&path, MAX_IMAGE).unwrap();
        let meta = retained.pair_meta();
        let mut offset = retained.file.try_clone().unwrap();
        offset.seek(SeekFrom::Start(3)).unwrap();
        // Moving the parent directory leaves the held file unchanged. Both
        // consumers must work while the old path is absent, and after a
        // different inode appears there; neither case permits reopening it.
        std::fs::rename(&source, directory.path().join("held-directory")).unwrap();
        for replacement in [false, true] {
            if replacement {
                std::fs::create_dir(&source).unwrap();
                std::fs::write(&path, b"replacement bytes").unwrap();
            } else {
                assert!(!path.exists());
            }
            retained.verify().unwrap();
            for _ in 0..2 {
                let adopted = RetainedFile::adopt_pair(
                    retained.file.try_clone().unwrap(),
                    meta.clone(),
                    MAX_IMAGE,
                )
                .unwrap();
                assert_eq!(adopted.state, retained.state);
                assert_eq!(adopted.bytes, b"original file DATA");
                assert_eq!(offset.stream_position().unwrap(), 3);
                adopted.verify().unwrap();
                assert_ne!(
                    unsafe { libc::fcntl(adopted.file.as_raw_fd(), libc::F_GETFD) }
                        & libc::FD_CLOEXEC,
                    0
                );
            }
        }
        assert!(RetainedFile::adopt_pair(File::open(&path).unwrap(), meta, MAX_IMAGE).is_err());
    }
    #[test]
    fn pair_file_moved_parent_does_not_hide_original_inode_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        std::fs::create_dir(&source).unwrap();
        let path = source.join("image");
        std::fs::write(&path, b"original file DATA").unwrap();
        let writer = OpenOptions::new().write(true).open(&path).unwrap();
        let retained = RetainedFile::open(&path, MAX_IMAGE).unwrap();
        let meta = retained.pair_meta();
        let adopted =
            RetainedFile::adopt_pair(retained.file.try_clone().unwrap(), meta.clone(), MAX_IMAGE)
                .unwrap();
        std::fs::rename(&source, directory.path().join("held-directory")).unwrap();
        assert!(!path.exists());
        retained.verify().unwrap();
        adopted.verify().unwrap();
        // Modify the original inode through its still-open writer, not a
        // replacement path. A retained owner is never a mutation exemption.
        writer.write_all_at(b"modified", 0).unwrap();
        assert!(retained.verify().is_err());
        assert!(adopted.verify().is_err());
        assert!(
            RetainedFile::adopt_pair(retained.file.try_clone().unwrap(), meta, MAX_IMAGE).is_err()
        );
    }
    #[test]
    fn pair_file_adoption_refuses_equal_bytes_on_another_inode_or_writable_fd() {
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("original");
        let substitute = directory.path().join("substitute");
        std::fs::write(&original, b"same bytes").unwrap();
        std::fs::write(&substitute, b"same bytes").unwrap();
        let retained = RetainedFile::open(&original, MAX_DATA).unwrap();
        assert!(
            RetainedFile::adopt_pair(
                File::open(&substitute).unwrap(),
                retained.pair_meta(),
                MAX_DATA
            )
            .is_err()
        );
        let writable = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&original)
            .unwrap();
        assert!(RetainedFile::adopt_pair(writable, retained.pair_meta(), MAX_DATA).is_err());
    }
    #[test]
    fn pair_file_checks_holder_digest_bounds_metadata_and_late_mutation() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("original");
        std::fs::write(&path, b"original DATA").unwrap();
        let retained = RetainedFile::open(&path, MAX_DATA).unwrap();
        let mut wrong_digest = retained.pair_meta();
        wrong_digest.sha256 = "00".repeat(32);
        assert!(
            RetainedFile::adopt_pair(retained.file.try_clone().unwrap(), wrong_digest, MAX_DATA)
                .is_err()
        );
        let mut wrong_identity = retained.pair_meta();
        wrong_identity.state[1] ^= 1;
        assert!(
            RetainedFile::adopt_pair(retained.file.try_clone().unwrap(), wrong_identity, MAX_DATA)
                .is_err()
        );
        let mut malformed = retained.pair_meta();
        malformed.sha256 = "AA".repeat(32);
        assert!(
            RetainedFile::adopt_pair(retained.file.try_clone().unwrap(), malformed, MAX_DATA)
                .is_err()
        );
        assert!(
            RetainedFile::adopt_pair(retained.file.try_clone().unwrap(), retained.pair_meta(), 1)
                .is_err()
        );
        let adopted = RetainedFile::adopt_pair(
            retained.file.try_clone().unwrap(),
            retained.pair_meta(),
            MAX_DATA,
        )
        .unwrap();
        // Same original inode, changed metadata: neither old nor adopted owner accepts it.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
        assert!(retained.verify().is_err());
        assert!(adopted.verify().is_err());
    }
    #[test]
    fn pair_file_bytes_changed_before_and_after_adoption_refuse() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("original");
        std::fs::write(&path, b"original DATA").unwrap();
        let writer = OpenOptions::new().write(true).open(&path).unwrap();
        let retained = RetainedFile::open(&path, MAX_DATA).unwrap();
        let meta = retained.pair_meta();
        let adopted =
            RetainedFile::adopt_pair(retained.file.try_clone().unwrap(), meta.clone(), MAX_DATA)
                .unwrap();
        writer.write_all_at(b"modified DATA", 0).unwrap();
        assert!(
            RetainedFile::adopt_pair(retained.file.try_clone().unwrap(), meta, MAX_DATA).is_err()
        );
        assert!(retained.verify().is_err());
        assert!(adopted.verify().is_err());
    }
    #[test]
    fn pair_image_adoption_still_requires_original_package_and_elf_validation() {
        let directory = tempfile::tempdir().unwrap();
        let image_path = directory.path().join("image");
        let descriptor_path = directory.path().join("descriptor");
        let bytes = [0; 4096];
        let mut descriptor = data();
        descriptor["image"]["sha256"] = json!(format!("{:x}", Sha256::digest(bytes)));
        std::fs::write(&image_path, bytes).unwrap();
        std::fs::write(&descriptor_path, serde_json::to_vec(&descriptor).unwrap()).unwrap();
        let image = RetainedFile::open(&image_path, MAX_IMAGE).unwrap();
        let descriptor = RetainedFile::open(&descriptor_path, MAX_DATA).unwrap();
        let meta = MappedPackageMeta {
            image: image.pair_meta(),
            descriptor: descriptor.pair_meta(),
        };
        // Matching original-file identities and hashes do NOT make a valid ELF.
        assert!(
            RetainedImage::adopt_pair_inputs(meta, [image.file.into(), descriptor.file.into()])
                .is_err()
        );
    }
    #[test]
    fn retained_descriptor_changes_are_not_reparsed_or_accepted() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        use std::io::Write;
        file.write_all(b"original descriptor DATA").unwrap();
        let retained = RetainedFile::open(file.path(), MAX_DATA).unwrap();
        retained.verify().unwrap();
        file.as_file()
            .write_all_at(b"modified descriptor DATA", 0)
            .unwrap();
        assert!(retained.verify().is_err());
    }
    #[test]
    fn package_missing_symlink_and_oversized_files_refuse_before_use() {
        let directory = tempfile::tempdir().unwrap();
        assert!(
            RetainedImage::open_paths(
                &directory.path().join("image"),
                &directory.path().join("data")
            )
            .is_err()
        );
        let target = directory.path().join("target");
        std::fs::write(&target, b"data").unwrap();
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(RetainedFile::open(&link, MAX_DATA).is_err());
        let oversized = File::create(directory.path().join("oversized")).unwrap();
        oversized.set_len(MAX_DATA + 1).unwrap();
        assert!(RetainedFile::open(&directory.path().join("oversized"), MAX_DATA).is_err());
    }
    #[test]
    fn stale_descriptor_cannot_select_different_image_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let image = directory.path().join("image");
        let descriptor = directory.path().join("data");
        // Refuses before ELF validation: this is an identity negative, not an ELF positive.
        std::fs::write(&image, [0; 4096]).unwrap();
        std::fs::write(&descriptor, serde_json::to_vec(&data()).unwrap()).unwrap();
        let error = RetainedImage::open_paths(&image, &descriptor)
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .contains("actual image length/SHA256 differs")
        );
    }
}
