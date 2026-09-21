#!/usr/bin/env -S rust-script --force
//! Prepare or consume source-bound Nextest executable metadata.
//! ```cargo
//! [dependencies]
//! hermit-manifest-plan = { path = "manifest-plan" }
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

#[path = "../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

fn run() -> Result<i32, String> {
    let root = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--print-executable") => {
            if args.next().is_some() { return Err("unexpected executable query argument".into()); }
            println!("{}", std::env::current_exe().map_err(|e| e.to_string())?.display());
            Ok(0)
        }
        Some("prepare") => {
            let profile = args.next().ok_or("prepare requires a committed graph profile")?;
            if args.next().is_some() { return Err("unexpected prepare argument".into()); }
            match hermit_manifest_plan::nextest_binaries::prepare(&root, &profile) {
                Ok(()) => Ok(0),
                Err(error) => {
                    eprintln!("prepared-nextest: {error}");
                    Ok(i32::from(error.status))
                }
            }
        }
        Some("assert") => {
            let profile = args.next().ok_or("assert requires a committed graph profile")?;
            if args.next().is_some() { return Err("unexpected assert argument".into()); }
            hermit_manifest_plan::nextest_binaries::assert_profile(&root, &profile)?;
            Ok(0)
        }
        Some("executable") => {
            let package = args.next().ok_or("executable requires a Cargo package")?;
            let name = args.next().ok_or("executable requires a test target")?;
            if args.next().is_some() { return Err("unexpected executable argument".into()); }
            println!("{}", hermit_manifest_plan::nextest_binaries::executable(&root, &package, &name)?.display());
            Ok(0)
        }
        Some(operation @ ("cpu-wrapper" | "build-cpu-wrapper")) => {
            if args.next().is_some() { return Err("unexpected CPU wrapper query argument".into()); }
            let path = if operation == "cpu-wrapper" {
                hermit_manifest_plan::nextest_binaries::cpu_wrapper(&root)?
            } else {
                match hermit_manifest_plan::nextest_binaries::build_cpu_wrapper(&root) {
                    Ok(path) => path,
                    Err(error) => {
                        eprintln!("prepared-nextest: {error}");
                        return Ok(i32::from(error.status));
                    }
                }
            };
            println!("{}", path.display());
            Ok(0)
        }
        Some(operation @ ("run" | "list")) => {
            let mut remaining = args.collect::<Vec<_>>();
            let config = if remaining.first().map(String::as_str) == Some("--config-file") {
                if remaining.len() < 2 { return Err("--config-file requires a path".into()); }
                let path = PathBuf::from(remaining.remove(1));
                remaining.remove(0);
                Some(path)
            } else { None };
            hermit_manifest_plan::nextest_binaries::run(&root, operation, config.as_deref(), &remaining)
        }
        _ => Err("usage: ci/nextest-binaries.rs prepare PROFILE | run|list [--config-file PATH] NEXTEST_ARGS...".into()),
    }
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    match run() {
        Ok(status) => ExitCode::from(u8::try_from(status).unwrap_or(1)),
        Err(error) => {
            eprintln!("prepared-nextest: {error}");
            ExitCode::from(2)
        }
    }
}
