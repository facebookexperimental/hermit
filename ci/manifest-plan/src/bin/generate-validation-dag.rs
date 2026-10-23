// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use hermit_manifest_plan::validation_dag::OUTPUT;
use hermit_manifest_plan::validation_dag::canonical_text;
use hermit_manifest_plan::validation_dag::generate;
use hermit_manifest_plan::validation_dag::repo_root;
use hermit_manifest_plan::validation_dag::require_fresh;

fn usage() -> &'static str {
    "usage: generate-validation-dag [--check | --write] [PATH]\n\
     Build the canonical quick/portable/full/super/privileged DAG.\n\
     --check is the default; PATH defaults to ci/dag/validate.json."
}

fn run() -> Result<(), String> {
    let mut write = false;
    let mut path = None;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--check" if !write => {}
            "--write" if path.is_none() => write = true,
            "-h" | "--help" => {
                println!("{}", usage());
                return Ok(());
            }
            value if !value.starts_with('-') && path.is_none() => path = Some(PathBuf::from(value)),
            _ => {
                return Err(format!(
                    "unrecognized or conflicting argument {arg:?}\n{}",
                    usage()
                ));
            }
        }
    }
    let root = repo_root()?;
    let path = path.map_or_else(
        || root.join(OUTPUT),
        |value| {
            if value.is_absolute() {
                value
            } else {
                root.join(value)
            }
        },
    );
    let generated = canonical_text(&generate(&root)?);
    if write {
        fs::write(&path, &generated)
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
        println!("wrote {}", path.display());
    } else {
        let committed = fs::read_to_string(&path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        require_fresh(&committed, &generated)?;
        println!("{} is canonical and fresh", path.display());
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("generate-validation-dag: {error}");
            ExitCode::from(2)
        }
    }
}
