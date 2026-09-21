#!/usr/bin/env -S rust-script --force
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Require Hermit to supply every `reverie_dbt_runtime_*` callback the DBT
//! client declares, before DynamoRIO discovers the gap at load time.
//!
//! # THE CONTRACT THIS GUARDS, AND WHY NOTHING ELSE CATCHES IT
//!
//! `reverie-dbt/native/client.c` declares and calls every
//! `reverie_dbt_runtime_*` callback UNCONDITIONALLY -- there is no `#ifdef`
//! around any of them. Their Rust definitions in `reverie-dbt/src/lib.rs` sit
//! behind `#[cfg(feature = "prototype-runtime")]`, which `reverie-dbt`'s own
//! manifest enables by default and which HERMIT DELIBERATELY DISABLES:
//! `detcore-dbt/Cargo.toml` pins `reverie-dbt` with `default-features = false`
//! because Hermit supplies Detcore's runtime instead. `docs/Developers/
//! CargoFeatures.md` records that decision.
//!
//! So the two sides are joined by a list that is maintained BY HAND in
//! `detcore-dbt/src/lib.rs`, and nothing compares the two lists. Cargo cannot:
//! the client is C, and the link happens when DynamoRIO loads the client, long
//! after `cargo check` and `cargo build` have both succeeded. A pin bump that
//! adds one upstream callback therefore produces a green build and a red gate.
//!
//! MEASURED 2026-08-20. `test.dbt_parity` had been red for nine days on one
//! missing symbol, `reverie_dbt_runtime_kind_code`. Advancing the Reverie pin
//! from `c261050cf` to `af82f1b9` took the gap from ONE symbol to FIVE, because
//! reverie `268a25b6` added a versioned ABI handshake
//! (`reverie_dbt_runtime_abi_version`, `reverie_dbt_runtime_callbacks_size`)
//! and renamed two callbacks to `_v2` spellings. Hermit kept exporting the two
//! un-suffixed originals, which nothing calls. DynamoRIO reports the whole
//! class as `<ERROR: using undefined symbol!>` and names no symbol.
//!
//! # THREE OUTCOMES, NOT TWO
//!
//! PASS, REFUSE, and COULD-NOT-DETERMINE are distinct, and every
//! could-not-determine FAILS CLOSED with rc=2. An unreadable artifact must
//! never read as "the symbol sets agree":
//!
//!   * either artifact missing or unreadable -> rc=2
//!   * `nm` absent or failing                -> rc=2
//!   * the client declares ZERO callbacks    -> rc=2, because a scope that
//!     selected nothing cannot certify anything. This one is not theoretical:
//!     `target/install_pkg/rsrcs/libdetcore_dbt.so` is a SYMLINK to
//!     `../../release/libdetcore_dbt.so`, a plain `cargo build --workspace
//!     --bins` leaves it dangling, and a dangling symlink reads as a library
//!     that exports nothing.
//!
//! # SCOPE
//!
//! Symbols named `reverie_dbt_runtime_*` only. Everything else the client needs
//! comes from DynamoRIO core or its extensions, which DynamoRIO's own build
//! system already checks.
//!
//! Usage:
//!   scripts/check-dbt-runtime-abi.rs [--client PATH] [--runtime PATH]

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

const PREFIX: &str = "reverie_dbt_runtime_";
const DEFAULT_CLIENT: &str = "target/install_pkg/rsrcs/libreverie_dbt_client.so";
const DEFAULT_RUNTIME: &str = "target/release/libdetcore_dbt.so";

fn refuse(title: &str, body: &str) -> ! {
    eprintln!("======================================================================");
    eprintln!("DBT RUNTIME ABI LINT: {title}");
    eprintln!("======================================================================");
    eprintln!("{body}");
    std::process::exit(if title.contains("CHECKER ERROR") {
        2
    } else {
        1
    });
}

/// Resolve through symlinks and require a real, readable file.
///
/// `install_pkg/rsrcs/libdetcore_dbt.so` is a symlink into `target/release`.
/// `Path::exists` follows links and so already returns false for a dangling
/// one, but the message has to say WHICH failure it was or the reader spends
/// the next ten minutes discovering the link.
fn require_artifact(path: &Path, what: &str) {
    if path.exists() {
        return;
    }
    let hint = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            let target = std::fs::read_link(path)
                .map(|t| t.display().to_string())
                .unwrap_or_else(|_| "<unreadable>".to_string());
            format!(
                "{} is a DANGLING SYMLINK -> {target}\nThe release cdylib is produced only by the \
                 runtime_release build:\n  cargo build --release --locked -p hermit --features \
                 third-party-backends -p detcore-dbt -p detcore-sabre -p hermit-install",
                path.display()
            )
        }
        _ => format!("{} does not exist", path.display()),
    };
    refuse(
        "CHECKER ERROR - COULD NOT DETERMINE",
        &format!("cannot read the {what}: {hint}"),
    );
}

/// Undefined (`U`) or defined symbols from a shared object, filtered to the
/// runtime-callback prefix. `nm -D` reads the dynamic table, which is the table
/// the loader itself consults.
fn symbols(path: &Path, undefined: bool) -> BTreeSet<String> {
    let flag = if undefined {
        "--undefined-only"
    } else {
        "--defined-only"
    };
    let out = Command::new("nm")
        .args(["-D", flag, &path.display().to_string()])
        .output()
        .unwrap_or_else(|e| {
            refuse(
                "CHECKER ERROR - COULD NOT DETERMINE",
                &format!("could not run `nm -D {flag}` on {}: {e}", path.display()),
            )
        });
    if !out.status.success() {
        refuse(
            "CHECKER ERROR - COULD NOT DETERMINE",
            &format!(
                "`nm -D {flag} {}` exited {}:\n{}",
                path.display(),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        );
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().last())
        .map(|s| s.split('@').next().unwrap_or(s).to_string())
        .filter(|s| s.starts_with(PREFIX))
        .collect()
}

fn main() {
    rust_script_prelude::init();
    let mut client = PathBuf::from(DEFAULT_CLIENT);
    let mut runtime = PathBuf::from(DEFAULT_RUNTIME);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--client" => client = PathBuf::from(args.next().unwrap_or_default()),
            "--runtime" => runtime = PathBuf::from(args.next().unwrap_or_default()),
            other => refuse(
                "CHECKER ERROR - COULD NOT DETERMINE",
                &format!("unknown argument {other:?}; usage: [--client PATH] [--runtime PATH]"),
            ),
        }
    }

    require_artifact(&client, "DBT client");
    require_artifact(&runtime, "Detcore DBT runtime");

    let needed = symbols(&client, true);
    let supplied = symbols(&runtime, false);

    // A scope that selected nothing cannot certify anything.
    if needed.is_empty() {
        refuse(
            "CHECKER ERROR - COULD NOT DETERMINE",
            &format!(
                "{} declares ZERO {PREFIX}* symbols. Either the artifact is not the DBT client or \
                 it was built without the native client; refusing to pass vacuously.",
                client.display()
            ),
        );
    }

    let missing: Vec<&String> = needed.difference(&supplied).collect();
    let orphaned: Vec<&String> = supplied.difference(&needed).collect();

    println!(
        "Scope: {} declares {} {PREFIX}* callback(s); {} supplies {}.",
        client.display(),
        needed.len(),
        runtime.display(),
        supplied.len()
    );

    if !missing.is_empty() {
        let list = missing
            .iter()
            .map(|s| format!("    {s}"))
            .collect::<Vec<_>>()
            .join("\n");
        refuse(
            "REFUSED - the client calls callbacks Hermit does not supply",
            &format!(
                "The DBT client will fail to load and DynamoRIO will report only\n  \
                 <ERROR: using undefined symbol!>\nwithout naming any of them.\n\n\
                 MISSING ({}):\n{list}\n\n\
                 Define each in detcore-dbt/src/lib.rs with #[unsafe(no_mangle)] and the exact\n\
                 signature declared in reverie-dbt/native/client.c. Hermit disables reverie-dbt's\n\
                 `prototype-runtime` feature by design, so upstream's definitions are not compiled\n\
                 in and every callback has to be supplied here.",
                missing.len()
            ),
        );
    }

    // An orphan is a warning, not a refusal: it is dead weight rather than a
    // load failure, and a deliberate transition may carry both spellings for
    // one commit. It is reported because an orphan is usually the other half of
    // a rename whose new spelling is already missing above.
    if !orphaned.is_empty() {
        println!(
            "WARNING: {} exports {} callback(s) the client does not declare -- dead exports, \
             usually the old half of a rename:",
            runtime.display(),
            orphaned.len()
        );
        for s in &orphaned {
            println!("    {s}");
        }
    }

    println!(
        "DBT runtime ABI is complete: all {} client callback(s) are supplied by Hermit.",
        needed.len()
    );
}
