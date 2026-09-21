#!/usr/bin/env -S rust-script --force
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! Ergonomic front-door to the schema-v3 e2e manifest corpus.
//!
//! Where `manifest-to-commands.rs` expands *every* enabled cell into bucket
//! command files, this CLI answers the three questions an operator actually
//! asks about a single test:
//!
//! ```text
//! ./tests/manifest-cli.rs list [--bucket B] [--backend BE] [--tag T] [--mode M]
//! ./tests/manifest-cli.rs get  <test-id> [--mode M] [--backend BE] [--lane L] [--log LVL]
//! ./tests/manifest-cli.rs run  <test-id> [--mode M] [--backend BE] [--lane L] [--log LVL] [-- <extra hermit flags>]
//! ```
//!
//! - `list` enumerates tests across all manifests, filterable by bucket,
//!   by a backend that is enabled in some mode, by a `requires` capability
//!   token ("tag"), and/or by mode.
//! - `get` prints the exact Hermit command(s) a test runs, ready to paste.
//! - `run` executes a single test cell directly, honoring `--log`, a backend
//!   override, a lane override, and any extra flags after `--` (injected into
//!   the hermit invocation before the `-- <guest>` separator).
//!
//! The command construction mirrors `manifest-to-commands.rs` exactly so a
//! `get`/`run` line is byte-for-byte the same contract the CI expansion uses.
//! `run` executes from the repository root and uses `target/release/hermit`
//! unless `HERMIT_BIN` is set (a release binary is required — the debug binary
//! is far too slow for the corpus timeouts).
//!
//! ```cargo
//! [dependencies]
//! serde = { version = "1", features = ["derive"] }
//! serde_yaml = "0.9"
//! ```

#[path = "../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

#[path = "../ci/manifest-plan/src/manifest_value.rs"]
mod manifest_value;

// This rust-script adapter uses only the schema-facing subset of the shared
// timeout module; the manifest-plan crate consumes its calibration API.
#[allow(dead_code)]
#[path = "../ci/manifest-plan/src/timeouts.rs"]
mod timeouts;

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
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
    eprintln!("manifest-cli: {}", message.as_ref());
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

fn required_timeout_seconds(value: Option<&Value>, context: &str) -> i64 {
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

fn first_chaos_seed(spec: &Value, id: &str) -> Result<i64, String> {
    integer_array(
        spec.get("seeds"),
        &format!("{id}.modes.chaos.seeds"),
    )
    .first()
    .copied()
    .ok_or_else(|| {
        format!(
            "{id}: chaos mode is unavailable because its manifest declares no seeds; no guest command can be printed or run"
        )
    })
}

fn test_id(test: &Value, bucket: &str) -> String {
    test.get("id")
        .and_then(Value::as_str)
        .unwrap_or_else(|| fail(format!("{bucket}: [[test]] is missing `id`")))
        .to_owned()
}

fn backend_vocabulary(mode: &str) -> &'static [&'static str] {
    if mode == "naked" {
        &NAKED_BACKENDS
    } else {
        &HERMIT_BACKENDS
    }
}

fn configured_mode_backends(
    spec: &Value,
    mode: &str,
    id: &str,
) -> Result<BTreeSet<String>, String> {
    let mut configured = BTreeSet::new();
    if let Some(enabled) = spec.get("backends_enabled") {
        let enabled = enabled
            .as_array()
            .ok_or_else(|| format!("{id}.modes.{mode}.backends_enabled must be an array"))?;
        for backend in enabled {
            configured.insert(
                backend
                    .as_str()
                    .ok_or_else(|| {
                        format!("{id}.modes.{mode}.backends_enabled entries must be strings")
                    })?
                    .to_owned(),
            );
        }
    }
    if let Some(disabled) = spec.get("backends_disabled") {
        let disabled = disabled
            .as_table()
            .ok_or_else(|| format!("{id}.modes.{mode}.backends_disabled must be a table"))?;
        configured.extend(disabled.keys().cloned());
    }
    Ok(configured)
}

fn validate_mode_guest_args(spec: &Value, mode: &str, id: &str) -> Result<(), String> {
    let configured = configured_mode_backends(spec, mode, id)?;
    let Some(value) = spec.get("guest_args") else {
        return Ok(());
    };
    let by_backend = value
        .as_table()
        .ok_or_else(|| format!("{id}.modes.{mode}.guest_args must be a table"))?;
    let vocabulary = backend_vocabulary(mode);
    for (backend, args) in by_backend {
        if !vocabulary.contains(&backend.as_str()) {
            return Err(format!(
                "{id}: modes.{mode}.guest_args.{backend} names an invalid backend for this mode; expected one of {vocabulary:?}"
            ));
        }
        if !configured.contains(backend) {
            return Err(format!(
                "{id}: modes.{mode}.guest_args.{backend} names a backend outside backends_enabled/backends_disabled"
            ));
        }
        let args = args
            .as_array()
            .ok_or_else(|| format!("{id}.modes.{mode}.guest_args.{backend} must be an array"))?;
        if args.iter().any(|argument| argument.as_str().is_none()) {
            return Err(format!(
                "{id}.modes.{mode}.guest_args.{backend} entries must be strings"
            ));
        }
        if args
            .iter()
            .filter_map(Value::as_str)
            .any(|argument| argument.contains('\0'))
        {
            return Err(format!(
                "{id}: modes.{mode}.guest_args.{backend} contains a NUL byte, which Linux argv cannot represent"
            ));
        }
    }
    Ok(())
}

fn validate_requested_backend(
    spec: &Value,
    mode: &str,
    backend: &str,
    id: &str,
) -> Result<(), String> {
    let vocabulary = backend_vocabulary(mode);
    if !vocabulary.contains(&backend) {
        return Err(format!(
            "{id}: backend `{backend}` is invalid for mode `{mode}`; expected one of {vocabulary:?}"
        ));
    }
    if !configured_mode_backends(spec, mode, id)?.contains(backend) {
        return Err(format!(
            "{id}: backend `{backend}` is outside modes.{mode}.backends_enabled/backends_disabled"
        ));
    }
    Ok(())
}

/// Build the shell setup prefix (compile/prepare the guest) and return the
/// guest invocation string. Identical contract to `manifest-to-commands.rs`.
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

/// Return only the arguments declared for this exact mode/backend pair.
/// An absent entry deliberately means no arguments; it must not inherit a
/// sibling backend's scenario.
fn mode_guest_args(spec: &Value, mode: &str, backend: &str, id: &str) -> Vec<String> {
    validate_mode_guest_args(spec, mode, id).unwrap_or_else(|error| fail(error));
    let Some(by_backend) = spec.get("guest_args") else {
        return Vec::new();
    };
    let by_backend = by_backend.as_table().unwrap();
    string_array(
        by_backend.get(backend),
        &format!("{id}.modes.{mode}.guest_args.{backend}"),
    )
}

/// Append guest arguments while preserving `sh -c`'s `$0` convention.
fn guest_with_args(test: &Value, guest: &str, guest_args: &[String]) -> String {
    if guest_args.is_empty() {
        return guest.to_owned();
    }
    let argv0 = if matches!(test.get("direct"), Some(Value::String(_))) {
        " --"
    } else {
        ""
    };
    format!(
        "{guest}{argv0} {}",
        guest_args
            .iter()
            .map(|arg| shell_quote(arg))
            .collect::<Vec<_>>()
            .join(" ")
    )
}

/// Assemble the Hermit invocation for one (mode, backend) cell. `log` overrides
/// the `--log=` level; `extra` are additional hermit flags injected before the
/// `-- <guest>` separator. Mirrors `manifest-to-commands.rs::hermit_command`
/// with the added override hooks used by `get`/`run`.
fn hermit_command(
    mode: &str,
    backend: &str,
    lane: &str,
    seed: Option<i64>,
    mode_args: &[String],
    verify_bitwise_parity: bool,
    log: &str,
    extra: &[String],
    guest: &str,
) -> String {
    let _lane = lane;
    let profile: Vec<String> = Vec::new();
    let be = shell_quote(backend);
    let run_extra_joined = {
        let mut all: Vec<String> = profile;
        all.extend(extra.iter().map(|x| shell_quote(x)));
        let joined = all.join(" ");
        if joined.is_empty() {
            String::new()
        } else {
            format!(" {joined}")
        }
    };
    let extra_joined = if extra.is_empty() {
        String::new()
    } else {
        format!(
            " {}",
            extra
                .iter()
                .map(|x| shell_quote(x))
                .collect::<Vec<_>>()
                .join(" ")
        )
    };
    let command = match mode {
        "verify" => {
            let _verify_bitwise_parity = verify_bitwise_parity;
            format!(
                "{HERMIT_RUN_ENV} \"$hermit_bin\" --log={log} run --base-env=minimal --backend {be} --strict $run_verify_strict --verify --verify-json \"$cell/captures/verify.json\"{run_extra_joined} -- {guest}"
            )
        }
        "replay" => format!(
            "{HERMIT_RUN_ENV} \"$hermit_bin\" --log {log} --backend {be} record start --strict $record_verify_strict --verify --verify-json \"$cell/captures/verify.json\" --data-dir \"$cell/recording\" --record-timeout \"$remaining\" {HERMIT_GUEST_ENV_ARGS}{extra_joined} -- {guest}"
        ),
        "chaos" => {
            let seed = seed.unwrap_or_else(|| {
                fail("internal error: chaos command construction requires a declared seed")
            });
            format!(
                "{HERMIT_RUN_ENV} \"$hermit_bin\" --log={log} run --base-env=minimal --backend {be} --strict $run_verify_strict --verify --verify-allow=both --verify-json \"$cell/captures/verify-seed-{seed}.json\" --chaos --sched-heuristic=random --seed={seed}{run_extra_joined} -- {guest}"
            )
        }
        "custom" => {
            let mut args = mode_args.to_vec();
            args.extend(extra.iter().cloned());
            let margs = args
                .iter()
                .map(|x| shell_quote(x))
                .collect::<Vec<_>>()
                .join(" ");
            let sep = if margs.is_empty() { "" } else { " " };
            format!(
                "{HERMIT_RUN_ENV} \"$hermit_bin\" --log={log} run --backend {be}{sep}{margs} -- {guest}"
            )
        }
        other => fail(format!("unsupported mode `{other}`")),
    };
    command
}

fn bounded_invocation(command: &str, id: &str) -> String {
    format!(
        "remaining=$((cell_deadline - SECONDS)); if [ \"$remaining\" -le 0 ]; then printf '%s\\n' {}; exit 124; fi; {command}",
        shell_quote(&format!(
            "{id} exceeded its per-cell timeout before attempt 1"
        ))
    )
}

fn outer_cell_command(setup: &str, run: &str, timeout: i64) -> String {
    let body = format!("cell_deadline=$((SECONDS + {timeout})); {setup} && {run}");
    format!(
        "timeout --kill-after=10s {timeout}s bash -c {}",
        shell_quote(&body)
    )
}

/// Default `--log` level per mode, matching the CI expansion.
fn default_log(_mode: &str) -> &'static str {
    "info"
}

struct Manifests {
    /// (bucket, inherited timeout, test-value) for every test, sorted.
    tests: Vec<(String, i64, Value)>,
}

fn load_manifests(root: &Path) -> Manifests {
    let dir = root.join("tests/e2e/manifests");
    let defaults_path = dir.join(DEFAULTS_FILE);
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
    let global_timeout_seconds = required_timeout_seconds(
        defaults.get("timeout_seconds"),
        "global default.timeout_seconds",
    );
    let mut paths = fs::read_dir(&dir)
        .unwrap_or_else(|e| fail(format!("cannot read {}: {e}", dir.display())))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "yaml")
                && path.file_name().is_some_and(|name| name != DEFAULTS_FILE)
        })
        .collect::<Vec<_>>();
    paths.sort();

    let mut tests = Vec::new();
    for path in paths {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_else(|| fail(format!("non-UTF-8 manifest name: {}", path.display())));
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
            .unwrap_or_else(|| fail(format!("{}: missing `bucket`", path.display())))
            .to_owned();
        if bucket != stem {
            fail(format!(
                "{}: bucket `{bucket}` must match file stem `{stem}`",
                path.display()
            ));
        }
        let bucket_timeout_seconds = manifest.get("timeout_seconds").map(|value| {
            required_timeout_seconds(Some(value), &format!("{bucket}.timeout_seconds"))
        });
        let inherited_timeout_seconds = resolve_timeout_seconds(
            global_timeout_seconds as u64,
            bucket_timeout_seconds.map(|value| value as u64),
            None,
        ) as i64;
        let entries = manifest
            .get("test")
            .and_then(Value::as_array)
            .unwrap_or_else(|| fail(format!("{}: missing [[test]] entries", path.display())));
        for test in entries {
            let test = test.clone();
            let id = test_id(&test, &bucket);
            for (mode, spec) in modes_table(&test, &id) {
                validate_mode_guest_args(spec, mode, &id).unwrap_or_else(|error| fail(error));
            }
            tests.push((bucket.clone(), inherited_timeout_seconds, test));
        }
    }
    Manifests { tests }
}

fn modes_table<'a>(test: &'a Value, id: &str) -> &'a std::collections::BTreeMap<String, Value> {
    test.get("modes")
        .and_then(Value::as_table)
        .unwrap_or_else(|| fail(format!("{id}: missing `modes`")))
}

/// Backends enabled for a given mode (native for `naked`).
fn mode_backends(spec: &Value, mode: &str, id: &str) -> Vec<String> {
    if mode == "naked" {
        return vec!["native".to_owned()];
    }
    string_array(
        spec.get("backends_enabled"),
        &format!("{id}.modes.{mode}.backends_enabled"),
    )
}

/// Union of backends enabled across all of a test's modes.
fn test_backends(test: &Value, id: &str) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    for (mode, spec) in modes_table(test, id) {
        for be in mode_backends(spec, mode, id) {
            set.insert(be);
        }
    }
    set
}

fn requires(test: &Value, id: &str) -> Vec<String> {
    string_array(test.get("requires"), &format!("{id}.requires"))
}

fn test_lane(test: &Value) -> &str {
    test.get("lane")
        .and_then(Value::as_str)
        .unwrap_or("portable")
}

fn cell_timeout_seconds(test: &Value, id: &str, mode: &str, backend: &str, inherited: i64) -> i64 {
    let Some(value) = modes_table(test, id)[mode].get("timeout_seconds") else {
        return inherited;
    };
    let table = value.as_table().unwrap_or_else(|| {
        fail(format!(
            "{id}.modes.{mode}.timeout_seconds must be a backend table"
        ))
    });
    table.get(backend).map_or(inherited, |value| {
        required_timeout_seconds(
            Some(value),
            &format!("{id}.modes.{mode}.timeout_seconds.{backend}"),
        )
    })
}

/// Pick a default mode: prefer `verify` if it has enabled backends, else the
/// first (sorted) non-naked mode with enabled backends, else `naked`.
fn default_mode(test: &Value, id: &str) -> String {
    let modes = modes_table(test, id);
    if let Some(spec) = modes.get("verify") {
        if !mode_backends(spec, "verify", id).is_empty() {
            return "verify".to_owned();
        }
    }
    let mut names = modes.keys().cloned().collect::<Vec<_>>();
    names.sort();
    for name in &names {
        if name == "naked" {
            continue;
        }
        if !mode_backends(&modes[name], name, id).is_empty() {
            return name.clone();
        }
    }
    if modes.contains_key("naked") {
        return "naked".to_owned();
    }
    fail(format!("{id}: no mode has an enabled backend"))
}

fn find_test<'a>(manifests: &'a Manifests, id: &str) -> (&'a Value, i64) {
    manifests
        .tests
        .iter()
        .find(|(bucket, _, test)| test_id(test, bucket) == id)
        .map(|(_, timeout_seconds, test)| (test, *timeout_seconds))
        .unwrap_or_else(|| {
            fail(format!(
                "no test with id `{id}` (try `manifest-cli list` to see ids)"
            ))
        })
}

// ---- argument parsing --------------------------------------------------------

/// Split argv into flags (--k v / --k=v), positionals, and a passthrough tail
/// after a literal `--`.
struct Args {
    positional: Vec<String>,
    flags: Vec<(String, Option<String>)>,
    passthrough: Vec<String>,
}

fn parse_args(argv: &[String]) -> Args {
    let mut positional = Vec::new();
    let mut flags = Vec::new();
    let mut passthrough = Vec::new();
    let mut iter = argv.iter().peekable();
    let mut after_dashdash = false;
    while let Some(arg) = iter.next() {
        if after_dashdash {
            passthrough.push(arg.clone());
            continue;
        }
        if arg == "--" {
            after_dashdash = true;
            continue;
        }
        if let Some(rest) = arg.strip_prefix("--") {
            if let Some((k, v)) = rest.split_once('=') {
                flags.push((k.to_owned(), Some(v.to_owned())));
            } else {
                // consume a following value unless the next token is a flag
                let takes_value = iter
                    .peek()
                    .map(|n| !n.starts_with("--") && *n != "--")
                    .unwrap_or(false);
                if takes_value {
                    flags.push((rest.to_owned(), Some(iter.next().unwrap().clone())));
                } else {
                    flags.push((rest.to_owned(), None));
                }
            }
        } else {
            positional.push(arg.clone());
        }
    }
    Args {
        positional,
        flags,
        passthrough,
    }
}

impl Args {
    fn flag(&self, name: &str) -> Option<&str> {
        self.flags
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .and_then(|(_, v)| v.as_deref())
    }
    fn has(&self, name: &str) -> bool {
        self.flags.iter().any(|(k, _)| k == name)
    }
}

// ---- subcommands -------------------------------------------------------------

fn cmd_list(manifests: &Manifests, args: &Args) -> ExitCode {
    let bucket_f = args.flag("bucket");
    let backend_f = args.flag("backend");
    let tag_f = args.flag("tag");
    let mode_f = args.flag("mode");
    let lane_f = args.flag("lane");
    let verbose = args.has("verbose");

    let mut shown = 0usize;
    for (bucket, _, test) in &manifests.tests {
        let id = test_id(test, bucket);
        if let Some(b) = bucket_f {
            if bucket != b {
                continue;
            }
        }
        if let Some(l) = lane_f {
            if test_lane(test) != l {
                continue;
            }
        }
        let backends = test_backends(test, &id);
        if let Some(be) = backend_f {
            // If a mode filter is present, restrict to that mode's backends.
            let ok = match mode_f {
                Some(m) => modes_table(test, &id)
                    .get(m)
                    .map(|spec| mode_backends(spec, m, &id).iter().any(|x| x == be))
                    .unwrap_or(false),
                None => backends.contains(be),
            };
            if !ok {
                continue;
            }
        } else if let Some(m) = mode_f {
            // mode filter without backend filter: test must define that mode
            // with at least one enabled backend
            let has = modes_table(test, &id)
                .get(m)
                .map(|spec| !mode_backends(spec, m, &id).is_empty())
                .unwrap_or(false);
            if !has {
                continue;
            }
        }
        let reqs = requires(test, &id);
        if let Some(t) = tag_f {
            if !reqs.iter().any(|r| r == t) {
                continue;
            }
        }
        shown += 1;
        let backends_str = backends.iter().cloned().collect::<Vec<_>>().join(",");
        println!(
            "{:<48} lane={:<10} backends=[{}]",
            id,
            test_lane(test),
            backends_str
        );
        if verbose {
            println!("    requires=[{}]", reqs.join(","));
            let modes = modes_table(test, &id);
            let mut names = modes.keys().cloned().collect::<Vec<_>>();
            names.sort();
            for name in names {
                let bes = mode_backends(&modes[name.as_str()], &name, &id);
                if !bes.is_empty() {
                    println!("    mode {:<8} -> [{}]", name, bes.join(","));
                }
            }
        }
    }
    eprintln!("manifest-cli: {shown} test(s) listed");
    ExitCode::SUCCESS
}

/// Resolve mode + backend + lane for a get/run, applying overrides.
fn resolve_cell(
    test: &Value,
    id: &str,
    inherited_timeout_seconds: i64,
    args: &Args,
) -> (String, String, String, i64) {
    let mode = args
        .flag("mode")
        .map(str::to_owned)
        .unwrap_or_else(|| default_mode(test, id));
    let modes = modes_table(test, id);
    let spec = modes.get(&mode).unwrap_or_else(|| {
        fail(format!(
            "{id}: no mode `{mode}` (have: {:?})",
            modes.keys().collect::<Vec<_>>()
        ))
    });
    let enabled = mode_backends(spec, &mode, id);
    let backend = match args.flag("backend") {
        Some(b) => b.to_owned(),
        None => enabled.first().cloned().unwrap_or_else(|| {
            fail(format!(
                "{id}: mode `{mode}` has no enabled backend; pass --backend"
            ))
        }),
    };
    validate_requested_backend(spec, &mode, &backend, id).unwrap_or_else(|error| fail(error));
    let lane = args
        .flag("lane")
        .map(str::to_owned)
        .unwrap_or_else(|| test_lane(test).to_owned());
    let timeout = cell_timeout_seconds(test, id, &mode, &backend, inherited_timeout_seconds);
    (mode, backend, lane, timeout)
}

fn build_full_command(
    test: &Value,
    id: &str,
    inherited_timeout_seconds: i64,
    args: &Args,
) -> (String, String, String) {
    let (mode, backend, lane, timeout) = resolve_cell(test, id, inherited_timeout_seconds, args);
    let (setup, guest) = setup_prefix(test, id);
    let guest_args = mode_guest_args(&modes_table(test, id)[&mode], &mode, &backend, id);
    let guest = guest_with_args(test, &guest, &guest_args);
    let log = args
        .flag("log")
        .map(str::to_owned)
        .unwrap_or_else(|| default_log(&mode).to_owned());
    let mode_args = if mode == "custom" {
        let modes = modes_table(test, id);
        string_array(modes[&mode].get("args"), &format!("{id}.modes.custom.args"))
    } else {
        Vec::new()
    };
    let verify_bitwise_parity = if mode == "verify" {
        modes_table(test, id)[&mode]
            .get("assert")
            .and_then(Value::as_table)
            .and_then(|assert| assert.get("bitwise_parity"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    } else {
        false
    };
    let seed = if mode == "chaos" {
        let modes = modes_table(test, id);
        Some(first_chaos_seed(&modes[&mode], id).unwrap_or_else(|error| fail(error)))
    } else {
        None
    };
    let run = if mode == "naked" {
        format!("{RUN_ENV} {guest}")
    } else {
        hermit_command(
            &mode,
            &backend,
            &lane,
            seed,
            &mode_args,
            verify_bitwise_parity,
            &log,
            &args.passthrough,
            &guest,
        )
    };
    let full = outer_cell_command(&setup, &bounded_invocation(&run, id), timeout);
    (full, mode, backend)
}

fn full_guest_argument_command_bracket() {
    let direct: serde_yaml::Value = serde_yaml::from_str(
        r#"
direct: 'printf "%s\0" "$0" "$@"'
lane: portable
modes:
  naked:
    ci: false
    backends_enabled: [native]
    guest_args:
      native: []
"#,
    )
    .unwrap();
    let work = std::env::temp_dir().join(format!(
        "hermit-manifest-cli-guest-args-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&work).unwrap();
    let options = parse_args(&["--mode".into(), "naked".into()]);
    for arguments in [
        Vec::<String>::new(),
        vec![String::new()],
        vec!["first".into(), "second".into()],
        vec![
            String::new(),
            "tab\tinside".into(),
            "line\ninside\n".into(),
            "carriage\rreturn".into(),
            "é日本".into(),
            "'\\$()`${UNEXPANDED}`".into(),
        ],
    ] {
        let mut value = direct.clone();
        value["modes"]["naked"]["guest_args"]["native"] = serde_yaml::to_value(&arguments).unwrap();
        let fixture: Value = serde_yaml::to_string(&value).unwrap().parse().unwrap();
        let (command, mode, backend) = build_full_command(&fixture, "fixture/argv", 15, &options);
        assert_eq!((mode.as_str(), backend.as_str()), ("naked", "native"));
        assert!(command.starts_with("timeout --kill-after=10s 15s bash -c "));
        assert!(command.is_ascii());
        assert!(!command.contains('\n'));
        // Exercise the same complete command and outer shell as cmd_run. The
        // explicit inner Bash owns ANSI-C quoting. Only native printf runs;
        // the help-query placeholder cannot invoke a Hermit binary.
        let output = Command::new("sh")
            .arg("-c")
            .arg(&command)
            .env("HERMIT_BIN", "/bin/false")
            .current_dir(&work)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut expected = if arguments.is_empty() {
            b"sh\0".to_vec()
        } else {
            b"--\0".to_vec()
        };
        for argument in arguments {
            expected.extend_from_slice(argument.as_bytes());
            expected.push(0);
        }
        assert_eq!(output.stdout, expected);
    }
    fs::remove_dir_all(work).unwrap();
}

fn self_test() -> ExitCode {
    let timeout_fixture: Value = r#"
modes:
  verify:
    timeout_seconds:
      ptrace: 30
"#
    .parse()
    .unwrap();
    assert_eq!(
        cell_timeout_seconds(&timeout_fixture, "fixture", "verify", "ptrace", 15),
        30
    );
    assert_eq!(
        cell_timeout_seconds(&timeout_fixture, "fixture", "verify", "liteinst", 15),
        15
    );

    let replay = hermit_command(
        "replay",
        "ptrace",
        "portable",
        None,
        &[],
        false,
        "info",
        &[],
        "guest",
    );
    assert!(replay.contains("--data-dir \"$cell/recording\" --record-timeout \"$remaining\""));
    assert!(replay.contains("--strict $record_verify_strict --verify"));
    assert!(replay.contains("--verify-json \"$cell/captures/verify.json\""));
    assert!(replay.contains(HERMIT_GUEST_ENV_ARGS));
    assert!(!replay.contains("--no-virtualize-cpuid"));

    let chaos = hermit_command(
        "chaos",
        "ptrace",
        "portable",
        Some(7),
        &[],
        false,
        "info",
        &[],
        "guest",
    );
    assert!(chaos.contains("run --base-env=minimal"));
    assert!(chaos.contains("--verify --verify-allow=both"));
    assert!(chaos.contains("--strict $run_verify_strict --verify"));
    assert!(chaos.contains("--verify-json \"$cell/captures/verify-seed-7.json\""));
    assert!(chaos.contains("--log=info"));
    assert!(!chaos.contains("--no-virtualize-cpuid"));
    assert!(!chaos.contains("--max-timeslice=disabled"));
    let seeded_chaos: Value = "seeds: [7, 9]".parse().unwrap();
    let no_seed_chaos = Value::Table(Default::default());
    assert_eq!(first_chaos_seed(&seeded_chaos, "fixture").unwrap(), 7);
    assert!(
        first_chaos_seed(&no_seed_chaos, "fixture")
            .unwrap_err()
            .contains("declares no seeds")
    );

    let custom = hermit_command(
        "custom",
        "ptrace",
        "portable",
        None,
        &["--base-env=minimal".to_owned()],
        false,
        "info",
        &[],
        "guest",
    );
    assert!(custom.contains("run --backend ptrace --base-env=minimal -- guest"));
    assert!(!custom.contains("--strict"));
    assert!(!custom.contains("--no-virtualize-cpuid"));

    let verify = hermit_command(
        "verify",
        "ptrace",
        "portable",
        None,
        &[],
        true,
        "info",
        &[],
        "guest",
    );
    assert!(verify.contains("run --base-env=minimal"));
    assert!(verify.contains(
        "--strict $run_verify_strict --verify --verify-json \"$cell/captures/verify.json\""
    ));
    assert!(!verify.contains("--no-virtualize-cpuid"));
    assert!(!verify.contains("--max-timeslice=disabled"));
    let weak_verify = hermit_command(
        "verify",
        "ptrace",
        "portable",
        None,
        &[],
        false,
        "info",
        &[],
        "guest",
    );
    assert!(weak_verify.contains("--verify --verify-json \"$cell/captures/verify.json\""));
    assert!(weak_verify.contains("--strict $run_verify_strict --verify"));

    let per_backend_guest_args: Value = r#"
direct: [/bin/echo]
lane: portable
modes:
  verify:
    backends_enabled: [ptrace]
    backends_disabled:
      kvm: configured but not selected for ordinary validation
    guest_args:
      ptrace: [ptrace-scenario]
      kvm: [kvm-scenario, value with spaces]
"#
    .parse()
    .unwrap();
    let kvm_args = parse_args(&[
        "--mode".to_owned(),
        "verify".to_owned(),
        "--backend".to_owned(),
        "kvm".to_owned(),
    ]);
    let (kvm_command, mode, backend) =
        build_full_command(&per_backend_guest_args, "fixture", 15, &kvm_args);
    assert_eq!((mode.as_str(), backend.as_str()), ("verify", "kvm"));
    assert!(kvm_command.contains("kvm-scenario"));
    assert!(kvm_command.contains("value with spaces"));
    assert!(!kvm_command.contains("ptrace-scenario"));
    assert_eq!(
        guest_with_args(
            &per_backend_guest_args,
            "/bin/echo",
            &["kvm-scenario".into(), "value with spaces".into()]
        ),
        "/bin/echo kvm-scenario 'value with spaces'"
    );

    let absent_kvm_guest_args: Value = r#"
direct: [/bin/echo]
lane: portable
modes:
  verify:
    backends_enabled: [ptrace]
    backends_disabled:
      kvm: configured but not selected for ordinary validation
    guest_args:
      ptrace: [ptrace-scenario]
"#
    .parse()
    .unwrap();
    let (kvm_without_args, _, _) =
        build_full_command(&absent_kvm_guest_args, "fixture", 15, &kvm_args);
    assert!(kvm_without_args.contains("-- /bin/echo"));
    assert!(!kvm_without_args.contains("ptrace-scenario"));

    assert!(
        validate_requested_backend(
            &per_backend_guest_args["modes"]["verify"],
            "verify",
            "kvm",
            "fixture"
        )
        .is_ok()
    );
    let outside_partition: Value = r#"
backends_enabled: [ptrace]
backends_disabled: {}
"#
    .parse()
    .unwrap();
    assert_eq!(
        validate_requested_backend(&outside_partition, "verify", "kvm", "fixture").unwrap_err(),
        "fixture: backend `kvm` is outside modes.verify.backends_enabled/backends_disabled"
    );
    assert_eq!(
        validate_requested_backend(&outside_partition, "verify", "native", "fixture").unwrap_err(),
        "fixture: backend `native` is invalid for mode `verify`; expected one of [\"ptrace\", \"dbt\", \"kvm\", \"sabre\", \"liteinst\"]"
    );
    let naked_partition: Value = r#"
backends_enabled: [native]
backends_disabled: {}
"#
    .parse()
    .unwrap();
    assert_eq!(
        validate_requested_backend(&naked_partition, "naked", "ptrace", "fixture").unwrap_err(),
        "fixture: backend `ptrace` is invalid for mode `naked`; expected one of [\"native\"]"
    );

    let empty_guest_args: Value = r#"
backends_enabled: [ptrace]
backends_disabled:
  kvm: configured but not selected for ordinary validation
guest_args:
  kvm: []
"#
    .parse()
    .unwrap();
    assert!(validate_mode_guest_args(&empty_guest_args, "verify", "fixture").is_ok());
    let nul_guest_args: Value = r#"
backends_enabled: [ptrace]
guest_args:
  ptrace: ["\0"]
"#
    .parse()
    .unwrap();
    assert_eq!(
        validate_mode_guest_args(&nul_guest_args, "verify", "fixture").unwrap_err(),
        "fixture: modes.verify.guest_args.ptrace contains a NUL byte, which Linux argv cannot represent"
    );
    let outside_guest_args: Value = r#"
backends_enabled: [ptrace]
backends_disabled: {}
guest_args:
  kvm: [kvm-scenario]
"#
    .parse()
    .unwrap();
    assert_eq!(
        validate_mode_guest_args(&outside_guest_args, "verify", "fixture").unwrap_err(),
        "fixture: modes.verify.guest_args.kvm names a backend outside backends_enabled/backends_disabled"
    );
    let unknown_guest_args: Value = r#"
backends_enabled: [ptrace]
backends_disabled:
  ptrcae: misspelled backend
guest_args:
  ptrcae: [scenario]
"#
    .parse()
    .unwrap();
    assert_eq!(
        validate_mode_guest_args(&unknown_guest_args, "verify", "fixture").unwrap_err(),
        "fixture: modes.verify.guest_args.ptrcae names an invalid backend for this mode; expected one of [\"ptrace\", \"dbt\", \"kvm\", \"sabre\", \"liteinst\"]"
    );
    let wrong_naked_guest_args: Value = r#"
backends_enabled: [native]
backends_disabled:
  ptrace: invalid in naked mode
guest_args:
  ptrace: [scenario]
"#
    .parse()
    .unwrap();
    assert_eq!(
        validate_mode_guest_args(&wrong_naked_guest_args, "naked", "fixture").unwrap_err(),
        "fixture: modes.naked.guest_args.ptrace names an invalid backend for this mode; expected one of [\"native\"]"
    );

    let direct_string: Value = r#"
direct: 'printf "%s\0" "$0" "$@"'
"#
    .parse()
    .unwrap();
    let (_, direct_guest) = setup_prefix(&direct_string, "fixture");
    let no_args = guest_with_args(&direct_string, &direct_guest, &[]);
    assert_eq!(no_args, "sh -c 'printf \"%s\\0\" \"$0\" \"$@\"'");
    assert_eq!(no_args.lines().count(), 1);
    // A guest fragment runs inside the production wrapper's explicit Bash.
    let output = Command::new("bash")
        .arg("-c")
        .arg(&no_args)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"sh\0");

    let direct_guest = guest_with_args(
        &direct_string,
        &direct_guest,
        &["".into(), "tab\tinside".into(), "line\ninside".into()],
    );
    assert_eq!(
        direct_guest,
        "sh -c 'printf \"%s\\0\" \"$0\" \"$@\"' -- '' $'tab\\tinside' $'line\\ninside'"
    );
    assert_eq!(direct_guest.lines().count(), 1);
    let output = Command::new("bash")
        .arg("-c")
        .arg(&direct_guest)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"--\0\0tab\tinside\0line\ninside\0");
    full_guest_argument_command_bracket();
    println!("manifest-cli self-test: PASS");
    ExitCode::SUCCESS
}

fn cmd_get(manifests: &Manifests, args: &Args) -> ExitCode {
    let id = args
        .positional
        .first()
        .unwrap_or_else(|| fail("get: missing <test-id>"));
    let (test, timeout_seconds) = find_test(manifests, id);
    if args.has("all-modes") {
        let modes = modes_table(test, id);
        let mut names = modes.keys().cloned().collect::<Vec<_>>();
        names.sort();
        for name in names {
            for be in mode_backends(&modes[name.as_str()], &name, id) {
                let mut sub = parse_args(&[]);
                sub.flags.push(("mode".to_owned(), Some(name.clone())));
                sub.flags.push(("backend".to_owned(), Some(be.clone())));
                if let Some(l) = args.flag("log") {
                    sub.flags.push(("log".to_owned(), Some(l.to_owned())));
                }
                if let Some(l) = args.flag("lane") {
                    sub.flags.push(("lane".to_owned(), Some(l.to_owned())));
                }
                sub.positional.push(id.clone());
                let (full, mode, backend) = build_full_command(test, id, timeout_seconds, &sub);
                println!("# {id} mode={mode} backend={backend}");
                println!("{full}\n");
            }
        }
        return ExitCode::SUCCESS;
    }
    let (full, mode, backend) = build_full_command(test, id, timeout_seconds, args);
    println!("# {id} mode={mode} backend={backend}");
    println!("{full}");
    ExitCode::SUCCESS
}

fn cmd_run(manifests: &Manifests, args: &Args, root: &Path) -> ExitCode {
    let id = args
        .positional
        .first()
        .unwrap_or_else(|| fail("run: missing <test-id>"));
    let (test, timeout_seconds) = find_test(manifests, id);
    let (full, mode, backend) = build_full_command(test, id, timeout_seconds, args);
    eprintln!("manifest-cli: running {id} mode={mode} backend={backend}");
    eprintln!("manifest-cli: $ {full}");
    let status = Command::new("sh")
        .arg("-c")
        .arg(&full)
        .current_dir(root)
        .status()
        .unwrap_or_else(|e| fail(format!("failed to spawn shell: {e}")));
    match status.code() {
        Some(0) => {
            eprintln!("manifest-cli: {id} exited 0");
            ExitCode::SUCCESS
        }
        Some(code) => {
            eprintln!("manifest-cli: {id} exited {code}");
            ExitCode::from(code as u8)
        }
        None => {
            eprintln!("manifest-cli: {id} terminated by signal");
            ExitCode::from(1)
        }
    }
}

fn usage() -> ! {
    eprintln!(
        "manifest-cli — front-door to the e2e manifest corpus

USAGE:
  manifest-cli list [--bucket B] [--backend BE] [--tag T] [--mode M] [--lane L] [--verbose]
  manifest-cli get  <test-id> [--mode M] [--backend BE] [--lane L] [--log LVL] [--all-modes]
  manifest-cli run  <test-id> [--mode M] [--backend BE] [--lane L] [--log LVL] [-- <extra hermit flags>]

FILTERS (list):
  --bucket   manifest bucket (e.g. system-utils, c-programs)
  --backend  a backend enabled in some mode (ptrace, dbt, kvm, sabre, liteinst, native)
  --tag      a `requires` capability token (e.g. python3, bash, kvm, cpuid)
  --mode     verify | naked | replay | chaos | custom
  --lane     portable | privileged
  --verbose  also print requires + per-mode backend breakdown

get/run:
  --mode/--backend/--lane pick the cell (defaults: verify mode, first enabled backend, test lane)
  --log      override the --log= level (info|debug|trace|off); default info for every mode
  --all-modes (get only) print every enabled (mode,backend) command
  -- <flags> (run only) extra hermit flags injected before the `-- <guest>` separator
  A chaos mode without declared seeds is unavailable; get/run refuse rather than invent seed 0.

ENV:
  HERMIT_BIN  hermit binary for `run` (default target/release/hermit; a RELEASE binary is required)"
    );
    std::process::exit(2)
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() {
        usage();
    }
    let sub = argv[0].clone();
    if sub == "-h" || sub == "--help" || sub == "help" {
        usage();
    }
    if sub == "self-test" {
        return self_test();
    }
    let rest = &argv[1..];
    let args = parse_args(rest);
    let root = repo_root();
    let manifests = load_manifests(&root);
    match sub.as_str() {
        "list" => cmd_list(&manifests, &args),
        "get" => cmd_get(&manifests, &args),
        "run" => cmd_run(&manifests, &args, &root),
        other => {
            eprintln!("manifest-cli: unknown subcommand `{other}`\n");
            usage();
        }
    }
}
