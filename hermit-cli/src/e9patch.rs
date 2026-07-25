/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Content-addressed offline rewriting for the experimental e9patch backend.

use std::env;
use std::fs;
use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use digest::Digest;
use serde::Deserialize;
use serde::Serialize;

use crate::Context;
use crate::Error;
use crate::instruction_map::CacheStatus;
use crate::instruction_map::InstructionSite;
use crate::instruction_map::default_cache_dir;
use crate::instruction_map::load_or_generate;

/// Environment variable that overrides the e9tool executable.
// TODO-HUMAN-REVIEW(PR-594): Review the public e9patch tool override.
pub const E9TOOL_ENV: &str = "HERMIT_E9TOOL";
/// Environment variable that overrides the e9patch backend executable.
// TODO-HUMAN-REVIEW(PR-594): Review the public e9patch backend override.
pub const E9PATCH_BACKEND_ENV: &str = "HERMIT_E9PATCH_BACKEND";

const REWRITE_SCHEMA_VERSION: u32 = 6;

/// Result of preparing the main guest ELF for the e9patch backend.
// TODO-HUMAN-REVIEW(PR-594): Review cached rewrite result semantics.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct PreparedBinary {
    /// Original executable when no sites are patched, or the cached rewritten ELF otherwise.
    pub binary: PathBuf,
    /// Number of candidate sites found by the linear instruction-map scan.
    // TODO-HUMAN-REVIEW(PR-664): Review e9patch candidate-site reporting.
    pub candidate_sites: usize,
    /// Number of candidate sites recovered and rewritten by e9tool.
    pub patched_sites: usize,
    /// Whether the instruction map was already cached.
    pub instruction_map_cache_status: CacheStatus,
    /// Whether the rewritten ELF was already cached.
    pub rewrite_cache_hit: bool,
    /// Number of B0 signal-fallback sites. This backend rejects nonzero values.
    pub b0_sites: usize,
    /// SHA-256 of the rewritten ELF, absent when no rewrite artifact is retained.
    pub artifact_sha256: Option<String>,
}

#[derive(Debug)]
struct BinarySnapshot {
    original: PathBuf,
    binary: PathBuf,
    digest: Digest,
    mode: u32,
}

#[derive(Debug, Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
struct RewriteIdentity {
    schema_version: u32,
    input_mode: u32,
    input_digest: Digest,
    e9tool_digest: Digest,
    instruction_map_digest: Digest,
    candidate_sites: usize,
    e9patch_backend_digest: Digest,
}

#[derive(Debug, Deserialize, Serialize)]
struct RewriteMetadata {
    #[serde(flatten)]
    identity: RewriteIdentity,
    output_digest: Digest,
    patched_sites: usize,
    recovered_sites: usize,
    b0_sites: usize,
}

/// Return an actionable error when e9tool cannot be executed.
// TODO-HUMAN-REVIEW(PR-594): Review public e9patch availability reporting.
pub fn unavailable_reason() -> Option<String> {
    let e9tool = match resolve_e9tool() {
        Ok(e9tool) => e9tool,
        Err(error) => return Some(error.to_string()),
    };
    resolve_e9patch_backend(&e9tool)
        .err()
        .map(|error| error.to_string())
}

/// Generate or load a cached e9patch rewrite for one ELF executable.
// TODO-HUMAN-REVIEW(PR-594): Review the public cached rewrite entry point.
pub fn prepare(binary: impl AsRef<Path>) -> Result<PreparedBinary, Error> {
    prepare_in(binary, runtime_cache_dir())
}

fn runtime_cache_dir() -> PathBuf {
    guest_visible_cache_dir(default_cache_dir())
}

fn guest_visible_cache_dir(cache_dir: PathBuf) -> PathBuf {
    if cache_dir.starts_with("/tmp") {
        PathBuf::from("/var/tmp")
            .join(format!("hermit-{}", nix::unistd::geteuid().as_raw()))
            .join("instruction-maps")
    } else {
        cache_dir
    }
}

fn prepare_in(
    binary: impl AsRef<Path>,
    cache_dir: impl AsRef<Path>,
) -> Result<PreparedBinary, Error> {
    let cache_dir = cache_dir.as_ref();
    ensure_private_cache_dir(cache_dir)?;
    let snapshot = snapshot_binary(binary.as_ref(), cache_dir)?;
    let result = load_or_generate(&snapshot.binary, cache_dir)?;
    if !trusted_regular_file(&result.cache_path, false) {
        return Err(Error::msg(format!(
            "instruction map cache {} is not a trusted regular file",
            result.cache_path.display()
        )));
    }
    let instruction_map_digest = Digest::new(&serde_json::to_vec(&result.map.sites)?);
    if result.map.sites.is_empty() {
        return Ok(PreparedBinary {
            binary: snapshot.original,
            candidate_sites: 0,
            patched_sites: 0,
            instruction_map_cache_status: result.cache_status,
            rewrite_cache_hit: false,
            b0_sites: 0,
            artifact_sha256: None,
        });
    }

    let e9tool_path = resolve_e9tool()?;
    let e9patch_backend_path = resolve_e9patch_backend(&e9tool_path)?;
    let e9tool = snapshot_binary(&e9tool_path, cache_dir)?;
    let e9patch_backend = snapshot_binary(&e9patch_backend_path, cache_dir)?;
    let rewrite_identity = RewriteIdentity {
        schema_version: REWRITE_SCHEMA_VERSION,
        input_digest: snapshot.digest,
        instruction_map_digest,
        e9tool_digest: e9tool.digest,
        e9patch_backend_digest: e9patch_backend.digest,
        input_mode: snapshot.mode,
        candidate_sites: result.map.sites.len(),
    };
    let rewrite_key = Digest::new(&serde_json::to_vec(&rewrite_identity)?).to_string();
    let metadata_path = cache_dir.join(format!("{rewrite_key}.json"));
    if let Some((binary, metadata)) =
        read_valid_rewrite(cache_dir, &rewrite_key, &metadata_path, &rewrite_identity)
    {
        return Ok(PreparedBinary {
            binary,
            candidate_sites: result.map.sites.len(),
            patched_sites: metadata.patched_sites,
            instruction_map_cache_status: result.cache_status,
            rewrite_cache_hit: true,
            b0_sites: metadata.b0_sites,
            artifact_sha256: Some(metadata.output_digest.to_string()),
        });
    }

    let temporary = tempfile::Builder::new()
        .prefix(".e9patch-rewrite-")
        .tempdir_in(cache_dir)
        .with_context(|| {
            format!(
                "failed to create temporary e9patch directory in {}",
                cache_dir.display()
            )
        })?;
    let temporary_binary = temporary.path().join("guest");
    let matcher = offset_matcher(&result.map.sites);
    let output = Command::new(&e9tool.binary)
        .arg("--backend")
        .arg(&e9patch_backend.binary)
        .arg("--seed=1")
        .arg("--option=--tactic-B0=false")
        // TODO-HUMAN-REVIEW(PR-676): Review the correctness-first e9tool
        // optimizer selection required by combined syscall/RDTSC Go rewrites.
        .arg("-O0")
        .arg("-M")
        .arg(&matcher)
        .arg("-P")
        .arg("before empty")
        .arg(&snapshot.binary)
        .arg("-o")
        .arg(&temporary_binary)
        .output()
        .with_context(|| format!("failed to execute e9tool {}", e9tool.original.display()))?;

    let diagnostic = command_diagnostic(&output.stdout, &output.stderr);
    if !output.status.success() {
        return Err(Error::msg(format!(
            "e9tool failed while rewriting {} (status {}):\n{diagnostic}",
            snapshot.original.display(),
            output.status
        )));
    }
    let (patched, total) = parse_metric(&diagnostic, "num_patched").ok_or_else(|| {
        Error::msg(format!(
            "e9tool did not report patch coverage for {}:\n{diagnostic}",
            snapshot.original.display()
        ))
    })?;
    validate_patch_coverage(patched, total, result.map.sites.len()).map_err(|reason| {
        Error::msg(format!(
            "e9tool coverage check failed for {}: {reason}:\n{diagnostic}",
            snapshot.original.display()
        ))
    })?;
    let b0_sites = if let Some((b0_sites, b0_total)) = parse_metric(&diagnostic, "num_patched_B0") {
        if b0_total != total {
            return Err(Error::msg(
                "e9tool B0 coverage total did not match its recovered-site total",
            ));
        }
        b0_sites
    } else {
        0
    };
    if b0_sites != 0 {
        return Err(Error::msg(format!(
            "e9tool used B0 signal fallback for {b0_sites} sites in {}; refusing a rewrite that \
             would reserve SIGILL and change guest signal semantics:\n{diagnostic}",
            snapshot.original.display()
        )));
    }
    if patched == 0 {
        return Ok(PreparedBinary {
            binary: snapshot.original,
            candidate_sites: result.map.sites.len(),
            patched_sites: 0,
            instruction_map_cache_status: result.cache_status,
            rewrite_cache_hit: false,
            b0_sites: 0,
            artifact_sha256: None,
        });
    }
    if !is_executable_file(&temporary_binary) {
        return Err(Error::msg(format!(
            "e9tool exited successfully without producing executable {}",
            temporary_binary.display()
        )));
    }

    let mut permissions = fs::metadata(&temporary_binary)
        .with_context(|| format!("failed to stat {}", temporary_binary.display()))?
        .permissions();
    permissions.set_mode(snapshot.mode);
    fs::set_permissions(&temporary_binary, permissions).with_context(|| {
        format!(
            "failed to restrict permissions on rewritten executable {}",
            temporary_binary.display()
        )
    })?;

    let output_digest = Digest::digest_path(&temporary_binary).with_context(|| {
        format!(
            "failed to hash rewritten executable {}",
            temporary_binary.display()
        )
    })?;
    let rewritten = rewrite_artifact_path(cache_dir, &rewrite_key, output_digest);
    fs::rename(&temporary_binary, &rewritten).with_context(|| {
        format!(
            "failed to persist rewritten executable {}",
            rewritten.display()
        )
    })?;
    write_metadata(
        &metadata_path,
        &RewriteMetadata {
            identity: rewrite_identity,
            output_digest,
            patched_sites: patched,
            recovered_sites: total,
            b0_sites,
        },
    )?;

    Ok(PreparedBinary {
        binary: rewritten,
        candidate_sites: result.map.sites.len(),
        patched_sites: patched,
        instruction_map_cache_status: result.cache_status,
        rewrite_cache_hit: false,
        b0_sites,
        artifact_sha256: Some(output_digest.to_string()),
    })
}

fn file_has_security_capability(file: &File) -> Result<bool, Error> {
    // SAFETY: the file descriptor and static xattr name are valid, and a null
    // value with size zero asks Linux for the attribute length without writing.
    let size = unsafe {
        libc::fgetxattr(
            file.as_raw_fd(),
            c"security.capability".as_ptr(),
            std::ptr::null_mut(),
            0,
        )
    };
    if size >= 0 {
        return Ok(size != 0);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ENODATA) | Some(libc::ENOTSUP) => Ok(false),
        _ => Err(error).context("failed to inspect executable file capabilities"),
    }
}

fn snapshot_binary(binary: &Path, cache_dir: &Path) -> Result<BinarySnapshot, Error> {
    let original = fs::canonicalize(binary)
        .with_context(|| format!("failed to resolve binary {}", binary.display()))?;
    let mut file = File::open(&original)
        .with_context(|| format!("failed to open binary {}", original.display()))?;
    let before = file
        .metadata()
        .with_context(|| format!("failed to stat binary {}", original.display()))?;
    if !before.is_file() {
        return Err(Error::msg(format!(
            "e9patch input is not a regular file: {}",
            original.display()
        )));
    }
    if before.mode() & 0o6000 != 0 || file_has_security_capability(&file)? {
        return Err(Error::msg(format!(
            "e9patch does not support privilege-bearing executable {}; refusing to discard \
             set-ID or file-capability semantics",
            original.display()
        )));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("failed to read binary {}", original.display()))?;
    let after = file
        .metadata()
        .with_context(|| format!("failed to restat binary {}", original.display()))?;
    if after.mode() & 0o6000 != 0 || file_has_security_capability(&file)? {
        return Err(Error::msg(format!(
            "e9patch input became privilege-bearing while creating its snapshot: {}",
            original.display()
        )));
    }
    if before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
        || before.mode() != after.mode()
    {
        return Err(Error::msg(format!(
            "binary changed while creating e9patch snapshot: {}",
            original.display()
        )));
    }

    let digest = Digest::new(&bytes);
    let snapshot_dir = cache_dir.join("elf-snapshots");
    fs::create_dir_all(&snapshot_dir).with_context(|| {
        format!(
            "failed to create e9patch snapshot directory {}",
            snapshot_dir.display()
        )
    })?;
    let snapshot = snapshot_dir.join(format!("{digest}.elf"));
    if !trusted_file_with_digest(&snapshot, digest, true) {
        let mut temporary = tempfile::Builder::new()
            .prefix(".elf-snapshot-")
            .tempfile_in(&snapshot_dir)
            .with_context(|| {
                format!(
                    "failed to create binary snapshot in {}",
                    snapshot_dir.display()
                )
            })?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        let mut permissions = temporary.as_file().metadata()?.permissions();
        permissions.set_mode(0o500);
        temporary.as_file().set_permissions(permissions)?;
        temporary
            .persist(&snapshot)
            .with_context(|| format!("failed to persist binary snapshot {}", snapshot.display()))?;
    }

    Ok(BinarySnapshot {
        original,
        binary: snapshot,
        digest,
        mode: before.mode() & 0o777,
    })
}

fn read_valid_rewrite(
    cache_dir: &Path,
    rewrite_key: &str,
    metadata_path: &Path,
    expected: &RewriteIdentity,
) -> Option<(PathBuf, RewriteMetadata)> {
    if !trusted_regular_file(metadata_path, false) {
        return None;
    }
    let metadata: RewriteMetadata =
        serde_json::from_reader(File::open(metadata_path).ok()?).ok()?;
    if metadata.identity != *expected {
        return None;
    }
    valid_cached_coverage(&metadata, expected).then_some(())?;
    let binary = rewrite_artifact_path(cache_dir, rewrite_key, metadata.output_digest);
    let mode = fs::metadata(&binary).ok()?.permissions().mode() & 0o777;
    (mode == expected.input_mode && trusted_file_with_digest(&binary, metadata.output_digest, true))
        .then_some((binary, metadata))
}

fn valid_cached_coverage(metadata: &RewriteMetadata, expected: &RewriteIdentity) -> bool {
    metadata.b0_sites == 0
        && metadata.recovered_sites != 0
        && validate_patch_coverage(
            metadata.patched_sites,
            metadata.recovered_sites,
            expected.candidate_sites,
        )
        .is_ok()
}

fn rewrite_artifact_path(cache_dir: &Path, rewrite_key: &str, digest: Digest) -> PathBuf {
    cache_dir.join(format!("{rewrite_key}-{digest}.e9patch"))
}

fn write_metadata(path: &Path, metadata: &RewriteMetadata) -> Result<(), Error> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::msg("e9patch metadata path has no parent"))?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".e9patch-metadata-")
        .tempfile_in(parent)
        .with_context(|| {
            format!(
                "failed to create temporary e9patch metadata in {}",
                parent.display()
            )
        })?;
    serde_json::to_writer(&mut temporary, metadata)?;
    temporary.write_all(b"\n")?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .with_context(|| format!("failed to persist e9patch metadata {}", path.display()))?;
    Ok(())
}

fn validate_cache_ancestors(path: &Path) -> Result<(), Error> {
    if !path.is_absolute() {
        return Err(Error::msg(format!(
            "e9patch cache path must be absolute: {}",
            path.display()
        )));
    }
    let expected_uid = nix::unistd::geteuid().as_raw();
    for ancestor in path.ancestors() {
        let metadata = fs::symlink_metadata(ancestor)
            .with_context(|| format!("failed to inspect cache ancestor {}", ancestor.display()))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(Error::msg(format!(
                "e9patch cache ancestor {} must be a real directory",
                ancestor.display()
            )));
        }
        let mode = metadata.permissions().mode();
        let trusted_owner = metadata.uid() == 0 || metadata.uid() == expected_uid;
        let safe_writable = mode & 0o022 == 0 || (metadata.uid() == 0 && mode & 0o1000 != 0);
        if !trusted_owner || !safe_writable {
            return Err(Error::msg(format!(
                "unsafe e9patch cache ancestor {}",
                ancestor.display()
            )));
        }
    }
    Ok(())
}

fn ensure_private_cache_dir(path: &Path) -> Result<(), Error> {
    fs::create_dir_all(path)
        .with_context(|| format!("failed to create e9patch cache {}", path.display()))?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect e9patch cache {}", path.display()))?;
    let expected_uid = nix::unistd::geteuid().as_raw();
    validate_cache_ancestors(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() || metadata.uid() != expected_uid {
        return Err(Error::msg(format!(
            "e9patch cache {} must be a real directory owned by uid {expected_uid}",
            path.display()
        )));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).with_context(|| {
            format!(
                "failed to restrict permissions on e9patch cache {}",
                path.display()
            )
        })?;
    }
    Ok(())
}

fn trusted_regular_file(path: &Path, executable: bool) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == nix::unistd::geteuid().as_raw()
            && metadata.permissions().mode() & 0o022 == 0
            && (!executable || metadata.permissions().mode() & 0o111 != 0)
    })
}

fn trusted_file_with_digest(path: &Path, expected: Digest, executable: bool) -> bool {
    trusted_regular_file(path, executable)
        && Digest::digest_path(path).is_ok_and(|actual| actual == expected)
}

fn resolve_e9tool() -> Result<PathBuf, Error> {
    let requested = env::var_os(E9TOOL_ENV).unwrap_or_else(|| "e9tool".into());
    if requested.is_empty() {
        return Err(Error::msg(format!("{E9TOOL_ENV} is empty")));
    }
    let path = Path::new(&requested);
    if path.components().count() > 1 {
        return is_executable_file(path)
            .then(|| path.to_path_buf())
            .ok_or_else(|| {
                Error::msg(format!(
                    "{E9TOOL_ENV}={} is not an executable file",
                    path.display()
                ))
            });
    }

    let path_env = env::var_os("PATH").unwrap_or_default();
    env::split_paths(&path_env)
        .map(|directory| directory.join(&requested))
        .find(|candidate| is_executable_file(candidate))
        .ok_or_else(|| {
            Error::msg(format!(
                "e9tool was not found in PATH; install e9patch or set {E9TOOL_ENV} to its executable"
            ))
        })
}

fn resolve_e9patch_backend(e9tool: &Path) -> Result<PathBuf, Error> {
    let requested = env::var_os(E9PATCH_BACKEND_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| e9tool.with_file_name("e9patch"));
    if requested.components().count() > 1 {
        return is_executable_file(&requested)
            .then_some(requested.clone())
            .ok_or_else(|| {
                Error::msg(format!(
                    "e9patch backend {} is not executable; install it beside e9tool or set \
                     {E9PATCH_BACKEND_ENV}",
                    requested.display()
                ))
            });
    }
    let path_env = env::var_os("PATH").unwrap_or_default();
    env::split_paths(&path_env)
        .map(|directory| directory.join(&requested))
        .find(|candidate| is_executable_file(candidate))
        .ok_or_else(|| {
            Error::msg(format!(
                "e9patch backend {:?} was not found in PATH; set {E9PATCH_BACKEND_ENV} to its \
                 executable",
                requested
            ))
        })
}

fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn offset_matcher(sites: &[InstructionSite]) -> String {
    let mut matcher = String::new();
    for (index, site) in sites.iter().enumerate() {
        if index != 0 {
            matcher.push_str(" || ");
        }
        matcher.push_str(&format!("offset == {:#x}", site.offset));
    }
    matcher
}

fn command_diagnostic(stdout: &[u8], stderr: &[u8]) -> String {
    let mut diagnostic = String::from_utf8_lossy(stdout).into_owned();
    diagnostic.push_str(&String::from_utf8_lossy(stderr));
    diagnostic
}

fn parse_metric(diagnostic: &str, name: &str) -> Option<(usize, usize)> {
    diagnostic.lines().find_map(|line| {
        let counts = line
            .trim()
            .strip_prefix(name)?
            .trim_start()
            .strip_prefix('=')?
            .trim();
        let (value, total) = counts.split_once('/')?;
        let total = total.split_whitespace().next()?;
        Some((value.trim().parse().ok()?, total.parse().ok()?))
    })
}

fn validate_patch_coverage(
    patched: usize,
    recovered: usize,
    candidate_sites: usize,
) -> Result<(), String> {
    if recovered > candidate_sites {
        return Err(format!(
            "e9tool recovered {recovered} matches from only {candidate_sites} candidate offsets"
        ));
    }
    if patched != recovered {
        return Err(format!(
            "e9tool patched only {patched}/{recovered} recovered matches"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(offset: u64) -> InstructionSite {
        InstructionSite {
            offset,
            instruction: "syscall".to_owned(),
            length: 2,
        }
    }

    #[test]
    fn matcher_uses_exact_file_offsets() {
        assert_eq!(
            offset_matcher(&[site(0x1007), site(0x1012)]),
            "offset == 0x1007 || offset == 0x1012"
        );
    }

    #[test]
    fn parses_e9tool_coverage_and_signal_fallback_summary() {
        let summary = "num_patched = 2 / 2 (100.00%)\nnum_patched_B0 = 1 / 2 (50.00%)\n";
        assert_eq!(parse_metric(summary, "num_patched"), Some((2, 2)));
        assert_eq!(parse_metric(summary, "num_patched_B0"), Some((1, 2)));
        assert_eq!(parse_metric(summary, "num_patched_B1"), None);
    }

    #[test]
    fn accepts_full_coverage_of_recovered_candidate_subset() {
        assert_eq!(validate_patch_coverage(24, 24, 49), Ok(()));
    }

    #[test]
    fn rejects_partial_e9tool_coverage() {
        assert_eq!(
            validate_patch_coverage(23, 24, 49),
            Err("e9tool patched only 23/24 recovered matches".to_owned())
        );
    }

    #[test]
    fn rejects_more_recovered_sites_than_candidates() {
        assert_eq!(
            validate_patch_coverage(50, 50, 49),
            Err("e9tool recovered 50 matches from only 49 candidate offsets".to_owned())
        );
    }

    #[test]
    fn cached_rewrite_rejects_corrupted_coverage_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let rewrite_key = "rewrite";
        let expected = RewriteIdentity {
            schema_version: REWRITE_SCHEMA_VERSION,
            input_mode: 0o755,
            input_digest: Digest::new(b"input"),
            e9tool_digest: Digest::new(b"e9tool"),
            instruction_map_digest: Digest::new(b"map"),
            candidate_sites: 49,
            e9patch_backend_digest: Digest::new(b"backend"),
        };
        let artifact_contents = b"rewritten";
        let output_digest = Digest::new(artifact_contents);
        let artifact = rewrite_artifact_path(directory.path(), rewrite_key, output_digest);
        fs::write(&artifact, artifact_contents).unwrap();
        let mut permissions = fs::metadata(&artifact).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&artifact, permissions).unwrap();
        let metadata_path = directory.path().join("rewrite.json");
        let mut metadata = RewriteMetadata {
            identity: expected,
            output_digest,
            patched_sites: 24,
            recovered_sites: 24,
            b0_sites: 0,
        };
        let write = |metadata: &RewriteMetadata| {
            fs::write(&metadata_path, serde_json::to_vec(metadata).unwrap()).unwrap();
        };

        write(&metadata);
        assert!(
            read_valid_rewrite(directory.path(), rewrite_key, &metadata_path, &expected).is_some()
        );

        metadata.b0_sites = 1;
        write(&metadata);
        assert!(
            read_valid_rewrite(directory.path(), rewrite_key, &metadata_path, &expected).is_none()
        );

        metadata.b0_sites = 0;
        metadata.patched_sites = 0;
        metadata.recovered_sites = 0;
        write(&metadata);
        assert!(
            read_valid_rewrite(directory.path(), rewrite_key, &metadata_path, &expected).is_none()
        );

        metadata.patched_sites = 23;
        metadata.recovered_sites = 24;
        write(&metadata);
        assert!(
            read_valid_rewrite(directory.path(), rewrite_key, &metadata_path, &expected).is_none()
        );

        metadata.patched_sites = 50;
        metadata.recovered_sites = 50;
        write(&metadata);
        assert!(
            read_valid_rewrite(directory.path(), rewrite_key, &metadata_path, &expected).is_none()
        );
    }

    #[test]
    fn snapshots_are_keyed_by_contents() {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("input");
        fs::write(&binary, b"first").unwrap();
        let mut permissions = fs::metadata(&binary).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&binary, permissions).unwrap();
        let cache = directory.path().join("cache");

        let first = snapshot_binary(&binary, &cache).unwrap();
        fs::write(&binary, b"other").unwrap();
        let second = snapshot_binary(&binary, &cache).unwrap();

        assert_ne!(first.digest, second.digest);
        assert_ne!(first.binary, second.binary);
        assert!(trusted_file_with_digest(&first.binary, first.digest, true));
        assert!(trusted_file_with_digest(
            &second.binary,
            second.digest,
            true
        ));
    }

    #[test]
    fn privilege_bearing_inputs_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("privileged");
        fs::write(&binary, b"fixture").unwrap();
        let mut permissions = fs::metadata(&binary).unwrap().permissions();
        permissions.set_mode(0o4755);
        fs::set_permissions(&binary, permissions).unwrap();
        assert_ne!(fs::metadata(&binary).unwrap().mode() & 0o4000, 0);

        let error = snapshot_binary(&binary, &directory.path().join("cache")).unwrap_err();
        assert!(error.to_string().contains("privilege-bearing executable"));
    }

    #[test]
    fn tmp_cache_is_moved_outside_isolated_guest_tmp() {
        assert_eq!(
            guest_visible_cache_dir(PathBuf::from("/tmp/custom")),
            PathBuf::from(format!(
                "/var/tmp/hermit-{}/instruction-maps",
                nix::unistd::geteuid().as_raw()
            ))
        );
        assert_eq!(
            guest_visible_cache_dir(PathBuf::from("/home/user/.cache/hermit")),
            PathBuf::from("/home/user/.cache/hermit")
        );
    }

    #[test]
    fn cache_directory_is_private_and_not_a_symlink() {
        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path().join("cache");
        fs::create_dir(&cache).unwrap();
        let mut permissions = fs::metadata(&cache).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&cache, permissions).unwrap();

        ensure_private_cache_dir(&cache).unwrap();
        assert_eq!(
            fs::metadata(&cache).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let link = directory.path().join("cache-link");
        std::os::unix::fs::symlink(&cache, &link).unwrap();
        assert!(
            ensure_private_cache_dir(&link)
                .unwrap_err()
                .to_string()
                .contains("real directory")
        );
    }

    #[test]
    fn writable_cache_ancestor_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().join("unsafe-parent");
        fs::create_dir(&parent).unwrap();
        let mut permissions = fs::metadata(&parent).unwrap().permissions();
        permissions.set_mode(0o777);
        fs::set_permissions(&parent, permissions).unwrap();
        let error = ensure_private_cache_dir(&parent.join("cache")).unwrap_err();
        assert!(error.to_string().contains("unsafe e9patch cache ancestor"));
    }

    #[test]
    fn writable_or_symlinked_artifacts_are_not_trusted() {
        let directory = tempfile::tempdir().unwrap();
        let artifact = directory.path().join("artifact");
        fs::write(&artifact, b"artifact").unwrap();
        let digest = Digest::digest_path(&artifact).unwrap();
        let mut permissions = fs::metadata(&artifact).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&artifact, permissions).unwrap();
        assert!(trusted_file_with_digest(&artifact, digest, true));

        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&artifact, &link).unwrap();
        assert!(!trusted_file_with_digest(&link, digest, true));

        let mut permissions = fs::metadata(&artifact).unwrap().permissions();
        permissions.set_mode(0o775);
        fs::set_permissions(&artifact, permissions).unwrap();
        assert!(!trusted_file_with_digest(&artifact, digest, true));
    }
}
