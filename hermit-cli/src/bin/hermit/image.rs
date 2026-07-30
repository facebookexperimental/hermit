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
//! Design note (prototype seam): the task frames this as "use PART of podman as
//! a library" — `containers/image` (pull by digest) + `containers/storage`
//! (unpack to a rootfs). Those are Go libraries; wiring them into this Rust
//! binary directly is out of scope for a prototype. Instead this shells out to
//! `buildah`, which embeds exactly those two libraries and exposes them as a
//! rootless CLI. `buildah from` pulls the image through `containers/image` into
//! `containers/storage`, and `buildah mount` (inside a `buildah unshare` user
//! namespace) exposes the unpacked overlay rootfs, which we copy out to a plain,
//! user-owned directory that hermit's own user namespace can then chroot into.
//! A production version would link the Go libraries (or a Rust OCI unpacker such
//! as `oci-spec` + a layer extractor) instead of forking `buildah`; the seam
//! this module exposes (`image_ref -> plain rootfs directory`) is unchanged by
//! that substitution.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

use hermit::Context;
use hermit::Error;

/// Pseudo-filesystem mount points the guest expects to exist inside the root but
/// that an image layer often omits (busybox, distroless, and `scratch`-derived
/// images ship no `/proc` or `/sys`). We create them so reverie can mount the
/// deterministic `/proc` into the chrooted root.
const REQUIRED_DIRS: &[&str] = &["proc", "sys", "dev", "tmp"];

/// Basename of the captured `Config.Env` file (one `KEY=VALUE` per line).
const ENV_BASENAME: &str = ".hermit-oci-env";
/// Basename of the captured `Config.WorkingDir` file (single line).
const WORKDIR_BASENAME: &str = ".hermit-oci-workdir";

/// Compute the cache directory that holds materialized rootfs trees. Keyed by a
/// filesystem-safe encoding of the image reference so that re-running the same
/// pinned reference reuses the same bytes (deterministic file inputs).
fn rootfs_cache_dir(image_ref: &str) -> Result<PathBuf, Error> {
    let base = if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        PathBuf::from(xdg)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".cache")
    } else {
        std::env::temp_dir()
    };
    // Sanitize the reference into a single path component.
    let key: String = image_ref
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    Ok(base.join("hermit").join("oci-rootfs").join(key))
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
pub(crate) struct ImageConfig {
    /// `Config.Env` entries, split into `(key, value)` pairs.
    pub env: Vec<(String, String)>,
    /// `Config.WorkingDir`, if the image set a non-empty one.
    pub workdir: Option<String>,
}

/// Read the persisted OCI config (Env + WorkingDir) captured at materialization
/// time. Missing files yield empty/None rather than an error so that a rootfs
/// materialized before config capture, or an image that declares neither, still
/// runs.
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
    let cache = rootfs_cache_dir(image_ref)?;
    let host = ImageConfig {
        env: parse_env(&std::fs::read_to_string(env_file(&cache)).unwrap_or_default()),
        workdir: parse_workdir(&std::fs::read_to_string(workdir_file(&cache)).unwrap_or_default()),
    };
    if !host.env.is_empty() {
        return Ok(host);
    }
    // Fall back to the in-root copy (post-chroot view).
    let in_root = ImageConfig {
        env: parse_env(
            &std::fs::read_to_string(Path::new("/").join(ENV_BASENAME)).unwrap_or_default(),
        ),
        workdir: parse_workdir(
            &std::fs::read_to_string(Path::new("/").join(WORKDIR_BASENAME)).unwrap_or_default(),
        ),
    };
    if !in_root.env.is_empty() {
        return Ok(in_root);
    }
    // Neither location had env; keep any workdir the host copy provided.
    Ok(host)
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
/// `image_ref` may be any reference `buildah` accepts, e.g.
/// `docker.io/library/busybox@sha256:...` (digest-pinned, recommended for
/// determinism) or `docker.io/library/busybox:latest`.
pub(crate) fn materialize_rootfs(image_ref: &str) -> Result<PathBuf, Error> {
    let cache = rootfs_cache_dir(image_ref)?;
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

    // Run the pull + unpack + copy-out inside a single `buildah unshare` user
    // namespace, where the rootless overlay mount produced by `buildah mount` is
    // resolvable. We copy the merged tree out to `$rootfs` so the result is a
    // plain directory owned by the invoking user, independent of the transient
    // overlay mount.
    //
    // The pseudo-filesystem mount points (`/proc`, `/sys`, ...) are created
    // *inside* this user namespace, where we act as ns-root. An image root is
    // often mode `0555` and can carry sub-UID-owned entries, so hermit could not
    // reliably `mkdir` them afterward as the plain invoking user; creating them
    // here sidesteps both the mode and the ownership. We also `chmod u+w` those
    // mountpoints so the later bind/proc mount targets are traversable.
    //
    // `--pull=missing` (buildah default) fetches the image through the proxy the
    // caller has configured in the environment (e.g. `with-proxy`).
    let mkdirs = REQUIRED_DIRS
        .iter()
        .map(|d| {
            format!(
                "{dest}/{d}",
                dest = shell_quote(&rootfs.to_string_lossy()),
                d = d
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    // Also capture the image's declared OCI config (WorkingDir + Env) into the
    // cache dir. These are pinned by the image digest and are part of the
    // deterministic inputs: a real container runtime applies the image `Env`
    // (e.g. the nixos/nix image's `PATH=/root/.nix-profile/bin:...`) so that the
    // image's own tools resolve. We persist them beside the rootfs so later runs
    // do not depend on the image remaining in buildah storage.
    let script = format!(
        r#"set -euo pipefail
cid=$(buildah from -- {ref})
trap 'buildah umount "$cid" >/dev/null 2>&1 || true; buildah rm "$cid" >/dev/null 2>&1 || true' EXIT
mp=$(buildah mount "$cid")
cp -a "$mp"/. {dest}/
mkdir -p {mkdirs}
chmod u+rwx {mkdirs}
buildah inspect --type image --format '{{{{.OCIv1.Config.WorkingDir}}}}' {ref} > {workdir_file} || true
buildah inspect --type image --format '{{{{range .OCIv1.Config.Env}}}}{{{{println .}}}}{{{{end}}}}' {ref} > {env_file} || true
cp -f {workdir_file} {dest}/{workdir_base} 2>/dev/null || true
cp -f {env_file} {dest}/{env_base} 2>/dev/null || true
"#,
        ref = shell_quote(image_ref),
        dest = shell_quote(&rootfs.to_string_lossy()),
        mkdirs = mkdirs,
        workdir_file = shell_quote(&workdir_file(&cache).to_string_lossy()),
        env_file = shell_quote(&env_file(&cache).to_string_lossy()),
        workdir_base = WORKDIR_BASENAME,
        env_base = ENV_BASENAME,
    );

    let status = Command::new("buildah")
        .arg("unshare")
        .arg("bash")
        .arg("-c")
        .arg(&script)
        .stdin(Stdio::null())
        .status()
        .context(
            "Failed to spawn `buildah unshare` to materialize the OCI rootfs. \
             The prototype requires a rootless `buildah` on PATH.",
        )?;
    if !status.success() {
        return Err(Error::msg(format!(
            "buildah failed to materialize OCI image {image_ref} (exit {status}); \
             ensure the reference is valid and the registry is reachable \
             (set a proxy in the environment if required)"
        )));
    }

    // Write the readiness marker last so an interrupted materialization is not
    // mistaken for a complete one on the next run.
    std::fs::write(ready_marker(&cache), image_ref.as_bytes())
        .context("Failed to write OCI rootfs readiness marker")?;

    Ok(rootfs)
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

/// Minimal single-quote shell escaping for embedding a value in the bash script
/// above. Wraps in single quotes and escapes embedded single quotes.
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
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
            comp.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "cache key component must be filesystem-safe, got {comp}"
        );
    }

    #[test]
    fn distinct_references_get_distinct_cache_dirs() {
        let a = rootfs_cache_dir("busybox@sha256:aaaa").unwrap();
        let b = rootfs_cache_dir("busybox@sha256:bbbb").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        assert_eq!(shell_quote("plain"), "'plain'");
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
