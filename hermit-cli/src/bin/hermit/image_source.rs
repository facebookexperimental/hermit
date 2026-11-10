/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Local image inputs for `hermit run --image`: OCI layout directories and
//! `docker save`-style archives, read directly by Hermit with no podman
//! invocation.
//!
//! `--image` stays one string. Two explicit prefixes select a local source,
//! but only when the remainder is unambiguously a filesystem path (it starts
//! with `/`, `./`, or `../`):
//!
//! - `--image=oci-dir:/path` — an OCI layout directory (`oci-layout`,
//!   `index.json`, `blobs/sha256/...`). The unique `linux/amd64` manifest is
//!   selected; the manifest digest, the config digest, every layer blob
//!   digest, and every layer `diff_id` are verified before anything runs.
//!   Identity (and therefore the rootfs cache key) is the manifest digest.
//! - `--image=docker-archive:/path.tar` — a `podman save`/`docker save`
//!   archive (`manifest.json`, one config, ordered layer tars). Identity is
//!   the SHA-256 of the config bytes, with every layer verified against the
//!   config's `rootfs.diff_ids`. A docker-archive carries no manifest
//!   *blob* and so has no manifest-blob digest to verify; verification is
//!   the config hash plus the `diff_id`s. (For `oci-dir:` the manifest blob
//!   digest, config digest, layer blob digests, and `diff_id`s are all
//!   verified.)
//!
//! Platform: when the image config (or an index descriptor) declares an
//! os/architecture it must be `linux`/`amd64`; an image that declares no
//! os/architecture is accepted as unspecified and treated as linux/amd64.
//!
//! Any other `--image` value is a store reference and flows through the
//! podman-store path in [`crate::app::image`] unchanged. In particular
//! `oci-dir:5000/img` is a store reference (registry host `oci-dir`, port
//! `5000`), not a local path. A prefixed remainder that is neither a rooted
//! path nor port-shaped is refused by name: it reads as an intended local
//! path, and silently treating it as a store reference would hide the typo.
//!
//! Layers apply base-first with overlay whiteout semantics. Within one
//! layer, whiteouts apply against the lower-layer tree only (two passes:
//! whiteouts first, then payload), matching the podman-store path in
//! [`crate::app::image`]; a payload entry created by the same layer
//! therefore survives that layer's opaque whiteout regardless of tar
//! stream order. Paths are confined to the rootfs, symlink ancestors are never
//! written through, hardlinks must name in-root files, device entries are
//! skipped only under `dev/`, and non-`user.*` xattrs are refused by name.
//! Layer uid/gid are flattened to the invoking user (the entries are created
//! by Hermit, unprivileged); modes and mtimes are preserved — subsecond
//! mtimes included where a tar PAX `mtime` record provides them — including
//! a deferred pass for directories. Directories created only as implicit
//! parents of a payload entry get deterministic mode `0o755` and mtime `0`
//! unless an explicit directory entry for the same path overrides them in
//! the deferred pass. Every refusal here is an
//! ordinary error (exit 125), never a guest-program fault (126/127): the
//! guest was never started.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io::Read;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use hermit::Context;
use hermit::Error;
use serde::Deserialize;

use crate::app::podman_store::ImageRunConfig;

/// Largest JSON document (OCI index, manifest, or image config, and
/// docker-archive `manifest.json`/config entries) read into memory. A larger
/// document is refused by name rather than buffered without bound.
pub(crate) const MAX_JSON_BYTES: u64 = 8 * 1024 * 1024;

/// Largest uncompressed (expanded) size of one layer tar. A gzip layer whose
/// tar expands past this is refused during `diff_id` verification, before
/// anything is written to the rootfs.
pub(crate) const MAX_LAYER_EXPANDED_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Largest total payload bytes written while applying all layers of one
/// image, and likewise the largest total bytes spooled for one
/// docker-archive's referenced layers. Exceeding it is refused by name.
pub(crate) const MAX_TOTAL_EXPANDED_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Which local image format a `--image` prefix names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalImageKind {
    OciDir,
    DockerArchive,
}

impl LocalImageKind {
    /// The `--image` prefix (without the colon) for this kind.
    pub(crate) fn prefix(self) -> &'static str {
        match self {
            LocalImageKind::OciDir => "oci-dir",
            LocalImageKind::DockerArchive => "docker-archive",
        }
    }
}

/// How a `--image` value dispatches: to a local source, to the podman store,
/// or to a by-name refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ImageSourceRef {
    /// An ordinary store reference; the existing podman-store path handles it.
    StoreReference,
    /// A local image input at `path`, selected by an explicit prefix whose
    /// remainder started with `/`, `./`, or `../`.
    Local { kind: LocalImageKind, path: PathBuf },
    /// A prefixed value whose remainder is a bare relative path. Kept distinct
    /// from `StoreReference` so the likely-intended local path is refused by
    /// name instead of being looked up in the store.
    BareRelative {
        kind: LocalImageKind,
        remainder: String,
    },
}

/// Classify a `--image` value. Dispatch to a local source happens only when a
/// known prefix is followed by `/`, `./`, or `../`; a `<digits>/...` remainder
/// keeps the whole value a store reference (registry `host:port/path`).
pub(crate) fn parse_image_source(image_ref: &str) -> ImageSourceRef {
    for kind in [LocalImageKind::OciDir, LocalImageKind::DockerArchive] {
        let Some(remainder) = image_ref
            .strip_prefix(kind.prefix())
            .and_then(|rest| rest.strip_prefix(':'))
        else {
            continue;
        };
        if remainder.starts_with('/') || remainder.starts_with("./") || remainder.starts_with("../")
        {
            return ImageSourceRef::Local {
                kind,
                path: PathBuf::from(remainder),
            };
        }
        // `oci-dir:5000/img` is a store reference to registry host `oci-dir`
        // port 5000, so a port-shaped remainder is not a local path either.
        if is_port_shaped(remainder) {
            return ImageSourceRef::StoreReference;
        }
        return ImageSourceRef::BareRelative {
            kind,
            remainder: remainder.to_string(),
        };
    }
    ImageSourceRef::StoreReference
}

/// Whether `remainder` looks like `<port>/<path>` of a registry reference.
fn is_port_shaped(remainder: &str) -> bool {
    let Some((head, _)) = remainder.split_once('/') else {
        return false;
    };
    !head.is_empty() && head.chars().all(|c| c.is_ascii_digit())
}

/// The refusal for a bare-relative prefixed `--image` value.
pub(crate) fn bare_relative_error(kind: LocalImageKind, remainder: &str) -> Error {
    Error::msg(format!(
        "--image={}:{remainder} names a bare relative path, which is ambiguous \
         with a store reference and is refused. Prefix it with ./ for a local \
         image (--image={}:./{remainder}) or pass an absolute path \
         (--image={}:/absolute/path).",
        kind.prefix(),
        kind.prefix(),
        kind.prefix(),
    ))
}

/// A verified local image, ready to apply to a rootfs directory.
pub(crate) struct LoadedImage {
    /// Cache-key identity: `oci-dir@sha256:<manifest>` or
    /// `docker-archive@sha256:<config>`.
    pub identity: String,
    /// The image's declared run config (Env, WorkingDir, Cmd, Entrypoint).
    pub config: ImageRunConfig,
    /// Layers in application (base-first) order.
    layers: Vec<LayerFile>,
    /// Spool directory holding docker-archive layers extracted from the
    /// outer tar; kept alive until the layers have been applied.
    _spool: Option<tempfile::TempDir>,
}

/// One layer tar on disk (an OCI blob, or a spooled docker-archive layer).
#[derive(Debug)]
struct LayerFile {
    path: PathBuf,
    compression: Compression,
    /// Human-readable origin for error messages.
    origin: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Compression {
    Plain,
    Gzip,
}

/// The SHA-256 hex digest of `data`.
fn sha256_hex(data: &[u8]) -> String {
    detcore::Digest::new(data).to_string()
}

/// Split a `sha256:<hex>` digest, refusing other algorithms by name.
fn parse_sha256_digest(digest: &str, what: &str) -> Result<String, Error> {
    let hex = digest.strip_prefix("sha256:").ok_or_else(|| {
        Error::msg(format!(
            "{what} uses digest {digest:?}; only sha256 digests are supported. \
             Re-export the image so its digests are sha256."
        ))
    })?;
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::msg(format!(
            "{what} uses malformed sha256 digest {digest:?}; re-export the image."
        )));
    }
    Ok(hex.to_ascii_lowercase())
}

// ---------------------------------------------------------------------------
// Image config (shared by OCI image config blobs and docker-archive configs)
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct RawImageConfig {
    architecture: Option<String>,
    os: Option<String>,
    config: Option<RawRuntimeConfig>,
    rootfs: Option<RawRootfs>,
}

#[derive(Deserialize, Default)]
struct RawRuntimeConfig {
    #[serde(rename = "Env")]
    env: Option<Vec<String>>,
    #[serde(rename = "WorkingDir")]
    working_dir: Option<String>,
    #[serde(rename = "Cmd")]
    cmd: Option<serde_json::Value>,
    #[serde(rename = "Entrypoint")]
    entrypoint: Option<serde_json::Value>,
}

#[derive(Deserialize, Default)]
struct RawRootfs {
    diff_ids: Option<Vec<String>>,
}

/// Normalize a Cmd/Entrypoint value: the image spec allows an array (exec
/// form) or a string (shell form, run through `/bin/sh -c` by runtimes).
fn normalize_argv(value: &Option<serde_json::Value>, what: &str) -> Result<Vec<String>, Error> {
    match value {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(serde_json::Value::String(s)) => {
            Ok(vec!["/bin/sh".to_string(), "-c".to_string(), s.clone()])
        }
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str().map(str::to_string).ok_or_else(|| {
                    Error::msg(format!("image config {what} contains a non-string entry"))
                })
            })
            .collect(),
        Some(_) => Err(Error::msg(format!(
            "image config {what} is neither a string nor an array"
        ))),
    }
}

/// Parse an image config body into the run config plus the ordered `diff_id`
/// hex strings every layer must verify against.
fn parse_image_config(bytes: &[u8], what: &str) -> Result<(ImageRunConfig, Vec<String>), Error> {
    let raw: RawImageConfig = serde_json::from_slice(bytes)
        .with_context(|| format!("{what} is not valid OCI image config JSON"))?;
    if let Some(arch) = &raw.architecture
        && arch != "amd64"
    {
        return Err(Error::msg(format!(
            "{what} declares architecture {arch:?}; only linux/amd64 images are \
             supported. Export the linux/amd64 variant of the image."
        )));
    }
    if let Some(os) = &raw.os
        && os != "linux"
    {
        return Err(Error::msg(format!(
            "{what} declares os {os:?}; only linux/amd64 images are supported. \
             Export the linux/amd64 variant of the image."
        )));
    }
    let runtime = raw.config.unwrap_or_default();
    let config = ImageRunConfig {
        env: runtime
            .env
            .unwrap_or_default()
            .iter()
            .filter_map(|entry| entry.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        workdir: runtime.working_dir.filter(|w| !w.is_empty() && w != "/"),
        cmd: normalize_argv(&runtime.cmd, "Cmd")?,
        entrypoint: normalize_argv(&runtime.entrypoint, "Entrypoint")?,
    };
    let diff_ids = raw
        .rootfs
        .and_then(|rootfs| rootfs.diff_ids)
        .ok_or_else(|| {
            Error::msg(format!(
                "{what} declares no rootfs.diff_ids, so its layers cannot be \
                 verified. Re-export the image with a complete config."
            ))
        })?
        .iter()
        .map(|digest| parse_sha256_digest(digest, "rootfs.diff_ids"))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((config, diff_ids))
}

// ---------------------------------------------------------------------------
// OCI layout directories (--image=oci-dir:...)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct OciIndex {
    #[serde(default)]
    manifests: Vec<OciDescriptor>,
}

#[derive(Deserialize)]
struct OciManifest {
    config: OciDescriptor,
    #[serde(default)]
    layers: Vec<OciDescriptor>,
}

#[derive(Deserialize)]
struct OciDescriptor {
    #[serde(rename = "mediaType")]
    media_type: Option<String>,
    digest: String,
    /// Declared size of the referenced blob, when the descriptor carries
    /// one. Honored as an exact bound: a blob whose length differs is
    /// refused before it is buffered or hashed.
    size: Option<u64>,
    platform: Option<OciPlatform>,
}

#[derive(Deserialize)]
struct OciPlatform {
    architecture: Option<String>,
    os: Option<String>,
}

/// An OCI layer blob to verify and apply.
struct OciLayer {
    blob: PathBuf,
    /// Expected SHA-256 hex of the blob bytes as stored (compressed form).
    blob_digest: String,
    /// Descriptor-declared blob size, when present.
    size: Option<u64>,
    compression: Compression,
}

/// The metadata half of an OCI-dir load: manifest selection, config parse,
/// and the layer list. Manifest and config digests are verified here; layer
/// blob digests and diff_ids are verified by the full load.
struct OciMetadata {
    manifest_digest: String,
    config: ImageRunConfig,
    diff_ids: Vec<String>,
    layers: Vec<OciLayer>,
}

fn oci_export_remedy(path: &Path) -> String {
    format!(
        "Export an OCI layout directory there, for example \
         `podman save --format oci-dir -o {} <image>`, then re-run.",
        path.display()
    )
}

/// Read at most `max` bytes from `reader`, refusing by name past the bound.
fn read_bounded<R: Read>(reader: R, max: u64, what: &str) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    reader
        .take(max + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("Failed to read {what}"))?;
    if bytes.len() as u64 > max {
        return Err(Error::msg(format!(
            "{what} exceeds the {max}-byte limit for image JSON documents; \
             refusing to buffer it. Re-export the image with a smaller \
             index, manifest, or config."
        )));
    }
    Ok(bytes)
}

/// Read and digest-verify one `blobs/sha256/<hex>` JSON blob into memory.
/// The read is bounded by [`MAX_JSON_BYTES`] and, when the descriptor
/// declares a size, by that exact size.
fn read_oci_blob(
    dir: &Path,
    digest_hex: &str,
    expected_size: Option<u64>,
    what: &str,
) -> Result<Vec<u8>, Error> {
    let blob = dir.join("blobs").join("sha256").join(digest_hex);
    let file = std::fs::File::open(&blob).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::msg(format!(
                "OCI layout at {} is missing blob sha256:{digest_hex} for {what} \
                 (expected at {}). {}",
                dir.display(),
                blob.display(),
                oci_export_remedy(dir)
            ))
        } else {
            Error::from(e).context(format!("Failed to read OCI blob {}", blob.display()))
        }
    })?;
    let bytes = read_bounded(file, MAX_JSON_BYTES, &format!("OCI {what}"))?;
    let actual = sha256_hex(&bytes);
    if actual != digest_hex {
        return Err(Error::msg(format!(
            "OCI {what} digest mismatch: the layout names sha256:{digest_hex} \
             but the blob hashes to sha256:{actual}. The layout is corrupt or \
             was tampered with; re-export the image."
        )));
    }
    // Honor a declared descriptor size once the digest checks out, so a
    // corrupt blob is still reported as the digest mismatch it is.
    if let Some(declared) = expected_size
        && bytes.len() as u64 != declared
    {
        return Err(Error::msg(format!(
            "OCI {what} size mismatch: the descriptor declares {declared} \
             bytes but blob sha256:{digest_hex} is {} bytes. The layout is \
             inconsistent; re-export the image.",
            bytes.len()
        )));
    }
    Ok(bytes)
}

/// Map a layer media type to its compression, refusing zstd and unknown
/// types by name.
fn layer_compression(media_type: Option<&str>, what: &str) -> Result<Compression, Error> {
    let media_type = media_type.unwrap_or("");
    if media_type.contains("zstd") {
        return Err(Error::msg(format!(
            "{what} uses zstd-compressed layers (media type {media_type:?}), \
             which are not supported. Re-export the image with gzip or \
             uncompressed layers."
        )));
    }
    if media_type.contains("gzip") {
        return Ok(Compression::Gzip);
    }
    if media_type.contains("tar") {
        return Ok(Compression::Plain);
    }
    Err(Error::msg(format!(
        "{what} uses unsupported layer media type {media_type:?}; only tar and \
         tar+gzip layers are supported. Re-export the image with gzip or \
         uncompressed layers."
    )))
}

fn load_oci_metadata(dir: &Path) -> Result<OciMetadata, Error> {
    match std::fs::metadata(dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            return Err(Error::msg(format!(
                "oci-dir image path {} is not a directory. {}",
                dir.display(),
                oci_export_remedy(dir)
            )));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::msg(format!(
                "oci-dir image path {} does not exist. {}",
                dir.display(),
                oci_export_remedy(dir)
            )));
        }
        Err(e) => {
            return Err(Error::from(e).context(format!(
                "Failed to inspect oci-dir image path {}",
                dir.display()
            )));
        }
    }
    if !dir.join("oci-layout").is_file() {
        return Err(Error::msg(format!(
            "{} is not an OCI layout directory: it has no oci-layout file. {}",
            dir.display(),
            oci_export_remedy(dir)
        )));
    }
    let index_path = dir.join("index.json");
    let index_file = std::fs::File::open(&index_path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::msg(format!(
                "{} is not an OCI layout directory: it has no index.json. {}",
                dir.display(),
                oci_export_remedy(dir)
            ))
        } else {
            Error::from(e).context(format!("Failed to read {}", index_path.display()))
        }
    })?;
    let index_bytes = read_bounded(
        index_file,
        MAX_JSON_BYTES,
        &format!("OCI index {}", index_path.display()),
    )?;
    let index: OciIndex = serde_json::from_slice(&index_bytes)
        .with_context(|| format!("{} is not valid OCI index JSON", index_path.display()))?;

    // Select the unique linux/amd64 image manifest. A descriptor with no
    // platform is a candidate too; its config is then the architecture
    // authority (parse_image_config refuses a non-linux/amd64 config).
    let candidates: Vec<&OciDescriptor> = index
        .manifests
        .iter()
        .filter(|descriptor| {
            descriptor
                .media_type
                .as_deref()
                .is_none_or(|mt| mt.contains("manifest") && !mt.contains("manifest.list"))
        })
        .filter(|descriptor| match &descriptor.platform {
            None => true,
            Some(platform) => {
                platform.os.as_deref().is_none_or(|os| os == "linux")
                    && platform
                        .architecture
                        .as_deref()
                        .is_none_or(|arch| arch == "amd64")
            }
        })
        .collect();
    let descriptor = match candidates.as_slice() {
        [] => {
            return Err(Error::msg(format!(
                "OCI layout at {} has no linux/amd64 manifest in index.json. \
                 Export the linux/amd64 variant of the image.",
                dir.display()
            )));
        }
        [only] => *only,
        many => {
            return Err(Error::msg(format!(
                "OCI layout at {} has {} linux/amd64 manifests in index.json, \
                 so the image is ambiguous. Export a single-platform image.",
                dir.display(),
                many.len()
            )));
        }
    };

    let manifest_digest = parse_sha256_digest(&descriptor.digest, "the OCI manifest")?;
    let manifest_bytes = read_oci_blob(dir, &manifest_digest, descriptor.size, "manifest")?;
    let manifest: OciManifest = serde_json::from_slice(&manifest_bytes).with_context(|| {
        format!(
            "OCI manifest sha256:{manifest_digest} at {} is not valid JSON",
            dir.display()
        )
    })?;

    let config_digest = parse_sha256_digest(&manifest.config.digest, "the image config")?;
    let config_bytes = read_oci_blob(dir, &config_digest, manifest.config.size, "config")?;
    let (config, diff_ids) = parse_image_config(&config_bytes, "the image config")?;

    if manifest.layers.len() != diff_ids.len() {
        return Err(Error::msg(format!(
            "OCI manifest sha256:{manifest_digest} lists {} layers but the \
             config declares {} rootfs.diff_ids; the image is inconsistent. \
             Re-export the image.",
            manifest.layers.len(),
            diff_ids.len()
        )));
    }
    let mut layers = Vec::new();
    for (i, layer) in manifest.layers.iter().enumerate() {
        let blob_digest = parse_sha256_digest(&layer.digest, &format!("layer {i}"))?;
        let compression = layer_compression(layer.media_type.as_deref(), &format!("layer {i}"))?;
        layers.push(OciLayer {
            blob: dir.join("blobs").join("sha256").join(&blob_digest),
            blob_digest,
            size: layer.size,
            compression,
        });
    }
    Ok(OciMetadata {
        manifest_digest,
        config,
        diff_ids,
        layers,
    })
}

/// Fully load and verify an OCI layout directory.
fn load_oci_dir(dir: &Path) -> Result<LoadedImage, Error> {
    let meta = load_oci_metadata(dir)?;
    let mut layers = Vec::new();
    for (i, layer) in meta.layers.iter().enumerate() {
        let what = format!("layer {i}");
        // Verify the blob digest (stored form) before trusting its diff_id.
        let stored = digest_of_file(&layer.blob, dir, &what)?;
        if stored != layer.blob_digest {
            return Err(Error::msg(format!(
                "OCI {what} digest mismatch: the manifest names \
                 sha256:{} but the blob hashes to sha256:{stored}. The layout \
                 is corrupt or was tampered with; re-export the image.",
                layer.blob_digest
            )));
        }
        if let Some(declared) = layer.size
            && let Ok(meta) = std::fs::metadata(&layer.blob)
            && meta.len() != declared
        {
            return Err(Error::msg(format!(
                "OCI {what} size mismatch: the manifest declares {declared} \
                 bytes but the blob is {} bytes. The layout is inconsistent; \
                 re-export the image.",
                meta.len()
            )));
        }
        let layer_file = LayerFile {
            path: layer.blob.clone(),
            compression: layer.compression,
            origin: format!("OCI blob sha256:{}", layer.blob_digest),
        };
        verify_diff_id(&layer_file, &meta.diff_ids[i], &what)?;
        layers.push(layer_file);
    }
    Ok(LoadedImage {
        identity: format!("oci-dir@sha256:{}", meta.manifest_digest),
        config: meta.config,
        layers,
        _spool: None,
    })
}

/// SHA-256 hex of a file, with a missing OCI blob named as such.
fn digest_of_file(path: &Path, dir: &Path, what: &str) -> Result<String, Error> {
    let file = std::fs::File::open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            let digest = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            Error::msg(format!(
                "OCI layout at {} is missing blob sha256:{digest} for {what} \
                 (expected at {}). {}",
                dir.display(),
                path.display(),
                oci_export_remedy(dir)
            ))
        } else {
            Error::from(e).context(format!("Failed to read {}", path.display()))
        }
    })?;
    Ok(detcore::Digest::digest_reader(file)
        .with_context(|| format!("Failed to hash {}", path.display()))?
        .to_string())
}

/// Open a layer's uncompressed tar stream.
fn open_layer(layer: &LayerFile) -> Result<Box<dyn Read>, Error> {
    let file = std::fs::File::open(&layer.path)
        .with_context(|| format!("Failed to open layer {}", layer.path.display()))?;
    match layer.compression {
        Compression::Plain => Ok(Box::new(file)),
        Compression::Gzip => Ok(Box::new(flate2::read::GzDecoder::new(file))),
    }
}

/// A reader that fails once more than `limit` expanded bytes are read, so a
/// gzip bomb is refused while streaming instead of expanding without bound.
struct ExpansionLimited<R: Read> {
    inner: R,
    remaining: u64,
    limit: u64,
    exceeded: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl<R: Read> Read for ExpansionLimited<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            // Distinguish a stream that ends exactly at the limit from one
            // with more bytes behind it.
            let mut probe = [0u8; 1];
            return match self.inner.read(&mut probe)? {
                0 => Ok(0),
                _ => {
                    self.exceeded
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("expanded layer exceeds the {}-byte quota", self.limit),
                    ))
                }
            };
        }
        let cap = usize::try_from(self.remaining.min(buf.len() as u64)).unwrap_or(buf.len());
        let n = self.inner.read(&mut buf[..cap])?;
        self.remaining -= n as u64;
        Ok(n)
    }
}

/// Verify a layer's uncompressed tar hashes to the config's `diff_id`.
fn verify_diff_id(layer: &LayerFile, expected_hex: &str, what: &str) -> Result<(), Error> {
    verify_diff_id_with_limit(layer, expected_hex, what, MAX_LAYER_EXPANDED_BYTES)
}

fn verify_diff_id_with_limit(
    layer: &LayerFile,
    expected_hex: &str,
    what: &str,
    expanded_limit: u64,
) -> Result<(), Error> {
    let exceeded = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = ExpansionLimited {
        inner: open_layer(layer)?,
        remaining: expanded_limit,
        limit: expanded_limit,
        exceeded: exceeded.clone(),
    };
    let actual = detcore::Digest::digest_reader(reader).map_err(|e| {
        if exceeded.load(std::sync::atomic::Ordering::SeqCst) {
            return Error::msg(format!(
                "expanded size of {what} ({}) exceeds the {expanded_limit}-byte \
                 quota for one layer; refusing the image. Re-export it with \
                 smaller layers.",
                layer.origin
            ));
        }
        if layer.compression == Compression::Gzip {
            Error::from(e).context(format!(
                "corrupt gzip in {what} ({}): the layer cannot be decompressed. \
                 Re-export the image.",
                layer.origin
            ))
        } else {
            Error::from(e).context(format!("Failed to read {what} ({})", layer.origin))
        }
    })?;
    if actual.to_string() != expected_hex {
        return Err(Error::msg(format!(
            "diff_id mismatch for {what} ({}): the config names \
             sha256:{expected_hex} but the uncompressed layer hashes to \
             sha256:{actual}. The image is corrupt or was tampered with; \
             re-export the image.",
            layer.origin
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Docker archives (--image=docker-archive:...)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct DockerManifestEntry {
    #[serde(rename = "Config")]
    config: String,
    #[serde(rename = "Layers", default)]
    layers: Vec<String>,
}

/// What one streaming pass over a docker-archive outer tar collects.
struct DockerArchiveScan {
    manifest_json: Option<Vec<u8>>,
    /// Regular `.json` entries by archive path (config candidates are small).
    json_entries: BTreeMap<String, Vec<u8>>,
    /// Spooled non-JSON regular entries by archive path (the layers).
    spooled: BTreeMap<String, PathBuf>,
    /// Non-JSON regular entry names seen when not spooling.
    entry_names: Vec<String>,
    spool: Option<tempfile::TempDir>,
}

fn docker_export_remedy(path: &Path) -> String {
    format!(
        "Create one with `podman save -o {} <image>` (or `docker save`), \
         then re-run.",
        path.display()
    )
}

/// Detect gzip/zstd by magic bytes at the start of `path`.
fn sniff_compression(path: &Path, what: &str) -> Result<Compression, Error> {
    let mut head = [0u8; 4];
    let mut file =
        std::fs::File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let mut read = 0;
    while read < head.len() {
        match file.read(&mut head[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(e) => {
                return Err(Error::from(e).context(format!("Failed to read {}", path.display())));
            }
        }
    }
    if head == [0x28, 0xb5, 0x2f, 0xfd] {
        return Err(Error::msg(format!(
            "{what} is zstd-compressed, which is not supported. Re-export the \
             image with gzip or uncompressed layers."
        )));
    }
    if head[..read.min(2)] == [0x1f, 0x8b] {
        return Ok(Compression::Gzip);
    }
    Ok(Compression::Plain)
}

/// Stream the outer tar of a docker archive once. When `spool_only` is
/// `Some(set)`, only the named (manifest-referenced) layer entries are
/// spooled to a temp directory, subject to [`MAX_TOTAL_EXPANDED_BYTES`];
/// unreferenced entries are skipped, never spooled. Callers that need the
/// referenced set scan first with `None` (names only), parse
/// `manifest.json`, then scan again spooling just those layers.
fn scan_docker_archive(
    path: &Path,
    spool_only: Option<&BTreeSet<String>>,
) -> Result<DockerArchiveScan, Error> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => {
            return Err(Error::msg(format!(
                "docker-archive image path {} is not a regular file. {}",
                path.display(),
                docker_export_remedy(path)
            )));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::msg(format!(
                "docker-archive image path {} does not exist. {}",
                path.display(),
                docker_export_remedy(path)
            )));
        }
        Err(e) => {
            return Err(Error::from(e).context(format!(
                "Failed to inspect docker-archive image path {}",
                path.display()
            )));
        }
    }
    let outer = sniff_compression(path, "the docker-archive")?;
    let file =
        std::fs::File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let reader: Box<dyn Read> = match outer {
        Compression::Plain => Box::new(file),
        Compression::Gzip => Box::new(flate2::read::GzDecoder::new(file)),
    };
    let mut archive = tar::Archive::new(reader);
    let spool_dir = if spool_only.is_some() {
        Some(
            tempfile::Builder::new()
                .prefix("hermit-docker-archive-")
                .tempdir()
                .context("Failed to create a spool directory for docker-archive layers")?,
        )
    } else {
        None
    };
    let mut scan = DockerArchiveScan {
        manifest_json: None,
        json_entries: BTreeMap::new(),
        spooled: BTreeMap::new(),
        entry_names: Vec::new(),
        spool: spool_dir,
    };
    let entries = archive.entries().map_err(|e| {
        Error::from(e).context(format!(
            "{} is a truncated or corrupt docker-archive: its tar entries \
             cannot be read. {}",
            path.display(),
            docker_export_remedy(path)
        ))
    })?;
    let mut index = 0usize;
    let mut spooled_bytes: u64 = 0;
    for entry in entries {
        let mut entry = entry.map_err(|e| {
            Error::from(e).context(format!(
                "{} is a truncated or corrupt docker-archive: a tar entry \
                 cannot be read. {}",
                path.display(),
                docker_export_remedy(path)
            ))
        })?;
        let entry_path = entry
            .path()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let normalized = entry_path.trim_start_matches("./").to_string();
        if !entry.header().entry_type().is_file() {
            continue;
        }
        // Bounded JSON reads: header size first, then a capped read, so a
        // hostile archive cannot make Hermit buffer an unbounded "config".
        let declared_size = entry.header().size().unwrap_or(0);
        if normalized == "manifest.json" || normalized.ends_with(".json") {
            if declared_size > MAX_JSON_BYTES {
                return Err(Error::msg(format!(
                    "docker-archive entry {normalized:?} in {} exceeds the \
                     {MAX_JSON_BYTES}-byte limit for image JSON documents; \
                     refusing to buffer it. Re-export the image.",
                    path.display()
                )));
            }
            let bytes = read_bounded(
                &mut entry,
                MAX_JSON_BYTES,
                &format!("docker-archive entry {normalized} in {}", path.display()),
            )
            .map_err(|e| {
                Error::msg(format!(
                    "{e} ({} is a truncated or corrupt docker-archive: {normalized} \
                     cannot be read.)",
                    path.display()
                ))
            })?;
            if normalized == "manifest.json" {
                scan.manifest_json = Some(bytes);
            } else {
                scan.json_entries.insert(normalized, bytes);
            }
        } else if let Some(referenced) = spool_only {
            if !referenced.contains(&normalized) {
                // Unreferenced by manifest.json: never spooled.
                scan.entry_names.push(normalized);
                continue;
            }
            let Some(spool_dir) = &scan.spool else {
                continue;
            };
            let target = spool_dir.path().join(format!("entry-{index}"));
            index += 1;
            let mut out = std::fs::File::create(&target)
                .with_context(|| format!("Failed to spool docker-archive entry {normalized}"))?;
            let copied = std::io::copy(&mut entry, &mut out).map_err(|e| {
                Error::from(e).context(format!(
                    "{} is a truncated or corrupt docker-archive: {normalized} \
                     cannot be read.",
                    path.display()
                ))
            })?;
            spooled_bytes = spooled_bytes.saturating_add(copied);
            if spooled_bytes > MAX_TOTAL_EXPANDED_BYTES {
                return Err(Error::msg(format!(
                    "docker-archive {} exceeds the {MAX_TOTAL_EXPANDED_BYTES}-byte \
                     quota for spooled layer bytes; refusing the image. \
                     Re-export it with smaller layers.",
                    path.display()
                )));
            }
            scan.spooled.insert(normalized, target);
        } else {
            scan.entry_names.push(normalized);
        }
    }
    Ok(scan)
}

/// Parse the scanned archive's manifest.json and config.
fn docker_manifest_and_config(
    scan: &DockerArchiveScan,
    path: &Path,
) -> Result<(DockerManifestEntry, Vec<u8>), Error> {
    let Some(manifest_json) = &scan.manifest_json else {
        return Err(Error::msg(format!(
            "{} has no manifest.json, so it is not a docker-archive image. A \
             plain rootfs tar is not an image input. {}",
            path.display(),
            docker_export_remedy(path)
        )));
    };
    let manifest: Vec<DockerManifestEntry> = serde_json::from_slice(manifest_json)
        .with_context(|| format!("manifest.json in {} is not valid JSON", path.display()))?;
    let entry = match manifest.len() {
        1 => manifest.into_iter().next().expect("one manifest entry"),
        0 => {
            return Err(Error::msg(format!(
                "docker-archive {} contains no image (manifest.json is empty). {}",
                path.display(),
                docker_export_remedy(path)
            )));
        }
        n => {
            return Err(Error::msg(format!(
                "docker-archive {} contains {n} images, so the image is \
                 ambiguous. Save exactly one image per archive.",
                path.display()
            )));
        }
    };
    // On a non-spooling scan, layer presence is still checkable by name.
    if scan.spool.is_none() {
        for layer in &entry.layers {
            let normalized = layer.trim_start_matches("./");
            if !scan.entry_names.iter().any(|name| name == normalized) {
                return Err(Error::msg(format!(
                    "docker-archive {} is missing layer {layer:?} named by \
                     manifest.json. {}",
                    path.display(),
                    docker_export_remedy(path)
                )));
            }
        }
    }
    let config_name = entry.config.trim_start_matches("./").to_string();
    let config_bytes = scan
        .json_entries
        .get(&config_name)
        .cloned()
        .ok_or_else(|| {
            Error::msg(format!(
                "docker-archive {} is missing its config {config_name:?} named \
                 by manifest.json. {}",
                path.display(),
                docker_export_remedy(path)
            ))
        })?;
    Ok((entry, config_bytes))
}

/// Fully load and verify a docker-archive image. Two outer-tar passes:
/// the first reads names and JSON only so `manifest.json` can name the
/// referenced layers; the second spools exactly those layers (unreferenced
/// entries are never spooled).
fn load_docker_archive(path: &Path) -> Result<LoadedImage, Error> {
    let first = scan_docker_archive(path, None)?;
    let (entry, config_bytes) = docker_manifest_and_config(&first, path)?;
    let referenced: BTreeSet<String> = entry
        .layers
        .iter()
        .map(|layer| layer.trim_start_matches("./").to_string())
        .collect();
    let scan = scan_docker_archive(path, Some(&referenced))?;
    let identity = format!("docker-archive@sha256:{}", sha256_hex(&config_bytes));
    let (config, diff_ids) = parse_image_config(&config_bytes, "the image config")?;
    if entry.layers.len() != diff_ids.len() {
        return Err(Error::msg(format!(
            "docker-archive {} lists {} layers but the config declares {} \
             rootfs.diff_ids; the image is inconsistent. Re-export the image.",
            path.display(),
            entry.layers.len(),
            diff_ids.len()
        )));
    }
    let mut layers = Vec::new();
    for (i, layer_name) in entry.layers.iter().enumerate() {
        let normalized = layer_name.trim_start_matches("./");
        let spooled = scan.spooled.get(normalized).ok_or_else(|| {
            Error::msg(format!(
                "docker-archive {} is missing layer {layer_name:?} named by \
                 manifest.json. {}",
                path.display(),
                docker_export_remedy(path)
            ))
        })?;
        let compression = sniff_compression(spooled, &format!("layer {i} ({layer_name})"))?;
        let layer_file = LayerFile {
            path: spooled.clone(),
            compression,
            origin: format!("docker-archive layer {layer_name}"),
        };
        verify_diff_id(&layer_file, &diff_ids[i], &format!("layer {i}"))?;
        layers.push(layer_file);
    }
    Ok(LoadedImage {
        identity,
        config,
        layers,
        _spool: scan.spool,
    })
}

/// Load and fully verify a local image of either kind.
pub(crate) fn load_local_image(kind: LocalImageKind, path: &Path) -> Result<LoadedImage, Error> {
    match kind {
        LocalImageKind::OciDir => load_oci_dir(path),
        LocalImageKind::DockerArchive => load_docker_archive(path),
    }
}

/// The namespaced cache identity of a local image, without layer
/// verification (manifest/config digests are still verified for oci-dir).
/// Used to find the cache directory before materialization.
pub(crate) fn local_identity(kind: LocalImageKind, path: &Path) -> Result<String, Error> {
    match kind {
        LocalImageKind::OciDir => Ok(format!(
            "oci-dir@sha256:{}",
            load_oci_metadata(path)?.manifest_digest
        )),
        LocalImageKind::DockerArchive => {
            let scan = scan_docker_archive(path, None)?;
            let (_, config_bytes) = docker_manifest_and_config(&scan, path)?;
            Ok(format!(
                "docker-archive@sha256:{}",
                sha256_hex(&config_bytes)
            ))
        }
    }
}

/// The declared run config of a local image, without layer verification.
/// Materialization (which always precedes guest execution) performs the
/// full verification; this only answers Entrypoint/Cmd questions early.
pub(crate) fn peek_local_config(
    kind: LocalImageKind,
    path: &Path,
) -> Result<ImageRunConfig, Error> {
    match kind {
        LocalImageKind::OciDir => Ok(load_oci_metadata(path)?.config),
        LocalImageKind::DockerArchive => {
            let scan = scan_docker_archive(path, None)?;
            let (_, config_bytes) = docker_manifest_and_config(&scan, path)?;
            Ok(parse_image_config(&config_bytes, "the image config")?.0)
        }
    }
}

// ---------------------------------------------------------------------------
// Layer application: sequential tar with whiteouts, confined to the rootfs
// ---------------------------------------------------------------------------

/// A directory whose mode/mtime are applied after every layer has streamed,
/// so later entries created inside it cannot clobber them. The path is kept
/// root-relative: the deferred pass re-runs symlink-ancestor confinement and
/// opens the directory with `openat(O_NOFOLLOW)` per component immediately
/// before touching it, so an ancestor replaced by a symlink after the entry
/// streamed cannot redirect the chmod/utimens outside the rootfs.
struct DeferredDir {
    rel: PathBuf,
    mode: u32,
    mtime_sec: u64,
    mtime_nsec: u32,
}

/// Bookkeeping shared across all layers of one materialization.
#[derive(Default)]
struct ApplyState {
    /// Entries whose layer uid/gid differed from the invoking user's and so
    /// were flattened by construction (Hermit creates them unprivileged).
    flattened: usize,
    /// Device entries skipped under `dev/`.
    skipped_devices: usize,
    /// Total payload bytes written, against [`MAX_TOTAL_EXPANDED_BYTES`].
    expanded_total: u64,
    deferred_dirs: Vec<DeferredDir>,
}

/// Apply every layer of `image` onto `rootfs`, base first, then fix up
/// directory modes/mtimes. Logs the uid/gid flattening count and any skipped
/// device entries.
pub(crate) fn apply_layers(image: &LoadedImage, rootfs: &Path) -> Result<(), Error> {
    let mut state = ApplyState::default();
    for layer in &image.layers {
        // Two passes per layer, matching the podman-store path
        // (`image::apply_overlay_layer` applies whiteouts before copying
        // payload): whiteouts act on the lower-layer tree only.
        let reader = open_layer(layer)?;
        apply_layer_stream(reader, rootfs, &layer.origin, &mut state, true)?;
        let reader = open_layer(layer)?;
        apply_layer_stream(reader, rootfs, &layer.origin, &mut state, false)?;
    }
    for dir in &state.deferred_dirs {
        // Re-run confinement at touch time: an ancestor may have been
        // replaced by a symlink by a later entry. open_dir_nofollow then
        // pins the directory by fd so the chmod/utimens cannot be
        // redirected either.
        ensure_no_symlink_ancestors(rootfs, &dir.rel, "deferred directory metadata")?;
        let dest = rootfs.join(&dir.rel);
        match std::fs::symlink_metadata(&dest) {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
            // Deleted or replaced by a later layer/entry: stale, drop it.
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(Error::from(e).context(format!("Failed to inspect {}", dest.display())));
            }
        }
        let fd_file = open_dir_nofollow(rootfs, &dir.rel)?;
        set_mode_fd(&fd_file, dir.mode, &dest)?;
        set_mtime_fd(&fd_file, dir.mtime_sec, dir.mtime_nsec, &dest)?;
    }
    if state.flattened > 0 {
        tracing::info!(
            "Image layers carried non-invoking-user uid/gid on {} entries; \
             ownership flattened to the invoking user (uid/gid are not \
             preserved by local image materialization)",
            state.flattened
        );
    }
    if state.skipped_devices > 0 {
        tracing::info!(
            "Skipped {} device entries under /dev in image layers; Hermit \
             binds its own device nodes",
            state.skipped_devices
        );
    }
    Ok(())
}

fn my_ids() -> (u32, u32) {
    // SAFETY: getuid/getgid always succeed.
    unsafe { (libc::getuid(), libc::getgid()) }
}

fn set_mode(path: &Path, mode: u32) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o7777))
        .with_context(|| format!("Failed to set mode {mode:o} on {}", path.display()))
}

fn timespecs(sec: u64, nsec: u32) -> [libc::timespec; 2] {
    let secs = libc::time_t::try_from(sec).unwrap_or(libc::time_t::MAX);
    let nsecs = libc::c_long::from(nsec.min(999_999_999));
    [
        libc::timespec {
            tv_sec: secs,
            tv_nsec: nsecs,
        },
        libc::timespec {
            tv_sec: secs,
            tv_nsec: nsecs,
        },
    ]
}

/// Set atime=mtime to `sec`+`nsec`. Tar carries no separate atime; the
/// subsecond part comes from a PAX `mtime` record when present. (The
/// `filetime` crate is only a transitive lockfile entry, not a direct
/// dependency of this crate, so the libc calls the code already used carry
/// the nanoseconds.)
fn set_mtime(path: &Path, sec: u64, nsec: u32, follow: bool) -> Result<(), Error> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("path {} contains a NUL byte", path.display()))?;
    let times = timespecs(sec, nsec);
    let flags = if follow { 0 } else { libc::AT_SYMLINK_NOFOLLOW };
    // SAFETY: c_path is a valid C string; times points to two timespecs.
    let ret = unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), flags) };
    if ret != 0 {
        return Err(Error::from(std::io::Error::last_os_error())
            .context(format!("Failed to set mtime on {}", path.display())));
    }
    Ok(())
}

/// Open `rootfs/rel` as a directory without following any symlink component,
/// walking `openat(O_NOFOLLOW|O_DIRECTORY)` from the rootfs fd. Refuses by
/// name when a component is a symlink (`ELOOP`).
fn open_dir_nofollow(rootfs: &Path, rel: &Path) -> Result<std::fs::File, Error> {
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;
    fn open_one(parent: &std::fs::File, name: &std::ffi::OsStr) -> Result<std::fs::File, Error> {
        let c_name = std::ffi::CString::new(name.as_bytes())
            .with_context(|| format!("path component {name:?} contains a NUL byte"))?;
        // SAFETY: parent is a live directory fd; c_name is a valid C string.
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                c_name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ELOOP) {
                return Err(Error::msg(format!(
                    "symlink ancestor in deferred directory metadata: \
                     component {name:?} is a symlink, and setting directory \
                     metadata through it could escape the image root. \
                     Re-export the image without entries beneath symlinks."
                )));
            }
            return Err(
                Error::from(err).context(format!("Failed to open directory component {name:?}"))
            );
        }
        // SAFETY: fd is a fresh owned descriptor from openat.
        Ok(unsafe { std::fs::File::from_raw_fd(fd) })
    }
    let mut current = std::fs::File::open(rootfs)
        .with_context(|| format!("Failed to open rootfs {}", rootfs.display()))?;
    for component in rel.components() {
        let Component::Normal(part) = component else {
            continue;
        };
        current = open_one(&current, part)?;
    }
    Ok(current)
}

fn set_mode_fd(file: &std::fs::File, mode: u32, shown: &Path) -> Result<(), Error> {
    use std::os::fd::AsRawFd;
    // SAFETY: file is a live fd for the directory being chmod'ed.
    let ret = unsafe { libc::fchmod(file.as_raw_fd(), (mode & 0o7777) as libc::mode_t) };
    if ret != 0 {
        return Err(
            Error::from(std::io::Error::last_os_error()).context(format!(
                "Failed to set mode {mode:o} on {}",
                shown.display()
            )),
        );
    }
    Ok(())
}

fn set_mtime_fd(file: &std::fs::File, sec: u64, nsec: u32, shown: &Path) -> Result<(), Error> {
    use std::os::fd::AsRawFd;
    let times = timespecs(sec, nsec);
    // SAFETY: file is a live fd; times points to two timespecs.
    let ret = unsafe { libc::futimens(file.as_raw_fd(), times.as_ptr()) };
    if ret != 0 {
        return Err(Error::from(std::io::Error::last_os_error())
            .context(format!("Failed to set mtime on {}", shown.display())));
    }
    Ok(())
}

/// Reduce a tar entry path to a rootfs-relative path, refusing absolute
/// paths and `..` traversal by name. An empty result is the root itself.
fn normalize_entry_path(path: &Path, origin: &str) -> Result<PathBuf, Error> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(Error::msg(format!(
                    "path traversal in {origin}: entry {path:?} contains \
                     \"../\", which would escape the image root. Re-export the \
                     image with confined relative paths."
                )));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(Error::msg(format!(
                    "absolute path in {origin}: entry {path:?} is not relative \
                     to the image root. Re-export the image with relative \
                     layer paths."
                )));
            }
        }
    }
    Ok(out)
}

/// Refuse to write `rootfs/rel` when any existing ancestor of `rel` (above its
/// final component) is a symlink: following it would write outside the rootfs
/// wherever the link points.
fn ensure_no_symlink_ancestors(rootfs: &Path, rel: &Path, origin: &str) -> Result<(), Error> {
    let mut current = rootfs.to_path_buf();
    let mut shown = PathBuf::new();
    let components: Vec<_> = rel.components().collect();
    for component in components.iter().take(components.len().saturating_sub(1)) {
        let Component::Normal(part) = component else {
            continue;
        };
        current.push(part);
        shown.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(Error::msg(format!(
                    "symlink ancestor in {origin}: {shown:?} is a symlink, and \
                     writing {rel:?} through it could escape the image root. \
                     Re-export the image without entries beneath symlinks."
                )));
            }
            Ok(meta) if !meta.is_dir() => {
                return Err(Error::msg(format!(
                    "in {origin}: ancestor {shown:?} of {rel:?} is not a \
                     directory, so the entry cannot be created. Re-export \
                     the image with a consistent tree."
                )));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(
                    Error::from(e).context(format!("Failed to inspect {}", current.display()))
                );
            }
        }
    }
    Ok(())
}

/// Remove whatever is at `path` (file, symlink, or whole directory tree).
fn remove_existing(path: &Path) -> Result<(), Error> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {
            // A directory from an earlier entry may carry a restrictive
            // deferred mode only after the final pass, so removal here still
            // works as the owner.
            std::fs::remove_dir_all(path)
                .with_context(|| format!("Failed to remove directory {}", path.display()))
        }
        Ok(_) => std::fs::remove_file(path)
            .with_context(|| format!("Failed to remove {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::from(e).context(format!("Failed to inspect {}", path.display()))),
    }
}

/// Ensure `rootfs/rel`'s parent directory chain exists as real directories.
/// A parent created here only implicitly (no explicit tar entry) gets
/// deterministic deferred metadata — mode `0o755`, mtime `0` — instead of
/// the host umask and wall clock; a later explicit directory entry for the
/// same path pushes its own deferred record after this one and wins.
fn ensure_parent_dirs(
    rootfs: &Path,
    rel: &Path,
    origin: &str,
    state: &mut ApplyState,
) -> Result<PathBuf, Error> {
    ensure_no_symlink_ancestors(rootfs, rel, origin)?;
    let mut current = rootfs.to_path_buf();
    let mut current_rel = PathBuf::new();
    if let Some(parent) = rel.parent() {
        for component in parent.components() {
            let Component::Normal(part) = component else {
                continue;
            };
            current.push(part);
            current_rel.push(part);
            match std::fs::symlink_metadata(&current) {
                Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
                Ok(_) => {
                    // ensure_no_symlink_ancestors already refused symlink
                    // and non-directory ancestors by name.
                    return Err(Error::msg(format!(
                        "in {origin}: ancestor {current_rel:?} of {rel:?} is \
                         not a directory, so the entry cannot be created. \
                         Re-export the image with a consistent tree."
                    )));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    std::fs::create_dir(&current).with_context(|| {
                        format!("Failed to create directory {}", current.display())
                    })?;
                    set_mode(&current, 0o755)?;
                    state.deferred_dirs.push(DeferredDir {
                        rel: current_rel.clone(),
                        mode: 0o755,
                        mtime_sec: 0,
                        mtime_nsec: 0,
                    });
                }
                Err(e) => {
                    return Err(
                        Error::from(e).context(format!("Failed to inspect {}", current.display()))
                    );
                }
            }
        }
    }
    Ok(rootfs.join(rel))
}

/// Apply an overlay whiteout named `.wh.<name>` (or the opaque marker) found
/// at rootfs-relative `rel`. The whiteout target must be exactly one
/// ordinary filename component: a target of `.` or `..` (from `.wh..` /
/// `.wh...`, including at the image root) would name the containing
/// directory itself or escape the rootfs to its parent, so it is refused
/// by name and nothing is removed. Removal therefore never targets the
/// rootfs itself or anything above it.
fn apply_whiteout(rootfs: &Path, rel: &Path, name: &str, origin: &str) -> Result<(), Error> {
    let parent_rel = rel.parent().map(Path::to_path_buf).unwrap_or_default();
    if name == ".wh..wh..opq" {
        // rel's final component is the marker itself, so ancestors of rel
        // here means the chain above the directory being made opaque.
        ensure_no_symlink_ancestors(rootfs, rel, origin)?;
        let dir = rootfs.join(&parent_rel);
        if dir.is_dir() && !dir.is_symlink() {
            for child in std::fs::read_dir(&dir)
                .with_context(|| format!("Failed to read directory {}", dir.display()))?
            {
                remove_existing(&child?.path())?;
            }
        }
        return Ok(());
    }
    let Some(target_name) = name.strip_prefix(".wh.") else {
        return Ok(());
    };
    let is_plain_component = {
        let mut components = Path::new(target_name).components();
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
    };
    if !is_plain_component || target_name.contains('\0') {
        return Err(Error::msg(format!(
            "refusing whiteout {rel:?} in {origin}: its target {target_name:?} \
             is not a plain filename (a whiteout may only delete one named \
             entry in its own directory, never \".\" or \"..\"). Re-export \
             the image without this whiteout."
        )));
    }
    let target_rel = parent_rel.join(target_name);
    if target_rel.as_os_str().is_empty() {
        return Err(Error::msg(format!(
            "refusing whiteout {rel:?} in {origin}: it would delete the \
             image root itself. Re-export the image without this whiteout."
        )));
    }
    // Check the chain above the deleted entry (deleting the entry itself,
    // symlink or not, never follows it).
    ensure_no_symlink_ancestors(rootfs, &target_rel, origin)?;
    remove_existing(&rootfs.join(&target_rel))
}

/// Parse a PAX `mtime` value (`<sec>[.<fraction>]`) into `(sec, nsec)`,
/// preserving subsecond precision to nanoseconds.
fn parse_pax_mtime(value: &str) -> Option<(u64, u32)> {
    let (sec, frac) = match value.split_once('.') {
        Some((sec, frac)) => (sec, frac),
        None => (value, ""),
    };
    let sec: u64 = sec.parse().ok()?;
    if !frac.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let mut digits = String::from(&frac[..frac.len().min(9)]);
    while digits.len() < 9 {
        digits.push('0');
    }
    let nsec: u32 = if digits.is_empty() {
        0
    } else {
        digits.parse().ok()?
    };
    Some((sec, nsec))
}

/// Metadata collected from one entry's PAX extensions: its `user.*`
/// xattrs and its optional `mtime` override as `(sec, nsec)`.
type PaxMetadata = (Vec<(String, Vec<u8>)>, Option<(u64, u32)>);

/// Collect an entry's pax-derived metadata: its `user.*` xattrs (refusing
/// any other xattr namespace by name: unprivileged materialization cannot
/// set `security.*`/`trusted.*`, and silently dropping a capability xattr
/// would change what the guest runs) and its optional PAX `mtime`
/// override, which is where subsecond mtimes live.
fn collect_pax<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    rel: &Path,
    origin: &str,
) -> Result<PaxMetadata, Error> {
    let mut xattrs = Vec::new();
    let mut mtime = None;
    let extensions = entry.pax_extensions().map_err(|e| {
        Error::from(e).context(format!("corrupt pax extensions for {rel:?} in {origin}"))
    })?;
    let Some(extensions) = extensions else {
        return Ok((xattrs, mtime));
    };
    for extension in extensions {
        let extension = extension.map_err(|e| {
            Error::from(e).context(format!("corrupt pax extensions for {rel:?} in {origin}"))
        })?;
        let Ok(key) = extension.key() else {
            continue;
        };
        if key == "mtime" {
            if let Ok(value) = extension.value() {
                mtime = parse_pax_mtime(value);
            }
            continue;
        }
        let Some(name) = key.strip_prefix("SCHILY.xattr.") else {
            continue;
        };
        if !name.starts_with("user.") {
            return Err(Error::msg(format!(
                "refusing non-user.* xattr {name:?} on {rel:?} in {origin}: \
                 only user.* extended attributes can be materialized \
                 unprivileged. Re-export the image without the {name:?} \
                 attribute."
            )));
        }
        xattrs.push((name.to_string(), extension.value_bytes().to_vec()));
    }
    Ok((xattrs, mtime))
}

fn apply_xattrs(path: &Path, xattrs: &[(String, Vec<u8>)]) -> Result<(), Error> {
    use std::os::unix::ffi::OsStrExt;
    if xattrs.is_empty() {
        return Ok(());
    }
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("path {} contains a NUL byte", path.display()))?;
    for (name, value) in xattrs {
        let c_name = std::ffi::CString::new(name.as_bytes())
            .with_context(|| format!("xattr name {name:?} contains a NUL byte"))?;
        // SAFETY: both C strings outlive the call; value is a byte buffer.
        let ret = unsafe {
            libc::lsetxattr(
                c_path.as_ptr(),
                c_name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        };
        if ret != 0 {
            return Err(
                Error::from(std::io::Error::last_os_error()).context(format!(
                    "Failed to set xattr {name:?} on {}",
                    path.display()
                )),
            );
        }
    }
    Ok(())
}

fn make_fifo(path: &Path, mode: u32) -> Result<(), Error> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("path {} contains a NUL byte", path.display()))?;
    // SAFETY: c_path is a valid C string.
    let ret = unsafe { libc::mkfifo(c_path.as_ptr(), (mode & 0o7777) as libc::mode_t) };
    if ret != 0 {
        return Err(Error::from(std::io::Error::last_os_error())
            .context(format!("Failed to create FIFO {}", path.display())));
    }
    Ok(())
}

/// Apply one pass of one layer's tar stream onto `rootfs`. With
/// `whiteouts_only`, only whiteout instructions are applied (against the
/// pre-layer tree); otherwise only payload entries are applied, in stream
/// order. See [`apply_layers`] for why a layer takes both passes.
fn apply_layer_stream<R: Read>(
    reader: R,
    rootfs: &Path,
    origin: &str,
    state: &mut ApplyState,
    whiteouts_only: bool,
) -> Result<(), Error> {
    let (my_uid, my_gid) = my_ids();
    let mut archive = tar::Archive::new(reader);
    let entries = archive
        .entries()
        .map_err(|e| tune_layer_read_error(e, origin))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| tune_layer_read_error(e, origin))?;
        let header = entry.header().clone();
        let raw_path = entry.path().map_err(|e| {
            Error::from(e).context(format!("unreadable tar entry path in {origin}"))
        })?;
        let rel = normalize_entry_path(&raw_path, origin)?;
        let mode = header.mode().unwrap_or(0o644);
        // Overlay whiteouts are instructions, not files. They apply in the
        // first pass only, against the lower-layer tree.
        let is_whiteout = rel
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| name.starts_with(".wh."));
        if whiteouts_only {
            if is_whiteout {
                let name = rel
                    .file_name()
                    .and_then(|n| n.to_str())
                    .expect("whiteout name checked above");
                // `name` borrows `rel`; copy it out before the call.
                let name = name.to_string();
                apply_whiteout(rootfs, &rel, &name, origin)?;
            }
            continue;
        }
        if is_whiteout {
            continue;
        }
        // Count ownership flattening once, in the payload pass.
        if (header.uid().unwrap_or(0) as u32) != my_uid
            || (header.gid().unwrap_or(0) as u32) != my_gid
        {
            state.flattened += 1;
        }
        if rel.as_os_str().is_empty() {
            // The root directory entry: finish_materialized_rootfs sets the
            // root mode, matching the store path.
            continue;
        }
        let (xattrs, pax_mtime) = collect_pax(&mut entry, &rel, origin)?;
        let (mtime_sec, mtime_nsec) = pax_mtime.unwrap_or((header.mtime().unwrap_or(0), 0));
        let entry_type = header.entry_type();
        if entry_type.is_dir() {
            let dest = ensure_parent_dirs(rootfs, &rel, origin, state)?;
            if let Ok(meta) = std::fs::symlink_metadata(&dest)
                && (!meta.is_dir() || meta.file_type().is_symlink())
            {
                remove_existing(&dest)?;
            }
            std::fs::create_dir_all(&dest)
                .with_context(|| format!("Failed to create directory {}", dest.display()))?;
            apply_xattrs(&dest, &xattrs)?;
            state.deferred_dirs.push(DeferredDir {
                rel: rel.clone(),
                mode,
                mtime_sec,
                mtime_nsec,
            });
        } else if entry_type.is_file() || entry_type == tar::EntryType::Continuous {
            let dest = ensure_parent_dirs(rootfs, &rel, origin, state)?;
            remove_existing(&dest)?;
            let mut out = std::fs::File::create(&dest)
                .with_context(|| format!("Failed to create {}", dest.display()))?;
            let written = std::io::copy(&mut entry, &mut out)
                .map_err(|e| tune_layer_read_error_ctx(e, origin, &rel))?;
            state.expanded_total = state.expanded_total.saturating_add(written);
            if state.expanded_total > MAX_TOTAL_EXPANDED_BYTES {
                return Err(Error::msg(format!(
                    "total expanded size of the image exceeds the \
                     {MAX_TOTAL_EXPANDED_BYTES}-byte quota at {rel:?} in \
                     {origin}; refusing the image. Re-export it with \
                     smaller layers."
                )));
            }
            drop(out);
            set_mode(&dest, mode)?;
            set_mtime(&dest, mtime_sec, mtime_nsec, true)?;
            apply_xattrs(&dest, &xattrs)?;
        } else if entry_type == tar::EntryType::Symlink {
            let target = entry
                .link_name()
                .map_err(|e| Error::from(e).context(format!("Unreadable link in {origin}")))?
                .ok_or_else(|| Error::msg(format!("symlink {rel:?} in {origin} has no target")))?;
            let dest = ensure_parent_dirs(rootfs, &rel, origin, state)?;
            remove_existing(&dest)?;
            std::os::unix::fs::symlink(&target, &dest)
                .with_context(|| format!("Failed to create symlink {}", dest.display()))?;
            set_mtime(&dest, mtime_sec, mtime_nsec, false)?;
            apply_xattrs(&dest, &xattrs)?;
        } else if entry_type == tar::EntryType::Link {
            let target = entry
                .link_name()
                .map_err(|e| Error::from(e).context(format!("Unreadable link in {origin}")))?
                .ok_or_else(|| Error::msg(format!("hardlink {rel:?} in {origin} has no target")))?;
            let target_rel = normalize_hardlink_target(&target, &rel, origin)?;
            ensure_no_symlink_ancestors(rootfs, &target_rel, origin)?;
            let source = rootfs.join(&target_rel);
            match std::fs::symlink_metadata(&source) {
                Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => {}
                _ => {
                    return Err(Error::msg(format!(
                        "hardlink {rel:?} in {origin} targets {target:?}, which \
                         is not a regular file materialized from an earlier \
                         entry. Re-export the image with the target present."
                    )));
                }
            }
            let dest = ensure_parent_dirs(rootfs, &rel, origin, state)?;
            remove_existing(&dest)?;
            std::fs::hard_link(&source, &dest).with_context(|| {
                format!(
                    "Failed to hardlink {} to {}",
                    dest.display(),
                    source.display()
                )
            })?;
        } else if entry_type == tar::EntryType::Fifo {
            let dest = ensure_parent_dirs(rootfs, &rel, origin, state)?;
            remove_existing(&dest)?;
            make_fifo(&dest, mode)?;
            set_mtime(&dest, mtime_sec, mtime_nsec, true)?;
            apply_xattrs(&dest, &xattrs)?;
        } else if entry_type == tar::EntryType::Char || entry_type == tar::EntryType::Block {
            let under_dev = rel
                .components()
                .next()
                .is_some_and(|c| c == Component::Normal(std::ffi::OsStr::new("dev")))
                && rel.components().count() > 1;
            if under_dev {
                // Never write through a symlinked dev/ on the way, either;
                // the entry is skipped, but a hostile tree must not be able
                // to make even the skip touch outside paths.
                ensure_no_symlink_ancestors(rootfs, &rel, origin)?;
                tracing::info!(
                    "Skipping device entry {rel:?} in {origin}; Hermit binds \
                     its own /dev nodes"
                );
                state.skipped_devices += 1;
            } else {
                return Err(Error::msg(format!(
                    "device entry {rel:?} outside /dev in {origin}: device \
                     nodes are only permitted (and skipped) under /dev, where \
                     Hermit binds its own. Re-export the image without device \
                     entries outside /dev."
                )));
            }
        } else {
            return Err(Error::msg(format!(
                "unsupported tar entry type {entry_type:?} for {rel:?} in \
                 {origin}. Re-export the image with ordinary file, \
                 directory, symlink, hardlink, and FIFO entries."
            )));
        }
    }
    Ok(())
}

/// A hardlink target must be an in-root relative path with no escape.
fn normalize_hardlink_target(target: &Path, rel: &Path, origin: &str) -> Result<PathBuf, Error> {
    match normalize_entry_path(target, origin) {
        Ok(p) if !p.as_os_str().is_empty() => Ok(p),
        _ => Err(Error::msg(format!(
            "escaping hardlink {rel:?} in {origin}: target {target:?} is not \
             an in-root relative path. Only hardlinks to files inside the \
             image root are supported; re-export the image without it."
        ))),
    }
}

fn tune_layer_read_error(e: std::io::Error, origin: &str) -> Error {
    Error::from(e).context(format!(
        "failed to read layer tar from {origin}: the layer is truncated or \
         corrupt (or its gzip stream is corrupt). Re-export the image."
    ))
}

fn tune_layer_read_error_ctx(e: std::io::Error, origin: &str, rel: &Path) -> Error {
    Error::from(e).context(format!(
        "failed to read {rel:?} from {origin}: the layer is truncated or \
         corrupt (or its gzip stream is corrupt). Re-export the image."
    ))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn parse_dispatches_only_rooted_prefixed_paths() {
        assert_eq!(
            parse_image_source("oci-dir:/srv/img"),
            ImageSourceRef::Local {
                kind: LocalImageKind::OciDir,
                path: PathBuf::from("/srv/img"),
            }
        );
        assert_eq!(
            parse_image_source("oci-dir:./img"),
            ImageSourceRef::Local {
                kind: LocalImageKind::OciDir,
                path: PathBuf::from("./img"),
            }
        );
        assert_eq!(
            parse_image_source("docker-archive:../img.tar"),
            ImageSourceRef::Local {
                kind: LocalImageKind::DockerArchive,
                path: PathBuf::from("../img.tar"),
            }
        );
    }

    #[test]
    fn parse_keeps_store_references_verbatim() {
        // A port-shaped remainder is a registry host:port reference whose
        // host happens to be spelled like a prefix.
        assert_eq!(
            parse_image_source("oci-dir:5000/img"),
            ImageSourceRef::StoreReference
        );
        assert_eq!(
            parse_image_source("docker-archive:5000/img:tag"),
            ImageSourceRef::StoreReference
        );
        assert_eq!(
            parse_image_source("docker.io/library/busybox:latest"),
            ImageSourceRef::StoreReference
        );
        assert_eq!(
            parse_image_source("busybox"),
            ImageSourceRef::StoreReference
        );
    }

    #[test]
    fn parse_refuses_bare_relative_prefixed_paths_by_name() {
        assert_eq!(
            parse_image_source("oci-dir:my-image"),
            ImageSourceRef::BareRelative {
                kind: LocalImageKind::OciDir,
                remainder: "my-image".to_string(),
            }
        );
        assert_eq!(
            parse_image_source("docker-archive:img.tar"),
            ImageSourceRef::BareRelative {
                kind: LocalImageKind::DockerArchive,
                remainder: "img.tar".to_string(),
            }
        );
        let err = bare_relative_error(LocalImageKind::OciDir, "my-image");
        assert!(format!("{err}").contains("./my-image"), "{err}");
    }

    /// One tar entry for applier tests.
    struct TestEntry {
        path: String,
        kind: tar::EntryType,
        mode: u32,
        mtime: u64,
        uid: u64,
        data: Vec<u8>,
        link: Option<String>,
    }

    fn file_entry(path: &str, data: &str) -> TestEntry {
        TestEntry {
            path: path.to_string(),
            kind: tar::EntryType::Regular,
            mode: 0o640,
            mtime: 1_700_000_000,
            uid: 0,
            data: data.as_bytes().to_vec(),
            link: None,
        }
    }

    fn build_layer(entries: &[TestEntry]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for entry in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(entry.kind);
            header.set_mode(entry.mode);
            header.set_mtime(entry.mtime);
            header.set_uid(entry.uid);
            header.set_gid(entry.uid);
            header.set_size(entry.data.len() as u64);
            if let Some(link) = &entry.link {
                header.set_link_name(link).unwrap();
            }
            builder
                .append_data(&mut header, &entry.path, entry.data.as_slice())
                .unwrap();
        }
        builder.into_inner().unwrap()
    }

    fn apply_to(rootfs: &Path, layer_bytes: &[u8]) -> Result<(), Error> {
        apply_many(rootfs, &[layer_bytes.to_vec()])
    }

    fn apply_many(rootfs: &Path, layers: &[Vec<u8>]) -> Result<(), Error> {
        let scratch = rootfs.parent().unwrap();
        let mut files = Vec::new();
        for (i, bytes) in layers.iter().enumerate() {
            let layer_path = scratch.join(format!("layer-{i}.tar"));
            std::fs::write(&layer_path, bytes).unwrap();
            files.push(LayerFile {
                path: layer_path,
                compression: Compression::Plain,
                origin: format!("test layer {i}"),
            });
        }
        let image = LoadedImage {
            identity: "test@sha256:0".to_string(),
            config: ImageRunConfig::default(),
            layers: files,
            _spool: None,
        };
        apply_layers(&image, rootfs)
    }

    fn dir_entry(path: &str, mode: u32, mtime: u64) -> TestEntry {
        let mut entry = file_entry(path, "");
        entry.kind = tar::EntryType::Directory;
        entry.mode = mode;
        entry.mtime = mtime;
        entry
    }

    #[test]
    fn applier_preserves_contents_mode_mtime_symlink_and_flattens_owner() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mut bin_dir = file_entry("bin", "");
        bin_dir.kind = tar::EntryType::Directory;
        bin_dir.mode = 0o750;
        bin_dir.mtime = 1_600_000_000;
        let mut prog = file_entry("bin/prog", "program-bytes");
        prog.mode = 0o751;
        prog.uid = 12345;
        let mut link = file_entry("bin/prog-link", "");
        link.kind = tar::EntryType::Symlink;
        link.link = Some("prog".to_string());
        let layer = build_layer(&[bin_dir, prog, link]);
        apply_to(&rootfs, &layer).unwrap();

        let prog_path = rootfs.join("bin/prog");
        assert_eq!(std::fs::read(&prog_path).unwrap(), b"program-bytes");
        let meta = std::fs::metadata(&prog_path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o7777, 0o751);
        assert_eq!(meta.mtime(), 1_700_000_000);
        let (uid, _) = my_ids();
        assert_eq!(meta.uid(), uid, "layer uid must flatten to the invoker");
        assert_eq!(
            std::fs::read_link(rootfs.join("bin/prog-link")).unwrap(),
            PathBuf::from("prog")
        );
        let dir_meta = std::fs::metadata(rootfs.join("bin")).unwrap();
        assert_eq!(dir_meta.permissions().mode() & 0o7777, 0o750);
        assert_eq!(dir_meta.mtime(), 1_600_000_000, "deferred dir mtime");
    }

    /// Store parity: a layer's whiteouts act on the lower-layer tree only
    /// (`image::apply_overlay_layer` deletes before copying payload), so a
    /// file created by the same layer survives its own whiteout, while a
    /// file from a lower layer is deleted.
    #[test]
    fn applier_applies_whiteouts_to_lower_layers_only() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let base = build_layer(&[
            file_entry("keep.txt", "keep"),
            file_entry("gone.txt", "gone"),
        ]);
        let upper = build_layer(&[
            file_entry("same-layer.txt", "new"),
            file_entry(".wh.same-layer.txt", ""),
            file_entry(".wh.gone.txt", ""),
        ]);
        apply_many(&rootfs, &[base, upper]).unwrap();
        assert!(rootfs.join("keep.txt").exists());
        assert!(
            !rootfs.join("gone.txt").exists(),
            "whiteout must delete the lower-layer file"
        );
        assert_eq!(
            std::fs::read(rootfs.join("same-layer.txt")).unwrap(),
            b"new",
            "same-layer creation must survive the layer's whiteout"
        );
        assert!(!rootfs.join(".wh.gone.txt").exists());
        assert!(!rootfs.join(".wh.same-layer.txt").exists());
    }

    /// Both stream orderings of an opaque whiteout relative to same-layer
    /// content give the store result: lower content cleared, same-layer
    /// content kept.
    #[test]
    fn applier_opaque_whiteout_keeps_same_layer_content_in_either_order() {
        for content_first in [true, false] {
            let tmp = tempfile::TempDir::new().unwrap();
            let rootfs = tmp.path().join("rootfs");
            std::fs::create_dir_all(&rootfs).unwrap();
            let base = build_layer(&[
                dir_entry("d", 0o755, 1_600_000_000),
                file_entry("d/old.txt", "old"),
            ]);
            let new_file = file_entry("d/new.txt", "new");
            let opaque = file_entry("d/.wh..wh..opq", "");
            let upper = if content_first {
                build_layer(&[new_file, opaque])
            } else {
                build_layer(&[opaque, new_file])
            };
            apply_many(&rootfs, &[base, upper]).unwrap();
            assert!(
                !rootfs.join("d/old.txt").exists(),
                "opaque must clear lower content (content_first={content_first})"
            );
            assert_eq!(
                std::fs::read(rootfs.join("d/new.txt")).unwrap(),
                b"new",
                "same-layer content must survive opaque (content_first={content_first})"
            );
        }
    }

    /// H1: a whiteout whose target is `.` or `..` names the containing
    /// directory or escapes the root; it must be refused by name and
    /// remove nothing — in particular not the rootfs or its parent.
    #[test]
    fn applier_refuses_whiteouts_targeting_dot_or_dotdot() {
        for whiteout in [".wh...", ".wh..", "sub/.wh..", "sub/.wh..."] {
            let tmp = tempfile::TempDir::new().unwrap();
            let rootfs = tmp.path().join("rootfs");
            std::fs::create_dir_all(rootfs.join("sub")).unwrap();
            std::fs::write(rootfs.join("sub/keep.txt"), b"keep").unwrap();
            let sentinel = tmp.path().join("sentinel.txt");
            std::fs::write(&sentinel, b"host-sentinel").unwrap();
            let mut entries = vec![];
            if whiteout.starts_with("sub/") {
                entries.push(dir_entry("sub", 0o755, 1_600_000_000));
            }
            entries.push(file_entry(whiteout, ""));
            let layer = build_layer(&entries);
            let err = apply_to(&rootfs, &layer).unwrap_err();
            assert!(
                format!("{err:#}").contains("refusing whiteout"),
                "{whiteout}: {err:#}"
            );
            assert_eq!(
                std::fs::read(&sentinel).unwrap(),
                b"host-sentinel",
                "{whiteout}: host sentinel outside the rootfs must be untouched"
            );
            assert_eq!(
                std::fs::read(rootfs.join("sub/keep.txt")).unwrap(),
                b"keep",
                "{whiteout}: nothing inside the rootfs may be removed"
            );
            assert!(rootfs.is_dir(), "{whiteout}: rootfs itself must remain");
        }
    }

    /// H2: a deferred directory whose ancestor is replaced by a symlink
    /// after the directory streamed must be refused at deferred-fixup
    /// time, never chmod'ed/utime'ed through the link.
    #[test]
    fn applier_deferred_dir_refuses_symlink_ancestor_replacement() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        // Host sentinel tree outside the rootfs: outside/b with a known
        // mode and mtime in the past.
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(outside.join("b")).unwrap();
        std::fs::set_permissions(outside.join("b"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        set_mtime(&outside.join("b"), 1_500_000_000, 0, true).unwrap();
        let mut link = file_entry("a", "");
        link.kind = tar::EntryType::Symlink;
        link.link = Some(outside.to_string_lossy().into_owned());
        let layer = build_layer(&[
            dir_entry("a", 0o755, 1_600_000_000),
            dir_entry("a/b", 0o777, 1_700_000_000),
            link,
        ]);
        let err = apply_to(&rootfs, &layer).unwrap_err();
        assert!(format!("{err:#}").contains("symlink ancestor"), "{err:#}");
        let meta = std::fs::metadata(outside.join("b")).unwrap();
        assert_eq!(
            meta.permissions().mode() & 0o7777,
            0o700,
            "deferred chmod must not reach through the replaced ancestor"
        );
        assert_eq!(
            meta.mtime(),
            1_500_000_000,
            "deferred utimens must not reach through the replaced ancestor"
        );
    }

    /// M4: implicit parents get mode 0o755 / mtime 0, and a later explicit
    /// directory entry overrides the implicit record.
    #[test]
    fn applier_implicit_parents_are_deterministic_and_explicit_dirs_win() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let layer = build_layer(&[file_entry("a/b/c.txt", "data")]);
        apply_to(&rootfs, &layer).unwrap();
        for rel in ["a", "a/b"] {
            let meta = std::fs::metadata(rootfs.join(rel)).unwrap();
            assert_eq!(meta.permissions().mode() & 0o7777, 0o755, "{rel}");
            assert_eq!(meta.mtime(), 0, "{rel}");
        }

        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let layer = build_layer(&[
            file_entry("x/file.txt", "data"),
            dir_entry("x", 0o750, 1_600_000_123),
        ]);
        apply_to(&rootfs, &layer).unwrap();
        let meta = std::fs::metadata(rootfs.join("x")).unwrap();
        assert_eq!(meta.permissions().mode() & 0o7777, 0o750);
        assert_eq!(meta.mtime(), 1_600_000_123);
    }

    /// Rewrite a built tar's placeholder entry name to `target` (same
    /// length) and fix the header checksum. The tar *writer* refuses `..`
    /// and absolute names, so hostile reader inputs are patched in.
    fn patch_entry_name(tar: &mut [u8], placeholder: &str, target: &str) {
        assert_eq!(placeholder.len(), target.len());
        let at = tar
            .windows(placeholder.len())
            .position(|w| w == placeholder.as_bytes())
            .expect("placeholder name in built tar");
        tar[at..at + target.len()].copy_from_slice(target.as_bytes());
        let block = at / 512 * 512;
        for byte in &mut tar[block + 148..block + 156] {
            *byte = b' ';
        }
        let sum: u32 = tar[block..block + 512].iter().map(|b| *b as u32).sum();
        let encoded = format!("{sum:06o}\0 ");
        tar[block + 148..block + 156].copy_from_slice(encoded.as_bytes());
    }

    #[test]
    fn applier_refuses_traversal_absolute_and_symlink_ancestors() {
        for (entries, patch, needle) in [
            (
                vec![file_entry("zz/evil", "x")],
                Some(("zz/evil", "../evil")),
                "path traversal",
            ),
            (
                vec![file_entry("zabs/evil", "x")],
                Some(("zabs/evil", "/abs/evil")),
                "absolute path",
            ),
            (
                vec![
                    {
                        let mut e = file_entry("link", "");
                        e.kind = tar::EntryType::Symlink;
                        e.link = Some("/etc".to_string());
                        e
                    },
                    file_entry("link/pwn", "x"),
                ],
                None,
                "symlink ancestor",
            ),
        ] {
            let tmp = tempfile::TempDir::new().unwrap();
            let rootfs = tmp.path().join("rootfs");
            std::fs::create_dir_all(&rootfs).unwrap();
            let mut layer = build_layer(&entries);
            if let Some((placeholder, target)) = patch {
                patch_entry_name(&mut layer, placeholder, target);
            }
            let err = apply_to(&rootfs, &layer).unwrap_err();
            assert!(format!("{err:#}").contains(needle), "{needle}: {err:#}");
        }
    }

    #[test]
    fn applier_refuses_devices_outside_dev_and_escaping_hardlinks() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mut device = file_entry("usr/tty0", "");
        device.kind = tar::EntryType::Char;
        let layer = build_layer(&[device]);
        let err = apply_to(&rootfs, &layer).unwrap_err();
        assert!(format!("{err:#}").contains("outside /dev"), "{err:#}");

        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mut hardlink = file_entry("escape", "");
        hardlink.kind = tar::EntryType::Link;
        hardlink.link = Some("/etc/passwd".to_string());
        let layer = build_layer(&[hardlink]);
        let err = apply_to(&rootfs, &layer).unwrap_err();
        assert!(format!("{err:#}").contains("escaping hardlink"), "{err:#}");
    }

    #[test]
    fn applier_refuses_non_user_xattrs_by_name() {
        // Hand-build a pax header carrying SCHILY.xattr.security.capability.
        let record = {
            let kv = "SCHILY.xattr.security.capability=cap";
            let mut len = kv.len() + 2;
            loop {
                let total = len.to_string().len() + 1 + kv.len() + 1;
                if total == len {
                    break;
                }
                len = total;
            }
            format!("{len} {kv}\n")
        };
        let mut builder = tar::Builder::new(Vec::new());
        let mut pax = tar::Header::new_gnu();
        pax.set_entry_type(tar::EntryType::XHeader);
        pax.set_size(record.len() as u64);
        builder
            .append_data(&mut pax, "PaxHeaders/file", record.as_bytes())
            .unwrap();
        let entry = file_entry("file", "data");
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(entry.kind);
        header.set_mode(entry.mode);
        header.set_size(entry.data.len() as u64);
        builder
            .append_data(&mut header, &entry.path, entry.data.as_slice())
            .unwrap();
        let layer = builder.into_inner().unwrap();

        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let err = apply_to(&rootfs, &layer).unwrap_err();
        assert!(format!("{err:#}").contains("non-user.* xattr"), "{err:#}");
        assert!(
            format!("{err:#}").contains("security.capability"),
            "{err:#}"
        );
    }

    fn pax_record(kv: &str) -> String {
        let mut len = kv.len() + 2;
        loop {
            let total = len.to_string().len() + 1 + kv.len() + 1;
            if total == len {
                break;
            }
            len = total;
        }
        format!("{len} {kv}\n")
    }

    /// M4: a PAX `mtime` with fractional seconds survives to the file.
    #[test]
    fn applier_preserves_pax_subsecond_mtime() {
        let record = pax_record("mtime=1700000000.123456789");
        let mut builder = tar::Builder::new(Vec::new());
        let mut pax = tar::Header::new_gnu();
        pax.set_entry_type(tar::EntryType::XHeader);
        pax.set_size(record.len() as u64);
        builder
            .append_data(&mut pax, "PaxHeaders/file", record.as_bytes())
            .unwrap();
        let entry = file_entry("file", "data");
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(entry.kind);
        header.set_mode(entry.mode);
        header.set_mtime(1_700_000_000);
        header.set_size(entry.data.len() as u64);
        builder
            .append_data(&mut header, &entry.path, entry.data.as_slice())
            .unwrap();
        let layer = builder.into_inner().unwrap();

        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        apply_to(&rootfs, &layer).unwrap();
        let meta = std::fs::metadata(rootfs.join("file")).unwrap();
        assert_eq!(meta.mtime(), 1_700_000_000);
        assert_eq!(meta.mtime_nsec(), 123_456_789);
    }

    #[test]
    fn pax_mtime_parsing_preserves_fraction_digits() {
        assert_eq!(parse_pax_mtime("1700000000"), Some((1_700_000_000, 0)));
        assert_eq!(
            parse_pax_mtime("1700000000.5"),
            Some((1_700_000_000, 500_000_000))
        );
        assert_eq!(
            parse_pax_mtime("1700000000.1234567899"),
            Some((1_700_000_000, 123_456_789))
        );
        assert_eq!(parse_pax_mtime("soon"), None);
    }

    /// M3: an oversize OCI index is refused by name under the stated
    /// [`MAX_JSON_BYTES`] bound instead of being buffered.
    #[test]
    fn oci_index_over_json_limit_is_refused_by_name() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("oci");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("oci-layout"), br#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();
        let mut index = b"{}".to_vec();
        index.resize((MAX_JSON_BYTES + 1) as usize, b' ');
        std::fs::write(dir.join("index.json"), index).unwrap();
        let err = match load_oci_metadata(&dir) {
            Ok(_) => panic!("oversize index must be refused"),
            Err(err) => err,
        };
        assert!(
            format!("{err:#}").contains("exceeds"),
            "oversize index must be refused by name: {err:#}"
        );
        assert!(
            format!("{err:#}").contains(&MAX_JSON_BYTES.to_string()),
            "{err:#}"
        );
    }

    /// M3: a gzip layer whose expanded tar passes the quota fails
    /// `diff_id` verification with the quota named (exercised through the
    /// bounded helper with a small limit; production uses
    /// [`MAX_LAYER_EXPANDED_BYTES`]).
    #[test]
    fn gzip_expansion_over_quota_is_refused_by_name() {
        use std::io::Write;
        let tmp = tempfile::TempDir::new().unwrap();
        let expanded = vec![0u8; 2 * 1024 * 1024];
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&expanded).unwrap();
        let gzipped = encoder.finish().unwrap();
        assert!(gzipped.len() < expanded.len() / 4, "fixture must expand");
        let layer_path = tmp.path().join("bomb.tar.gz");
        std::fs::write(&layer_path, gzipped).unwrap();
        let layer = LayerFile {
            path: layer_path,
            compression: Compression::Gzip,
            origin: "test gzip layer".to_string(),
        };
        let err = verify_diff_id_with_limit(&layer, &sha256_hex(&expanded), "layer 0", 1_048_576)
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("quota"),
            "expansion refusal must name the quota: {err:#}"
        );
    }

    /// M3: only manifest-referenced docker-archive layers are spooled; an
    /// unreferenced entry never lands in the spool directory.
    #[test]
    fn docker_archive_spools_only_referenced_layers() {
        let tmp = tempfile::TempDir::new().unwrap();
        let layer = build_layer(&[file_entry("hello.txt", "hi")]);
        let config = serde_json::to_vec(&serde_json::json!({
            "architecture": "amd64",
            "os": "linux",
            "config": {},
            "rootfs": {"type": "layers", "diff_ids": [format!("sha256:{}", sha256_hex(&layer))]},
        }))
        .unwrap();
        let manifest = serde_json::to_vec(&serde_json::json!([{
            "Config": "config.json",
            "RepoTags": ["t:latest"],
            "Layers": ["layer.tar"],
        }]))
        .unwrap();
        let junk = vec![0xa5u8; 256 * 1024];
        let config_text = String::from_utf8(config).unwrap();
        let manifest_text = String::from_utf8(manifest).unwrap();
        let outer = build_layer(&[
            file_entry("config.json", &config_text),
            TestEntry {
                path: "unreferenced.bin".to_string(),
                kind: tar::EntryType::Regular,
                mode: 0o644,
                mtime: 0,
                uid: 0,
                data: junk,
                link: None,
            },
            TestEntry {
                path: "layer.tar".to_string(),
                kind: tar::EntryType::Regular,
                mode: 0o644,
                mtime: 0,
                uid: 0,
                data: layer,
                link: None,
            },
            file_entry("manifest.json", &manifest_text),
        ]);
        let archive = tmp.path().join("image.tar");
        std::fs::write(&archive, outer).unwrap();
        let loaded = load_docker_archive(&archive).unwrap();
        assert_eq!(loaded.layers.len(), 1);
        let spool = loaded._spool.as_ref().expect("docker layers are spooled");
        let spooled: Vec<_> = std::fs::read_dir(spool.path()).unwrap().collect();
        assert_eq!(
            spooled.len(),
            1,
            "only the one referenced layer may be spooled"
        );
    }
}
