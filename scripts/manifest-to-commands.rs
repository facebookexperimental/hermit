#!/usr/bin/env -S rust-script --force
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! Expand the schema-v3 e2e manifests into runnable command files.
//!
//! Run from anywhere inside the checkout:
//!
//! ```text
//! ./scripts/manifest-to-commands.rs
//! ```
//!
//! Each `ignored/e2e-commands/<bucket>.txt` file contains one self-contained
//! shell command per enabled `(test, mode, backend)` cell. Commands compile
//! implicit C/Rust guests and prepare shell wrappers before invoking Hermit, so
//! any individual line can be rerun from the repository root.
//!
//! ```cargo
//! [dependencies]
//! serde = { version = "1", features = ["derive"] }
//! serde_json = "1"
//! serde_yaml = "0.9"
//! toml = "0.8"
//! ```

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

#[path = "../ci/manifest-plan/src/manifest_value.rs"]
mod manifest_value;

// This rust-script adapter uses only the schema-facing subset of the shared
// timeout module; the manifest-plan crate consumes its calibration API.
#[allow(dead_code)]
#[path = "../ci/manifest-plan/src/timeouts.rs"]
mod timeouts;

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;

use manifest_value::Value;
use timeouts::DEFAULTS_FILE;
use timeouts::MANIFEST_SCHEMA;
use timeouts::MAX_TIMEOUT_SECONDS;
use timeouts::MIN_TIMEOUT_SECONDS;
use timeouts::resolve_timeout_seconds;

const HERMIT_BACKENDS: [&str; 5] = ["ptrace", "dbt", "kvm", "sabre", "liteinst"];
const NAKED_BACKENDS: [&str; 1] = ["native"];
const RUN_ENV: &str = "env LC_ALL=C TZ=UTC HOME=\"$cell/home\" XDG_CONFIG_HOME=\"$cell/xdg-config\" E2E_TMPDIR=\"$cell/tmp\" E2E_FIXTURE_DIR=\"$cell/fixtures\"";
const HERMIT_RUN_ENV: &str = "env LC_ALL=C TZ=UTC HOME=\"$cell/home\" XDG_CONFIG_HOME=\"$cell/xdg-config\" E2E_TMPDIR=/tmp/hermit-e2e E2E_FIXTURE_DIR=\"$cell/fixtures\"";
const HERMIT_GUEST_ENV_ARGS: &str = "--env LC_ALL=C --env TZ=UTC --env HOME=\"$cell/home\" --env XDG_CONFIG_HOME=\"$cell/xdg-config\" --env E2E_TMPDIR=/tmp/hermit-e2e --env E2E_FIXTURE_DIR=\"$cell/fixtures\"";

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
    if value.bytes().any(|byte| !(b' '..=b'~').contains(&byte)) {
        let mut quoted = String::from("$'");
        for byte in value.bytes() {
            match byte {
                b'\\' => quoted.push_str("\\\\"),
                b'\'' => quoted.push_str("\\'"),
                b'\n' => quoted.push_str("\\n"),
                b'\r' => quoted.push_str("\\r"),
                b'\t' => quoted.push_str("\\t"),
                b' '..=b'~' => quoted.push(char::from(byte)),
                _ => {
                    quoted.push_str("\\x");
                    quoted.push(char::from(b"0123456789abcdef"[(byte >> 4) as usize]));
                    quoted.push(char::from(b"0123456789abcdef"[(byte & 0x0f) as usize]));
                }
            }
        }
        quoted.push('\'');
        return quoted;
    }
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

fn timeout_seconds(value: Option<&Value>, context: &str) -> i64 {
    match value.and_then(Value::as_integer) {
        Some(timeout)
            if (MIN_TIMEOUT_SECONDS as i64..=MAX_TIMEOUT_SECONDS as i64).contains(&timeout) =>
        {
            timeout
        }
        other => fail(format!(
            "{context} must be {MIN_TIMEOUT_SECONDS}..={MAX_TIMEOUT_SECONDS}, got {other:?}"
        )),
    }
}

fn cell_timeout_seconds(spec: &Value, backend: &str, inherited: i64, id: &str, mode: &str) -> i64 {
    let Some(value) = spec.get("timeout_seconds") else {
        return inherited;
    };
    let table = value.as_table().unwrap_or_else(|| {
        fail(format!(
            "{id}.modes.{mode}.timeout_seconds must be a backend table"
        ))
    });
    table.get(backend).map_or(inherited, |value| {
        timeout_seconds(
            Some(value),
            &format!("{id}.modes.{mode}.timeout_seconds.{backend}"),
        )
    })
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
        "run_verify_strict=; if run_help=$(\"$hermit_bin\" run --help 2>&1); then case \"$run_help\" in *--verify-strict*) run_verify_strict=--verify-strict;; esac; fi".to_owned(),
        "record_verify_strict=; if record_help=$(\"$hermit_bin\" record start --help 2>&1); then case \"$record_help\" in *--verify-strict*) record_verify_strict=--verify-strict;; esac; fi".to_owned(),
        "mkdir -p \"$cell/home\" \"$cell/xdg-config\" \"$cell/tmp\" \"$cell/fixtures\" \"$cell/captures\""
            .to_owned(),
        "if [ -d tests/e2e/xdg-config ]; then cp -a tests/e2e/xdg-config/. \"$cell/xdg-config/\"; fi"
            .to_owned(),
    ];

    let program = test.get("program").and_then(Value::as_str);
    let direct = test.get("direct");
    let guest = match (program, direct) {
        (Some(_), Some(_)) => fail(format!("{id}: set only one of `program` and `direct`")),
        (None, None) => fail(format!("{id}: missing `program` or `direct`")),
        (None, Some(Value::String(command))) => format!("sh -c {}", shell_quote(command)),
        (None, Some(Value::Array(_))) => {
            let argv = string_array(direct, &format!("{id}.direct"));
            if argv.is_empty() {
                fail(format!("{id}: direct argv must not be empty"));
            }
            argv.iter()
                .map(|argument| shell_quote(argument))
                .collect::<Vec<_>>()
                .join(" ")
        }
        (None, Some(_)) => fail(format!(
            "{id}: direct must be a shell command string or an argv array"
        )),
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

/// Per-backend guest arguments declared by `modes.<mode>.guest_args.<backend>`.
///
/// These are arguments for the **guest**, not for Hermit, so they go after the
/// `--` separator. They are per-backend because a manifest may qualify one
/// backend on a cheaper scenario than another.
///
/// A guest that requires an argument and is not given one prints usage and
/// exits non-zero. Before this channel existed, every consumer of the manifests
/// invoked such a guest bare, and the resulting non-zero exit was recorded as a
/// determinism failure — see rrnewton/hermit#1815.
fn mode_guest_args(spec: &Value, mode: &str, backend: &str, id: &str) -> Vec<String> {
    let Some(by_backend) = spec.get("guest_args") else {
        return Vec::new();
    };
    let by_backend = by_backend
        .as_table()
        .unwrap_or_else(|| fail(format!("{id}.modes.{mode}.guest_args must be a table")));
    let args = string_array(
        by_backend.get(backend),
        &format!("{id}.modes.{mode}.guest_args.{backend}"),
    );
    if args.iter().any(|argument| argument.contains('\0')) {
        fail(format!(
            "{id}: modes.{mode}.guest_args.{backend} contains a NUL byte, which Linux argv cannot represent"
        ));
    }
    args
}

/// Append the guest's own arguments to an already-quoted guest command.
///
/// `sh -c` consumes the next word as `$0`, so string-form direct commands need
/// an explicit placeholder when (and only when) manifest arguments follow.
fn guest_with_args(test: &Value, guest: &str, guest_args: &[String]) -> String {
    if guest_args.is_empty() {
        return guest.to_owned();
    }
    let rendered = guest_args
        .iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    let argv0 = if matches!(test.get("direct"), Some(Value::String(_))) {
        " --"
    } else {
        ""
    };
    format!("{guest}{argv0} {rendered}")
}

fn hermit_command(
    mode: &str,
    backend: &str,
    lane: &str,
    seed: Option<i64>,
    extra: &[String],
    verify_bitwise_parity: bool,
    guest: &str,
) -> String {
    let _lane = lane;
    let profile = "";
    let command = match mode {
        "verify" => {
            let _verify_bitwise_parity = verify_bitwise_parity;
            format!(
                "{HERMIT_RUN_ENV} \"$hermit_bin\" --log=info run --base-env=minimal --backend {} --strict $run_verify_strict --verify --verify-json \"$cell/captures/verify.json\"{profile} -- {guest}",
                shell_quote(backend)
            )
        }
        "replay" => format!(
            "{HERMIT_RUN_ENV} \"$hermit_bin\" --log info --backend {} record start --strict $record_verify_strict --verify --verify-json \"$cell/captures/verify.json\" --data-dir \"$cell/recording\" --record-timeout \"$remaining\" {HERMIT_GUEST_ENV_ARGS} -- {guest}",
            shell_quote(backend)
        ),
        "chaos" => format!(
            "{HERMIT_RUN_ENV} \"$hermit_bin\" --log=info run --base-env=minimal --backend {} --strict $run_verify_strict --verify --verify-allow=both --verify-json \"$cell/captures/verify-seed-{}.json\" --chaos --sched-heuristic=random --seed={}{profile} -- {guest}",
            shell_quote(backend),
            seed.unwrap_or(0),
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
                "{HERMIT_RUN_ENV} \"$hermit_bin\" --log=info run --backend {}{separator}{extra} -- {guest}",
                shell_quote(backend)
            )
        }
        other => fail(format!("unsupported mode `{other}`")),
    };
    command
}

fn bounded_invocation(command: &str, id: &str, attempt: &str) -> String {
    format!(
        "remaining=$((cell_deadline - SECONDS)); if [ \"$remaining\" -le 0 ]; then printf '%s\\n' {}; exit 124; fi; {command}",
        shell_quote(&format!(
            "{id} exceeded its per-cell timeout before {attempt}"
        ))
    )
}

fn repeat(command: &str, count: i64, id: &str) -> String {
    if count <= 1 {
        return bounded_invocation(command, id, "attempt 1");
    }
    let iterations = (1..=count)
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let command = bounded_invocation(command, id, "a repeated attempt");
    format!("for _run in {iterations}; do {command} || exit; done")
}

fn outer_cell_command(setup: &str, run: &str, timeout: i64) -> String {
    let body = format!("cell_deadline=$((SECONDS + {timeout})); {setup} && {run}");
    format!(
        "timeout --kill-after=10s {timeout}s bash -c {}",
        shell_quote(&body)
    )
}

fn commands_for_test(test: &Value, bucket: &str, inherited_timeout_seconds: i64) -> Vec<String> {
    let id = test_id(test, bucket);
    let lane = test
        .get("lane")
        .and_then(Value::as_str)
        .unwrap_or("portable");
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
            let backends = string_array(
                spec.get("backends_enabled"),
                &format!("{id}.modes.naked.backends_enabled"),
            );
            if !backends.iter().any(|backend| backend == "native") {
                continue;
            }
            let runs = spec.get("runs").and_then(Value::as_integer).unwrap_or(3);
            let timeout =
                cell_timeout_seconds(spec, "native", inherited_timeout_seconds, &id, mode);
            let guest_args = mode_guest_args(spec, mode, "native", &id);
            let guest = guest_with_args(test, &guest, &guest_args);
            let run = format!("{RUN_ENV} {guest}");
            lines.push(format!(
                "{} # {id} mode=naked backend=native",
                outer_cell_command(&setup, &repeat(&run, runs, &id), timeout)
            ));
            continue;
        }

        let backends = string_array(
            spec.get("backends_enabled"),
            &format!("{id}.modes.{mode}.backends_enabled"),
        );
        if backends.is_empty() {
            continue;
        }
        let extra = string_array(spec.get("args"), &format!("{id}.modes.{mode}.args"));
        // `args` are Hermit's; `guest_args` are the guest's and are per-backend,
        // so they are resolved inside the backend loop below.
        let assert = spec.get("assert").and_then(Value::as_table);
        let custom_runs = assert
            .and_then(|a| a.get("runs"))
            .and_then(Value::as_integer)
            .unwrap_or(1);
        let verify_bitwise_parity = mode == "verify"
            && assert
                .and_then(|a| a.get("bitwise_parity"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
        let seeds = if mode == "chaos" {
            let seeds = integer_array(spec.get("seeds"), &format!("{id}.modes.chaos.seeds"));
            if seeds.is_empty() { vec![0, 1] } else { seeds }
        } else {
            vec![0]
        };

        for backend in backends {
            let timeout =
                cell_timeout_seconds(spec, &backend, inherited_timeout_seconds, &id, mode);
            let guest_args = mode_guest_args(spec, mode, &backend, &id);
            let guest = guest_with_args(test, &guest, &guest_args);
            let mut invocations = Vec::new();
            for seed in &seeds {
                let seed = (mode == "chaos").then_some(*seed);
                let command = hermit_command(
                    mode,
                    &backend,
                    lane,
                    seed,
                    &extra,
                    verify_bitwise_parity,
                    &guest,
                );
                let runs = if mode == "custom" { custom_runs } else { 1 };
                let attempt = seed
                    .map(|value| format!("seed {value}"))
                    .unwrap_or_else(|| "attempt 1".into());
                let invocation = if runs > 1 {
                    repeat(&command, runs, &id)
                } else {
                    bounded_invocation(&command, &id, &attempt)
                };
                invocations.push(invocation);
            }
            lines.push(format!(
                "{} # {id} mode={mode} backend={backend}",
                outer_cell_command(&setup, &invocations.join(" && "), timeout)
            ));
        }
    }
    lines
}

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd, serde::Serialize)]
struct GuestArgsRecord {
    test_id: String,
    mode: String,
    backend: String,
    args: Vec<String>,
}

/// Emit every declared per-backend guest-argument vector as JSON Lines on
/// stdout, sorted by test id, mode, and backend.
///
/// This is the machine-readable form of the same `guest_args` the generated
/// commands embed, so an out-of-tree harness (the `compat-envelope` corpus
/// collector) can invoke a guest correctly without maintaining a second copy of
/// the argument list, which would drift. JSON preserves empty strings, tabs,
/// newlines, and explicitly empty vectors without delimiter ambiguity.
fn guest_args_json_lines(tests: &[(String, i64, Value)]) -> Result<Vec<String>, String> {
    let mut records = Vec::new();
    for (bucket, _, test) in tests {
        let id = test_id(test, bucket);
        let Some(modes) = test.get("modes").and_then(Value::as_table) else {
            continue;
        };
        let mut mode_names = modes.keys().map(String::as_str).collect::<Vec<_>>();
        mode_names.sort_unstable();
        for mode in mode_names {
            let spec = &modes[mode];
            let known_backends = if mode == "naked" {
                &NAKED_BACKENDS[..]
            } else {
                &HERMIT_BACKENDS[..]
            };
            let Some(by_backend) = spec.get("guest_args") else {
                continue;
            };
            let by_backend = by_backend
                .as_table()
                .unwrap_or_else(|| fail(format!("{id}.modes.{mode}.guest_args must be a table")));
            let enabled = string_array(
                spec.get("backends_enabled"),
                &format!("{id}.modes.{mode}.backends_enabled"),
            );
            let disabled = spec.get("backends_disabled").and_then(Value::as_table);
            let mut backends = by_backend.keys().map(String::as_str).collect::<Vec<_>>();
            backends.sort_unstable();
            for backend in backends {
                if !known_backends.contains(&backend) {
                    return Err(format!(
                        "{id}: modes.{mode}.guest_args.{backend} names unknown backend for this mode; expected one of {known_backends:?}"
                    ));
                }
                if !enabled.iter().any(|name| name == backend)
                    && !disabled.is_some_and(|backends| backends.contains_key(backend))
                {
                    return Err(format!(
                        "{id}: modes.{mode}.guest_args.{backend} names a backend outside backends_enabled/backends_disabled"
                    ));
                }
                let args = mode_guest_args(spec, mode, backend, &id);
                records.push(GuestArgsRecord {
                    test_id: id.clone(),
                    mode: mode.to_owned(),
                    backend: backend.to_owned(),
                    args,
                });
            }
        }
    }
    records.sort();
    records
        .iter()
        .map(|record| {
            serde_json::to_string(record)
                .map_err(|error| format!("cannot encode guest arguments as JSON: {error}"))
        })
        .collect()
}

// TODO-HUMAN-REVIEW(PR-1081): Review the manifest-to-command CLI and generated shell contract.
const USAGE: &str = "\
Usage: manifest-to-commands.rs [-h|--help] [--guest-args]

Regenerate the flattened e2e command files under ignored/e2e-commands/ from the
YAML manifests in tests/e2e/manifests/. It discovers the repo root from git and
rewrites the generated *.txt files in place.

  --guest-args  Write nothing; print the declared per-backend guest arguments as
                JSON Lines records on stdout instead.";

/// Parse every manifest under `manifests`, resolving the inherited timeout and
/// returning `(bucket, timeout_seconds, test)` tuples in file order.
fn load_manifest_tests(manifests: &Path) -> Vec<(String, i64, Value)> {
    let defaults_path = manifests.join(DEFAULTS_FILE);
    let defaults_source = fs::read_to_string(&defaults_path)
        .unwrap_or_else(|e| fail(format!("cannot read {}: {e}", defaults_path.display())));
    let defaults: Value = defaults_source
        .parse()
        .unwrap_or_else(|e| fail(format!("{}: invalid YAML: {e}", defaults_path.display())));
    let schema = defaults.get("schema").and_then(Value::as_integer);
    if schema != Some(MANIFEST_SCHEMA as i64) {
        fail(format!(
            "{}: expected schema {MANIFEST_SCHEMA}, got {schema:?}",
            defaults_path.display()
        ));
    }
    let global_timeout_seconds = timeout_seconds(
        defaults.get("timeout_seconds"),
        "global default.timeout_seconds",
    );
    let mut paths = fs::read_dir(manifests)
        .unwrap_or_else(|e| fail(format!("cannot read {}: {e}", manifests.display())))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "yaml")
                && path.file_name().is_some_and(|name| name != DEFAULTS_FILE)
        })
        .collect::<Vec<_>>();
    paths.sort();

    let mut collected = Vec::new();
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
            .unwrap_or_else(|e| fail(format!("{}: invalid YAML: {e}", path.display())));
        let schema = manifest.get("schema").and_then(Value::as_integer);
        if schema != Some(MANIFEST_SCHEMA as i64) {
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
        let bucket_timeout_seconds = manifest
            .get("timeout_seconds")
            .map(|value| timeout_seconds(Some(value), &format!("{bucket}.timeout_seconds")));
        let inherited_timeout_seconds = resolve_timeout_seconds(
            global_timeout_seconds as u64,
            bucket_timeout_seconds.map(|value| value as u64),
            None,
        ) as i64;
        let tests = manifest
            .get("test")
            .and_then(Value::as_array)
            .unwrap_or_else(|| fail(format!("{}: missing [[test]] entries", path.display())));
        for test in tests {
            collected.push((bucket.to_owned(), inherited_timeout_seconds, test.clone()));
        }
    }
    collected
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    if std::env::args().skip(1).any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let root = repo_root();
    let manifests = root.join("tests/e2e/manifests");
    if std::env::args().skip(1).any(|a| a == "--guest-args") {
        let lines = guest_args_json_lines(&load_manifest_tests(&manifests))
            .unwrap_or_else(|error| fail(error));
        for line in lines {
            println!("{line}");
        }
        return ExitCode::SUCCESS;
    }
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

    let mut by_bucket: Vec<(String, Vec<String>)> = Vec::new();
    for (bucket, timeout_seconds, test) in load_manifest_tests(&manifests) {
        let lines = commands_for_test(&test, &bucket, timeout_seconds);
        match by_bucket.iter_mut().find(|(name, _)| name == &bucket) {
            Some((_, existing)) => existing.extend(lines),
            None => by_bucket.push((bucket, lines)),
        }
    }

    let mut files = 0usize;
    let mut commands = 0usize;
    for (bucket, lines) in by_bucket {
        let destination = output.join(format!("{bucket}.txt"));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(body: &str) -> Vec<(String, i64, Value)> {
        let value: Value = body.parse().expect("test manifest must parse");
        value
            .get("test")
            .and_then(Value::as_array)
            .expect("test manifest needs [[test]]")
            .iter()
            .map(|test| ("c-programs".to_owned(), 15, test.clone()))
            .collect()
    }

    const DECLARED: &str = r#"
test:
  - id: c-programs/example
    program: tests/c/example.c
    modes:
      verify:
        backends_enabled: [ptrace, liteinst]
        backends_disabled:
          kvm: not selected for ordinary validation
        guest_args:
          ptrace: [multi, value with spaces]
          liteinst: [edge]
          kvm: [kvm-edge]
"#;

    /// POSITIVE side: a declared argument vector must reach the guest word, and
    /// must be quoted, so a scenario name containing a space stays one argument.
    #[test]
    fn declared_guest_args_are_appended_and_quoted() {
        let tests = manifest(DECLARED);
        let spec = &tests[0].2["modes"]["verify"];
        let args = mode_guest_args(spec, "verify", "ptrace", "c-programs/example");
        assert_eq!(args, vec!["multi", "value with spaces"]);
        assert_eq!(
            guest_with_args(&tests[0].2, "\"$cell/guest\"", &args),
            "\"$cell/guest\" multi 'value with spaces'"
        );
    }

    /// The channel is per-BACKEND, not per-cell: two backends of the same cell
    /// must be able to disagree. A per-cell channel would silently hand one
    /// backend the other's scenario.
    #[test]
    fn guest_args_are_resolved_per_backend() {
        let tests = manifest(DECLARED);
        let spec = &tests[0].2["modes"]["verify"];
        assert_eq!(
            mode_guest_args(spec, "verify", "liteinst", "c-programs/example"),
            vec!["edge"]
        );
        assert_ne!(
            mode_guest_args(spec, "verify", "ptrace", "c-programs/example"),
            mode_guest_args(spec, "verify", "liteinst", "c-programs/example")
        );
    }

    /// NEGATIVE side: a cell that declares nothing must gain nothing. If this
    /// flips, every argument-less guest starts receiving a stray argument.
    #[test]
    fn undeclared_guest_args_leave_the_guest_word_untouched() {
        let tests = manifest(
            r#"
test:
  - id: c-programs/bare
    program: tests/c/bare.c
    modes:
      verify:
        backends_enabled: [ptrace]
"#,
        );
        let spec = &tests[0].2["modes"]["verify"];
        let args = mode_guest_args(spec, "verify", "ptrace", "c-programs/bare");
        assert!(args.is_empty());
        assert_eq!(
            guest_with_args(&tests[0].2, "\"$cell/guest\"", &args),
            "\"$cell/guest\""
        );
    }

    #[test]
    fn string_direct_commands_are_one_line_and_preserve_exact_arguments() {
        let without_args = manifest(
            r#"
test:
  - id: c-programs/direct-string-empty
    direct: 'printf "%s\0" "$0" "$@"'
    modes:
      naked:
        backends_enabled: [native]
        runs: 3
        guest_args:
          native: []
"#,
        );
        let (_, guest) = setup_prefix(&without_args[0].2, "c-programs/direct-string-empty");
        assert_eq!(
            guest_with_args(&without_args[0].2, &guest, &[]),
            "sh -c 'printf \"%s\\0\" \"$0\" \"$@\"'"
        );
        let commands = commands_for_test(&without_args[0].2, "c-programs", 15);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].lines().count(), commands.len());
        let empty_dir =
            std::env::temp_dir().join(format!("manifest-direct-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&empty_dir);
        std::fs::create_dir_all(&empty_dir).unwrap();
        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(&commands[0])
            .current_dir(&empty_dir)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"sh\0sh\0sh\0");
        std::fs::remove_dir_all(empty_dir).unwrap();

        let with_args = manifest(
            r#"
test:
  - id: c-programs/direct-string-arguments
    direct: 'printf "%s\0" "$0" "$@"'
    modes:
      naked:
        backends_enabled: [native]
        runs: 3
        guest_args:
          native: ['', "tab\tinside", "line\ninside"]
"#,
        );
        let (_, guest) = setup_prefix(&with_args[0].2, "c-programs/direct-string-arguments");
        assert_eq!(
            guest_with_args(
                &with_args[0].2,
                &guest,
                &["".into(), "tab\tinside".into(), "line\ninside".into()]
            ),
            "sh -c 'printf \"%s\\0\" \"$0\" \"$@\"' -- '' $'tab\\tinside' $'line\\ninside'"
        );
        let commands = commands_for_test(&with_args[0].2, "c-programs", 15);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].lines().count(), commands.len());
        let args_dir =
            std::env::temp_dir().join(format!("manifest-direct-args-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&args_dir);
        std::fs::create_dir_all(&args_dir).unwrap();
        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(&commands[0])
            .current_dir(&args_dir)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            output.stdout,
            b"--\0\0tab\tinside\0line\ninside\0--\0\0tab\tinside\0line\ninside\0--\0\0tab\tinside\0line\ninside\0"
        );
        std::fs::remove_dir_all(args_dir).unwrap();
    }

    #[test]
    fn program_commands_are_one_line_and_preserve_exact_arguments() {
        use std::os::unix::fs::PermissionsExt;

        let work = std::env::temp_dir().join(format!("manifest-program-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).unwrap();
        let script = work.join("argv.sh");
        std::fs::write(
            &script,
            b"#!/usr/bin/env bash\ncase \"$1\" in --prepare) exit 0;; --run) shift; printf '%s\\0' \"$@\";; *) exit 2;; esac\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();

        let tests = manifest(&format!(
            r#"
test:
  - id: c-programs/program-arguments
    program: {}
    modes:
      naked:
        backends_enabled: [native]
        runs: 3
        guest_args:
          native: ['', "tab\tinside", "line\ninside"]
"#,
            script.display()
        ));
        let commands = commands_for_test(&tests[0].2, "c-programs", 15);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].lines().count(), commands.len());
        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(&commands[0])
            .current_dir(&work)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            output.stdout,
            b"\0tab\tinside\0line\ninside\0\0tab\tinside\0line\ninside\0\0tab\tinside\0line\ninside\0"
        );
        std::fs::remove_dir_all(work).unwrap();
    }

    #[test]
    fn naked_command_appends_native_guest_arguments() {
        let tests = manifest(
            r#"
test:
  - id: c-programs/native-args
    direct: [/bin/echo]
    modes:
      naked:
        backends_enabled: [native]
        guest_args:
          native: [native-scenario]
"#,
        );
        let commands = commands_for_test(&tests[0].2, "c-programs", 15);
        assert_eq!(commands.len(), 1);
        assert!(
            commands[0].contains("native-scenario"),
            "naked command omitted its explicit native guest argument: {}",
            commands[0]
        );
    }

    /// A backend that is enabled but not named in `guest_args` gets nothing,
    /// rather than inheriting a sibling backend's arguments.
    #[test]
    fn enabled_backend_absent_from_guest_args_gets_none() {
        let tests = manifest(
            r#"
test:
  - id: c-programs/partial
    program: tests/c/partial.c
    modes:
      verify:
        backends_enabled: [ptrace, kvm]
        guest_args:
          ptrace: [multi]
"#,
        );
        let spec = &tests[0].2["modes"]["verify"];
        assert!(mode_guest_args(spec, "verify", "kvm", "c-programs/partial").is_empty());
    }

    /// The JSON Lines dump is the out-of-tree harness's only source for these
    /// arguments, so its shape is a contract: one line per (id, mode, backend)
    /// that declares arguments, sorted, and cells declaring nothing omitted.
    #[test]
    fn guest_args_json_lines_emit_one_sorted_record_per_declaring_cell() {
        let mut tests = manifest(DECLARED);
        tests.extend(manifest(
            r#"
test:
  - id: c-programs/bare
    program: tests/c/bare.c
    modes:
      verify:
        backends_enabled: [ptrace]
"#,
        ));
        let lines = guest_args_json_lines(&tests).expect("valid guest_args must export");
        assert_eq!(
            lines,
            vec![
                r#"{"test_id":"c-programs/example","mode":"verify","backend":"kvm","args":["kvm-edge"]}"#,
                r#"{"test_id":"c-programs/example","mode":"verify","backend":"liteinst","args":["edge"]}"#,
                r#"{"test_id":"c-programs/example","mode":"verify","backend":"ptrace","args":["multi","value with spaces"]}"#,
            ]
        );
        assert!(
            !lines.iter().any(|line| line.starts_with("c-programs/bare")),
            "a cell declaring no guest_args must not appear in the dump"
        );
    }

    #[test]
    fn guest_args_json_lines_reject_a_globally_unknown_disabled_backend() {
        let tests = manifest(
            r#"
test:
  - id: c-programs/unknown-backend
    program: tests/c/unknown-backend.c
    modes:
      verify:
        backends_enabled: [ptrace]
        backends_disabled:
          kvm: not selected for ordinary validation
          ptrcae: misspelled backend must never be exported
        guest_args:
          ptrcae: [multi]
"#,
        );
        let error = guest_args_json_lines(&tests).expect_err("unknown backend must be rejected");
        assert_eq!(
            error,
            "c-programs/unknown-backend: modes.verify.guest_args.ptrcae names unknown backend for this mode; expected one of [\"ptrace\", \"dbt\", \"kvm\", \"sabre\", \"liteinst\"]"
        );
    }

    #[test]
    fn guest_args_json_lines_accept_native_only_for_naked_mode() {
        let naked = manifest(
            r#"
test:
  - id: c-programs/native-args
    program: tests/c/native-args.c
    modes:
      naked:
        backends_enabled: [native]
        guest_args:
          native: [native-scenario]
"#,
        );
        assert_eq!(
            guest_args_json_lines(&naked).expect("native is the naked-mode backend"),
            [
                r#"{"test_id":"c-programs/native-args","mode":"naked","backend":"native","args":["native-scenario"]}"#
            ]
        );

        let normal = manifest(
            r#"
test:
  - id: c-programs/native-args
    program: tests/c/native-args.c
    modes:
      verify:
        backends_enabled: [ptrace]
        backends_disabled:
          native: invalid outside naked mode
        guest_args:
          native: [native-scenario]
"#,
        );
        assert_eq!(
            guest_args_json_lines(&normal).expect_err("native must be rejected outside naked mode"),
            "c-programs/native-args: modes.verify.guest_args.native names unknown backend for this mode; expected one of [\"ptrace\", \"dbt\", \"kvm\", \"sabre\", \"liteinst\"]"
        );
    }

    #[test]
    fn guest_args_json_lines_preserve_empty_vectors_and_special_arguments() {
        let tests = manifest(
            r#"
test:
  - id: c-programs/empty-args
    program: tests/c/empty-args.c
    modes:
      verify:
        backends_enabled: [ptrace, kvm]
        guest_args:
          kvm: []
          ptrace: ['', "tab\tinside", "line\ninside"]
"#,
        );
        let lines = guest_args_json_lines(&tests).expect("Linux-valid arguments must export");
        assert_eq!(
            lines,
            [
                r#"{"test_id":"c-programs/empty-args","mode":"verify","backend":"kvm","args":[]}"#,
                r#"{"test_id":"c-programs/empty-args","mode":"verify","backend":"ptrace","args":["","tab\tinside","line\ninside"]}"#,
            ]
        );
    }

    #[test]
    fn generated_mode_commands_match_the_harness_contract() {
        let replay = hermit_command("replay", "ptrace", "portable", None, &[], false, "guest");
        assert!(replay.contains("--data-dir \"$cell/recording\" --record-timeout \"$remaining\""));
        assert!(replay.contains("--strict $record_verify_strict --verify"));
        assert!(replay.contains("--verify-json \"$cell/captures/verify.json\""));
        assert!(replay.contains(HERMIT_GUEST_ENV_ARGS));
        assert!(!replay.contains("--no-virtualize-cpuid"));

        let chaos = hermit_command("chaos", "ptrace", "portable", Some(7), &[], false, "guest");
        assert!(chaos.contains("run --base-env=minimal"));
        assert!(chaos.contains("--verify --verify-allow=both"));
        assert!(chaos.contains("--strict $run_verify_strict --verify"));
        assert!(chaos.contains("--verify-json \"$cell/captures/verify-seed-7.json\""));
        assert!(chaos.contains("--log=info"));
        assert!(!chaos.contains("--no-virtualize-cpuid"));
        assert!(!chaos.contains("--max-timeslice=disabled"));

        let custom = hermit_command(
            "custom",
            "ptrace",
            "portable",
            None,
            &["--base-env=minimal".to_owned()],
            false,
            "guest",
        );
        assert!(custom.contains("run --backend ptrace --base-env=minimal -- guest"));
        assert!(!custom.contains("--strict"));
        assert!(!custom.contains("--no-virtualize-cpuid"));

        let verify = hermit_command("verify", "ptrace", "portable", None, &[], true, "guest");
        assert!(verify.contains("run --base-env=minimal"));
        assert!(verify.contains(
            "--strict $run_verify_strict --verify --verify-json \"$cell/captures/verify.json\""
        ));
    }

    #[test]
    fn one_chaos_seed_emits_one_internally_verified_command() {
        let tests = manifest(
            r#"
test:
  - id: c-programs/one-chaos-seed
    program: tests/c/one-chaos-seed.c
    modes:
      chaos:
        backends_enabled: [ptrace]
        seeds: [7]
"#,
        );
        let commands = commands_for_test(&tests[0].2, &tests[0].0, tests[0].1);

        assert_eq!(commands.len(), 1, "one seed must emit one outer command");
        assert!(commands[0].contains("--seed=7"));
        assert_eq!(commands[0].matches("--verify-json").count(), 1);
        assert!(
            !commands[0].contains("for _run"),
            "Hermit's internal --verify repeat must not be wrapped in another repeat"
        );
    }

    #[test]
    fn chaos_seeds_share_one_outer_cell_timeout() {
        let tests = manifest(
            r#"
test:
  - id: c-programs/two-chaos-seeds
    program: tests/c/two-chaos-seeds.c
    modes:
      chaos:
        backends_enabled: [ptrace]
        seeds: [7, 9]
"#,
        );
        let commands = commands_for_test(&tests[0].2, &tests[0].0, tests[0].1);

        assert_eq!(commands.len(), 1, "one manifest cell must emit one command");
        assert_eq!(
            commands[0].matches("timeout --kill-after=10s 15s").count(),
            1
        );
        assert_eq!(
            commands[0]
                .matches("remaining=$((cell_deadline - SECONDS))")
                .count(),
            2
        );
        assert!(commands[0].contains("--seed=7"));
        assert!(commands[0].contains("--seed=9"));
    }

    #[test]
    fn generated_commands_use_the_inherited_and_exact_cell_timeouts() {
        let tests = manifest(
            r#"
test:
  - id: c-programs/timeout-policy
    program: tests/c/timeout-policy.c
    modes:
      verify:
        backends_enabled: [ptrace, liteinst]
        timeout_seconds:
          ptrace: 30
"#,
        );
        let commands = commands_for_test(&tests[0].2, &tests[0].0, tests[0].1);
        assert_eq!(commands.len(), 2);
        assert!(commands.iter().any(|line| {
            line.contains("backend liteinst") && line.contains("timeout --kill-after=10s 15s")
        }));
        assert!(commands.iter().any(|line| {
            line.contains("backend ptrace") && line.contains("timeout --kill-after=10s 30s")
        }));
    }
}
