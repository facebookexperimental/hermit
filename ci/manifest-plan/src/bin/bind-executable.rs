// Copyright (c) Meta Platforms, Inc. and affiliates.
// This source code is licensed under the BSD-style license found in LICENSE.
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use hermit_manifest_plan::executable_binding::measure;
fn run() -> Result<(), String> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() < 7 || args[0] != "--output" || (args.len() - 2) % 5 != 0 {
        return Err("usage: bind-executable --output FILE --entry LOGICAL_BACKEND PRODUCER_BACKEND ABSOLUTE_ELF BUILD_SHA [--entry ...]".into());
    }
    let output = PathBuf::from(&args[1]);
    if !output.is_absolute() {
        return Err("binding output must be absolute".into());
    }
    let mut held = Vec::new();
    let mut names = std::collections::BTreeSet::new();
    for entry in args[2..].as_chunks::<5>().0 {
        if entry[0] != "--entry" || !names.insert(entry[1].clone()) {
            return Err("unknown or duplicate catalog entry".into());
        }
        let binary = PathBuf::from(&entry[3]);
        if !binary.is_absolute() {
            return Err("catalog ELF must be absolute".into());
        }
        held.push(measure(
            entry[1].clone(),
            entry[2].clone(),
            binary,
            entry[4].clone(),
        )?);
    }
    for file in &held {
        file.recheck()?;
    }
    let data=serde_json::to_vec_pretty(&serde_json::json!({"schema":1,"bindings":held.iter().map(|h|&h.identity).collect::<Vec<_>>()})).map_err(|e|e.to_string())?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(|e| e.to_string())?;
    file.write_all(&data).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    for file in &held {
        file.recheck()?;
    }
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("bind-executable: {error}");
        std::process::exit(2)
    }
}
