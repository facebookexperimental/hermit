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

/// Copy `source` into a new memfd and seal it, returning the memfd, its size
/// and its sha256. The memfd is close-on-exec until [`inherit`] clears that.
fn seal_into_memfd(
    name: &std::ffi::CStr,
    source: &mut impl Read,
) -> Result<(OwnedFd, u64, [u8; 32])> {
    // SAFETY: memfd_create takes a valid C string and returns a new descriptor
    // or -1; nothing else owns it.
    let raw =
        unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error()).context("memfd_create");
    }
    // SAFETY: `raw` is the new descriptor memfd_create returned.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
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

    /// Capture the declared native cohort once for the existing pair program.
    #[cfg(feature = "native-exec")]
    #[clap(long, conflicts_with = "inputs", value_name = "DECLARATION")]
    native_exec_declaration: Option<PathBuf>,

    /// The pair program and its arguments.
    #[clap(last = true, required = true)]
    program: Vec<String>,
}

impl VerifyInputsOpts {
    pub fn main(&self) -> Result<hermit::ExitStatus> {
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
        let stdin = freeze_stdin(super::startup_stdin()?)?;
        #[cfg(feature = "native-exec")]
        let (producer, objects) = if let Some(declaration) = &self.native_exec_declaration {
            super::native_exec_cli::Capture::prepare_shared(declaration)?.export()?
        } else {
            SealedFiles::seal(&inputs)?.export()?
        };
        #[cfg(not(feature = "native-exec"))]
        let (producer, objects) = SealedFiles::seal(&inputs)?.export()?;
        let manifest = place(producer, objects, self.manifest_fd)?;
        let (manifest_fd, _, _) = seal_into_memfd(
            c"hermit-shared-inputs-manifest",
            &mut serde_json::to_vec(&manifest)?.as_slice(),
        )?;
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

/// The pair's frozen stdin: the bytes the existing stdin snapshot owner would
/// replay (`hermit::reserve_output_stdin_snapshot`), sealed, so neither run can
/// see the caller change them. A terminal or an absent stdin is `/dev/null`,
/// as there.
fn freeze_stdin(stdin: Option<File>) -> Result<OwnedFd> {
    let Some(mut stdin) = stdin.filter(|file| {
        // SAFETY: isatty only queries the descriptor.
        let terminal = unsafe { libc::isatty(file.as_raw_fd()) } == 1;
        !terminal
    }) else {
        return Ok(File::open("/dev/null")?.into());
    };
    if stdin.stream_position().is_ok() {
        stdin.seek(SeekFrom::Start(0))?;
    }
    let (fd, _, _) = seal_into_memfd(c"hermit-shared-stdin", &mut stdin)?;
    Ok(fd)
}

/// The inputs a run adopted from `--native-exec-shared-inputs`, checked
/// against the manifest and kept from the guest.
#[derive(Debug)]
pub(crate) struct AdoptedInputs {
    producer: String,
    manifest: OwnedFd,
    objects: Vec<(SharedObjectMeta, OwnedFd)>,
}

impl AdoptedInputs {
    /// Hand the checked objects to their owner's [`SharedInputs::adopt`].
    #[allow(dead_code)] // The native-exec consumer is the first caller.
    pub(crate) fn into_inputs<T: SharedInputs>(self) -> Result<T> {
        T::adopt(&self.producer, self.objects)
    }
}

/// Read and check the manifest at `manifest_fd` and every object it names:
/// each must be open, carry its recorded seals and at least
/// [`REQUIRED_SEALS`], and have its recorded size and sha256. Every adopted
/// descriptor, the manifest's included, becomes close-on-exec, so none
/// reaches the guest.
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
    let manifest = decode_manifest(&bytes)?;
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
    Ok(AdoptedInputs {
        producer: manifest.producer,
        manifest: manifest_owner,
        objects,
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
fn shared_inputs_fd_in(argv: &[std::ffi::OsString]) -> Option<String> {
    const FLAG: &[u8] = b"--native-exec-shared-inputs";
    // Other operands (including the declaration path) are opaque Unix bytes.
    // Only this flag's numeric value must be UTF-8 for clap's integer parser.
    let mut words = argv.iter().skip(1).map(|word| word.as_bytes());
    while let Some(word) = words.next() {
        match word {
            b"--" => return None,
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
