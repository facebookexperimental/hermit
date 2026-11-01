/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! PROTOTYPE: OCI image rootfs materialization for `hermit run --image`.
//!
//! This is the *filesystem half* of hermit-as-container-runtime. Hermit already
//! owns the *namespace half* (PID/mount/user namespaces via reverie-process);
//! this module bolts on deterministic FILE INPUTS by materializing a rootfs from
//! a pinned OCI image (ideally by digest) and letting `run` chroot into it.
//!
//! Podman is the image manager: images are fetched into the local
//! containers/storage store by `podman pull` (or `hermit oci pull`), and this
//! module materializes a rootfs by copying the merged overlay layers out of
//! that store (see [`crate::podman_store`]). Hermit never pulls on the run
//! path; a reference the store does not hold is refused with the remedy.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

use detcore::Digest;
use hermit::Context;
use hermit::Error;

/// Pseudo-filesystem mount points the guest expects to exist inside the root but
/// that an image layer often omits (busybox, distroless, and `scratch`-derived
/// images ship no `/proc` or `/sys`). We create them so reverie can mount the
/// deterministic `/proc` into the chrooted root.
///
/// `dev/pts` is here rather than in [`DEV_BIND_TARGETS`] because it receives a
/// whole filesystem (a fresh `devpts` instance), not a single bind.
const REQUIRED_DIRS: &[&str] = &["proc", "sys", "dev", "dev/pts", "tmp"];

/// Character devices the guest gets as bind mounts of the host node, relative to
/// the image root. See `container::image_container` for the mounts themselves and
/// for why each is safe to expose; this list only has to guarantee the mount
/// *targets* exist, because the image root is remounted read-only before the
/// binds are applied and a bind mount cannot create its own target.
///
/// An unprivileged user namespace cannot `mknod` a character device, so binding
/// the host node is the only way to give the guest one. That is also what
/// rootless podman and runc do.
pub(super) const DEV_BIND_TARGETS: &[&str] = &["null", "zero", "full", "random", "urandom"];

/// Extra device placeholder created in every rootfs but bound only for the
/// KVM backend (whose VMM process opens `/dev/kvm` after chroot). For other
/// backends it remains an empty regular file.
pub(super) const KVM_DEV_TARGETS: &[&str] = &["kvm"];

/// Conventional `/dev` symlinks, as `(link, target)`. These need no privilege and
/// no mount: they resolve through `/proc`, which Hermit already replaces with its
/// deterministic instance inside the image root.
///
/// `ptmx` completes the fresh `devpts` instance mounted at `/dev/pts`; the
/// kernel's devpts documentation names exactly this symlink (or a bind) as the
/// supported way to expose a per-instance `ptmx`. `fd`, `stdin`, `stdout` and
/// `stderr` are what bash process substitution (`<(...)`) and countless build
/// scripts open.
const DEV_SYMLINKS: &[(&str, &str)] = &[
    ("ptmx", "pts/ptmx"),
    ("fd", "/proc/self/fd"),
    ("stdin", "/proc/self/fd/0"),
    ("stdout", "/proc/self/fd/1"),
    ("stderr", "/proc/self/fd/2"),
];

/// Version of the materialized-rootfs *layout* (which mount targets and symlinks
/// the tree must contain), mixed into the cache key.
///
/// A cached tree is reused whenever its readiness marker exists, so a tree
/// materialized by an older Hermit would silently lack the `/dev` targets added
/// here and the binds would fail at mount time. Versioning the key
/// re-materializes instead. The key is used rather than the marker because an
/// image root is often mode `0555` and can carry entries owned by unmapped
/// sub-UIDs, so hermit cannot reliably rewrite or delete a tree it already
/// produced -- but it can always create a new one beside it. Superseded trees are
/// left on disk; clearing `~/.cache/hermit/oci-rootfs` reclaims them.
const ROOTFS_LAYOUT_VERSION: &str = "rootfs-layout-v4-dev";

/// Basename of the captured `Config.Env` file (one `KEY=VALUE` per line).
const ENV_BASENAME: &str = ".hermit-oci-env";
/// Basename of the captured `Config.WorkingDir` file (single line).
const WORKDIR_BASENAME: &str = ".hermit-oci-workdir";
const CMD_BASENAME: &str = ".hermit-oci-cmd";
const ENTRYPOINT_BASENAME: &str = ".hermit-oci-entrypoint";

/// Reduce an image reference to the identity the rootfs cache is keyed by.
///
/// A tag is a mutable pointer: keying the cache by the raw reference string
/// lets a moved tag silently alias an older rootfs. When a local podman store
/// is available the reference is therefore resolved to the store's canonical
/// image ID first (a local-only lookup that never contacts a registry), so
/// `podman pull` moving a tag produces a fresh cache entry. References that
/// are already an image ID, and references the store cannot resolve (the
/// store cannot resolve it), fall back to the reference itself.
pub(crate) fn canonical_image_ref(image_ref: &str) -> String {
    let candidate = image_ref
        .trim()
        .strip_prefix("sha256:")
        .unwrap_or(image_ref.trim());
    if candidate.len() == 64 && candidate.chars().all(|c| c.is_ascii_hexdigit()) {
        return candidate.to_ascii_lowercase();
    }
    if let Ok(Some(store)) = crate::podman_store::PodmanStore::probe()
        && let Ok(resolved) = store.resolve(image_ref)
    {
        return resolved.id;
    }
    image_ref.to_string()
}

/// Compute the cache directory that holds materialized rootfs trees. Keyed by
/// the SHA-256 of the complete image reference so distinct references cannot
/// alias after filesystem sanitization.
fn rootfs_cache_dir(image_ref: &str) -> Result<PathBuf, Error> {
    let base = if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        PathBuf::from(xdg)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".cache")
    } else {
        std::env::temp_dir()
    };
    // NUL-separate the two fields so no image reference can collide with a
    // different (reference, layout) pair by concatenation.
    let mut keyed = Vec::from(image_ref.as_bytes());
    keyed.push(0);
    keyed.extend_from_slice(ROOTFS_LAYOUT_VERSION.as_bytes());
    let key = Digest::new(&keyed).to_string();
    Ok(base.join("hermit").join("oci-rootfs").join(key))
}

/// Basenames of cache entries still referenced by `live_refs`, for GC.
pub(crate) fn live_cache_keys(
    live_refs: &[String],
) -> Result<std::collections::BTreeSet<String>, Error> {
    let mut keys = std::collections::BTreeSet::new();
    for r in live_refs {
        let dir = rootfs_cache_dir(&canonical_image_ref(r))?;
        if let Some(name) = dir.file_name().and_then(|n| n.to_str()) {
            keys.insert(name.to_string());
        }
    }
    Ok(keys)
}

/// The oci-rootfs cache base directory, when it exists.
pub(crate) fn cache_base() -> Result<Option<PathBuf>, Error> {
    // Any ref works: only the parent of the keyed dir is wanted.
    let probe = rootfs_cache_dir("probe")?;
    Ok(probe
        .parent()
        .map(|p| p.to_path_buf())
        .filter(|p| p.is_dir()))
}

/// A rootfs is considered already materialized if the marker file is present.
/// The marker is written last, so its presence means the copy finished.
///
/// The marker lives in the cache directory *beside* `rootfs`, not inside it: an
/// image root is frequently mode `0555` (read-only) and can contain entries
/// owned by unmapped sub-UIDs, so hermit (running as the plain invoking user,
/// outside any user namespace) cannot reliably write into the rootfs after
/// materialization. The cache directory itself is created and owned by hermit,
/// so the marker is always writable there.
fn ready_marker(cache: &Path) -> PathBuf {
    cache.join(".hermit-oci-ready")
}

/// Cache file holding the image's declared `WorkingDir` (a single line).
fn workdir_file(cache: &Path) -> PathBuf {
    cache.join(WORKDIR_BASENAME)
}

/// Cache file holding the image's declared `Env` (one `KEY=VALUE` per line).
fn env_file(cache: &Path) -> PathBuf {
    cache.join(ENV_BASENAME)
}

/// The subset of an image's OCI runtime config that the prototype applies so the
/// guest sees the deterministic, digest-pinned environment the image declares.
#[derive(Default)]
pub(crate) struct ImageConfig {
    /// `Config.Env` entries, split into `(key, value)` pairs.
    pub env: Vec<(String, String)>,
    /// `Config.WorkingDir`, if the image set a non-empty one.
    pub workdir: Option<String>,
    /// `Config.Cmd` (one argument per persisted line).
    pub cmd: Vec<String>,
    /// `Config.Entrypoint` (one argument per persisted line).
    pub entrypoint: Vec<String>,
}

fn read_image_config_files(
    env_path: &Path,
    workdir_path: &Path,
) -> Result<Option<ImageConfig>, Error> {
    let env_exists = env_path.try_exists().with_context(|| {
        format!(
            "Failed to inspect OCI environment file {}",
            env_path.display()
        )
    })?;
    let workdir_exists = workdir_path.try_exists().with_context(|| {
        format!(
            "Failed to inspect OCI working-directory file {}",
            workdir_path.display()
        )
    })?;
    if !env_exists && !workdir_exists {
        return Ok(None);
    }

    let env = if env_exists {
        parse_env(&std::fs::read_to_string(env_path).with_context(|| {
            format!("Failed to read OCI environment file {}", env_path.display())
        })?)
    } else {
        Vec::new()
    };
    let workdir = if workdir_exists {
        parse_workdir(&std::fs::read_to_string(workdir_path).with_context(|| {
            format!(
                "Failed to read OCI working-directory file {}",
                workdir_path.display()
            )
        })?)
    } else {
        None
    };
    Ok(Some(ImageConfig {
        env,
        workdir,
        ..Default::default()
    }))
}

fn read_argv_file(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .map(|text| {
            text.lines()
                .filter(|line| !line.is_empty())
                .map(|line| line.to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Read the persisted OCI config (Env + WorkingDir) captured at materialization
/// time. Missing files yield empty/None so images that declare neither still
/// run. File *presence*, rather than a non-empty Env, selects the location: an
/// image may legitimately declare a WorkingDir and no Env.
///
/// This is read from two candidate locations, in order, and the first that
/// yields a non-empty `Env` wins:
///
/// 1. The host cache dir beside the rootfs — valid when `guest_command` runs
///    *before* the chroot (e.g. program validation).
/// 2. The absolute in-root path (`/.hermit-oci-env`) — valid when
///    `guest_command` runs *inside* the container, after `chroot(rootfs)`, where
///    the host cache path (an absolute host path) no longer resolves. The
///    materializer writes a copy of the config here for exactly this case.
///
/// The two-location read is what makes the image-declared environment reach the
/// guest: the environment that actually runs the program is built in the forked,
/// chrooted child, so it can only see files that live inside the rootfs.
pub(crate) fn read_image_config(image_ref: &str) -> Result<ImageConfig, Error> {
    let cache = rootfs_cache_dir(&canonical_image_ref(image_ref))?;
    if let Some(mut host) = read_image_config_files(&env_file(&cache), &workdir_file(&cache))? {
        host.cmd = read_argv_file(&cache.join(CMD_BASENAME));
        host.entrypoint = read_argv_file(&cache.join(ENTRYPOINT_BASENAME));
        return Ok(host);
    }
    // Fall back to the in-root copy (post-chroot view).
    if let Some(mut in_root) = read_image_config_files(
        &Path::new("/").join(ENV_BASENAME),
        &Path::new("/").join(WORKDIR_BASENAME),
    )? {
        in_root.cmd = read_argv_file(&Path::new("/").join(CMD_BASENAME));
        in_root.entrypoint = read_argv_file(&Path::new("/").join(ENTRYPOINT_BASENAME));
        return Ok(in_root);
    }
    Ok(ImageConfig::default())
}

fn parse_env(contents: &str) -> Vec<(String, String)> {
    contents
        .lines()
        .filter_map(|line| {
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                return None;
            }
            line.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect()
}

fn parse_workdir(contents: &str) -> Option<String> {
    let trimmed = contents.trim();
    if trimmed.is_empty() || trimmed == "/" {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Materialize the given OCI image reference into a plain, user-owned rootfs
/// directory and return its path. Reusing a previously materialized reference is
/// a no-op that returns the cached directory.
///
/// `image_ref` must already be in the local podman store, e.g.
/// `docker.io/library/busybox@sha256:...` (digest-pinned, recommended for
/// determinism) or `docker.io/library/busybox:latest`.
pub(crate) fn materialize_rootfs(image_ref: &str) -> Result<PathBuf, Error> {
    let cache = rootfs_cache_dir(&canonical_image_ref(image_ref))?;
    let rootfs = cache.join("rootfs");
    if ready_marker(&cache).exists() {
        tracing::info!(
            "Reusing cached OCI rootfs for {} at {}",
            image_ref,
            rootfs.display()
        );
        return Ok(rootfs);
    }

    tracing::info!("Materializing OCI rootfs for {}", image_ref);
    std::fs::create_dir_all(&rootfs)
        .with_context(|| format!("Failed to create rootfs cache dir {}", rootfs.display()))?;

    // Copy the merged tree out of the local podman store's overlay layers.
    if materialize_via_podman_store(image_ref, &cache, &rootfs)? {
        std::fs::write(ready_marker(&cache), image_ref.as_bytes())
            .context("Failed to write OCI rootfs readiness marker")?;
        return Ok(rootfs);
    }

    Err(Error::msg(format!(
        "image {image_ref} is not in the local podman store, so there is no \
         rootfs to materialize. Fetch it first with `hermit oci pull {image_ref}` \
         (or `podman pull {image_ref}`), then re-run."
    )))
}

/// Copy the merged rootfs out of the podman store's overlay layers.
/// Returns `Ok(false)` when no store holds the image. Layers apply
/// base-first; overlay whiteouts are honored.
fn materialize_via_podman_store(
    image_ref: &str,
    cache: &Path,
    rootfs: &Path,
) -> Result<bool, Error> {
    let Some(store) = crate::podman_store::PodmanStore::probe()? else {
        return Ok(false);
    };
    let Ok(resolved) = store.resolve(image_ref) else {
        return Ok(false);
    };
    let Ok(layers) = store.overlay_dirs(&resolved) else {
        return Ok(false);
    };
    if layers.is_empty() {
        return Ok(false);
    }
    // overlay_dirs is topmost-first; overlay application is base-first.
    for layer in layers.iter().rev() {
        apply_overlay_layer(Path::new(layer), rootfs)?;
    }
    // Mount points, device placeholders, and symlinks (same set the buildah
    // path creates, here as plain filesystem operations on our own cache).
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(rootfs, std::fs::Permissions::from_mode(0o755));
    }
    for dir in REQUIRED_DIRS {
        std::fs::create_dir_all(rootfs.join(dir))
            .with_context(|| format!("Failed to create rootfs mount point {dir}"))?;
    }
    // Container runtimes provide /etc/hostname; images often omit it.
    let hostname_path = rootfs.join("etc/hostname");
    if !hostname_path.exists() {
        if let Some(parent) = hostname_path.parent() {
            std::fs::create_dir_all(parent).context("Failed to create rootfs etc directory")?;
        }
        std::fs::write(&hostname_path, b"hermetic-container.local\n")
            .context("Failed to write rootfs /etc/hostname")?;
    }
    for node in DEV_BIND_TARGETS.iter().chain(KVM_DEV_TARGETS.iter()) {
        let target = rootfs.join("dev").join(node);
        let _ = std::fs::remove_file(&target);
        std::fs::write(&target, b"")
            .with_context(|| format!("Failed to create device placeholder {node}"))?;
    }
    for (link, target) in DEV_SYMLINKS {
        let link_path = rootfs.join("dev").join(link);
        let _ = std::fs::remove_file(&link_path);
        std::os::unix::fs::symlink(target, &link_path)
            .with_context(|| format!("Failed to create /dev symlink {link}"))?;
    }
    // Persist the image config beside and inside the rootfs, as a container runtime does, so later runs never re-read the store for Env/WorkingDir.
    let config = store.config(&resolved)?;
    let env_text: String = config
        .env
        .iter()
        .map(|(k, v)| format!("{k}={v}\n"))
        .collect();
    let workdir_text: String = config
        .workdir
        .as_deref()
        .map(|w| format!("{w}\n"))
        .unwrap_or_default();
    std::fs::write(env_file(cache), &env_text).context("Failed to write OCI env file")?;
    std::fs::write(workdir_file(cache), &workdir_text)
        .context("Failed to write OCI workdir file")?;
    let argv_text =
        |argv: &[String]| -> String { argv.iter().map(|arg| format!("{arg}\n")).collect() };
    std::fs::write(cache.join(CMD_BASENAME), argv_text(&config.cmd))
        .context("Failed to write OCI cmd file")?;
    std::fs::write(
        cache.join(ENTRYPOINT_BASENAME),
        argv_text(&config.entrypoint),
    )
    .context("Failed to write OCI entrypoint file")?;
    std::fs::write(rootfs.join(CMD_BASENAME), argv_text(&config.cmd))
        .context("Failed to write in-root OCI cmd file")?;
    std::fs::write(
        rootfs.join(ENTRYPOINT_BASENAME),
        argv_text(&config.entrypoint),
    )
    .context("Failed to write in-root OCI entrypoint file")?;
    std::fs::write(rootfs.join(ENV_BASENAME), &env_text)
        .context("Failed to write in-root OCI env file")?;
    std::fs::write(rootfs.join(WORKDIR_BASENAME), &workdir_text)
        .context("Failed to write in-root OCI workdir file")?;
    Ok(true)
}

/// Apply one overlay layer onto `rootfs`, honoring whiteouts.
fn apply_overlay_layer(layer: &Path, rootfs: &Path) -> Result<(), Error> {
    // Whiteouts first: `.wh.<name>` deletes `<name>`; `.wh..wh..opq` makes
    // the containing directory opaque (clear its current contents).
    let mut whiteouts: Vec<PathBuf> = Vec::new();
    collect_whiteouts(layer, layer, &mut whiteouts);
    for whiteout in &whiteouts {
        let rel = whiteout.strip_prefix(layer).unwrap_or(whiteout);
        if rel.file_name().and_then(|n| n.to_str()) == Some(".wh..wh..opq") {
            let dir = rootfs.join(rel.parent().map(|p| p.to_path_buf()).unwrap_or_default());
            if dir.is_dir() {
                for entry in std::fs::read_dir(&dir)? {
                    let path = entry?.path();
                    if path.is_dir() && !path.is_symlink() {
                        let _ = std::fs::remove_dir_all(&path);
                    } else {
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
        } else if let Some(name) = rel
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix(".wh."))
        {
            let target = rootfs
                .join(rel.parent().map(|p| p.to_path_buf()).unwrap_or_default())
                .join(name);
            if target.is_dir() && !target.is_symlink() {
                let _ = std::fs::remove_dir_all(&target);
            } else {
                let _ = std::fs::remove_file(&target);
            }
        }
    }
    // Copy the layer's payload (excluding whiteout markers) with `cp -a` so
    // modes, symlinks, and hardlinks survive exactly as in the buildah path.
    // The copy runs under `podman unshare` because layer entries can be
    // owned by mapped sub-UIDs the plain invoking user cannot read.
    let status = Command::new("podman")
        .args(["unshare", "cp", "-a"])
        .arg(format!("{}/.", layer.display()))
        .arg(format!("{}/", rootfs.display()))
        .stdin(Stdio::null())
        .status()
        .context("Failed to spawn `podman unshare cp` to apply an overlay layer")?;
    if !status.success() {
        return Err(Error::msg(format!(
            "cp failed applying overlay layer {} (exit {status})",
            layer.display()
        )));
    }
    // Drop any whiteout markers the copy carried over.
    let mut copied: Vec<PathBuf> = Vec::new();
    collect_whiteouts(rootfs, rootfs, &mut copied);
    for marker in copied {
        let _ = std::fs::remove_file(&marker);
    }
    Ok(())
}

fn collect_whiteouts(dir: &Path, root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(".wh.") {
            out.push(path.clone());
        }
        if path.is_dir() && !path.is_symlink() && path != root {
            collect_whiteouts(&path, root, out);
        }
    }
}

/// Resolve an absolute guest path to its on-host location *inside* the rootfs,
/// following symlinks the way the kernel would once the guest has `chroot`ed in.
///
/// This is necessary because images (nixos/nix is the extreme case) populate
/// `/bin/sh`, `/usr/bin/env`, etc. as symlinks whose targets are **absolute**
/// guest paths (e.g. `/nix/store/...-bash/bin/bash`). Those targets only resolve
/// correctly relative to the image root; a naive `fs::metadata` on the host
/// would follow them against the *host* `/` and fail. We therefore walk the path
/// component by component, and whenever we hit a symlink we re-root an absolute
/// target back onto `rootfs` (a relative target stays relative to the link's
/// directory), exactly as a chrooted kernel would. The hop budget guards against
/// symlink loops.
///
/// Returns the final on-host path (which the caller can `stat`/validate). The
/// returned path is not guaranteed to exist; the caller performs that check so
/// it can produce a task-appropriate error message.
pub(crate) fn resolve_in_rootfs(rootfs: &Path, guest_abs: &Path) -> PathBuf {
    const MAX_HOPS: usize = 40;

    // `current` is an on-host path that always stays within `rootfs`.
    let mut current = rootfs.to_path_buf();
    // Remaining guest components to process, as a stack (front = next).
    let mut pending: Vec<std::ffi::OsString> = guest_abs
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(part) => Some(part.to_os_string()),
            // Root/prefix are absorbed by starting at `rootfs`; `.`/`..` are
            // handled below.
            std::path::Component::ParentDir => Some(std::ffi::OsString::from("..")),
            _ => None,
        })
        .rev()
        .collect();

    let mut hops = 0usize;
    while let Some(part) = pending.pop() {
        if part == ".." {
            // Never let `..` escape above the rootfs.
            if current != rootfs {
                current.pop();
            }
            continue;
        }
        let candidate = current.join(&part);
        match std::fs::symlink_metadata(&candidate) {
            Ok(meta) if meta.file_type().is_symlink() => {
                hops += 1;
                if hops > MAX_HOPS {
                    // Give up untangling; return best-effort so the caller's
                    // existence check reports a clean error.
                    return candidate;
                }
                match std::fs::read_link(&candidate) {
                    Ok(target) => {
                        if target.is_absolute() {
                            // Re-root the absolute target onto the rootfs and
                            // re-process its components.
                            current = rootfs.to_path_buf();
                        }
                        // Push target components back onto the stack (relative
                        // ones continue from `current`, i.e. the link's dir).
                        for comp in target
                            .components()
                            .filter_map(|c| match c {
                                std::path::Component::Normal(p) => Some(p.to_os_string()),
                                std::path::Component::ParentDir => {
                                    Some(std::ffi::OsString::from(".."))
                                }
                                _ => None,
                            })
                            .rev()
                        {
                            pending.push(comp);
                        }
                    }
                    Err(_) => return candidate,
                }
            }
            _ => {
                // Regular file/dir, or nonexistent: advance and let the caller
                // decide.
                current = candidate;
            }
        }
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_is_filesystem_safe_and_stable() {
        let a = rootfs_cache_dir("docker.io/library/busybox@sha256:abc123").unwrap();
        let b = rootfs_cache_dir("docker.io/library/busybox@sha256:abc123").unwrap();
        assert_eq!(a, b, "same reference must map to the same cache dir");
        let comp = a.file_name().unwrap().to_string_lossy();
        assert!(
            comp.len() == 64 && comp.chars().all(|c| c.is_ascii_hexdigit()),
            "cache key component must be a SHA-256 hex digest, got {comp}"
        );
    }

    #[test]
    fn canonical_ref_keeps_image_ids_unchanged() {
        let id = "aaef90e065235eb0b2d9938be85d1b81add2902cf89affe2cdfa19be33476ac0";
        assert_eq!(canonical_image_ref(id), id);
        assert_eq!(canonical_image_ref(&format!("sha256:{id}")), id);
    }

    #[test]
    fn canonical_ref_falls_back_for_unresolvable_references() {
        // No store holds this reference, so resolution cannot succeed and the
        // buildah pull-on-first-use spelling must survive unchanged.
        assert_eq!(
            canonical_image_ref("example.invalid/no-such-image:nope"),
            "example.invalid/no-such-image:nope"
        );
    }

    /// The `/dev` bind targets and `/dev/pts` must be created by the
    /// materializer, because the image root is remounted read-only before the
    /// binds are applied and a bind mount cannot create its own target. A node
    /// added to `DEV_BIND_TARGETS` without a matching target is a mount failure
    /// at run time, not a compile error, so pin the relationship here.
    #[test]
    fn every_dev_bind_target_is_created_by_the_materializer() {
        assert!(
            REQUIRED_DIRS.contains(&"dev"),
            "the /dev directory itself must be created"
        );
        assert!(
            REQUIRED_DIRS.contains(&"dev/pts"),
            "the devpts mount point must be created"
        );
        for node in DEV_BIND_TARGETS {
            assert!(
                !node.contains('/'),
                "{node} must be a bare basename under /dev; a nested target would \
                 need its parent directory created too"
            );
            assert!(
                Path::new("/dev").join(node).exists(),
                "/dev/{node} does not exist on this host, so binding it would fail"
            );
        }
    }

    /// The host entropy devices may be bound in, but only because Detcore
    /// virtualizes reads on them by path. Anything else host-coupled must stay
    /// out: `/dev/tty` is the caller's controlling terminal, and `/dev/shm` is
    /// writable cross-process shared state.
    #[test]
    fn minimal_dev_excludes_host_coupled_nodes() {
        for excluded in ["tty", "shm", "console", "kvm", "fuse"] {
            assert!(
                !DEV_BIND_TARGETS.contains(&excluded),
                "/dev/{excluded} is host-coupled and must not be bound into an image guest"
            );
        }
        // These two are present ONLY because detcore/src/syscalls/files.rs
        // recognizes the random devices by device number (character 1:8 and
        // 1:9) and serves their reads from the deterministic PRNG on every
        // backend. A bind keeps the device number, so the bound nodes are
        // recognized as well. If that recognition is ever removed, this bind
        // starts leaking host entropy and must be revisited.
        assert!(DEV_BIND_TARGETS.contains(&"random"));
        assert!(DEV_BIND_TARGETS.contains(&"urandom"));
    }

    /// `/dev/ptmx` must point at the per-container devpts instance, never at a
    /// host node: allocating out of the host's devpts would leak host-global pty
    /// numbers into the guest.
    #[test]
    fn ptmx_resolves_into_the_container_devpts_instance() {
        let ptmx = DEV_SYMLINKS
            .iter()
            .find(|(link, _)| *link == "ptmx")
            .expect("a /dev/ptmx symlink must be created");
        assert_eq!(ptmx.1, "pts/ptmx", "ptmx must be instance-relative");
        assert!(
            !ptmx.1.starts_with('/'),
            "an absolute ptmx target would resolve outside the devpts instance"
        );
        assert!(
            !DEV_BIND_TARGETS.contains(&"ptmx"),
            "ptmx must come from the container's own devpts, not a host bind"
        );
    }

    /// Bumping the layout must change the cache key, or a tree materialized by
    /// an older Hermit is silently reused without the new `/dev` targets.
    #[test]
    fn layout_version_participates_in_the_cache_key() {
        let reference = "docker.io/library/busybox:latest";
        let current = rootfs_cache_dir(reference).unwrap();

        let mut keyed = Vec::from(reference.as_bytes());
        keyed.push(0);
        keyed.extend_from_slice(b"some-other-layout");
        let other = Digest::new(&keyed).to_string();
        assert_ne!(
            current.file_name().unwrap().to_string_lossy(),
            other,
            "a different layout version must produce a different cache dir"
        );

        // ...and the un-versioned key (what pre-/dev Hermit computed) must not
        // collide with the current one, which is the whole point of the bump.
        let unversioned = Digest::new(reference.as_bytes()).to_string();
        assert_ne!(
            current.file_name().unwrap().to_string_lossy(),
            unversioned,
            "the versioned key must differ from the historical un-versioned key"
        );
    }

    #[test]
    fn distinct_references_get_distinct_cache_dirs() {
        let a = rootfs_cache_dir("busybox@sha256:aaaa").unwrap();
        let b = rootfs_cache_dir("busybox@sha256:bbbb").unwrap();
        assert_ne!(a, b);

        // These collided under the old punctuation-to-underscore sanitizer.
        let slash = rootfs_cache_dir("registry.example/a/b:latest").unwrap();
        let colon = rootfs_cache_dir("registry.example/a:b/latest").unwrap();
        assert_ne!(slash, colon);
    }

    #[test]
    fn parse_env_splits_key_value_and_skips_blanks() {
        let parsed = parse_env("USER=root\n\nPATH=/a:/b\r\nNOEQUALS\n");
        assert_eq!(
            parsed,
            vec![
                ("USER".to_string(), "root".to_string()),
                ("PATH".to_string(), "/a:/b".to_string()),
                // A line with no '=' is skipped, not panicked on.
            ]
        );
    }

    #[test]
    fn parse_workdir_treats_root_and_blank_as_none() {
        assert_eq!(parse_workdir(""), None);
        assert_eq!(parse_workdir("  \n"), None);
        assert_eq!(parse_workdir("/"), None);
        assert_eq!(parse_workdir("/srv/app\n"), Some("/srv/app".to_string()));
    }

    #[test]
    fn config_location_with_empty_env_still_preserves_workdir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let env = tmp.path().join("env");
        let workdir = tmp.path().join("workdir");
        std::fs::write(&env, "").unwrap();
        std::fs::write(&workdir, "/srv/app\n").unwrap();

        let config = read_image_config_files(&env, &workdir).unwrap().unwrap();
        assert!(config.env.is_empty());
        assert_eq!(config.workdir.as_deref(), Some("/srv/app"));
    }

    // The chroot-aware resolver must follow an *absolute* symlink target as if
    // it were rooted at the rootfs (the nixos/nix `/bin/sh -> /nix/store/...`
    // case), never against the host `/`.
    #[test]
    fn resolve_in_rootfs_reroots_absolute_symlink_targets() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path();
        // Build: /store/bash (real file), /bin/sh -> /store/bash (absolute).
        std::fs::create_dir_all(rootfs.join("store")).unwrap();
        std::fs::write(rootfs.join("store/bash"), b"#!/bin/true\n").unwrap();
        std::fs::create_dir_all(rootfs.join("bin")).unwrap();
        std::os::unix::fs::symlink("/store/bash", rootfs.join("bin/sh")).unwrap();

        let resolved = resolve_in_rootfs(rootfs, Path::new("/bin/sh"));
        assert_eq!(
            resolved,
            rootfs.join("store/bash"),
            "absolute symlink target must be re-rooted onto the rootfs"
        );
        assert!(resolved.exists(), "resolved target should exist on host");
    }

    // A relative symlink stays relative to the link's own directory, and `..`
    // must never escape above the rootfs.
    #[test]
    fn resolve_in_rootfs_handles_relative_symlinks_and_bounded_parent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path();
        std::fs::create_dir_all(rootfs.join("usr/bin")).unwrap();
        std::fs::write(rootfs.join("usr/bin/coreutils"), b"x").unwrap();
        // /bin/ls -> ../usr/bin/coreutils (relative).
        std::fs::create_dir_all(rootfs.join("bin")).unwrap();
        std::os::unix::fs::symlink("../usr/bin/coreutils", rootfs.join("bin/ls")).unwrap();
        assert_eq!(
            resolve_in_rootfs(rootfs, Path::new("/bin/ls")),
            rootfs.join("usr/bin/coreutils")
        );

        // Excess leading `..` cannot climb above the rootfs.
        assert_eq!(
            resolve_in_rootfs(rootfs, Path::new("/../../../store")),
            rootfs.join("store")
        );
    }
}
