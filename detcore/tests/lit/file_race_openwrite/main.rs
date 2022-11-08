/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// RUN: %me | FileCheck %s
// CHECK: {{(1*2+|2*1+)}}

// FIXME: This doesn't work yet:
// %hermit run --strict --verify -- %me

use std::fs;
use std::io;
use std::io::Write;
use std::path::Path;

use tempfile::NamedTempFile;

fn write(path: &Path, s: &[u8], count: usize) -> io::Result<()> {
    let mut file = fs::File::create(path)?;
    for _ in 0..count {
        file.write_all(s)?;
    }
    file.sync_all()
}

fn main() {
    let path = NamedTempFile::new().unwrap().into_temp_path();

    let thread = {
        let path = path.to_path_buf();
        std::thread::spawn(move || write(&path, b"1", 10))
    };

    write(&path, b"2", 10).unwrap();

    thread.join().unwrap().unwrap();

    let contents = fs::read_to_string(&path).unwrap();

    // Only one thread wins.
    assert_eq!(contents.len(), 10);

    println!("{}", contents);
}
