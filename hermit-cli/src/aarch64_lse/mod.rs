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
//! The decoder, the idioms and the rewrite of an ELF file's bytes are
//! Detcore's ([`detcore::aarch64_lse`]), re-exported here; this module adds
//! what only the overlay needs: the host check, the cache and the file
//! writes.

pub mod elf;
pub use detcore::aarch64_lse::insn;
pub use detcore::aarch64_lse::mapping;
pub use detcore::aarch64_lse::rewrite;

/// Bumped whenever the rewrite's output for the same input can change. It is
/// part of the cache key, so a cached artifact from an older rewriter is
/// never reused. 3: only code is rewritten, as [`mapping`] tells it from
/// data; none of the changes since 2 had bumped it, though the canonical
/// shapes the refusal census added, the conditional compare-and-swap
/// refusal, the direct-entry decoding and the positive allow-list each
/// changed what the rewrite writes.
pub const REWRITE_FORMAT_VERSION: u32 = 3;

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
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;

/// Where rewritten images are cached, without creating or checking
/// anything: the directory [`cache_dir`] uses. It follows this process's
/// environment (`XDG_CACHE_HOME`, `HOME`), so a caller that remembers what
/// it prepared must remember this too.
///
/// Never under `/tmp`: Hermit replaces the guest's `/tmp` with an isolated
/// directory, so an artifact there would not be visible to the guest that
/// has to map it.
pub fn cache_path() -> PathBuf {
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
    base.join("exclusive-rewrites")
}

/// The cache entry holding `rewritten`, the rewrite of `source`, which the
/// overlay bind-mounts over the path the guest loads `source` from; written
/// if need be.
///
/// The entry is named by everything that decides the bytes the guest maps:
/// the rewriter's format, a digest of the rewritten bytes themselves, and the
/// source's permissions, which the copy carries ([`elf::write_rewritten`]);
/// `name`, the source's file name, is there for a reader. So two runs that
/// would map different bytes -- two builds of the rewriter, say -- never
/// share an entry, and what one run checked cannot be replaced by another
/// run's bytes before it mounts it. Nor is an entry ever written in place: a
/// missing one, or one that does not hold the bytes and the mode its name
/// promises, is written whole under a temporary name and renamed over the
/// name, so a run that already mounted the file it replaces keeps that file.
pub fn cached_rewrite(
    cache: &Path,
    rewritten: &[u8],
    source: &Path,
    name: &str,
) -> anyhow::Result<PathBuf> {
    let mode = std::fs::metadata(source)
        .with_context(|| format!("reading the permissions of {}", source.display()))?
        .permissions()
        .mode();
    let entry = cache.join(format!(
        "v{REWRITE_FORMAT_VERSION}-{}-{:o}-{name}",
        detcore::Digest::new(rewritten),
        mode & 0o7777
    ));
    let holds_it = std::fs::metadata(&entry)
        .is_ok_and(|cached| cached.permissions().mode() == mode)
        && std::fs::read(&entry).is_ok_and(|cached| cached == rewritten);
    if !holds_it {
        elf::write_rewritten(&entry, rewritten, source)?;
    }
    Ok(entry)
}

/// Where rewritten images are cached ([`cache_path`]), created private to
/// this user.
pub fn cache_dir() -> anyhow::Result<PathBuf> {
    let dir = cache_path();
    let base = dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| dir.clone());
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Two rewrites of one source share one cache directory, and each gets
    /// only its own bytes: entries are named by
    /// the bytes they hold, written in either order, and a run that holds
    /// one entry open still reads its bytes after the other run's write.
    /// An entry that does not hold what its name promises is replaced by
    /// a new file, not rewritten in place.
    #[test]
    fn each_rewrite_maps_only_its_own_bytes_from_one_cache() {
        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path().join("cache");
        std::fs::create_dir(&cache).unwrap();
        let source = directory.path().join("libstripped.so");
        let original = b"\x7fELF stripped, as the default leaves it".to_vec();
        let other = b"\x7fELF stripped, another rewrite".to_vec();
        std::fs::write(&source, &original).unwrap();
        let entry =
            |bytes: &[u8]| cached_rewrite(&cache, bytes, &source, "libstripped.so").unwrap();
        let mapped = entry(&other);
        let mut held = std::fs::File::open(&mapped).unwrap();
        let default = entry(&original);
        assert_ne!(default, mapped);
        assert_eq!(std::fs::read(&mapped).unwrap(), other);
        assert_eq!(std::fs::read(&default).unwrap(), original);
        // An entry that lost its bytes is replaced by a new file; the one a
        // run holds keeps what it held.
        std::fs::remove_file(&mapped).unwrap();
        std::fs::write(&mapped, b"someone else's bytes").unwrap();
        let mut stale = std::fs::File::open(&mapped).unwrap();
        assert_eq!(entry(&other), mapped);
        assert_eq!(std::fs::read(&mapped).unwrap(), other);
        let mut read = Vec::new();
        std::io::Read::read_to_end(&mut stale, &mut read).unwrap();
        assert_eq!(read, b"someone else's bytes");
        read.clear();
        std::io::Read::read_to_end(&mut held, &mut read).unwrap();
        assert_eq!(read, other);
    }
}
