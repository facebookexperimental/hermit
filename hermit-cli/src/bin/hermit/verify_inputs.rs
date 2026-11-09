/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The native-exec pairing seam: one sealed input set shared by both runs of
//! a verification pair.
//!
//! INTERNAL. `hermit verify-inputs` and `hermit run
//! --native-exec-shared-inputs` are hidden from help until a native-exec
//! consumer lands. The one user-visible verification form stays two `hermit
//! run --verify-run` processes compared by `hermit log-diff --verify-json`;
//! this module only lets those two processes start from the same sealed
//! inputs.
//!
//! A run that reads its inputs from the host filesystem cannot share them with
//! a second process: a file changed between the two runs gives them different
//! bytes, and comparing names or digests afterwards does not make them the
//! same objects. So the pair runs under the holder:
//!
//! ```text
//! hermit verify-inputs --manifest-fd N [--input ROLE:PATH]... -- PAIR-PROGRAM...
//! ```
//!
//! The holder seals every declared input once, each into its own memfd, writes
//! a manifest naming them into another sealed memfd at descriptor N, freezes
//! its stdin into a sealed memfd at descriptor 0, and execs the pair program
//! with all of them inherited. There is no daemon: the objects live exactly as
//! long as the pair program and the runs it starts, and close when it exits,
//! including after a first run that is refused. Each run is started with
//! `--native-exec-shared-inputs=N`, the same descriptor number in both, so
//! Hermit's command line, which its guest can read through /proc/1/cmdline,
//! stays identical. Hermit closes every descriptor nobody claimed as its first
//! act, so the run adopts the set at CLI entry, before that
//! ([`retire_inherited_keeping_shared_inputs`]): [`adopt`] checks every object
//! against the manifest and keeps the descriptors from reaching the guest.
//!
//! With `--in-guest-exec-stop`, this SAME holder retains the actual mapped
//! runtime image/package and adds the typed schema-3 envelope. An optional
//! native declaration remains on its existing capture path.
//! Each child adopts the inherited package through the existing RetainedImage;
//! no resource path is opened again and no second holder is started.
//!
//! Stdin is not part of the manifest. The pair program hands each run its own
//! open file description of the frozen bytes by reopening
//! `/proc/self/fd/0`, so every run reads them from the start whether the runs
//! execute one after the other or at once; a duplicated descriptor would share
//! one offset.

use std::fs::File;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use clap::Parser;
use serde::Deserialize;
use serde::Serialize;

/// The seals every shared object must carry: its bytes and size are fixed,
/// and nothing can change that.
pub(crate) const REQUIRED_SEALS: u32 =
    (libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE) as u32;

/// Descriptors the native child adapter reserves for its own roles inside each
/// run (stock loader, runner, initial packet, configuration, observer,
/// DETLOG). The manifest never names them.
const NATIVE_RESERVED_FDS: std::ops::RangeInclusive<RawFd> = 198..=204;

/// One sealed object of a shared native-exec input set, as the manifest
/// records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SharedObjectMeta {
    /// Original guest-visible path. An alias is its own entry. None is
    /// permitted only for an explicitly pathless role checked by its consuming
    /// owner; native config and declaration metadata have no guest path.
    pub(crate) path: Option<PathBuf>,
    /// The owner's role label, such as "executable", "library" or "group".
    pub(crate) role: String,
    pub(crate) size: u64,
    pub(crate) sha256: [u8; 32],
    /// The object's seals (`F_GET_SEALS`); always include [`REQUIRED_SEALS`].
    pub(crate) seals: u32,
}

/// An input set sealed once by the holder and adopted by each `--verify-run`.
pub(crate) trait SharedInputs: Sized {
    /// Holder side, once: every object as a sealed memfd with its metadata,
    /// and the producer identity a consumer must check.
    fn export(&self) -> Result<(String, Vec<(SharedObjectMeta, OwnedFd)>)>;

    /// Run side, per run: rebuild from the inherited objects. [`adopt`] has
    /// already checked each descriptor's seals, size and sha256 against the
    /// manifest; this must still check `producer` and its own invariants.
    fn adopt(producer: &str, objects: Vec<(SharedObjectMeta, OwnedFd)>) -> Result<Self>;
}

/// The manifest the holder writes and each run reads: paths, roles, sizes,
/// digests, seals and the producer identity, and the descriptor each object
/// is inherited at.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: u32,
    producer: String,
    objects: Vec<ManifestObject>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ManifestObject {
    path: ManifestPath,
    role: String,
    size: u64,
    /// Lower-case hex.
    sha256: String,
    seals: u32,
    fd: RawFd,
}

// Schema 1 keeps its original string paths. Schema 2 uses raw Unix bytes for
// every path in the same manifest; mixed encodings and unknown schemas refuse.
#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum ManifestPath {
    Utf8(String),
    UnixBytes(Option<Vec<u8>>),
}
impl ManifestPath {
    fn into_path(self, schema: u32) -> Result<Option<PathBuf>> {
        match (schema, self) {
            (1, Self::Utf8(path)) => Ok(Some(PathBuf::from(path))),
            (2, Self::UnixBytes(bytes)) => {
                Ok(bytes.map(|bytes| std::ffi::OsString::from_vec(bytes).into()))
            }
            _ => bail!("shared-input path encoding differs from manifest schema {schema}"),
        }
    }
    fn matches_schema(&self, schema: u32) -> bool {
        matches!((schema, self), (1, Self::Utf8(_)) | (2, Self::UnixBytes(_)))
    }
}
fn decode_manifest(bytes: &[u8]) -> Result<Manifest> {
    let manifest: Manifest =
        serde_json::from_slice(bytes).context("shared-input manifest is not versioned JSON")?;
    ensure!(
        [1, 2].contains(&manifest.schema),
        "unknown shared-input manifest schema"
    );
    ensure!(
        manifest
            .objects
            .iter()
            .all(|object| object.path.matches_schema(manifest.schema)),
        "shared-input path encoding differs from manifest schema"
    );
    Ok(manifest)
}

// Schema 3 is an envelope around an UNCHANGED schema-1/2 sealed input set.
// Its two fixed package members are original regular files, authenticated by
// RetainedImage's original identity/byte contract, never a seal exemption for
// an arbitrary role. These records confer no runtime/source qualification.
#[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RetainedPackageFileMeta {
    pub(crate) state: [u64; 8],
    pub(crate) sha256: String,
}
#[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MappedPackageMeta {
    pub(crate) image: RetainedPackageFileMeta,
    pub(crate) descriptor: RetainedPackageFileMeta,
}
#[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MappedPackageManifest {
    schema: u32,
    sealed: Manifest,
    package: MappedPackageMeta,
    image_fd: RawFd,
    descriptor_fd: RawFd,
}
#[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
fn decode_with_mapped_package(bytes: &[u8]) -> Result<(Manifest, Option<MappedPackageManifest>)> {
    // The final concrete decoder still denies unknown fields and schemas.
    // A schema probe changes no legacy schema or sealed-role validation.
    #[derive(Deserialize)]
    struct Version {
        schema: u32,
    }
    if serde_json::from_slice::<Version>(bytes)?.schema != 3 {
        return Ok((decode_manifest(bytes)?, None));
    }
    let mut envelope: MappedPackageManifest = serde_json::from_slice(bytes)?;
    ensure!(
        envelope.schema == 3,
        "unknown mapped package manifest schema"
    );
    let sealed = decode_manifest(&serde_json::to_vec(&envelope.sealed)?)?;
    // Move the sealed objects into the original adoption path once.
    envelope.sealed.objects.clear();
    Ok((sealed, Some(envelope)))
}
#[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
fn package_fd_set(
    package: &MappedPackageManifest,
    manifest_fd: RawFd,
) -> Result<std::collections::BTreeSet<RawFd>> {
    let mut seen = std::collections::BTreeSet::new();
    for fd in [package.image_fd, package.descriptor_fd] {
        ensure!(
            fd > 2 && fd != manifest_fd && !NATIVE_RESERVED_FDS.contains(&fd) && seen.insert(fd),
            "mapped package descriptor is reserved, aliased or duplicated"
        );
        // SAFETY: F_GETFD queries the descriptor before ownership is adopted.
        ensure!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0,
            "mapped package descriptor is closed"
        );
    }
    Ok(seen)
}

/// Plain sealed files, the set the holder seals from `--input ROLE:PATH`.
pub(crate) struct SealedFiles {
    objects: Vec<(SharedObjectMeta, OwnedFd)>,
}

impl SealedFiles {
    /// The producer identity of a plain sealed-file set.
    pub(crate) const PRODUCER: &'static str = "hermit-sealed-files";

    /// Seal each `(role, path)` once, reading the file at `path`.
    pub(crate) fn seal(inputs: &[(String, PathBuf)]) -> Result<Self> {
        let objects = inputs
            .iter()
            .map(|(role, path)| {
                let mut file = File::open(path)
                    .with_context(|| format!("cannot open shared input {}", path.display()))?;
                let (fd, size, sha256) = seal_into_memfd(c"hermit-shared-input", &mut file)
                    .with_context(|| format!("cannot seal shared input {}", path.display()))?;
                let meta = SharedObjectMeta {
                    path: Some(path.clone()),
                    role: role.clone(),
                    size,
                    sha256,
                    seals: seals_of(fd.as_raw_fd())?,
                };
                Ok((meta, fd))
            })
            .collect::<Result<_>>()?;
        Ok(Self { objects })
    }

    /// The adopted objects, for a consumer.
    #[allow(dead_code)] // The native-exec consumer is the first caller.
    pub(crate) fn objects(&self) -> &[(SharedObjectMeta, OwnedFd)] {
        &self.objects
    }
}

impl SharedInputs for SealedFiles {
    fn export(&self) -> Result<(String, Vec<(SharedObjectMeta, OwnedFd)>)> {
        let objects = self
            .objects
            .iter()
            .map(|(meta, fd)| Ok((meta.clone(), fd.try_clone()?)))
            .collect::<Result<_>>()?;
        Ok((Self::PRODUCER.into(), objects))
    }

    fn adopt(producer: &str, objects: Vec<(SharedObjectMeta, OwnedFd)>) -> Result<Self> {
        ensure!(
            producer == Self::PRODUCER,
            "shared inputs were produced by {producer:?}, not {:?}",
            Self::PRODUCER
        );
        ensure!(
            objects.iter().all(|(meta, _)| meta
                .path
                .as_ref()
                .is_some_and(|path| !path.as_os_str().is_empty())),
            "plain sealed files require original paths"
        );
        Ok(Self { objects })
    }
}

/// Create the unsealed owned memfd used by the existing sealed-input producer.
/// The caller owns filling and immutable sealing before any consumer reads it.
pub(super) fn new_unsealed_memfd(name: &std::ffi::CStr) -> Result<OwnedFd> {
    // SAFETY: memfd_create takes a valid C string and returns a new descriptor
    // or -1; nothing else owns it.
    let raw =
        unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error()).context("memfd_create");
    }
    // SAFETY: `raw` is the new descriptor memfd_create returned.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// Copy `source` into a new memfd and seal it, returning the memfd, its size
/// and its sha256. The memfd is close-on-exec until [`inherit`] clears that.
fn seal_into_memfd(
    name: &std::ffi::CStr,
    source: &mut impl Read,
) -> Result<(OwnedFd, u64, [u8; 32])> {
    let fd = new_unsealed_memfd(name)?;
    let raw = fd.as_raw_fd();
    let mut file = File::from(fd.try_clone()?);
    let size = std::io::copy(source, &mut file)?;
    // SAFETY: fcntl on a descriptor this function owns.
    if unsafe { libc::fcntl(raw, libc::F_ADD_SEALS, REQUIRED_SEALS as libc::c_int) } < 0 {
        return Err(std::io::Error::last_os_error()).context("F_ADD_SEALS");
    }
    // The digest is of the sealed object itself, not of what was read.
    let (sealed_size, sha256) = sha256_of_fd(raw)?;
    ensure!(sealed_size == size, "sealed {sealed_size} bytes of {size}");
    Ok((fd, size, sha256))
}

fn seals_of(fd: RawFd) -> Result<u32> {
    // SAFETY: fcntl only queries the descriptor.
    let seals = unsafe { libc::fcntl(fd, libc::F_GET_SEALS) };
    if seals < 0 {
        return Err(std::io::Error::last_os_error()).context("F_GET_SEALS");
    }
    Ok(seals as u32)
}

fn set_cloexec(fd: RawFd, cloexec: bool) -> Result<()> {
    let flags = if cloexec { libc::FD_CLOEXEC } else { 0 };
    // SAFETY: fcntl only changes the descriptor flag.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags) } < 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("descriptor {fd}"));
    }
    Ok(())
}

/// Move `fd` to descriptor `target`, inheritable across exec.
fn inherit_at(fd: OwnedFd, target: RawFd) -> Result<()> {
    if fd.as_raw_fd() != target {
        // SAFETY: dup2 onto a descriptor number this process chose; the
        // source stays owned by `fd` and closes when it drops.
        if unsafe { libc::dup2(fd.as_raw_fd(), target) } < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("dup2 to {target}"));
        }
    } else {
        std::mem::forget(fd);
    }
    set_cloexec(target, false)
}

fn sha256_hex(digest: &[u8; 32]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn sha256_of_fd(fd: RawFd) -> Result<(u64, [u8; 32])> {
    // A fresh open file description, so the digest leaves no shared offset
    // behind for the consumer.
    let mut file = File::open(format!("/proc/self/fd/{fd}"))
        .with_context(|| format!("cannot reopen shared descriptor {fd}"))?;
    let size = file.metadata()?.len();
    let digest = detcore::Digest::digest_reader(&mut file)?;
    Ok((size, *digest))
}

/// `hermit verify-inputs`: seal a pair's inputs once, then exec the pair.
#[derive(Debug, Parser)]
pub struct VerifyInputsOpts {
    /// The descriptor the manifest is inherited at, which both runs are given
    /// as `--native-exec-shared-inputs`. It must be above 2 and outside the
    /// native adapter's reserved 198-204.
    #[clap(long, value_name = "FD")]
    manifest_fd: RawFd,

    /// One input to seal, as ROLE:PATH. Repeatable.
    #[clap(long = "input", value_name = "ROLE:PATH")]
    inputs: Vec<String>,

    /// Retain the mapped runtime package once for the existing pair program.
    /// Each child must explicitly adopt it; this grants no entry authority.
    #[clap(long)]
    in_guest_exec_stop: bool,

    /// Capture the declared native cohort once for the existing pair program.
    #[cfg(feature = "native-exec")]
    #[clap(long, conflicts_with = "inputs", value_name = "DECLARATION")]
    native_exec_declaration: Option<PathBuf>,

    /// The pair program and its arguments, after the options; a `--` before
    /// it is accepted and not needed. Without one, the first `--` of the whole
    /// command line stays the one inside the pair program (the run's guest
    /// separator), which is where readers of a recorded command line look for
    /// the end of Hermit's options.
    #[clap(trailing_var_arg = true, required = true)]
    program: Vec<String>,
}

impl VerifyInputsOpts {
    // The holder captures and exports one input set. Selection changes only
    // launch-specific file roles; both variants use the existing sealed owner.
    fn capture_objects(
        &self,
        inputs: &[(String, PathBuf)],
    ) -> Result<(String, Vec<(SharedObjectMeta, OwnedFd)>)> {
        #[cfg(feature = "native-exec")]
        if let Some(declaration) = &self.native_exec_declaration {
            ensure!(
                !self.in_guest_exec_stop,
                "exec-stop-runtime-not-ready: native successor verification is not admitted"
            );
            return super::native_exec_cli::Capture::prepare_shared(declaration)?.export();
        }
        SealedFiles::seal(inputs)?.export()
    }
    pub fn main(&self) -> Result<hermit::ExitStatus> {
        #[cfg(not(all(feature = "exec-stop-launcher", feature = "liteinst")))]
        ensure!(
            !self.in_guest_exec_stop,
            "exec-stop-launcher-not-compiled: mapped verify-inputs requires the loader feature"
        );
        ensure!(
            self.manifest_fd > 2 && !NATIVE_RESERVED_FDS.contains(&self.manifest_fd),
            "--manifest-fd {} must be above 2 and outside the native adapter's reserved {}-{}",
            self.manifest_fd,
            NATIVE_RESERVED_FDS.start(),
            NATIVE_RESERVED_FDS.end()
        );
        let inputs = self
            .inputs
            .iter()
            .map(|input| {
                let (role, path) = input
                    .split_once(':')
                    .with_context(|| format!("--input {input:?} is not ROLE:PATH"))?;
                ensure!(
                    !role.is_empty() && !path.is_empty(),
                    "--input {input:?} is not ROLE:PATH"
                );
                Ok((role.to_string(), PathBuf::from(path)))
            })
            .collect::<Result<Vec<_>>>()?;
        // Authenticate resources once, before consuming real stdin. The
        // inherited original OFDs, not later path discovery, serve both runs.
        #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
        let image = self
            .in_guest_exec_stop
            .then(super::mapped_first_admission::RetainedImage::open)
            .transpose()?;
        let stdin = freeze_stdin(super::startup_stdin()?)?;
        let (producer, objects) = self.capture_objects(&inputs)?;
        #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
        let bytes = if let Some(image) = image {
            encode_with_mapped_package(producer, objects, self.manifest_fd, image)?
        } else {
            serde_json::to_vec(&place(producer, objects, self.manifest_fd)?)?
        };
        #[cfg(not(all(feature = "exec-stop-launcher", feature = "liteinst")))]
        let bytes = serde_json::to_vec(&place(producer, objects, self.manifest_fd)?)?;
        let (manifest_fd, _, _) =
            seal_into_memfd(c"hermit-shared-inputs-manifest", &mut bytes.as_slice())?;
        inherit_at(stdin, libc::STDIN_FILENO)?;
        inherit_at(manifest_fd, self.manifest_fd)?;
        let error = Command::new(&self.program[0])
            .args(&self.program[1..])
            .exec();
        bail!(
            "cannot exec the pair program {:?}: {error}",
            self.program[0]
        )
    }
}

/// Give every object an inheritable descriptor that is neither standard I/O,
/// the manifest's, nor one the native adapter reserves, and record it.
fn place(
    producer: String,
    objects: Vec<(SharedObjectMeta, OwnedFd)>,
    manifest_fd: RawFd,
) -> Result<Manifest> {
    let schema = if objects
        .iter()
        .all(|(meta, _)| meta.path.as_ref().and_then(|path| path.to_str()).is_some())
    {
        1
    } else {
        2
    };
    let mut placed = Vec::with_capacity(objects.len());
    for (meta, fd) in objects {
        let (raw, owned) = place_object_fd(fd, manifest_fd)?;
        placed.push(ManifestObject {
            path: if schema == 1 {
                ManifestPath::Utf8(
                    meta.path
                        .as_ref()
                        .and_then(|path| path.to_str())
                        .context("schema-1 path is not UTF-8")?
                        .to_owned(),
                )
            } else {
                ManifestPath::UnixBytes(
                    meta.path
                        .as_ref()
                        .map(|path| path.as_os_str().as_bytes().to_vec()),
                )
            },
            role: meta.role,
            size: meta.size,
            sha256: sha256_hex(&meta.sha256),
            seals: meta.seals,
            fd: raw,
        });
        // The descriptor stays open for the pair program to inherit.
        std::mem::forget(owned);
    }
    Ok(Manifest {
        schema,
        producer,
        objects: placed,
    })
}

// Shared placement for sealed objects and the two typed original package
// files; both remain owned until their manifest entries are constructed.
fn place_object_fd(fd: OwnedFd, manifest_fd: RawFd) -> Result<(RawFd, OwnedFd)> {
    let mut raw = fd.as_raw_fd();
    let mut owned = fd;
    while raw <= 2 || raw == manifest_fd || NATIVE_RESERVED_FDS.contains(&raw) {
        // SAFETY: F_DUPFD_CLOEXEC returns a new descriptor at or above the
        // given number; the old one closes when `owned` is replaced.
        let next = unsafe { libc::fcntl(raw, libc::F_DUPFD_CLOEXEC, raw.max(205)) };
        if next < 0 {
            return Err(std::io::Error::last_os_error()).context("F_DUPFD_CLOEXEC");
        }
        // SAFETY: `next` is the new descriptor fcntl returned.
        owned = unsafe { OwnedFd::from_raw_fd(next) };
        raw = next;
    }
    set_cloexec(raw, false)?;
    Ok((raw, owned))
}

/// Encode the existing holder's input manifest with one retained package.
/// Consumes the holder's image after exporting its original OFDs. The caller
/// seals these bytes with its original manifest producer; neither child run
/// may re-discover a resource path.
#[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
pub(crate) fn encode_with_mapped_package(
    producer: String,
    objects: Vec<(SharedObjectMeta, OwnedFd)>,
    manifest_fd: RawFd,
    image: super::mapped_first_admission::RetainedImage,
) -> Result<Vec<u8>> {
    ensure!(
        manifest_fd > 2 && !NATIVE_RESERVED_FDS.contains(&manifest_fd),
        "invalid pair manifest descriptor"
    );
    let (package, files) = image.export_pair_inputs()?;
    // Only the exported references survive to the pair. Drop the original
    // holder descriptors before its caller installs the manifest at a number
    // that could have belonged to one of those original descriptors.
    drop(image);
    let [image, descriptor] = files;
    let sealed = place(producer, objects, manifest_fd)?;
    let (image_fd, image) = place_object_fd(image, manifest_fd)?;
    let (descriptor_fd, descriptor) = place_object_fd(descriptor, manifest_fd)?;
    let envelope = MappedPackageManifest {
        schema: 3,
        sealed,
        package,
        image_fd,
        descriptor_fd,
    };
    let bytes = serde_json::to_vec(&envelope)?;
    // Same inheritable lifetime as the original place(): the pair exec owns
    // them, and each run adopts its inherited references before retirement.
    std::mem::forget(image);
    std::mem::forget(descriptor);
    Ok(bytes)
}

/// The pair's frozen stdin: the bytes the existing stdin snapshot owner would
/// replay (`hermit::reserve_output_stdin_snapshot`), sealed, so neither run can
/// see the caller change them. A terminal or an absent stdin is `/dev/null`,
/// as there, and so is the null device itself: it holds no bytes to freeze,
/// and an empty sealed file in its place would change what each guest finds
/// at its standard input (a regular file, not the character device a run
/// without the holder gives it).
fn freeze_stdin(stdin: Option<File>) -> Result<OwnedFd> {
    let Some(mut stdin) = stdin.filter(|file| {
        // SAFETY: isatty only queries the descriptor.
        let terminal = unsafe { libc::isatty(file.as_raw_fd()) } == 1;
        !terminal && !is_null_device(file)
    }) else {
        return Ok(File::open("/dev/null")?.into());
    };
    if stdin.stream_position().is_ok() {
        stdin.seek(SeekFrom::Start(0))?;
    }
    let (fd, _, _) = seal_into_memfd(c"hermit-shared-stdin", &mut stdin)?;
    Ok(fd)
}

/// Whether `file` is the null device (character device 1:3, Linux
/// Documentation/admin-guide/devices.txt).
fn is_null_device(file: &File) -> bool {
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::fs::MetadataExt;
    file.metadata().is_ok_and(|metadata| {
        metadata.file_type().is_char_device() && metadata.rdev() == libc::makedev(1, 3)
    })
}

/// The inputs a run adopted from `--native-exec-shared-inputs`, checked
/// against the manifest and kept from the guest.
#[derive(Debug)]
pub(crate) struct AdoptedInputs {
    producer: String,
    manifest: OwnedFd,
    objects: Vec<(SharedObjectMeta, OwnedFd)>,
    #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
    mapped_package: Option<super::mapped_first_admission::RetainedImage>,
}

impl AdoptedInputs {
    /// Hand the checked objects to their owner's [`SharedInputs::adopt`].
    #[allow(dead_code)] // The native-exec consumer is the first caller.
    pub(crate) fn into_inputs<T: SharedInputs>(self) -> Result<T> {
        #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
        ensure!(
            self.mapped_package.is_none(),
            "mapped package must be explicitly retained before consuming inputs"
        );
        T::adopt(&self.producer, self.objects)
    }

    /// The set `hermit verify-inputs --input ROLE:PATH...` would hand a run,
    /// sealed here in-process, for tests of what a run does with it.
    #[cfg(test)]
    pub(crate) fn sealed_for_test(inputs: &[(String, PathBuf)]) -> Result<Self> {
        let (producer, objects) = SealedFiles::seal(inputs)?.export()?;
        Ok(Self {
            producer,
            manifest: File::open("/dev/null")?.into(),
            objects,
            #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
            mapped_package: None,
        })
    }

    /// Whether the holder sealed an object for `role`.
    pub(crate) fn has_role(&self, role: &str) -> bool {
        self.objects.iter().any(|(meta, _)| meta.role == role)
    }

    /// The bytes of the one object the holder sealed (`hermit verify-inputs
    /// --input ROLE:PATH`) for `role`, or `None` when the set has none. That
    /// object must come from [`SealedFiles`] and record `path`, so a role
    /// cannot stand for a different file.
    pub(crate) fn read_role(
        &self,
        role: &str,
        path: &std::path::Path,
    ) -> Result<Option<(Vec<u8>, detcore_model::host_input::HostFileIdentity)>> {
        let mut found = self.objects.iter().filter(|(meta, _)| meta.role == role);
        let Some((meta, fd)) = found.next() else {
            return Ok(None);
        };
        ensure!(
            found.next().is_none(),
            "the shared inputs seal the role {role:?} more than once"
        );
        ensure!(
            self.producer == SealedFiles::PRODUCER,
            "the shared input {role:?} comes from {:?}, not {:?}",
            self.producer,
            SealedFiles::PRODUCER
        );
        ensure!(
            meta.path.as_deref() == Some(path),
            "the shared input {role:?} records {:?}, not {}",
            meta.path,
            path.display()
        );
        // A fresh open description: its own offset, from the start.
        // Provenance is the authenticated sealed object, not a new stat of
        // `meta.path`, whose contents may since have been replaced.
        let mut file = File::open(format!("/proc/self/fd/{}", fd.as_raw_fd()))?;
        let identity =
            detcore_model::host_input::HostFileIdentity::from_stat(&nix::sys::stat::fstat(&file)?);
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 == meta.size,
            "the shared input {role:?} read {} bytes, not its sealed {}",
            bytes.len(),
            meta.size
        );
        Ok(Some((bytes, identity)))
    }
    /// Presence DATA only, so callers can reject a mapped package in a
    /// non-mapped domain before retaining unrelated generic inputs.
    #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
    pub(crate) fn has_mapped_package(&self) -> bool {
        self.mapped_package.is_some()
    }
    /// Transfer the original image owner once; native/ordinary sealed inputs
    /// remain here for their existing consumer. Missing/repeated take refuses.
    #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
    pub(crate) fn take_mapped_package(
        &mut self,
    ) -> Result<super::mapped_first_admission::RetainedImage> {
        self.mapped_package
            .take()
            .context("shared mapped package missing or already taken")
    }
}

/// Read and check the sealed manifest and its original sealed objects:
/// every original object must carry its recorded seals and at least
/// [`REQUIRED_SEALS`], with the recorded size and sha256. A schema-3 envelope
/// additionally delegates its two original regular files to RetainedImage's
/// exact identity/byte/package validation. Every adopted descriptor, including
/// the manifest and package, becomes close-on-exec and is kept from the guest.
pub(crate) fn adopt(manifest_fd: RawFd) -> Result<AdoptedInputs> {
    ensure!(
        manifest_fd > 2 && !NATIVE_RESERVED_FDS.contains(&manifest_fd),
        "--native-exec-shared-inputs {manifest_fd} must be above 2 and outside the native adapter's reserved {}-{}",
        NATIVE_RESERVED_FDS.start(),
        NATIVE_RESERVED_FDS.end()
    );
    let manifest_seals = seals_of(manifest_fd)
        .with_context(|| format!("shared-input manifest descriptor {manifest_fd} is not sealed"))?;
    ensure!(
        manifest_seals & REQUIRED_SEALS == REQUIRED_SEALS,
        "shared-input manifest descriptor {manifest_fd} is not sealed"
    );
    set_cloexec(manifest_fd, true)?;
    // SAFETY: the descriptor is open (F_GET_SEALS succeeded) and this run
    // adopts it once.
    let manifest_owner = unsafe { OwnedFd::from_raw_fd(manifest_fd) };
    let mut bytes = Vec::new();
    File::open(format!("/proc/self/fd/{manifest_fd}"))?.read_to_end(&mut bytes)?;
    #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
    let (manifest, package) = decode_with_mapped_package(&bytes)?;
    #[cfg(not(all(feature = "exec-stop-launcher", feature = "liteinst")))]
    let manifest = decode_manifest(&bytes)?;
    #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
    let mut seen = package
        .as_ref()
        .map(|package| package_fd_set(package, manifest_fd))
        .transpose()?
        .unwrap_or_default();
    #[cfg(not(all(feature = "exec-stop-launcher", feature = "liteinst")))]
    let mut seen = std::collections::BTreeSet::new();
    let mut objects = Vec::with_capacity(manifest.objects.len());
    for object in manifest.objects {
        let fd = object.fd;
        let object_path = object.path.into_path(manifest.schema)?;
        let path = object_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<pathless owner role>".into());
        ensure!(
            fd > 2 && fd != manifest_fd && !NATIVE_RESERVED_FDS.contains(&fd) && seen.insert(fd),
            "shared input {path} names descriptor {fd}, which no object may use"
        );
        let seals = seals_of(fd).with_context(|| {
            format!("shared input {path} at descriptor {fd} is not a sealed object")
        })?;
        ensure!(
            seals == object.seals && seals & REQUIRED_SEALS == REQUIRED_SEALS,
            "shared input {path} at descriptor {fd} has seals {seals:#x}, not the sealed {:#x}",
            object.seals
        );
        let (size, digest) = sha256_of_fd(fd)?;
        ensure!(
            size == object.size && sha256_hex(&digest) == object.sha256,
            "shared input {path} at descriptor {fd} is not the object the manifest records"
        );
        set_cloexec(fd, true)?;
        // SAFETY: the descriptor is open (F_GET_SEALS succeeded), and the
        // manifest names it once, so this is its only owner in the run.
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        let meta = SharedObjectMeta {
            path: object_path,
            role: object.role,
            size,
            sha256: digest,
            seals,
        };
        objects.push((meta, owned));
    }
    #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
    let mapped_package = package
        .map(|package| {
            let files = [package.image_fd, package.descriptor_fd].map(|fd| {
                set_cloexec(fd, true)?;
                // SAFETY: package_fd_set checked live, unique descriptors distinct
                // from the manifest and every original sealed object.
                Ok(unsafe { OwnedFd::from_raw_fd(fd) })
            });
            let [image, descriptor]: [Result<OwnedFd>; 2] = files;
            super::mapped_first_admission::RetainedImage::adopt_pair_inputs(
                package.package,
                [image?, descriptor?],
            )
            .map_err(anyhow::Error::from)
        })
        .transpose()?;
    Ok(AdoptedInputs {
        producer: manifest.producer,
        manifest: manifest_owner,
        objects,
        #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
        mapped_package,
    })
}

/// The run's adoption made at CLI entry, before
/// `retire_inherited_descriptors` closes every descriptor nobody claimed:
/// the flag's descriptor and what adopting it produced.
static ENTRY_ADOPTION: std::sync::Mutex<Option<(RawFd, Result<AdoptedInputs>)>> =
    std::sync::Mutex::new(None);

/// The descriptor a raw command line passes as `--native-exec-shared-inputs`
/// before its first `--`, which ends Hermit's own options. Only the two exact
/// spellings clap accepts count; anything else is left to clap to refuse.
///
/// The holder (`hermit verify-inputs`) takes no such flag, and its pair
/// program follows its options without a `--`: the scan stops at that
/// subcommand, so the flag of a run inside the program is not the holder's.
fn shared_inputs_fd_in(argv: &[std::ffi::OsString]) -> Option<String> {
    const FLAG: &[u8] = b"--native-exec-shared-inputs";
    // Other operands (including the declaration path) are opaque Unix bytes.
    // Only this flag's numeric value must be UTF-8 for clap's integer parser.
    let mut words = argv.iter().skip(1).map(|word| word.as_bytes());
    while let Some(word) = words.next() {
        match word {
            b"--" | b"verify-inputs" => return None,
            word if word == FLAG => {
                return words
                    .next()
                    .and_then(|value| std::str::from_utf8(value).ok())
                    .map(str::to_owned);
            }
            word => {
                if let Some(value) = word
                    .strip_prefix(FLAG)
                    .and_then(|rest| rest.strip_prefix(b"="))
                {
                    return std::str::from_utf8(value).ok().map(str::to_owned);
                }
            }
        }
    }
    None
}

/// At CLI entry, before any descriptor is retired: adopt the shared inputs a
/// `--native-exec-shared-inputs` run names, and run `retire` with the
/// descriptors that adoption owns, so they survive it. A failed adoption is
/// kept to be reported by the run, through its ordinary error path.
pub(crate) fn retire_inherited_keeping_shared_inputs(
    argv: &[std::ffi::OsString],
    retire: impl FnOnce(&[std::os::fd::BorrowedFd<'_>]) -> std::io::Result<()>,
) -> std::io::Result<()> {
    use std::os::fd::AsFd;
    let Some(value) = shared_inputs_fd_in(argv) else {
        return retire(&[]);
    };
    let Ok(fd) = value.parse::<RawFd>() else {
        // clap refuses the value with its own message.
        return retire(&[]);
    };
    let mut entry = ENTRY_ADOPTION
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    *entry = Some((fd, adopt(fd)));
    let mut kept = Vec::new();
    if let Some((_, Ok(adopted))) = entry.as_ref() {
        kept.push(adopted.manifest.as_fd());
        kept.extend(adopted.objects.iter().map(|(_, object)| object.as_fd()));
        #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
        if let Some(package) = &adopted.mapped_package {
            kept.extend(package.pair_fds());
        }
    }
    retire(&kept)
}

/// The run's side: the inputs adopted at entry for `fd`, the descriptor its
/// parsed `--native-exec-shared-inputs` names.
pub(crate) fn take_adopted(fd: RawFd) -> Result<AdoptedInputs> {
    let entry = ENTRY_ADOPTION
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .take();
    match entry {
        Some((adopted_fd, adopted)) if adopted_fd == fd => adopted,
        Some((adopted_fd, _)) => bail!(
            "--native-exec-shared-inputs {fd} is not descriptor {adopted_fd}, which this run adopted at entry"
        ),
        None => bail!(
            "--native-exec-shared-inputs {fd} was not adopted at entry: give it on the command line, before `--`"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn shared_cli_entry_keeps_default_admission_order_and_adopts_mapped_once() {
        // Exercise the real CLI-entry adoption and top-level dispatch. These
        // empty sealed inputs need no runtime fixture and never start a guest.
        // The host callback stops dispatch after checking the ownership order.
        let install = |backend: &str, explicit: bool| {
            let manifest = Manifest {
                schema: 2,
                producer: SealedFiles::PRODUCER.into(),
                objects: Vec::new(),
            };
            let bytes = serde_json::to_vec(&manifest).unwrap();
            let (fd, _, _) = seal_into_memfd(c"entry-order-test", &mut bytes.as_slice()).unwrap();
            // Use the holder's legal range even when the test process already
            // has many descriptors open. CLI entry becomes the sole owner.
            // SAFETY: fcntl borrows the live memfd and returns a new descriptor.
            let fd = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 205) };
            assert!(fd >= 205);
            let mut argv: Vec<std::ffi::OsString> = [
                "hermit",
                backend,
                "run",
                "--epoch=2026-01-01T00:00:00Z",
                "--verify-run=/unused/entry-order",
            ]
            .into_iter()
            .map(Into::into)
            .collect();
            argv.push(format!("--native-exec-shared-inputs={fd}").into());
            if explicit {
                argv.push("--in-guest-exec-stop".into());
            }
            argv.extend(["--".into(), "/bin/true".into()]);
            retire_inherited_keeping_shared_inputs(&argv, |kept| {
                assert_eq!(kept.len(), 1);
                assert_eq!(kept[0].as_raw_fd(), fd);
                Ok(())
            })
            .unwrap();
            (super::super::Args::try_parse_from(argv).unwrap(), fd)
        };
        let (mut args, fd) = install("--backend=ptrace", false);
        let called = std::cell::Cell::new(false);
        let error = args
            .command
            .main_with_host_admission(&args.global, |_| {
                assert!(
                    ENTRY_ADOPTION.lock().unwrap().is_some(),
                    "omitted loader flag keeps main's host-admission order"
                );
                called.set(true);
                Err(anyhow::Error::new(std::io::Error::from_raw_os_error(
                    libc::ECANCELED,
                )))
            })
            .unwrap_err();
        assert!(called.get());
        assert_eq!(
            error
                .downcast_ref::<std::io::Error>()
                .and_then(std::io::Error::raw_os_error),
            Some(libc::ECANCELED)
        );
        let super::super::Subcommand::Run(run) = &mut args.command else {
            unreachable!()
        };
        // RunOpts::main performs the same preparation for direct callers. The
        // already transferred owner survives that preparation; no second take.
        run.adopt_verification_inputs().unwrap();
        run.adopt_verification_inputs().unwrap();
        assert!(
            take_adopted(fd)
                .unwrap_err()
                .to_string()
                .contains("was not adopted at entry")
        );
        drop(args);

        #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
        {
            // A wrong backend must retain its typed refusal before consuming
            // even an ordinary holder. Cover top-level and direct entry.
            for direct in [false, true] {
                let (mut args, fd) = install("--backend=ptrace", true);
                let error = if direct {
                    let super::super::Subcommand::Run(run) = &mut args.command else {
                        unreachable!()
                    };
                    run.main(&args.global).unwrap_err()
                } else {
                    args.command
                        .main_with_host_admission(&args.global, |_| {
                            panic!("wrong-backend mapped request reached host admission")
                        })
                        .unwrap_err()
                };
                assert!(
                    error.is::<super::super::container::PolicyRefusal>(),
                    "{error:#}"
                );
                assert!(error.to_string().contains("exec-stop-runtime-not-ready"));
                assert!(
                    ENTRY_ADOPTION.lock().unwrap().is_some(),
                    "option refusal must precede taking the holder's entry"
                );
                drop(take_adopted(fd).unwrap());
                assert!(ENTRY_ADOPTION.lock().unwrap().is_none());
            }

            // Omission keeps ordinary preload admission. An explicit loader
            // request must refuse this ordinary manifest before host admission.
            for explicit in [false, true] {
                let (mut args, fd) = install("--backend=in-guest-trap", explicit);
                let called = std::cell::Cell::new(false);
                let error = args
                    .command
                    .main_with_host_admission(&args.global, |_| {
                        if explicit {
                            panic!("a missing mapped package must not reach host admission");
                        }
                        assert!(
                            ENTRY_ADOPTION.lock().unwrap().is_some(),
                            "omitted loader flag keeps main's host-admission order"
                        );
                        called.set(true);
                        Err(anyhow::Error::new(std::io::Error::from_raw_os_error(
                            libc::ECANCELED,
                        )))
                    })
                    .unwrap_err();
                assert_eq!(called.get(), !explicit);
                if explicit {
                    assert!(
                        format!("{error:#}")
                            .contains("shared mapped package missing or already taken")
                    );
                } else {
                    assert_eq!(
                        error
                            .downcast_ref::<std::io::Error>()
                            .and_then(std::io::Error::raw_os_error),
                        Some(libc::ECANCELED)
                    );
                    let super::super::Subcommand::Run(run) = &mut args.command else {
                        unreachable!()
                    };
                    run.adopt_verification_inputs().unwrap();
                    run.adopt_verification_inputs().unwrap();
                }
                assert!(ENTRY_ADOPTION.lock().unwrap().is_none());
                assert!(
                    take_adopted(fd)
                        .unwrap_err()
                        .to_string()
                        .contains("was not adopted at entry")
                );
            }
        }
    }

    #[cfg(not(all(feature = "exec-stop-launcher", feature = "liteinst")))]
    #[test]
    fn mapped_holder_without_feature_refuses_before_stdin_or_program() {
        let opts = VerifyInputsOpts::try_parse_from([
            "verify-inputs",
            "--in-guest-exec-stop",
            "--manifest-fd=197",
            "--",
            "/nonexistent/must-not-run",
        ])
        .unwrap();
        let error = opts.main().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("exec-stop-launcher-not-compiled"),
            "{error:#}"
        );
    }

    #[test]
    fn holder_plain_capture_still_exports_exact_sealed_input_roles() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("data");
        std::fs::write(&path, b"original DATA").unwrap();
        let opts = VerifyInputsOpts::try_parse_from([
            "verify-inputs",
            "--manifest-fd",
            "197",
            "--",
            "true",
        ])
        .unwrap();
        let (producer, objects) = opts
            .capture_objects(&[("data".into(), path.clone())])
            .unwrap();
        assert_eq!(producer, SealedFiles::PRODUCER);
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].0.path, Some(path.clone()));
        assert_eq!(objects[0].0.role, "data");
        assert_eq!(objects[0].0.seals & REQUIRED_SEALS, REQUIRED_SEALS);
        std::fs::write(&path, b"replacement").unwrap();
        assert_eq!(
            sha256_of_fd(objects[0].1.as_raw_fd()).unwrap(),
            (13, *detcore::Digest::new(b"original DATA"))
        );
    }
    #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
    fn numeric_package_envelope() -> serde_json::Value {
        // Decoder DATA only: this does not model an executable package positive.
        serde_json::json!({"schema":3,
            "sealed":{"schema":2,"producer":"existing-owner","objects":[
                {"path":null,"role":"existing-sealed-role","size":1,
                 "sha256":"11".repeat(32),"seals":REQUIRED_SEALS,"fd":205}]},
            "package":{"image":{"state":[1,2,3,4,5,6,7,8],"sha256":"22".repeat(32)},
                       "descriptor":{"state":[9,10,11,12,13,14,15,16],"sha256":"33".repeat(32)}},
            "image_fd":206,"descriptor_fd":207})
    }
    #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
    #[test]
    fn package_envelope_preserves_legacy_schema_and_all_sealed_metadata() {
        let value = numeric_package_envelope();
        let bytes = serde_json::to_vec(&value).unwrap();
        assert!(decode_manifest(&bytes).is_err()); // Not a legacy seal exemption.
        let (sealed, package) = decode_with_mapped_package(&bytes).unwrap();
        assert_eq!(serde_json::to_value(sealed).unwrap(), value["sealed"]);
        assert!(package.is_some());
        let old = serde_json::to_vec(&value["sealed"]).unwrap();
        assert!(decode_with_mapped_package(&old).unwrap().1.is_none());
        for (schema, path) in [
            (1, serde_json::Value::Null),
            (2, serde_json::json!("/wrong-encoding")),
            (3, serde_json::Value::Null),
        ] {
            let mut bad = value.clone();
            bad["sealed"]["schema"] = schema.into();
            bad["sealed"]["objects"][0]["path"] = path;
            assert!(decode_with_mapped_package(&serde_json::to_vec(&bad).unwrap()).is_err());
        }
    }
    #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
    #[test]
    fn package_envelope_refuses_foreign_missing_duplicate_and_unknown_members() {
        let value = numeric_package_envelope();
        for key in ["package", "image_fd", "descriptor_fd", "sealed"] {
            let mut bad = value.clone();
            bad.as_object_mut().unwrap().remove(key);
            assert!(decode_with_mapped_package(&serde_json::to_vec(&bad).unwrap()).is_err());
        }
        for field in ["image", "descriptor"] {
            let mut bad = value.clone();
            bad["package"].as_object_mut().unwrap().remove(field);
            assert!(decode_with_mapped_package(&serde_json::to_vec(&bad).unwrap()).is_err());
        }
        for at in ["top", "package", "file"] {
            let mut bad = value.clone();
            match at {
                "top" => bad["foreign"] = true.into(),
                "package" => bad["package"]["runtime_override"] = true.into(),
                _ => bad["package"]["image"]["ready"] = true.into(),
            }
            assert!(decode_with_mapped_package(&serde_json::to_vec(&bad).unwrap()).is_err());
        }
        let encoded = serde_json::to_string(&value).unwrap();
        let duplicate = format!("{{\"schema\":3,{}", &encoded[1..]);
        assert!(decode_with_mapped_package(duplicate.as_bytes()).is_err());
        let mut unknown = value;
        unknown["schema"] = 4.into();
        assert!(decode_with_mapped_package(&serde_json::to_vec(&unknown).unwrap()).is_err());
    }
    #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
    #[test]
    fn package_descriptors_require_distinct_live_nonreserved_numbers() {
        let high_file = || {
            let file = tempfile::tempfile().unwrap();
            // SAFETY: obtain a new owned descriptor above all reserved slots.
            let raw = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 205) };
            assert!(raw >= 205);
            // SAFETY: raw is the fresh duplicate returned above.
            unsafe { OwnedFd::from_raw_fd(raw) }
        };
        let first = high_file();
        let second = high_file();
        let mut value = numeric_package_envelope();
        value["image_fd"] = first.as_raw_fd().into();
        value["descriptor_fd"] = second.as_raw_fd().into();
        let decode = |value: &serde_json::Value| {
            decode_with_mapped_package(&serde_json::to_vec(value).unwrap())
                .unwrap()
                .1
                .unwrap()
        };
        let valid = decode(&value);
        let numbers = package_fd_set(&valid, 197).unwrap();
        assert_eq!(numbers.len(), 2);
        assert!(numbers.contains(&first.as_raw_fd()));
        let closed = high_file();
        let closed_fd = closed.as_raw_fd();
        drop(closed);
        let mut bad = value.clone();
        bad["descriptor_fd"] = closed_fd.into();
        assert!(package_fd_set(&decode(&bad), 197).is_err());
        for fd in [0, 1, 2, 197, 198, 204, first.as_raw_fd(), -1] {
            let mut bad = value.clone();
            bad["descriptor_fd"] = fd.into();
            assert!(package_fd_set(&decode(&bad), 197).is_err());
        }
    }
    #[test]
    fn a_sealed_input_keeps_the_bytes_it_was_sealed_with() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("input");
        std::fs::write(&path, b"original").unwrap();
        let sealed = SealedFiles::seal(&[("data".into(), path.clone())]).unwrap();
        std::fs::write(&path, b"changed after sealing").unwrap();
        let (meta, fd) = &sealed.objects()[0];
        assert_eq!(meta.size, 8);
        assert_eq!(meta.seals & REQUIRED_SEALS, REQUIRED_SEALS);
        let (size, digest) = sha256_of_fd(fd.as_raw_fd()).unwrap();
        assert_eq!((size, digest), (8, *detcore::Digest::new(b"original")));
        // A sealed object refuses writes.
        let mut file = File::from(fd.try_clone().unwrap());
        assert!(file.write_all(b"x").is_err());
    }

    // The run side of /etc/ld.so.cache sharing
    // (https://github.com/rrnewton/hermit/issues/4693): the one object sealed
    // for a role, from the plain-files producer, at the path the caller names.
    #[test]
    fn a_role_reads_its_one_sealed_object_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ld.so.cache");
        std::fs::write(&path, b"sealed cache").unwrap();
        let adopted = |producer: &str| {
            let (_, objects) = SealedFiles::seal(&[("ld-so-cache".into(), path.clone())])
                .unwrap()
                .export()
                .unwrap();
            AdoptedInputs {
                producer: producer.into(),
                manifest: File::open("/dev/null").unwrap().into(),
                objects,
                #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
                mapped_package: None,
            }
        };
        let inputs = adopted(SealedFiles::PRODUCER);
        std::fs::write(&path, b"replaced on the host").unwrap();
        let (bytes, origin) = inputs.read_role("ld-so-cache", &path).unwrap().unwrap();
        assert_eq!(bytes, b"sealed cache");
        let physical = detcore_model::host_input::HostFileIdentity::from_stat(
            &nix::sys::stat::fstat(&inputs.objects[0].1).unwrap(),
        );
        assert_eq!(
            origin, physical,
            "the sealed holder, not the now-replaced host path"
        );
        assert_eq!(
            inputs.read_role("ld-so-cache", &path).unwrap().unwrap().1,
            origin
        );
        assert_eq!(inputs.read_role("absent", &path).unwrap(), None);
        let elsewhere = dir.path().join("other");
        assert!(inputs.read_role("ld-so-cache", &elsewhere).is_err());
        assert!(
            adopted("another-producer")
                .read_role("ld-so-cache", &path)
                .is_err()
        );
    }

    #[test]
    fn the_null_device_stays_the_null_device_and_a_file_is_frozen() {
        use std::os::unix::fs::FileTypeExt;
        let null = File::from(freeze_stdin(Some(File::open("/dev/null").unwrap())).unwrap());
        assert!(is_null_device(&null));
        assert!(null.metadata().unwrap().file_type().is_char_device());

        let mut input = tempfile::tempfile().unwrap();
        input.write_all(b"guest input").unwrap();
        let frozen = freeze_stdin(Some(input)).unwrap();
        assert_eq!(
            seals_of(frozen.as_raw_fd()).unwrap() & REQUIRED_SEALS,
            REQUIRED_SEALS
        );
        let mut bytes = Vec::new();
        File::open(format!("/proc/self/fd/{}", frozen.as_raw_fd()))
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"guest input");
    }

    // The pair program follows the holder's options with or without a `--`,
    // and every word of it -- its own `--` (the run's guest separator)
    // included -- reaches the program unchanged.
    #[test]
    fn a_misspelled_holder_option_is_refused_not_run() {
        let error = VerifyInputsOpts::try_parse_from([
            "verify-inputs",
            "--manifest-fd",
            "9",
            "--inptu",
            "r:/p",
            "bash",
        ])
        .expect_err("clap refuses an unknown option before the program");
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn the_pair_program_follows_the_options_with_or_without_a_separator() {
        let program = [
            "bash",
            "-c",
            "script",
            "name",
            "--log",
            "--",
            "/bin/true",
            "--x",
        ];
        let parse = |words: &[&str]| {
            VerifyInputsOpts::try_parse_from(
                ["verify-inputs", "--manifest-fd", "9", "--input", "r:/p"]
                    .iter()
                    .chain(words),
            )
            .unwrap()
        };
        for words in [&program[..], &[&["--"][..], &program[..]].concat()[..]] {
            let opts = parse(words);
            assert_eq!(opts.program, program, "{words:?}");
            assert_eq!(opts.manifest_fd, 9);
            assert_eq!(opts.inputs, ["r:/p"]);
        }
    }

    #[test]
    fn the_entry_scan_finds_the_flag_only_among_hermits_own_options() {
        let scan = |words: &[&str]| {
            let argv: Vec<std::ffi::OsString> = std::iter::once("hermit")
                .chain(words.iter().copied())
                .map(Into::into)
                .collect();
            shared_inputs_fd_in(&argv)
        };
        assert_eq!(
            scan(&["run", "--native-exec-shared-inputs=9", "--", "true"]),
            Some("9".into())
        );
        assert_eq!(
            scan(&["run", "--native-exec-shared-inputs", "7", "--", "true"]),
            Some("7".into())
        );
        // A guest argument is not Hermit's.
        assert_eq!(
            scan(&["run", "--", "echo", "--native-exec-shared-inputs=9"]),
            None
        );
        assert_eq!(
            scan(&["run", "--native-exec-shared-inputsX=9", "--", "true"]),
            None
        );
        assert_eq!(scan(&["run", "--", "true"]), None);
        // The holder's pair program is not Hermit's options.
        assert_eq!(
            scan(&[
                "verify-inputs",
                "--manifest-fd",
                "9",
                "bash",
                "-c",
                "x",
                "hermit",
                "run",
                "--native-exec-shared-inputs=9",
                "--",
                "true",
            ]),
            None
        );
    }

    #[test]
    fn the_entry_scan_preserves_non_utf8_operands_and_exact_flag_boundaries() {
        use std::ffi::OsString;

        let path = OsString::from_vec(b"/inputs/native-\xff.json".to_vec());
        for flag in [
            vec![OsString::from("--native-exec-shared-inputs=197")],
            vec![
                OsString::from("--native-exec-shared-inputs"),
                OsString::from("197"),
            ],
        ] {
            let mut argv = vec![
                OsString::from("hermit"),
                OsString::from("run"),
                OsString::from("--native-exec-inputs"),
                path.clone(),
            ];
            argv.extend(flag.clone());
            argv.extend([OsString::from("--"), OsString::from("/bin/true")]);
            assert_eq!(shared_inputs_fd_in(&argv), Some("197".into()));

            // A guest's identically named argument must never retain a cohort.
            let mut guest_argv = vec![
                OsString::from("hermit"),
                OsString::from("run"),
                OsString::from("--"),
                path.clone(),
            ];
            guest_argv.extend(flag);
            assert_eq!(shared_inputs_fd_in(&guest_argv), None);
        }
        for malformed in [
            vec![OsString::from_vec(
                b"--native-exec-shared-inputs\xff=197".to_vec(),
            )],
            vec![OsString::from_vec(
                b"--native-exec-shared-inputs=197\xff".to_vec(),
            )],
            vec![
                OsString::from("--native-exec-shared-inputs"),
                OsString::from_vec(b"197\xff".to_vec()),
            ],
        ] {
            let mut argv = vec![OsString::from("hermit"), OsString::from("run")];
            argv.extend(malformed);
            assert_eq!(shared_inputs_fd_in(&argv), None);
        }
    }

    #[test]
    fn plain_sealed_files_adopt_only_their_own_producer() {
        assert!(SealedFiles::adopt(SealedFiles::PRODUCER, Vec::new()).is_ok());
        assert!(SealedFiles::adopt("someone-else", Vec::new()).is_err());
    }

    #[test]
    fn manifest_path_versions_preserve_legacy_and_refuse_mixed_encodings() {
        let object = serde_json::json!({"path":"/input","role":"data","size":0,
            "sha256":"00".repeat(32),"seals":REQUIRED_SEALS,"fd":205});
        let legacy = serde_json::json!({"schema":1,"producer":"legacy","objects":[object]});
        let parsed = decode_manifest(&serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert_eq!(
            parsed
                .objects
                .into_iter()
                .next()
                .unwrap()
                .path
                .into_path(1)
                .unwrap(),
            Some(PathBuf::from("/input"))
        );
        for (schema, path) in [
            (1, serde_json::json!([47, 255])),
            (2, serde_json::json!("/input")),
            (3, serde_json::json!([47, 255])),
        ] {
            let mut invalid = legacy.clone();
            invalid["schema"] = schema.into();
            invalid["objects"][0]["path"] = path;
            assert!(decode_manifest(&serde_json::to_vec(&invalid).unwrap()).is_err());
        }
        let raw_path = b"/input-\xff".to_vec();
        let mut raw = legacy.clone();
        raw["schema"] = 2.into();
        raw["objects"][0]["path"] = serde_json::json!(raw_path);
        let parsed = decode_manifest(&serde_json::to_vec(&raw).unwrap()).unwrap();
        assert_eq!(
            parsed
                .objects
                .into_iter()
                .next()
                .unwrap()
                .path
                .into_path(2)
                .unwrap(),
            Some(PathBuf::from(std::ffi::OsString::from_vec(raw_path)))
        );
        raw["objects"][0]["path"] = serde_json::Value::Null;
        let parsed = decode_manifest(&serde_json::to_vec(&raw).unwrap()).unwrap();
        assert_eq!(
            parsed
                .objects
                .into_iter()
                .next()
                .unwrap()
                .path
                .into_path(2)
                .unwrap(),
            None
        );
        let mut invalid = legacy;
        invalid["schema"] = 2.into();
        invalid["objects"][0]["path"] = serde_json::json!([47, 256]);
        assert!(decode_manifest(&serde_json::to_vec(&invalid).unwrap()).is_err());
    }

    #[test]
    fn plain_owner_refuses_pathless_and_empty_path_roles() {
        for path in [None, Some(PathBuf::new())] {
            let (fd, size, sha256) = seal_into_memfd(c"path-test", &mut b"x".as_slice()).unwrap();
            let seals = seals_of(fd.as_raw_fd()).unwrap();
            assert!(
                SealedFiles::adopt(
                    SealedFiles::PRODUCER,
                    vec![(
                        SharedObjectMeta {
                            path,
                            role: "data".into(),
                            size,
                            sha256,
                            seals
                        },
                        fd
                    )]
                )
                .is_err()
            );
        }
    }

    #[test]
    fn actual_non_utf8_manifest_retains_one_cohort_for_two_consumers() {
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::MetadataExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory
            .path()
            .join(std::ffi::OsString::from_vec(b"source-\xff".to_vec()));
        std::fs::write(&path, b"original").unwrap();
        let held = SealedFiles::seal(&[("data".into(), path.clone())]).unwrap();
        let original = File::from(held.objects[0].1.try_clone().unwrap())
            .metadata()
            .unwrap();
        let (producer, objects) = held.export().unwrap();
        let manifest = place(producer, objects, 197).unwrap();
        let bytes =
            serde_json::to_vec(&manifest).expect("original Unix path bytes must be representable");
        let (manifest_fd, _, _) =
            seal_into_memfd(c"test-shared-manifest", &mut bytes.as_slice()).unwrap();
        std::fs::write(&path, b"mutated host").unwrap();
        for _ in 0..2 {
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0);
            if pid == 0 {
                let adopted = adopt(manifest_fd.as_raw_fd())
                    .unwrap()
                    .into_inputs::<SealedFiles>()
                    .unwrap();
                let actual = File::from(adopted.objects[0].1.try_clone().unwrap())
                    .metadata()
                    .unwrap();
                assert_eq!(
                    (actual.dev(), actual.ino()),
                    (original.dev(), original.ino())
                );
                assert_eq!(
                    sha256_of_fd(adopted.objects[0].1.as_raw_fd()).unwrap(),
                    (8, *detcore::Digest::new(b"original"))
                );
                unsafe { libc::_exit(0) };
            }
            let mut status = 0;
            assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
            assert_eq!(status, 0);
        }
        for object in manifest.objects {
            unsafe {
                libc::close(object.fd);
            }
        }
    }
}
