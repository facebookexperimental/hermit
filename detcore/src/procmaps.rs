/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

pub use procfs::process::MMapPath;
pub use procfs::process::MemoryMap;
use reverie::Guest;
use reverie::Pid;
use reverie::Tool;
use reverie::syscalls::Addr;
use reverie::syscalls::MemoryAccess;

use crate::Digest;

fn display_pathname(p: &MMapPath) -> String {
    match p {
        MMapPath::Vdso => String::from("[vsdo]"),
        MMapPath::Stack => String::from("[stack]"),
        MMapPath::TStack(tid) => format!("[tstack:{}]", tid),
        MMapPath::Vvar => String::from("[vvar]"),
        MMapPath::Vsyscall => String::from("[syscalls]"),
        MMapPath::Heap => String::from("[heap]"),
        MMapPath::Other(s) => format!("[other: {}]", s),
        MMapPath::Anonymous => String::from("[annonymous]"),
        MMapPath::Path(s) => s.display().to_string(),
        MMapPath::Rollup => String::from("[rollup]"),
        MMapPath::Vsys(vsys) => vsys.to_string(),
    }
}

pub fn display(map: &MemoryMap) -> String {
    display_as(map, &display_pathname(&map.pathname))
}

/// Render a mapping with an explicit pathname column.
///
/// Used to report a region that *is* the guest heap but which the kernel did
/// not label `[heap]`, so the record is textually comparable with the labelled
/// one another backend produces. Every other column is the mapping's real
/// procfs data.
pub fn display_as(map: &MemoryMap, pathname: &str) -> String {
    display_range_as(map, map.address.0, map.address.1, pathname)
}

/// Render a mapping's real procfs columns against an EXPLICIT address range.
///
/// The two address columns come from `start`/`end`; every other column is the
/// mapping's real procfs data. Used where the region being reported is a SUBSET
/// of the mapping that contains it -- the observed program break inside its
/// enclosing anonymous mapping -- so the record's extent is the region actually
/// hashed rather than the arena that happens to hold it.
pub fn display_range_as(map: &MemoryMap, start: u64, end: u64, pathname: &str) -> String {
    format!(
        "{:#x}-{:#x} {:?} {:x} {:x}:{:x} {} {}",
        start, end, map.perms, map.offset, map.dev.0, map.dev.1, map.inode, pathname
    )
}

/// Reverie ptrace's own pages in every guest process, `[start, end)`: its
/// trampoline at `0x7100_0000` and the SIGTRAP repairs' scratch page after
/// it (reverie-ptrace `cp::PRIVATE_PAGE_OFFSET`, `cp::SCRATCH_BASE` and
/// `cp::SCRATCH_SIZE`, which that crate keeps private; hermit-cli's
/// `REVERIE_PTRACE_PRIVATE_PAGE`, pinned by a cli test, names the first).
/// They are private anonymous mappings, but Reverie's state, not the guest's.
pub const REVERIE_PTRACE_PAGES: (u64, u64) = (0x7100_0000, 0x7100_2000);

/// Whether `map` is one `--detlog-anon` hashes: anonymous (unnamed, named by
/// `PR_SET_VMA_ANON_NAME` as `[anon:<name>]`, or a thread stack an older
/// kernel labels `[stack:<tid>]`), private, readable and writable, and not
/// one of Reverie's own pages ([`REVERIE_PTRACE_PAGES`]). `[heap]` and
/// `[stack]` are `--detlog-heap`'s and `--detlog-stack`'s; a shared anonymous
/// mapping (`[anon_shmem:<name>]`, or `/dev/zero (deleted)`) is another
/// process's memory as much as this one's; and a mapping that cannot be read
/// (`PROT_NONE`, such as an allocator's reservation) or written holds nothing
/// the guest changed through it.
pub fn is_hashed_anonymous(map: &MemoryMap) -> bool {
    use procfs::process::MMPermissions;
    let anonymous = match &map.pathname {
        MMapPath::Anonymous | MMapPath::TStack(_) => true,
        MMapPath::Other(name) => name.starts_with("anon:"),
        _ => false,
    };
    let reverie =
        REVERIE_PTRACE_PAGES.0 <= map.address.0 && map.address.1 <= REVERIE_PTRACE_PAGES.1;
    anonymous
        && !reverie
        && map
            .perms
            .contains(MMPermissions::READ | MMPermissions::WRITE | MMPermissions::PRIVATE)
}

/// The size of the pages `--detlog-anon=pages` digests.
const DIGESTED_PAGE: usize = 4096;

/// The largest mapping `--detlog-anon` reads. A larger one, such as a
/// sanitizer's shadow (terabytes, `MAP_NORESERVE`) or a heap a runtime
/// reserves read-write up front, is recorded by its size alone.
pub const ANON_HASH_LIMIT: u64 = 256 << 20;

/// How much of a mapping `--detlog-anon` reads at a time, a whole number of
/// pages, so that page digests never straddle a read.
const ANON_READ_CHUNK: usize = 1 << 20;

/// The digest of one page for `--detlog-anon=pages`: `<index>:<the first 8
/// bytes of its SHA-256 in hex>`, or `None` for an all-zero page, which an
/// untouched anonymous page reads as.
fn page_digest(index: usize, page: &[u8]) -> Option<String> {
    page.iter()
        .any(|byte| *byte != 0)
        .then(|| format!("{index}:{}", &Digest::new(page).to_string()[..16]))
}

/// For `--detlog-anon=pages`: each 4 KiB page of `bytes` that is not all zero
/// ([`page_digest`]), comma-separated, computed at once: the reference
/// [`hash_in_chunks`] is checked against. A page index counts from the start
/// of the mapping, so a mapping that merges with a neighbour or moves
/// (`mremap`) renumbers its pages.
#[cfg(test)]
fn page_digests(bytes: &[u8]) -> String {
    bytes
        .chunks(DIGESTED_PAGE)
        .enumerate()
        .filter_map(|(index, page)| page_digest(index, page))
        .collect::<Vec<_>>()
        .join(",")
}

/// What `--detlog-anon` records for one mapping.
pub enum AnonHash {
    /// Its digest and, where asked for, its page digests (`<index>:<hex>`,
    /// comma-separated; [`hash_in_chunks`]).
    Hashed(Digest, Option<String>),
    /// Larger than [`ANON_HASH_LIMIT`]: its size, unread.
    TooLarge(u64),
}

/// The digest of `size` bytes and, where `pages`, their page digests, read
/// `chunk` bytes at a time (a multiple of the page size) through `read`,
/// which fills its buffer from the given offset. Equal to hashing all the
/// bytes at once, which the unit tests check, without holding them at once.
fn hash_in_chunks(
    size: u64,
    chunk: usize,
    pages: bool,
    mut read: impl FnMut(u64, &mut [u8]) -> Result<(), reverie::Error>,
) -> Result<(Digest, Option<String>), reverie::Error> {
    debug_assert_eq!(chunk % DIGESTED_PAGE, 0);
    let mut digest = crate::digest::DigestBuilder::default();
    let mut listed = Vec::new();
    let mut buf = vec![0; chunk.min(size as usize)];
    let mut offset = 0u64;
    while offset < size {
        let n = (size - offset).min(chunk as u64) as usize;
        read(offset, &mut buf[..n])?;
        digest.update(&buf[..n]);
        if pages {
            let first = (offset / DIGESTED_PAGE as u64) as usize;
            listed.extend(
                buf[..n]
                    .chunks(DIGESTED_PAGE)
                    .enumerate()
                    .filter_map(|(index, page)| page_digest(first + index, page)),
            );
        }
        offset += n as u64;
    }
    Ok((digest.finish(), pages.then(|| listed.join(","))))
}

/// What `--detlog-anon` records for the guest bytes in `[start, end)`
/// ([`AnonHash`]), read in [`ANON_READ_CHUNK`]s.
pub fn compute_hash_and_pages<G, T: Tool>(
    guest: &mut G,
    start: u64,
    end: u64,
    pages: bool,
) -> Result<AnonHash, reverie::Error>
where
    G: Guest<T>,
{
    let size = end.saturating_sub(start);
    if size > ANON_HASH_LIMIT {
        return Ok(AnonHash::TooLarge(size));
    }
    let memory = guest.memory();
    let (digest, listed) = hash_in_chunks(size, ANON_READ_CHUNK, pages, |offset, buf| {
        let address = Addr::<u8>::from_raw((start + offset) as usize).unwrap();
        memory.read_values(address, buf)?;
        Ok(())
    })?;
    Ok(AnonHash::Hashed(digest, listed))
}

fn map_error(err: procfs::ProcError) -> reverie::Error {
    match err {
        procfs::ProcError::Io(err, _) => reverie::Error::Io(err),
        err => reverie::Error::Tool(anyhow::anyhow!(err)),
    }
}

pub fn from_pid<F>(pid: Pid, filter: F) -> Result<Vec<MemoryMap>, reverie::Error>
where
    F: Fn(&MemoryMap) -> bool,
{
    match procfs::process::Process::new(pid.as_raw()) {
        Ok(process) => match process.maps() {
            Ok(mut maps) => {
                maps.0.retain(filter);
                Ok(maps.0)
            }
            Err(err) => Err(map_error(err)),
        },
        Err(err) => Err(map_error(err)),
    }
}

pub fn compute_hash<G, T: Tool>(guest: &mut G, map: &MemoryMap) -> Result<Digest, reverie::Error>
where
    G: Guest<T>,
{
    compute_hash_range(guest, map.address.0, map.address.1)
}

/// Hash the guest bytes in the half-open guest-virtual range `[start, end)`,
/// read through `guest.memory()`. Used for backend-reported memory regions
/// ([`reverie::Guest::detlog_memory_regions`]) where the range is a guest
/// address rather than an entry parsed from a host `/proc/<pid>/maps`.
pub fn compute_hash_range<G, T: Tool>(
    guest: &mut G,
    start: u64,
    end: u64,
) -> Result<Digest, reverie::Error>
where
    G: Guest<T>,
{
    let size = end.saturating_sub(start) as usize;
    let memory = guest.memory();
    let mut buf = vec![0; size];
    let start = Addr::<u8>::from_raw(start as usize).unwrap();
    memory.read_values(start, buf.as_mut_slice())?;
    Ok(Digest::new(buf.as_slice()))
}

#[cfg(test)]
mod tests {
    use procfs::process::MMPermissions;

    use super::*;

    fn map(perms: MMPermissions, pathname: MMapPath) -> MemoryMap {
        MemoryMap {
            address: (0x7f00_0000_0000, 0x7f00_0000_2000),
            perms,
            offset: 0,
            dev: (0, 0),
            inode: 0,
            pathname,
            extension: Default::default(),
        }
    }

    #[test]
    fn only_private_writable_anonymous_mappings_are_hashed() {
        let rw = MMPermissions::READ | MMPermissions::WRITE | MMPermissions::PRIVATE;
        for pathname in [
            MMapPath::Anonymous,
            MMapPath::Other("anon:scudo:primary".to_owned()),
            MMapPath::TStack(12),
        ] {
            assert!(
                is_hashed_anonymous(&map(rw, pathname.clone())),
                "{pathname:?}"
            );
        }
        for pathname in [
            MMapPath::Heap,
            MMapPath::Stack,
            MMapPath::Vvar,
            MMapPath::Other("anon_shmem:x".to_owned()),
            MMapPath::Path("/usr/lib/libc.so.6".into()),
        ] {
            assert!(
                !is_hashed_anonymous(&map(rw, pathname.clone())),
                "{pathname:?}"
            );
        }
        // Reverie's trampoline (read-write-execute) and scratch page.
        let rwx = rw | MMPermissions::EXECUTE;
        let mut trampoline = map(rwx, MMapPath::Anonymous);
        trampoline.address = (0x7100_0000, 0x7100_1000);
        let mut scratch = map(rw, MMapPath::Anonymous);
        scratch.address = (0x7100_1000, 0x7100_2000);
        assert!(!is_hashed_anonymous(&trampoline) && !is_hashed_anonymous(&scratch));
        // A reservation, a read-only mapping, and a shared one.
        for perms in [
            MMPermissions::PRIVATE,
            MMPermissions::READ | MMPermissions::PRIVATE,
            MMPermissions::READ | MMPermissions::WRITE | MMPermissions::SHARED,
        ] {
            assert!(
                !is_hashed_anonymous(&map(perms, MMapPath::Anonymous)),
                "{perms:?}"
            );
        }
    }

    #[test]
    fn page_digests_list_the_pages_that_are_not_all_zero() {
        let mut bytes = vec![0u8; 3 * 4096];
        assert_eq!(page_digests(&bytes), "");
        bytes[4096 + 17] = 1;
        let digests = page_digests(&bytes);
        let expected = &Digest::new(&bytes[4096..8192]).to_string()[..16];
        assert_eq!(digests, format!("1:{expected}"));
        bytes[2 * 4096] = 2;
        assert_eq!(page_digests(&bytes).split(',').count(), 2);
        // A short last page counts as a page.
        assert_eq!(page_digests(&[0, 0, 3]).split(':').next(), Some("0"));
    }

    #[test]
    fn reading_in_chunks_equals_hashing_at_once() {
        // Two and a half chunks of two pages each, with a page written in
        // each chunk and the last, short, chunk.
        let mut bytes = vec![0u8; 5 * 4096 + 100];
        for (index, byte) in [(1, 7u8), (2 * 4096 + 9, 8), (5 * 4096 + 50, 9)] {
            bytes[index] = byte;
        }
        let (digest, pages) = hash_in_chunks(bytes.len() as u64, 2 * 4096, true, |offset, buf| {
            let offset = offset as usize;
            buf.copy_from_slice(&bytes[offset..offset + buf.len()]);
            Ok(())
        })
        .unwrap();
        assert_eq!(digest, Digest::new(&bytes));
        assert_eq!(pages.as_deref(), Some(page_digests(&bytes).as_str()));
        assert_eq!(pages.unwrap().split(',').count(), 3);
        let (empty, none) = hash_in_chunks(0, 4096, false, |_, _| Ok(())).unwrap();
        assert_eq!((empty, none), (Digest::new(&[]), None));
    }
}
