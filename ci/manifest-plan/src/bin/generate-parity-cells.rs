// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

use std::fs;
use std::process::ExitCode;

use hermit_manifest_plan::cli_help::is_help_flag;
use hermit_manifest_plan::parity::PARITY_CELLS_DIR;
use hermit_manifest_plan::parity::PARITY_CELLS_PATH;
use hermit_manifest_plan::parity::PARITY_CELLS_REGENERATE;
use hermit_manifest_plan::parity::ParityCells;
use hermit_manifest_plan::parity::generate;
use hermit_manifest_plan::parity::require_fresh;
use hermit_manifest_plan::validation_dag::repo_root;

fn usage() -> &'static str {
    "Usage: generate-parity-cells [--check | --write]\n\
     Derive every cross-backend parity cell (each manifest test's verify mode on\n\
     kvm, liteinst, in-guest-trap, sabre and dbt, against ptrace) and the parity\n\
     population (every test whose ptrace verify cell full validation selects,\n\
     on every candidate backend) from tests/e2e/manifests, and compare it with, or write\n\
     it to, ci/compat-envelope/parity-cells.json.\n\
     --check is the default. It refuses a stale snapshot and prints the counts."
}

fn summary(snapshot: &ParityCells) -> String {
    let all = &snapshot.counts.all;
    let mut text = format!(
        "{} cells, {} applicable, {} selectable; population {} ({} candidate selected, {} \
         candidate not selected, {} candidate disabled)",
        all.cells,
        all.applicable,
        all.selectable,
        all.population,
        all.population_candidate_selected,
        all.population_candidate_not_selected,
        all.population_candidate_disabled
    );
    for (backend, count) in &snapshot.counts.by_backend {
        text.push_str(&format!(
            "\n  {backend}: {} applicable, {} selectable; population {} ({} candidate selected, \
             {} not selected, {} disabled)",
            count.applicable,
            count.selectable,
            count.population,
            count.population_candidate_selected,
            count.population_candidate_not_selected,
            count.population_candidate_disabled
        ));
    }
    text
}

fn run() -> Result<(), String> {
    // One mode flag at most. `--check --write` used to write, while
    // `--write --check` was refused: the same two flags gave opposite results
    // depending on their order.
    let mut mode: Option<&'static str> = None;
    for arg in std::env::args().skip(1) {
        let flag = match arg.as_str() {
            help if is_help_flag(help) => {
                println!("{}", usage());
                return Ok(());
            }
            "--check" => "--check",
            "--write" => "--write",
            _ => {
                return Err(format!("unrecognized argument {arg:?}\n{}", usage()));
            }
        };
        match mode.replace(flag) {
            None => {}
            Some(first) if first == flag => {
                return Err(format!("{flag} may be given only once\n{}", usage()));
            }
            Some(_) => {
                return Err(format!(
                    "--check and --write are mutually exclusive\n{}",
                    usage()
                ));
            }
        }
    }
    let write = mode == Some("--write");
    let root = repo_root()?;
    let dir = root.join(PARITY_CELLS_DIR);
    let snapshot = generate(&root)?;
    let shards = hermit_manifest_plan::parity::shard_texts(&snapshot)?;
    if write {
        fs::create_dir_all(&dir)
            .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
        for entry in
            fs::read_dir(&dir).map_err(|error| format!("cannot list {}: {error}", dir.display()))?
        {
            let entry = entry.map_err(|error| format!("cannot list {}: {error}", dir.display()))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".json") && !shards.contains_key(&name) {
                fs::remove_file(entry.path()).map_err(|error| {
                    format!(
                        "cannot remove stale shard {}: {error}",
                        entry.path().display()
                    )
                })?;
            }
        }
        for (name, text) in &shards {
            let path = dir.join(name);
            fs::write(&path, text)
                .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
            println!("wrote {} ({} bytes)", path.display(), text.len());
        }
        let legacy = root.join(PARITY_CELLS_PATH);
        if legacy.exists() {
            fs::remove_file(&legacy)
                .map_err(|error| format!("cannot remove {}: {error}", legacy.display()))?;
            println!("removed superseded {}", legacy.display());
        }
    } else {
        if root.join(PARITY_CELLS_PATH).exists() {
            return Err(format!(
                "{PARITY_CELLS_PATH} still exists beside the sharded {PARITY_CELLS_DIR}/; \
                 regenerate with: {PARITY_CELLS_REGENERATE}"
            ));
        }
        let mut committed_names: Vec<String> = fs::read_dir(&dir)
            .map_err(|error| format!("cannot list {}: {error}", dir.display()))?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".json"))
            .collect();
        committed_names.sort();
        let generated_names: Vec<String> = shards.keys().cloned().collect();
        if committed_names != generated_names {
            return Err(format!(
                "{PARITY_CELLS_DIR} shard set differs (committed {committed_names:?}, generated \
                 {generated_names:?}); regenerate with: {PARITY_CELLS_REGENERATE}"
            ));
        }
        for (name, generated) in &shards {
            let path = dir.join(name);
            let committed = fs::read_to_string(&path)
                .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
            require_fresh(&committed, generated)
                .map_err(|error| format!("{PARITY_CELLS_DIR}/{name}: {error}"))?;
        }
        println!("{PARITY_CELLS_DIR}/ is canonical and fresh");
    }
    println!("{}", summary(&snapshot));
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("generate-parity-cells: {error}");
            ExitCode::from(2)
        }
    }
}
