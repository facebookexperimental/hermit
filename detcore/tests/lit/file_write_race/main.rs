// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

//! Threads racing to write the same file, dup'ing the file descriptor. Exercises
//! SYS_fcntl with F_DUPFD_CLOEXEC.

// RUN: %me | FileCheck %s
// CHECK: {{([12]{20})}}

// FIXME: This doesn't work yet:
// %hermit run --strict --verify -- %me

use std::fs;
use std::io::Write;

use tempfile::NamedTempFile;

fn main() {
    let (mut file, path) = NamedTempFile::new().unwrap().into_parts();

    let mut file2 = file.try_clone().unwrap(); // Implicitly dups fd w/ fcntl

    let child = std::thread::spawn(move || {
        for _ in 0..10 {
            file2.write_all(b"1").unwrap();
        }
    });

    for _ in 0..10 {
        file.write_all(b"2").unwrap();
    }

    child.join().unwrap();

    let contents = fs::read_to_string(path).unwrap();

    // None of the writes from either thread should be dropped:
    assert_eq!(contents.len(), 20);

    println!("{}", contents);
}
