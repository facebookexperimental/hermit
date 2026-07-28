#!/usr/bin/env rust-script
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! Expand the schema-v2 e2e manifests into runnable command files.
//!
//! Run from anywhere inside the checkout:
//!
//! ```text
//! ./scripts/manifest-to-commands.rs
//! ```
//!
//! Each `ignored/e2e-commands/<bucket>.txt` file contains one self-contained
//! shell command per enabled `(test, mode, backend)` cell. Chaos seeds expand
//! to separate lines. Commands compile implicit C/Rust guests and prepare shell
//! wrappers before invoking Hermit, so any individual line can be rerun from
//! the repository root.
//!
//! ```cargo
//! [dependencies]
//! toml = "0.8"
//! ```

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;

use toml::Value;

const MANIFEST_SCHEMA: i64 = 2;
const RUN_ENV: &str = "env LC_ALL=C TZ=UTC HOME=\"$cell/home\" XDG_CONFIG_HOME=\"$cell/xdg-config\" E2E_TMPDIR=\"$cell/tmp\" E2E_FIXTURE_DIR=\"$cell/fixtures\"";

fn fail(message: impl AsRef<str>) -> ! {
    eprintln!("manifest-to-commands: {}", message.as_ref());
    std::process::exit(2);
}

fn repo_root() -> PathBuf {
    let script = Path::new(file!());
    let root = script
        .parent()
        .and_then(Path::parent)
        .unwrap_or_else(|| Path::new("."));
    root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&c))
    {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn slug(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn string_array(value: Option<&Value>, context: &str) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    let array = value
        .as_array()
        .unwrap_or_else(|| fail(format!("{context} must be an array")));
    array
        .iter()
        .map(|item| {
            item.as_str()
                .unwrap_or_else(|| fail(format!("{context} entries must be strings")))
                .to_owned()
        })
        .collect()
}

fn integer_array(value: Option<&Value>, context: &str) -> Vec<i64> {
    let Some(value) = value else {
        return Vec::new();
    };
    let array = value
        .as_array()
        .unwrap_or_else(|| fail(format!("{context} must be an array")));
    array
        .iter()
        .map(|item| {
            item.as_integer()
                .unwrap_or_else(|| fail(format!("{context} entries must be integers")))
        })
        .collect()
}

fn test_id(test: &Value, bucket: &str) -> String {
    test.get("id")
        .and_then(Value::as_str)
        .unwrap_or_else(|| fail(format!("{bucket}: [[test]] is missing `id`")))
        .to_owned()
}

fn setup_prefix(test: &Value, id: &str) -> (String, String) {
    let cell = format!("ignored/e2e-commands/work/{}", slug(id));
    let mut commands = vec![
        format!("cell={}", shell_quote(&cell)),
        "hermit_bin=${HERMIT_BIN:-target/release/hermit}".to_owned(),
        "mkdir -p \"$cell/home\" \"$cell/xdg-config\" \"$cell/tmp\" \"$cell/fixtures\" \"$cell/captures\""
            .to_owned(),
        "if [ -d tests/e2e/xdg-config ]; then cp -a tests/e2e/xdg-config/. \"$cell/xdg-config/\"; fi"
            .to_owned(),
    ];

    let program = test.get("program").and_then(Value::as_str);
    let direct = test.get("direct").and_then(Value::as_str);
    let guest = match (program, direct) {
        (Some(_), Some(_)) => fail(format!("{id}: set only one of `program` and `direct`")),
        (None, None) => fail(format!("{id}: missing `program` or `direct`")),
        (None, Some(command)) => format!("sh -c {}", shell_quote(command)),
        (Some(program), None) => match Path::new(program).extension().and_then(|x| x.to_str()) {
            Some("sh") => {
                let script = shell_quote(program);
                commands.push(format!("{RUN_ENV} {script} --prepare"));
                format!("{script} --run")
            }
            Some("c") => {
                let build = test.get("build").and_then(Value::as_table);
                let mut args = vec![
                    "-std=c11".to_owned(),
                    "-O2".to_owned(),
                    "-g".to_owned(),
                    "-Wall".to_owned(),
                    "-Wextra".to_owned(),
                    "-Werror".to_owned(),
                ];
                if let Some(build) = build {
                    args.extend(string_array(
                        build.get("cflags"),
                        &format!("{id}.build.cflags"),
                    ));
                }
                args.push(program.to_owned());
                if let Some(build) = build {
                    args.extend(string_array(
                        build.get("extra_sources"),
                        &format!("{id}.build.extra_sources"),
                    ));
                }
                let args = args
                    .iter()
                    .map(|x| shell_quote(x))
                    .collect::<Vec<_>>()
                    .join(" ");
                commands.push(format!("${{CC:-cc}} {args} -o \"$cell/guest\""));
                "\"$cell/guest\"".to_owned()
            }
            Some("rs") => {
                let build = test.get("build").and_then(Value::as_table);
                let mut args = vec!["-O".to_owned()];
                if let Some(build) = build {
                    args.extend(string_array(
                        build.get("cflags"),
                        &format!("{id}.build.cflags"),
                    ));
                }
                args.push(program.to_owned());
                let args = args
                    .iter()
                    .map(|x| shell_quote(x))
                    .collect::<Vec<_>>()
                    .join(" ");
                commands.push(format!("${{RUSTC:-rustc}} {args} -o \"$cell/guest\""));
                "\"$cell/guest\"".to_owned()
            }
            other => fail(format!("{id}: unsupported program extension {other:?}")),
        },
    };

    (commands.join(" && "), guest)
}

fn hermit_command(
    mode: &str,
    backend: &str,
    lane: &str,
    timeout: i64,
    seed: Option<i64>,
    extra: &[String],
    guest: &str,
) -> String {
    let portable = lane == "portable";
    let profile = if portable {
        " --no-virtualize-cpuid --max-timeslice=disabled"
    } else {
        ""
    };
    let command = match mode {
        "verify" => format!(
            "{RUN_ENV} \"$hermit_bin\" --log=info run --backend {} --strict --verify{profile} -- {guest}",
            shell_quote(backend)
        ),
        "replay" => format!(
            "{RUN_ENV} \"$hermit_bin\" --log=info --backend {} record start --strict --verify -- {guest}",
            shell_quote(backend)
        ),
        "chaos" => format!(
            "{RUN_ENV} \"$hermit_bin\" --log=off run --backend {} --strict --chaos --sched-heuristic=random --seed={}{profile} -- {guest}",
            shell_quote(backend),
            seed.unwrap_or(0)
        ),
        "custom" => {
            let extra = extra
                .iter()
                .map(|x| shell_quote(x))
                .collect::<Vec<_>>()
                .join(" ");
            let separator = if extra.is_empty() { "" } else { " " };
            format!(
                "{RUN_ENV} \"$hermit_bin\" --log=info run --backend {} --strict{separator}{extra} -- {guest}",
                shell_quote(backend)
            )
        }
        other => fail(format!("unsupported mode `{other}`")),
    };
    format!("timeout --kill-after=10s {timeout}s {command}")
}

fn repeat(command: &str, count: i64) -> String {
    if count <= 1 {
        return command.to_owned();
    }
    let iterations = (1..=count)
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    format!("for _run in {iterations}; do {command} || exit; done")
}

fn commands_for_test(test: &Value, bucket: &str) -> Vec<String> {
    let id = test_id(test, bucket);
    let lane = test
        .get("lane")
        .and_then(Value::as_str)
        .unwrap_or("portable");
    let timeout = test
        .get("timeout_seconds")
        .and_then(Value::as_integer)
        .unwrap_or(60);
    let modes = test
        .get("modes")
        .and_then(Value::as_table)
        .unwrap_or_else(|| fail(format!("{id}: missing `modes`")));
    let (setup, guest) = setup_prefix(test, &id);
    let mut mode_names = modes.keys().map(String::as_str).collect::<Vec<_>>();
    mode_names.sort_unstable();
    let mut lines = Vec::new();

    for mode in mode_names {
        let spec = &modes[mode];
        if mode == "naked" {
            let runs = spec.get("runs").and_then(Value::as_integer).unwrap_or(3);
            let run = format!("timeout --kill-after=10s {timeout}s {RUN_ENV} {guest}");
            lines.push(format!(
                "{setup} && {} # {id} mode=naked backend=native",
                repeat(&run, runs)
            ));
            continue;
        }

        let backends = string_array(
            spec.get("backends_enabled"),
            &format!("{id}.modes.{mode}.backends_enabled"),
        );
        if backends.is_empty() {
            fail(format!("{id}: mode `{mode}` has no enabled backend"));
        }
        let extra = string_array(spec.get("args"), &format!("{id}.modes.{mode}.args"));
        let assert = spec.get("assert").and_then(Value::as_table);
        let custom_runs = assert
            .and_then(|a| a.get("runs"))
            .and_then(Value::as_integer)
            .unwrap_or(1);
        let seeds = if mode == "chaos" {
            let seeds = integer_array(spec.get("seeds"), &format!("{id}.modes.chaos.seeds"));
            if seeds.is_empty() { vec![0, 1] } else { seeds }
        } else {
            vec![0]
        };

        for backend in backends {
            for seed in &seeds {
                let seed = (mode == "chaos").then_some(*seed);
                let command = hermit_command(mode, &backend, lane, timeout, seed, &extra, &guest);
                let runs = match mode {
                    "chaos" => 2,
                    "custom" => custom_runs,
                    _ => 1,
                };
                let seed_note = seed.map(|s| format!(" seed={s}")).unwrap_or_default();
                lines.push(format!(
                    "{setup} && {} # {id} mode={mode} backend={backend}{seed_note}",
                    repeat(&command, runs)
                ));
            }
        }
    }
    lines
}

// TODO-HUMAN-REVIEW(PR-1081): Review the manifest-to-command CLI and generated shell contract.
fn main() -> ExitCode {
    let root = repo_root();
    let manifests = root.join("tests/e2e/manifests");
    let output = root.join("ignored/e2e-commands");
    fs::create_dir_all(&output)
        .unwrap_or_else(|e| fail(format!("cannot create {}: {e}", output.display())));
    for entry in fs::read_dir(&output)
        .unwrap_or_else(|e| fail(format!("cannot read {}: {e}", output.display())))
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if path.is_file() && path.extension().is_some_and(|ext| ext == "txt") {
            fs::remove_file(&path)
                .unwrap_or_else(|e| fail(format!("cannot remove stale {}: {e}", path.display())));
        }
    }

    let mut paths = fs::read_dir(&manifests)
        .unwrap_or_else(|e| fail(format!("cannot read {}: {e}", manifests.display())))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect::<Vec<_>>();
    paths.sort();

    let mut files = 0usize;
    let mut commands = 0usize;
    for path in paths {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_else(|| {
                fail(format!(
                    "manifest has a non-UTF-8 file name: {}",
                    path.display()
                ))
            });
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|e| fail(format!("cannot read {}: {e}", path.display())));
        let manifest: Value = source
            .parse()
            .unwrap_or_else(|e| fail(format!("{}: invalid TOML: {e}", path.display())));
        let schema = manifest.get("schema").and_then(Value::as_integer);
        if schema != Some(MANIFEST_SCHEMA) {
            fail(format!(
                "{}: expected schema {MANIFEST_SCHEMA}, got {schema:?}",
                path.display()
            ));
        }
        let bucket = manifest
            .get("bucket")
            .and_then(Value::as_str)
            .unwrap_or_else(|| fail(format!("{}: missing `bucket`", path.display())));
        if bucket != stem {
            fail(format!(
                "{}: bucket `{bucket}` must match file stem `{stem}`",
                path.display()
            ));
        }
        let tests = manifest
            .get("test")
            .and_then(Value::as_array)
            .unwrap_or_else(|| fail(format!("{}: missing [[test]] entries", path.display())));
        let mut lines = Vec::new();
        for test in tests {
            lines.extend(commands_for_test(test, bucket));
        }
        let destination = output.join(format!("{stem}.txt"));
        let body = if lines.is_empty() {
            String::new()
        } else {
            format!("{}\n", lines.join("\n"))
        };
        fs::write(&destination, body)
            .unwrap_or_else(|e| fail(format!("cannot write {}: {e}", destination.display())));
        println!(
            "{}: {} commands",
            destination
                .strip_prefix(&root)
                .unwrap_or(&destination)
                .display(),
            lines.len()
        );
        files += 1;
        commands += lines.len();
    }

    println!("generated {commands} commands across {files} bucket files");
    ExitCode::SUCCESS
}
