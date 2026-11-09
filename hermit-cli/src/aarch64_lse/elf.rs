/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Writing rewritten aarch64 ELF files. The rewrite itself, a pure function
//! of a file's bytes, is Detcore's ([`detcore::aarch64_lse::elf`]), because
//! Detcore also applies it to a guest's mapped pages; this module adds the
//! files the overlay and `hermit rewrite-exclusives` write.

use std::path::Path;

pub use detcore::aarch64_lse::elf::*;

/// Write already-rewritten bytes to `output`, with `source`'s permissions.
///
/// The file is written whole under a temporary name the filesystem
/// guarantees is unused and renamed into place, so a failed or concurrent
/// write is never observed under the final name.
pub fn write_rewritten(output: &Path, bytes: &[u8], source: &Path) -> anyhow::Result<()> {
    use std::io::Write;

    let directory = output.parent().unwrap_or(Path::new("."));
    let mut temporary = tempfile::Builder::new()
        .prefix(".rewrite-")
        .tempfile_in(directory)?;
    temporary.write_all(bytes)?;
    temporary.flush()?;
    std::fs::set_permissions(temporary.path(), std::fs::metadata(source)?.permissions())?;
    temporary.persist(output).map_err(|error| error.error)?;
    Ok(())
}

/// Read `input`, rewrite it, and write the result to `output` with `input`'s
/// permissions.
pub fn rewrite_file(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
) -> anyhow::Result<Option<FileReport>> {
    let input = input.as_ref();
    let mut bytes = std::fs::read(input)?;
    let report = match rewrite_bytes(&mut bytes)? {
        Some(report) => report,
        None => return Ok(None),
    };
    write_rewritten(output.as_ref(), &bytes, input)?;
    Ok(Some(report))
}
