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
const ROOTFS_LAYOUT_VERSION: &str = "rootfs-layout-v2-dev";

/// Basename of the captured `Config.Env` file (one `KEY=VALUE` per line).
const ENV_BASENAME: &str = ".hermit-oci-env";
/// Basename of the captured `Config.WorkingDir` file (single line).
const WORKDIR_BASENAME: &str = ".hermit-oci-workdir";

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
    Ok(Some(ImageConfig { env, workdir }))
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
    let cache = rootfs_cache_dir(image_ref)?;
    if let Some(host) = read_image_config_files(&env_file(&cache), &workdir_file(&cache))? {
        return Ok(host);
    }
    // Fall back to the in-root copy (post-chroot view).
    if let Some(in_root) = read_image_config_files(
        &Path::new("/").join(ENV_BASENAME),
        &Path::new("/").join(WORKDIR_BASENAME),
    )? {
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
    // Bind-mount targets for the character devices, plus the conventional
    // symlinks. Both are created here, inside the `buildah unshare` user
    // namespace where we are ns-root, for the same reason as the directories
    // above: afterwards the tree may be mode 0555 with sub-UID-owned entries.
    // They must also exist *before* the image root is remounted read-only at run
    // time, because a bind mount cannot create its own target.
    //
    // The device targets are empty regular files: a bind mount replaces the
    // target inode, so the placeholder's type does not matter, and creating a
    // real character device would need a CAP_MKNOD we do not have.
    let dev_targets = DEV_BIND_TARGETS
        .iter()
        .map(|node| {
            format!(
                "{dest}/dev/{node}",
                dest = shell_quote(&rootfs.to_string_lossy())
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let dev_symlinks = DEV_SYMLINKS
        .iter()
        .map(|(link, target)| {
            format!(
                "\tln -sfn {target} {dest}/dev/{link}\n",
                target = shell_quote(target),
                dest = shell_quote(&rootfs.to_string_lossy()),
            )
        })
        .collect::<String>();
    let script = format!(
        r#"set -euo pipefail
cid=$(buildah from -- {ref})
trap 'buildah umount "$cid" >/dev/null 2>&1 || true; buildah rm "$cid" >/dev/null 2>&1 || true' EXIT
mp=$(buildah mount "$cid")
	cp -a "$mp"/. {dest}/
	root_mode=$(stat -c '%a' {dest})
	chmod u+w {dest}
	mkdir -p {mkdirs}
	chmod u+rwx {mkdirs}
	rm -f {dev_targets}
	touch {dev_targets}
	chmod u+rw {dev_targets}
{dev_symlinks}	buildah inspect --type image --format '{{{{.OCIv1.Config.WorkingDir}}}}' {ref} > {workdir_file}
	buildah inspect --type image --format '{{{{range .OCIv1.Config.Env}}}}{{{{println .}}}}{{{{end}}}}' {ref} > {env_file}
	cp -f {workdir_file} {dest}/{workdir_base}
	cp -f {env_file} {dest}/{env_base}
	chmod "$root_mode" {dest}
"#,
        ref = shell_quote(image_ref),
        dest = shell_quote(&rootfs.to_string_lossy()),
        mkdirs = mkdirs,
        dev_targets = dev_targets,
        dev_symlinks = dev_symlinks,
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
            comp.len() == 64 && comp.chars().all(|c| c.is_ascii_hexdigit()),
            "cache key component must be a SHA-256 hex digest, got {comp}"
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
        // These two are present ONLY because detcore/src/syscalls/files.rs maps
        // the paths "/dev/random" and "/dev/urandom" to FdType::Rng and serves
        // reads from the deterministic PRNG. If that classification is ever
        // keyed on something other than the path, this bind starts leaking host
        // entropy and must be revisited.
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
