/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A dynamic-linker cache that a test builds for itself, for the tests that
//! freeze `/etc/ld.so.cache` or rename a new one over it.
//!
//! Such a test must not take the host's `/etc/ld.so.cache` as its input. The
//! validation's pinned root has none (no version of its image ever had one),
//! so a test that reads the host's fails there, and a file the host's package
//! manager rewrites is not an input whose provenance the test controls.
//! [`build`] instead runs `ldconfig` and writes the cache to a path the test
//! owns (`-C`). It reads the system's own library configuration
//! (`/etc/ld.so.conf`, where there is one, and the C library's trusted
//! directories), so the cache resolves the libraries of every program loaded
//! under it, Hermit's included, as the system's would; `-X` updates no
//! library symlink. ldconfig also saves its auxiliary cache under
//! `/var/cache/ldconfig` beside every cache it builds, `-C` or not, so it runs
//! in a private user and mount namespace whose `/var/cache` (where the host
//! has one) is a private tmpfs: nothing of the host's is written.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

/// The magic numbers a glibc loader cache starts with: the current format,
/// and the old format that a compatibility cache begins with.
const CACHE_MAGICS: [&[u8]; 2] = [b"glibc-ld.so.cache", b"ld.so-1.7.0"];

/// The `ldconfig` to run: the first on `PATH`, else the one in `/usr/sbin` or
/// `/sbin`, where distributions keep it off an unprivileged user's `PATH`.
fn ldconfig() -> Result<PathBuf, String> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain([PathBuf::from("/usr/sbin"), PathBuf::from("/sbin")])
        .map(|directory| directory.join("ldconfig"))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            format!(
                "no ldconfig on PATH ({}) or in /usr/sbin or /sbin to build the test's \
                 loader cache with",
                path.to_string_lossy()
            )
        })
}

/// Builds a loader cache at `cache` with `ldconfig` and returns its bytes, or
/// why it could not.
pub fn build(cache: &Path) -> Result<Vec<u8>, String> {
    let ldconfig = ldconfig()?;
    let rendered = format!("{} -X -C {}", ldconfig.display(), cache.display());
    let output = Command::new("unshare")
        .args([
            "-rm",
            "/bin/sh",
            "-c",
            r#"if [ -d /var/cache ]; then mount -t tmpfs tmpfs /var/cache || exit 125; fi
exec "$1" -X -C "$2""#,
            "sh",
        ])
        .arg(&ldconfig)
        .arg(cache)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("cannot start unshare to run {rendered}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{rendered}, in a private user and mount namespace, failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let bytes = std::fs::read(cache)
        .map_err(|error| format!("{rendered} left no cache at {}: {error}", cache.display()))?;
    if !CACHE_MAGICS.iter().any(|magic| bytes.starts_with(magic)) {
        return Err(format!(
            "{rendered} wrote {} bytes that do not start as a loader cache does",
            bytes.len()
        ));
    }
    Ok(bytes)
}
