#!/usr/bin/env run-cargo-script
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Script to update hermit_syscalls.csv
//!
//! This is for interactive use on devservers, so it's fine to use cargo.
//! Prereqs:
//!    cargo install cargo-script
//!
//! Partial Cargo manifest:
//!
//! ```cargo
//! [dependencies]
//! csv = "1.1.3"
//! ```

extern crate csv;
use std::io;
use std::process::Command;

use csv::Reader;
use csv::StringRecord;
use csv::Writer;
use csv::WriterBuilder;

fn search_and_count_hits(syscall_name: &str) -> usize {
    let output = Command::new("rg")
        .arg(format!("SYS_{}", syscall_name))
        .arg("../detcore/tests/")
        .output()
        .expect("failed to execute ripgrep");
    String::from_utf8(output.stdout).unwrap().lines().count()
}

fn main() {
    let fd =
        std::fs::File::open("./hermit_syscalls.csv").expect("Could not open ./hermit_syscalls.csv");
    let mut rdr = Reader::from_reader(fd);
    let headers = rdr.headers().unwrap();
    let headers2 = headers.clone();
    let lookup = |key: &str, record: &StringRecord| -> String {
        if let Some(idx) = headers2.iter().position(|k| k == key) {
            String::from(record.get(idx).expect("internal error"))
        } else {
            panic!("Could not lookup key {}, schema:\n {:?}", key, headers2);
        }
    };
    let update = |key: &str, val: &str, record: &StringRecord| -> StringRecord {
        if let Some(idx) = headers2.iter().position(|k| k == key) {
            let mut vec: Vec<&str> = record.iter().collect();
            vec[idx] = val;
            StringRecord::from(vec)
        } else {
            panic!("Could not lookup key {}, schema:\n {:?}", key, headers2);
        }
    };

    let mut wtr = Writer::from_writer(io::stdout());
    wtr.write_record(headers).unwrap();

    for result in rdr.records() {
        let record = result.unwrap();
        if let Some(name) = lookup("SYSTEM_CALL", &record).strip_prefix("SYS_") {
            let record = update(
                "TEST_COVERAGE",
                &format!("{}", search_and_count_hits(name)),
                &record,
            );
            wtr.write_record(&record);
        } else {
            wtr.write_record(&record);
        }
    }
}
