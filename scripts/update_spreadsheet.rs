#!/usr/bin/env rust-script
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! Script to update hermit_syscalls.csv
//!
//! This is for interactive use on devservers. It follows the repository's
//! rust-script convention so it shares the same CLI process prelude.
//!
//! Partial Cargo manifest:
//!
//! ```cargo
//! [dependencies]
//! csv = "1.1.3"
//! ```

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude; // rust-script cache-key: 088ae17fa4a1 (regen: scripts/lib/prelude-cache-key.sh --write)

extern crate csv;
use std::io;
use std::process::Command;

use csv::Reader;
use csv::StringRecord;
use csv::Writer;

fn search_and_count_hits(syscall_name: &str) -> usize {
    let output = Command::new("rg")
        .arg(format!("SYS_{}", syscall_name))
        .arg("../detcore/tests/")
        .output()
        .expect("failed to execute ripgrep");
    String::from_utf8(output.stdout).unwrap().lines().count()
}

const USAGE: &str = "\
Usage: update_spreadsheet.rs [-h|--help]

Recompute the TEST_COVERAGE column of ./hermit_syscalls.csv by counting
`SYS_<name>` hits under ../detcore/tests/ (via ripgrep), and write the updated
CSV to stdout. Run from a directory containing hermit_syscalls.csv; requires
`rg` on PATH. This is for interactive use on devservers.";

fn main() {
    rust_script_prelude::init();
    if std::env::args().skip(1).any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return;
    }
    let fd = match std::fs::File::open("./hermit_syscalls.csv") {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("update_spreadsheet: cannot open ./hermit_syscalls.csv: {e}");
            eprintln!("Run this script from a directory that contains hermit_syscalls.csv.");
            std::process::exit(1);
        }
    };
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
            wtr.write_record(&record).unwrap();
        } else {
            wtr.write_record(&record).unwrap();
        }
    }
}
