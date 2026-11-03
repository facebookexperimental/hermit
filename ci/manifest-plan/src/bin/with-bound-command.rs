// Copyright (c) Meta Platforms, Inc. and affiliates.
// This source code is licensed under the BSD-style license found in LICENSE.
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Command;

use hermit_manifest_plan::executable_binding::read_bindings;
fn run() -> Result<i32, String> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() < 6 || args[0] != "--bindings" || args[2] != "--backend" || args[4] != "--" {
        return Err(
            "usage: with-bound-command --bindings FILE --backend NAME -- COMMAND [ARG...]".into(),
        );
    }
    let held = read_bindings(Some(Path::new(&args[1])))?;
    let binding = held
        .get(&args[3])
        .ok_or("required catalog executable binding is missing")?;
    binding.recheck()?;
    let result = Command::new(&args[5]).args(&args[6..]).status();
    // Identity refusal never turns a physical failure into success; recheck on
    // both returns, retaining the original child status when custody is intact.
    binding.recheck()?;
    let status = result.map_err(|e| e.to_string())?;
    Ok(status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)))
}
fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("with-bound-command: {e}");
            std::process::exit(2)
        }
    }
}
