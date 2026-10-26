/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Rewriting aarch64 load/store-exclusive retry loops to FEAT_LSE atomics.
//!
//! On aarch64 an `LDXR`/`STXR` pair retries whenever the local exclusive
//! monitor is cleared between the two, which any exception does: a timer tick,
//! an IPI, a page fault, a PMU overflow, a ptrace stop. Each retry retires the
//! loop's branches again, so the retired-branch count of identical guest work
//! depends on host interrupt timing, and Hermit's branch-counted schedule
//! cannot be replayed exactly. A single LSE atomic cannot retry, so a program
//! whose exclusive loops have been rewritten retires a fixed number of
//! branches for the same work.
//!
//! This is binary rewriting used as *preprocessing* for the ptrace backend, in
//! the same sense as e9patch (see `docs/E9PATCH_COMPATIBILITY.md`): it never
//! loads Detcore as a Reverie tool and is not a backend.
//!
//! [`insn`] holds the decoder and the encoders; [`rewrite`] holds the idioms
//! and the in-place replacement; [`elf`] applies them to the executable
//! segments of an ELF file.

pub mod elf;
pub mod insn;
pub mod rewrite;

/// Bumped whenever the rewrite's output for the same input can change. It is
/// part of the cache key, so a cached artifact from an older rewriter is
/// never reused.
pub const REWRITE_FORMAT_VERSION: u32 = 2;

/// Whether this host executes the FEAT_LSE atomics the rewrite emits.
///
/// The rewritten image is a program for the machine that is about to run it:
/// without FEAT_LSE every rewritten site would raise SIGILL.
pub fn host_has_lse_atomics() -> bool {
    #[cfg(target_arch = "aarch64")]
    {
        // HWCAP_ATOMICS, Linux Documentation/arch/arm64/elf_hwcaps.rst.
        const HWCAP_ATOMICS: libc::c_ulong = 1 << 8;
        // SAFETY: getauxval reads the process's own auxiliary vector.
        (unsafe { libc::getauxval(libc::AT_HWCAP) } & HWCAP_ATOMICS) != 0
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        false
    }
}

use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use anyhow::Context;

/// Where rewritten images are cached, created private to this user.
///
/// Never under `/tmp`: Hermit replaces the guest's `/tmp` with an isolated
/// directory, so an artifact there would not be visible to the guest that has
/// to map it.
pub fn cache_dir() -> anyhow::Result<PathBuf> {
    let base = dirs::cache_dir().map_or_else(
        || PathBuf::from("/var/tmp").join(format!("hermit-{}", nix::unistd::geteuid().as_raw())),
        |dir| dir.join("hermit"),
    );
    // An XDG cache under /tmp is no use: Hermit replaces the guest's /tmp
    // with an isolated directory, so an artifact there is not visible to the
    // guest that has to map it.
    let base = if base.starts_with("/tmp") {
        PathBuf::from("/var/tmp").join(format!("hermit-{}", nix::unistd::geteuid().as_raw()))
    } else {
        base
    };
    let dir = base.join("exclusive-rewrites");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create the rewrite cache {}", dir.display()))?;
    // The cache holds images the guest will execute, so every directory this
    // code creates must be ours and private: the `/var/tmp/hermit-<uid>`
    // fallback lives in a sticky directory anyone can write, so another user
    // could pre-create it and choose what the cache offers. Only the
    // directories this function creates are checked; the standard locations
    // above them (a home directory, `/var/tmp`) are the system's business.
    let uid = nix::unistd::geteuid().as_raw();
    for path in [base.as_path(), dir.as_path()] {
        let metadata = std::fs::symlink_metadata(path)
            .with_context(|| format!("failed to inspect the rewrite cache {}", path.display()))?;
        anyhow::ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink() && metadata.uid() == uid,
            "the rewrite cache path {} must be a real directory owned by uid {uid}",
            path.display()
        );
        if metadata.permissions().mode() & 0o077 != 0 {
            let mut permissions = metadata.permissions();
            permissions.set_mode(0o700);
            std::fs::set_permissions(path, permissions)?;
        }
    }
    Ok(dir)
}
