/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Prints `st_blocks` for files whose host counts depend on the
//! allocator (hermit#462). The expectations live in the `.lit`
//! FileCheck files: under Hermit the count is derived from the size
//! at a fixed 4096-byte unit, so a fragmented file and a contiguous
//! one agree, a sparse file reports dense, and a
//! `fallocate(KEEP_SIZE)` reservation is invisible.

// RUN: %me

use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::os::unix::fs::symlink;

use nix::sys::stat::fstat;
use nix::sys::stat::lstat;
use tempfile::NamedTempFile;

fn blocks_of(file: &NamedTempFile) -> (i64, i64, i64) {
    let stat = fstat(file).unwrap();
    (stat.st_size, stat.st_blocks, stat.st_blksize as i64)
}

fn main() {
    // Empty file: no blocks.
    let empty = NamedTempFile::new().unwrap();
    let (size, blocks, blksize) = blocks_of(&empty);
    println!("empty: size={size} blocks={blocks} blksize={blksize}");

    // The #462 pair at 91,320 bytes: contiguous and fragmented layouts
    // must both report ceil(91320/4096)*8 = 184. On ext4 the fragmented
    // file (six separated writes, then the gaps filled) can take more
    // than four extents and an extra extent-tree block natively (192).
    let mut contiguous = NamedTempFile::new().unwrap();
    contiguous.write_all(&vec![b'a'; 91_320]).unwrap();
    let (size, blocks, _) = blocks_of(&contiguous);
    println!("contiguous: size={size} blocks={blocks}");

    let mut fragmented = NamedTempFile::new().unwrap();
    for i in 0..6 {
        fragmented.seek(SeekFrom::Start(i * 16_384)).unwrap();
        fragmented.write_all(&vec![b'b'; 4096]).unwrap();
    }
    fragmented.seek(SeekFrom::Start(0)).unwrap();
    fragmented.write_all(&vec![b'c'; 91_320]).unwrap();
    let (size, blocks, _) = blocks_of(&fragmented);
    println!("fragmented: size={size} blocks={blocks}");

    // Sparse: 1 MiB with a single written byte reports dense.
    let mut sparse = NamedTempFile::new().unwrap();
    sparse.seek(SeekFrom::Start((1 << 20) - 1)).unwrap();
    sparse.write_all(b"x").unwrap();
    let (size, blocks, _) = blocks_of(&sparse);
    println!("sparse: size={size} blocks={blocks}");

    // A fallocate(KEEP_SIZE) reservation beyond EOF does not count.
    let mut reserved = NamedTempFile::new().unwrap();
    reserved.write_all(&vec![b'd'; 4096]).unwrap();
    let _ = nix::fcntl::fallocate(
        &reserved,
        nix::fcntl::FallocateFlags::FALLOC_FL_KEEP_SIZE,
        0,
        1 << 20,
    );
    let (size, blocks, _) = blocks_of(&reserved);
    println!("keep_size: size={size} blocks={blocks}");

    // Symlinks follow ext4's rule: under 60 bytes in the inode, else
    // one block. The targets need not exist for `lstat`.
    let dir = std::env::temp_dir();
    let short = dir.join("stblocks-short-link");
    let long = dir.join("stblocks-long-link");
    let _ = std::fs::remove_file(&short);
    let _ = std::fs::remove_file(&long);
    symlink("0123456789", &short).unwrap();
    symlink("x".repeat(100), &long).unwrap();
    let short_stat = lstat(&short).unwrap();
    let long_stat = lstat(&long).unwrap();
    println!(
        "symlink: short_blocks={} long_blocks={}",
        short_stat.st_blocks, long_stat.st_blocks
    );
    std::fs::remove_file(&short).unwrap();
    std::fs::remove_file(&long).unwrap();
}
