#!/usr/bin/env -S rust-script --force
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Enforce the repository's GitHub Actions trigger policy.
//!
//! Local exact-head validation is the landing authority. The portable workflow
//! is supplemental evidence and may run automatically only after a commit is
//! pushed to `integration`. Two owner-approved maintenance workflows also run
//! at their exact reviewed schedules; every workflow may remain manually
//! dispatchable. This checker deliberately accepts only the small YAML shape
//! used here. An unfamiliar or ambiguous trigger block is an error, never a
//! silent pass.

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

const WORKFLOW_DIR: &str = ".github/workflows";
const PORTABLE: &str = "ci-portable.yml";
const VALIDATION_LEVELS: &str = "validation-levels.yml";
const BUCK2_OSS_NIGHTLY: &str = "buck2-oss-nightly.yml";
const RUST_SCRIPT_VERSION: &str = "0.36.0";

#[derive(Debug, PartialEq, Eq)]
struct Triggers {
    events: BTreeSet<String>,
    push_branches: Option<Vec<String>>,
    schedule_crons: Option<Vec<String>>,
}

fn indentation(line: &str) -> Result<usize, String> {
    let prefix = line.len() - line.trim_start_matches([' ', '\t']).len();
    if line[..prefix].contains('\t') {
        return Err("tabs are not accepted in YAML indentation".to_string());
    }
    Ok(prefix)
}

fn code(line: &str) -> &str {
    line.split_once('#')
        .map_or(line, |(before, _)| before)
        .trim_end()
}

fn key(line: &str) -> Option<&str> {
    let trimmed = code(line).trim();
    let (name, value) = trimmed.split_once(':')?;
    if !value.trim().is_empty()
        || name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    Some(name)
}

fn inline_list(value: &str) -> Option<Vec<String>> {
    let value = value.trim();
    let inner = value.strip_prefix('[')?.strip_suffix(']')?;
    let entries = inner
        .split(',')
        .map(|entry| entry.trim().trim_matches(['\'', '"']).to_string())
        .collect::<Vec<_>>();
    if entries.is_empty() || entries.iter().any(String::is_empty) {
        return None;
    }
    Some(entries)
}

fn quoted_scalar(value: &str) -> Option<String> {
    let value = value.trim();
    let inner = value.strip_prefix('"')?.strip_suffix('"')?;
    if inner.is_empty() || inner.contains('"') {
        return None;
    }
    Some(inner.to_string())
}

fn parse_triggers(source: &str) -> Result<Triggers, String> {
    let lines = source.lines().collect::<Vec<_>>();
    let mut on_entries = Vec::new();
    for (index, raw) in lines.iter().enumerate() {
        let line = code(raw);
        if line.trim().is_empty() || indentation(line)? != 0 {
            continue;
        }
        let trimmed = line.trim();
        if matches!(trimmed, "---" | "...") {
            continue;
        }
        let (spelling, value) = trimmed
            .split_once(':')
            .ok_or_else(|| format!("unsupported top-level YAML structure: `{trimmed}`"))?;
        let spelling = spelling.trim();
        if spelling.is_empty()
            || !spelling
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(format!(
                "top-level YAML keys must use an unquoted simple spelling: `{spelling}`"
            ));
        }
        if spelling == "on" {
            on_entries.push((index, value.trim()));
        }
    }
    if on_entries.len() != 1 {
        return Err(format!(
            "expected exactly one top-level `on` key, found {}",
            on_entries.len()
        ));
    }
    let (on_index, value) = on_entries[0];
    if !value.is_empty() {
        return Err("the top-level `on` value must be a mapping".to_string());
    }

    let mut events = BTreeSet::new();
    let mut event_indent = None;
    let mut push_indent = None;
    let mut push_child_indent = None;
    let mut push_branches = None;
    let mut schedule_indent = None;
    let mut schedule_child_indent = None;
    let mut schedule_crons = None;
    for raw in &lines[on_index + 1..] {
        let line = code(raw);
        if line.trim().is_empty() {
            continue;
        }
        let indent = indentation(line)?;
        if indent == 0 {
            break;
        }
        let direct_indent = *event_indent.get_or_insert(indent);
        if indent < direct_indent {
            return Err("inconsistent indentation in top-level `on:` mapping".to_string());
        }
        if indent == direct_indent {
            let event = key(line).ok_or_else(|| {
                format!(
                    "event entries must use a mapping key on its own line; found `{}`",
                    line.trim()
                )
            })?;
            if !events.insert(event.to_string()) {
                return Err(format!("duplicate `{event}` trigger"));
            }
            push_indent = (event == "push").then_some(indent);
            push_child_indent = None;
            schedule_indent = (event == "schedule").then_some(indent);
            schedule_child_indent = None;
            if event == "schedule" {
                schedule_crons = Some(Vec::new());
            }
            continue;
        }

        if let Some(indent_of_push) = push_indent {
            if indent > indent_of_push {
                let direct_push_indent = *push_child_indent.get_or_insert(indent);
                if indent != direct_push_indent {
                    return Err(format!(
                        "ambiguous nested structure in `push`: `{}`",
                        line.trim()
                    ));
                }
                let trimmed = line.trim();
                if let Some(value) = trimmed.strip_prefix("branches:") {
                    if push_branches.is_some() {
                        return Err("duplicate `push.branches` entry".to_string());
                    }
                    push_branches = Some(inline_list(value).ok_or_else(|| {
                        "`push.branches` must be an explicit inline list".to_string()
                    })?);
                } else {
                    return Err(format!(
                        "unsupported direct `push` key; only `branches` is permitted: `{trimmed}`"
                    ));
                }
            }
        } else if let Some(indent_of_schedule) = schedule_indent {
            if indent > indent_of_schedule {
                let direct_schedule_indent = *schedule_child_indent.get_or_insert(indent);
                if indent != direct_schedule_indent {
                    return Err(format!(
                        "ambiguous nested structure in `schedule`: `{}`",
                        line.trim()
                    ));
                }
                let trimmed = line.trim();
                let value = trimmed.strip_prefix("- cron:").ok_or_else(|| {
                    format!(
                        "unsupported direct `schedule` entry; only `- cron: \"...\"` is permitted: `{trimmed}`"
                    )
                })?;
                let cron = quoted_scalar(value).ok_or_else(|| {
                    "`schedule` cron must be an explicit nonempty double-quoted scalar".to_string()
                })?;
                schedule_crons
                    .as_mut()
                    .expect("schedule trigger initializes cron storage")
                    .push(cron);
            }
        }
    }

    if events.is_empty() {
        return Err("the top-level `on:` mapping has no events".to_string());
    }
    Ok(Triggers {
        events,
        push_branches,
        schedule_crons,
    })
}

fn approved_schedule(name: &str) -> Option<&'static [&'static str]> {
    match name {
        "buck2-oss-nightly.yml" => Some(&["17 10 * * *"]),
        // Daily at 08:23 UTC, as before; "0-6" instead of "*" so the commit
        // that wrote it re-attributed the schedule to its pusher (see the
        // comment on the cron line in docs.yml).
        "docs.yml" => Some(&["23 8 * * 0-6"]),
        _ => None,
    }
}

fn validate(name: &str, triggers: &Triggers) -> Vec<String> {
    let recognized = BTreeSet::from([
        "push".to_string(),
        "schedule".to_string(),
        "workflow_dispatch".to_string(),
    ]);
    let mut errors = Vec::new();
    for event in triggers.events.difference(&recognized) {
        errors.push(format!("automatic `{event}` trigger is not permitted"));
    }
    if !triggers.events.contains("workflow_dispatch") {
        errors.push("missing `workflow_dispatch` trigger".to_string());
    }
    if triggers.events.contains("push") {
        if triggers.push_branches.as_deref() != Some(&["integration".to_string()]) {
            errors.push("`push.branches` must contain exactly `integration`".to_string());
        }
    } else if triggers.push_branches.is_some() {
        errors.push("found `push.branches` without a `push` trigger".to_string());
    }

    if let Some(expected_crons) = approved_schedule(name) {
        let required_events =
            BTreeSet::from(["schedule".to_string(), "workflow_dispatch".to_string()]);
        if triggers.events != required_events {
            errors.push(format!(
                "{name} must have exactly `workflow_dispatch` and `schedule` triggers"
            ));
        }
        let actual_crons = triggers.schedule_crons.as_deref().unwrap_or_default();
        if !actual_crons
            .iter()
            .map(String::as_str)
            .eq(expected_crons.iter().copied())
        {
            errors.push(format!(
                "{name} must use exactly the approved cron schedule: {}",
                expected_crons.join(", ")
            ));
        }
    } else if triggers.events.contains("schedule") {
        errors.push("automatic `schedule` trigger is not permitted".to_string());
    }

    if name == PORTABLE {
        let allowed = BTreeSet::from(["push".to_string(), "workflow_dispatch".to_string()]);
        if triggers.events != allowed {
            errors.push(
                "ci-portable.yml must have exactly `workflow_dispatch` and `push` triggers"
                    .to_string(),
            );
        }
        if triggers.push_branches.as_deref() != Some(&["integration".to_string()]) {
            errors.push("ci-portable.yml must push-trigger exactly on `integration`".to_string());
        }
    }
    errors
}

fn named_step<'a>(job: &'a str, name: &str) -> Result<(usize, &'a str), String> {
    let marker = format!("      - name: {name}");
    if job.matches(&marker).count() != 1 {
        return Err(format!("expected exactly one `{name}` step"));
    }
    let start = job
        .find(&marker)
        .expect("unique marker must have an offset");
    let rest = &job[start..];
    let end = rest[1..]
        .find("\n      - ")
        .map_or(rest.len(), |offset| offset + 1);
    Ok((start, rest[..end].trim_end()))
}

fn validate_validation_levels_bootstrap(source: &str) -> Vec<String> {
    let mut errors = Vec::new();
    let version_declaration = format!("  RUST_SCRIPT_VERSION: \"{RUST_SCRIPT_VERSION}\"");
    if source.matches("RUST_SCRIPT_VERSION:").count() != 1
        || !source.lines().any(|line| line == version_declaration)
    {
        errors.push(format!(
            "validation-levels.yml must declare exactly one global RUST_SCRIPT_VERSION pinned to {RUST_SCRIPT_VERSION}"
        ));
    }

    let Some(quick_start) = source.find("\n  quick:\n") else {
        errors.push("validation-levels.yml is missing its quick job".to_string());
        return errors;
    };
    let Some(relative_quick_end) = source[quick_start + 1..].find("\n  full:\n") else {
        errors.push("validation-levels.yml quick job has no full-job boundary".to_string());
        return errors;
    };
    let quick_end = quick_start + 1 + relative_quick_end;
    let quick = &source[quick_start..quick_end];

    let expected_install = "      - name: Install rust-script\n        env:\n          RUSTFLAGS: \"\"\n        run: cargo install rust-script --version \"${RUST_SCRIPT_VERSION}\" --locked";
    let expected_verification = r#"      - name: Verify rust-script resolves at the pinned version
        run: |
          rust_script_path=$(command -v rust-script) || {
            echo "::error::rust-script is not on PATH; scripts/validate.rs would die at exec with status 127."
            exit 1
          }
          actual_version=$(rust-script --version)
          expected_version="rust-script ${RUST_SCRIPT_VERSION}"
          if [[ "$actual_version" != "$expected_version" ]]; then
            echo "::error::expected ${expected_version}, found ${actual_version} at ${rust_script_path}"
            exit 1
          fi
          echo "rust-script resolved: ${rust_script_path} ${actual_version}""#;
    let expected_validation = "      - name: GitHub-managed portable test lane\n        run: ./scripts/validate.rs --portable-only --no-label-pr";

    let install = named_step(quick, "Install rust-script");
    let verification = named_step(quick, "Verify rust-script resolves at the pinned version");
    let validation = named_step(quick, "GitHub-managed portable test lane");
    if install.as_ref().map(|(_, step)| *step) != Ok(expected_install) {
        errors.push(
            "validation-levels.yml quick job must install the pinned rust-script with tool-only RUSTFLAGS"
                .to_string(),
        );
    }
    if verification.as_ref().map(|(_, step)| *step) != Ok(expected_verification) {
        errors.push(
            "validation-levels.yml quick job must fail closed unless rust-script resolves at the pinned version"
                .to_string(),
        );
    }
    if validation.as_ref().map(|(_, step)| *step) != Ok(expected_validation) {
        errors.push(
            "validation-levels.yml quick job must run the canonical portable validation command"
                .to_string(),
        );
    }

    let validation_command = "./scripts/validate.rs --portable-only --no-label-pr";
    let actual_validation_offset = quick.find(validation_command);
    if quick.matches(validation_command).count() != 1
        || !matches!(
            (&install, &verification, &validation, actual_validation_offset),
            (Ok((install, _)), Ok((verify, _)), Ok((validation, _)), Some(actual))
                if install < verify && verify < validation && validation < &actual
        )
    {
        errors.push(
            "validation-levels.yml must install and verify rust-script before invoking scripts/validate.rs"
                .to_string(),
        );
    }
    errors
}

fn buck2_nightly_required_markers() -> Vec<(&'static str, &'static str)> {
    let path_export = r#"echo "$RUNNER_TEMP/cargo-home/bin" >>"$GITHUB_PATH""#;
    let install = r#"cargo install rust-script --version 0.36.0 --locked --root "$CARGO_HOME""#;
    let path_binding = r#"test "$(realpath "$resolved_rust_script")" = "$(realpath "$CARGO_HOME/bin/rust-script")""#;
    let version_binding = r#"test "$("$resolved_rust_script" --version)" = "rust-script 0.36.0""#;
    vec![
        (
            "180-minute supplemental release-evidence job",
            "  buck-release-evidence:\n    name: Supplemental feature-complete Buck release evidence\n    runs-on: ubuntu-24.04\n    timeout-minutes: 180",
        ),
        (
            "restored 120-minute legacy OSS Buck job",
            "  oss-buck2:\n    name: Regenerate and build the OSS Buck2 graph\n    runs-on: ubuntu-24.04\n    timeout-minutes: 120",
        ),
        (
            "read-only workflow permissions",
            "permissions:\n  contents: read",
        ),
        ("checkout credential refusal", "persist-credentials: false"),
        ("isolated rust-script PATH export", path_export),
        ("isolated rust-script install root", install),
        (
            "isolated rust-script executable check",
            r#"test -x "$CARGO_HOME/bin/rust-script""#,
        ),
        (
            "resolved rust-script path lookup",
            r#"resolved_rust_script="$(command -v rust-script)""#,
        ),
        ("resolved rust-script path binding", path_binding),
        ("pinned rust-script version binding", version_binding),
        (
            "Buck release driver refusal tests",
            "rust-script --test scripts/build-buck-release.rs",
        ),
        (
            "release capture step",
            "      - name: Capture the feature-complete Hermit release build\n        id: release-build",
        ),
        (
            "inner process-tree build deadline",
            "          timeout --foreground --kill-after=30s 40m \\\n            \"$PUBLIC_DOTSLASH\" ./bootstrap/buck2 build --show-output --no-remote-cache \\",
        ),
        (
            "capture EXIT receipt trap",
            "          trap retain_shell_exit EXIT",
        ),
        (
            "recognized release event-log filename",
            "          event_log=\"$evidence/hermit-release.json-lines.gz\"",
        ),
        (
            "Cargo package version derivation",
            "          version=\"$(sed -n '/^\\[package\\]/,/^\\[/s/^version = \"\\([^\"]*\\)\"/\\1/p' hermit-cli/Cargo.toml | head -n 1)\"",
        ),
        (
            "UTC build-date derivation",
            "          build_date=\"$(date -u +%F)\"",
        ),
        (
            "12-character Hermit SHA derivation",
            "          hermit_sha=\"$(git rev-parse --short=12 HEAD)\"",
        ),
        (
            "40-character Reverie gitlink derivation",
            "          reverie_sha=\"$(git rev-parse HEAD:reverie)\"",
        ),
        (
            "Hermit SHA length assertion",
            "          test \"${#hermit_sha}\" -eq 12",
        ),
        (
            "Reverie SHA length assertion",
            "          test \"${#reverie_sha}\" -eq 40",
        ),
        (
            "explicit public DotSlash release build",
            "            \"$PUBLIC_DOTSLASH\" ./bootstrap/buck2 build --show-output --no-remote-cache \\",
        ),
        (
            "recognized release event-log argument",
            "            --event-log \"$event_log\" \\",
        ),
        (
            "release version metadata argument",
            "            -c \"hermit_release.version=$version\" \\",
        ),
        (
            "release date metadata argument",
            "            -c \"hermit_release.build_date=$build_date\" \\",
        ),
        (
            "release Hermit SHA metadata argument",
            "            -c \"hermit_release.hermit_sha=$hermit_sha\" \\",
        ),
        (
            "release Reverie SHA metadata argument",
            "            -c \"hermit_release.reverie_sha=$reverie_sha\" \\",
        ),
        (
            "feature-complete release target argument",
            "            //hermit-cli:hermit-release \\",
        ),
        (
            "captured shell exit",
            "printf 'shell_exit\\t%s\\n' \"$capture_rc\" >\"$evidence/shell-exit.tsv\"",
        ),
        ("capture status propagation", "          exit \"$shell_rc\""),
        (
            "always-run reconciliation step",
            "      - name: Reconcile and scan release evidence\n        id: release-evidence\n        if: always()",
        ),
        (
            "explicit public DotSlash event summary",
            "          \"$PUBLIC_DOTSLASH\" ./bootstrap/buck2 log summary \\",
        ),
        (
            "typed release evidence reconciler",
            "          ./scripts/build-buck-release.rs --reconcile-release-evidence \\",
        ),
        (
            "reconciler show-output binding",
            "            --show-output \"$evidence/build.stdout\" \\",
        ),
        (
            "reconciler shell-exit binding",
            "            --shell-exit \"$shell_rc\" \\",
        ),
        (
            "combined captured-byte evidence packager",
            "          ./scripts/build-buck-release.rs --scan-and-package-public-evidence \\",
        ),
        (
            "combined scanner receipt output",
            "            --receipt-output \"$scan_receipt\" \\",
        ),
        (
            "scanner failure refusal",
            "          ./scripts/build-buck-release.rs --release-evidence-verdict \\",
        ),
        (
            "zero-marker assertion",
            "          test \"$marker_count\" = 0",
        ),
        (
            "reconciliation failure refusal",
            "            --reconciliation-exit \"$reconciliation_rc\"",
        ),
        (
            "external immutable archive binding",
            "            --archive \"$archive\"",
        ),
        (
            "archive canonical-path equality",
            "          test \"$artifact_path\" = \"$archive\"",
        ),
        (
            "canonical immutable archive path",
            "          artifact_path=\"$(realpath -e \"$archive\")\"",
        ),
        (
            "canonical artifact output",
            "          echo \"artifact-path=$artifact_path\" >>\"$GITHUB_OUTPUT\"",
        ),
        (
            "conditional safe upload output",
            "          echo \"safe-to-upload=true\" >>\"$GITHUB_OUTPUT\"",
        ),
        (
            "safe public upload condition",
            "if: always() && steps.release-evidence.outputs.safe-to-upload == 'true'",
        ),
        (
            "release evidence artifact path",
            "path: ${{ steps.release-evidence.outputs.artifact-path }}",
        ),
        (
            "upload missing-file refusal",
            "          if-no-files-found: error",
        ),
        (
            "diagnostic after evidence upload",
            "      - name: Run the full third-party diagnostic",
        ),
        (
            "diagnostic non-upload disclosure",
            "      - name: Scan and package the full third-party diagnostic",
        ),
        (
            "guarded legacy diagnostic upload",
            "if: always() && steps.third-party-evidence.outputs.safe-to-upload == 'true'",
        ),
        (
            "preserved legacy diagnostic artifact name",
            "name: buck2-third-party-diagnostic-${{ github.run_id }}",
        ),
        (
            "immutable legacy diagnostic staging path",
            "path: ${{ steps.third-party-evidence.outputs.artifact-path }}",
        ),
        (
            "combined captured-byte legacy staging",
            "          ./scripts/build-buck-release.rs --scan-and-stage-public-evidence \\",
        ),
        ("legacy Hermit target", "//hermit-cli:hermit'"),
    ]
}

// These non-cryptographic hashes are drift tripwires for reviewed workflow
// blocks. Semantic validators and mutation tests remain the security boundary.
const RELEASE_CAPTURE_BLOCK_FNV1A64: u64 = 0xd47269f7822be906;
const RELEASE_FINALIZE_BLOCK_FNV1A64: u64 = 0x2391b53a25c3213a;
const RELEASE_UPLOAD_BLOCK_FNV1A64: u64 = 0x729e01b3370e7f6f;
const LEGACY_REVERIE_BLOCK_FNV1A64: u64 = 0x560c7d2fcc02b62f;
const LEGACY_DIAGNOSTIC_CAPTURE_BLOCK_FNV1A64: u64 = 0x0ce72a075541cfdf;
const LEGACY_DIAGNOSTIC_FINALIZE_BLOCK_FNV1A64: u64 = 0x6c037b9cd4dabcaf;
const LEGACY_DIAGNOSTIC_UPLOAD_BLOCK_FNV1A64: u64 = 0x48723ec958bff433;
const LEGACY_FINAL_TARGET_BLOCK_FNV1A64: u64 = 0x2a320ec4e59c3edc;

fn named_step_block<'a>(source: &'a str, name: &str) -> Option<&'a str> {
    let marker = format!("      - name: {name}\n");
    let start = source.find(&marker)?;
    let rest = &source[start + marker.len()..];
    let end = rest
        .find("\n      - name: ")
        .map(|offset| start + marker.len() + offset + 1)
        .or_else(|| {
            rest.find("\n  oss-buck2:")
                .map(|offset| start + marker.len() + offset + 1)
        })
        .unwrap_or(source.len());
    source.get(start..end)
}

fn fnv1a64(text: &str) -> u64 {
    text.replace("\r\n", "\n")
        .trim_end()
        .bytes()
        .fold(0xcbf29ce484222325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        })
}

fn validate_bound_release_blocks(source: &str) -> Vec<String> {
    let mut errors = Vec::new();
    for (name, expected) in [
        (
            "Capture the feature-complete Hermit release build",
            RELEASE_CAPTURE_BLOCK_FNV1A64,
        ),
        (
            "Reconcile and scan release evidence",
            RELEASE_FINALIZE_BLOCK_FNV1A64,
        ),
        (
            "Upload feature-complete release evidence",
            RELEASE_UPLOAD_BLOCK_FNV1A64,
        ),
        (
            "Build the four Reverie targets",
            LEGACY_REVERIE_BLOCK_FNV1A64,
        ),
        (
            "Run the full third-party diagnostic",
            LEGACY_DIAGNOSTIC_CAPTURE_BLOCK_FNV1A64,
        ),
        (
            "Scan and package the full third-party diagnostic",
            LEGACY_DIAGNOSTIC_FINALIZE_BLOCK_FNV1A64,
        ),
        (
            "Upload the full third-party diagnostic log",
            LEGACY_DIAGNOSTIC_UPLOAD_BLOCK_FNV1A64,
        ),
        (
            "Build the Hermit green gate",
            LEGACY_FINAL_TARGET_BLOCK_FNV1A64,
        ),
    ] {
        match named_step_block(source, name) {
            Some(block) if fnv1a64(block) == expected => {}
            Some(block) => errors.push(format!(
                "buck2-oss-nightly.yml bound step {name:?} changed: expected fnv1a64={expected:016x}, got {:016x}",
                fnv1a64(block)
            )),
            None => errors.push(format!(
                "buck2-oss-nightly.yml lacks bound step {name:?}"
            )),
        }
    }
    errors
}

fn validate_buck2_nightly_contract(source: &str) -> Vec<String> {
    let mut errors = Vec::new();
    errors.extend(validate_bound_release_blocks(source));
    for (description, marker) in buck2_nightly_required_markers() {
        let expected = match description {
            "checkout credential refusal"
            | "isolated rust-script PATH export"
            | "isolated rust-script install root"
            | "isolated rust-script executable check"
            | "resolved rust-script path lookup"
            | "resolved rust-script path binding"
            | "pinned rust-script version binding"
            | "reconciler show-output binding"
            | "canonical artifact output"
            | "conditional safe upload output"
            | "upload missing-file refusal" => 2,
            _ => 1,
        };
        if source.matches(marker).count() != expected {
            errors.push(format!(
                "buck2-oss-nightly.yml must contain exactly {expected} occurrence(s) of {description}"
            ));
        }
    }

    let path_export = r#"echo "$RUNNER_TEMP/cargo-home/bin" >>"$GITHUB_PATH""#;
    let install = r#"cargo install rust-script --version 0.36.0 --locked --root "$CARGO_HOME""#;
    let path_binding = r#"test "$(realpath "$resolved_rust_script")" = "$(realpath "$CARGO_HOME/bin/rust-script")""#;
    let version_binding = r#"test "$("$resolved_rust_script" --version)" = "rust-script 0.36.0""#;
    let path_offset = source.find(path_export);
    let install_offset = source.find(install);
    let binding_offset = source.find(path_binding);
    let version_offset = source.find(version_binding);
    let regeneration_offset = source.find("./bootstrap/regenerate-rust-deps");
    let capture_offset =
        source.find("      - name: Capture the feature-complete Hermit release build");
    let reconcile_offset = source.find("      - name: Reconcile and scan release evidence");
    let scan_offset =
        source.find("./scripts/build-buck-release.rs --scan-and-package-public-evidence");
    let zero_marker_offset = source.find("test \"$marker_count\" = 0");
    let artifact_output_offset =
        source.find("echo \"artifact-path=$artifact_path\" >>\"$GITHUB_OUTPUT\"");
    let safe_output_offset = source.find("echo \"safe-to-upload=true\" >>\"$GITHUB_OUTPUT\"");
    let verdict_offset = source.find("./scripts/build-buck-release.rs --release-evidence-verdict");
    let upload_offset = source.find("      - name: Upload feature-complete release evidence");
    let diagnostic_offset = source.find("      - name: Run the full third-party diagnostic");
    let legacy_offset = source.find("      - name: Build the Hermit green gate");
    if !matches!(
        (
            path_offset,
            install_offset,
            binding_offset,
            version_offset,
            regeneration_offset,
            capture_offset,
            reconcile_offset,
            scan_offset,
            zero_marker_offset,
            artifact_output_offset,
            safe_output_offset,
            verdict_offset,
            upload_offset,
            diagnostic_offset,
            legacy_offset,
        ),
        (
            Some(path),
            Some(install),
            Some(binding),
            Some(version),
            Some(regeneration),
            Some(capture),
            Some(reconcile),
            Some(scan),
            Some(zero_marker),
            Some(artifact_output),
            Some(safe_output),
            Some(verdict),
            Some(upload),
            Some(diagnostic),
            Some(legacy),
        )
            if path < install
                && install < binding
                && binding < version
                && version < regeneration
                && regeneration < capture
                && capture < reconcile
                && reconcile < scan
                && scan < zero_marker
                && zero_marker < artifact_output
                && artifact_output < safe_output
                && safe_output < verdict
                && verdict < upload
                && upload < diagnostic
                && diagnostic < legacy
    ) {
        errors.push(
            "buck2-oss-nightly.yml load-bearing tool/build/reconcile/scan/upload/diagnostic ordering drifted"
                .to_string(),
        );
    }
    let global = source
        .split_once("\njobs:")
        .map(|(global, _)| global)
        .unwrap_or_default();
    let carriers = [
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "ACTIONS_RUNTIME_TOKEN",
        "ACTIONS_ID_TOKEN_REQUEST_TOKEN",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
    ];
    for carrier in carriers {
        if global.contains(&format!("  {carrier}:")) {
            errors.push(format!(
                "buck2-oss-nightly.yml must not globally blank token carrier {carrier}; upload actions need their runtime credentials"
            ));
        }
    }
    let mut starts = source
        .match_indices("      - name: ")
        .map(|(offset, _)| offset)
        .collect::<Vec<_>>();
    starts.push(source.len());
    for pair in starts.windows(2) {
        let block = &source[pair[0]..pair[1]];
        let name = block.lines().next().unwrap_or("unnamed step");
        if block.contains("        shell: bash\n") {
            for carrier in carriers {
                let marker = format!("          {carrier}: \"\"");
                if block.matches(&marker).count() != 1 {
                    errors.push(format!(
                        "buck2-oss-nightly.yml shell step {name:?} must explicitly clear {carrier} exactly once"
                    ));
                }
            }
        }
        if block.contains("uses: actions/upload-artifact@")
            && ["ACTIONS_RUNTIME_TOKEN", "ACTIONS_ID_TOKEN_REQUEST_TOKEN"]
                .into_iter()
                .any(|carrier| block.contains(&format!("{carrier}: \"\"")))
        {
            errors.push(format!(
                "buck2-oss-nightly.yml upload step {name:?} must inherit Actions runtime/id tokens"
            ));
        }
    }
    if upload_offset.is_some()
        && reconcile_offset.is_some()
        && source[..upload_offset.unwrap()]
            .rfind("\n      - name:")
            .is_some_and(|offset| offset + 1 != reconcile_offset.unwrap())
    {
        errors.push(
            "buck2-oss-nightly.yml must upload the immutable archive immediately after reconciliation"
                .to_owned(),
        );
    }
    let diagnostic_finalize =
        source.find("      - name: Scan and package the full third-party diagnostic");
    let diagnostic_upload = source.find("      - name: Upload the full third-party diagnostic log");
    if diagnostic_upload.is_none()
        || diagnostic_finalize.is_none()
        || source[..diagnostic_upload.unwrap()]
            .rfind("\n      - name:")
            .is_none_or(|offset| offset + 1 != diagnostic_finalize.unwrap())
    {
        errors.push(
            "buck2-oss-nightly.yml must immediately upload the guarded legacy diagnostic staging directory"
                .to_owned(),
        );
    }
    for forbidden in [
        "command_success == true",
        "release_output_count",
        "reconciliation=pass",
        "marker_count=0",
        "scan_rc=0",
        "shell_rc=0",
        "summary_rc=0",
        "reconciliation_rc=0",
        "|| true",
        "path: ${{ runner.temp }}/buck2-phase1-release",
        "continue-on-error:",
        "    needs:",
        "${{ github.token }}",
        " --scan-public-evidence ",
        " --package-public-evidence ",
        " --stage-public-evidence ",
        " --verify-staged-public-evidence ",
    ] {
        if source.contains(forbidden) {
            errors.push(format!(
                "buck2-oss-nightly.yml contains forbidden evidence shortcut {forbidden:?}"
            ));
        }
    }
    errors
}

fn workflow_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries =
        fs::read_dir(dir).map_err(|error| format!("cannot read {}: {error}", dir.display()))?;
    let mut files = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|error| format!("cannot read workflow directory entry: {error}"))?
            .path();
        if matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("yml" | "yaml")
        ) {
            files.push(path);
        }
    }
    files.sort();
    if files.is_empty() {
        return Err(format!("no YAML workflows found under {}", dir.display()));
    }
    Ok(files)
}

fn run(dir: &Path) -> Result<usize, Vec<String>> {
    let files = workflow_files(dir).map_err(|error| vec![error])?;
    let mut errors = Vec::new();
    let mut saw_portable = false;
    for path in &files {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        saw_portable |= name == PORTABLE;
        let source = match fs::read_to_string(path) {
            Ok(source) => source,
            Err(error) => {
                errors.push(format!("{}: cannot read: {error}", path.display()));
                continue;
            }
        };
        match parse_triggers(&source) {
            Ok(triggers) => {
                errors.extend(
                    validate(name, &triggers)
                        .into_iter()
                        .map(|error| format!("{}: {error}", path.display())),
                );
            }
            Err(error) => errors.push(format!("{}: {error}", path.display())),
        }
        if name == VALIDATION_LEVELS {
            errors.extend(
                validate_validation_levels_bootstrap(&source)
                    .into_iter()
                    .map(|error| format!("{}: {error}", path.display())),
            );
        }
        if name == BUCK2_OSS_NIGHTLY {
            errors.extend(
                validate_buck2_nightly_contract(&source)
                    .into_iter()
                    .map(|error| format!("{}: {error}", path.display())),
            );
        }
    }
    if !saw_portable {
        errors.push(format!("{PORTABLE} is missing"));
    }
    if errors.is_empty() {
        Ok(files.len())
    } else {
        Err(errors)
    }
}

fn main() {
    rust_script_prelude::init();
    match run(Path::new(WORKFLOW_DIR)) {
        Ok(count) => println!(
            "GitHub Actions triggers OK: {count} workflows; automatic runs are limited to integration and exact approved schedules"
        ),
        Err(errors) => {
            for error in errors {
                eprintln!("check-github-actions-triggers: {error}");
            }
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(source: &str) -> Triggers {
        parse_triggers(source).expect("fixture should parse")
    }

    #[test]
    fn portable_policy_accepts_integration_and_manual_only() {
        let triggers = parsed(
            "name: portable\non:\n  workflow_dispatch:\n  push:\n    branches: [integration]\njobs:\n",
        );
        assert!(validate(PORTABLE, &triggers).is_empty());
    }

    #[test]
    fn manual_only_workflow_is_allowed() {
        let triggers = parsed("on:\n  workflow_dispatch:\njobs:\n");
        assert!(validate("other.yml", &triggers).is_empty());
    }

    #[test]
    fn exact_approved_schedules_are_allowed() {
        for (name, cron) in [
            ("buck2-oss-nightly.yml", "17 10 * * *"),
            ("docs.yml", "23 8 * * 0-6"),
        ] {
            let triggers = parsed(&format!(
                "on:\n  schedule:\n    - cron: \"{cron}\"\n  workflow_dispatch:\njobs:\n"
            ));
            assert!(validate(name, &triggers).is_empty(), "{name}");
        }
    }

    #[test]
    fn pull_request_and_schedule_are_rejected() {
        for event in [
            "pull_request",
            "pull_request_target",
            "schedule",
            "workflow_run",
        ] {
            let triggers = parsed(&format!("on:\n  workflow_dispatch:\n  {event}:\njobs:\n"));
            assert!(
                !validate("other.yml", &triggers).is_empty(),
                "{event} unexpectedly passed"
            );
        }
    }

    #[test]
    fn copied_or_modified_schedules_are_rejected() {
        for (name, cron) in [
            ("other.yml", "17 10 * * *"),
            ("buck2-oss-nightly.yml", "18 10 * * *"),
            ("docs.yml", "23 8 * * 1"),
            ("docs.yml", "23 8 * * *"),
        ] {
            let triggers = parsed(&format!(
                "on:\n  schedule:\n    - cron: \"{cron}\"\n  workflow_dispatch:\njobs:\n"
            ));
            assert!(!validate(name, &triggers).is_empty(), "{name} {cron}");
        }
    }

    #[test]
    fn approved_schedule_rejects_extra_automatic_trigger() {
        let triggers = parsed(
            "on:\n  schedule:\n    - cron: \"17 10 * * *\"\n  workflow_dispatch:\n  push:\n    branches: [integration]\njobs:\n",
        );
        assert!(!validate("buck2-oss-nightly.yml", &triggers).is_empty());
    }

    #[test]
    fn ambiguous_schedule_shapes_fail_to_parse() {
        for source in [
            "on:\n  schedule:\n    cron: \"17 10 * * *\"\n  workflow_dispatch:\njobs:\n",
            "on:\n  schedule:\n    - cron: 17 10 * * *\n  workflow_dispatch:\njobs:\n",
            "on:\n  schedule:\n    - cron: \"17 10 * * *\"\n      timezone: UTC\n  workflow_dispatch:\njobs:\n",
        ] {
            assert!(
                parse_triggers(source).is_err(),
                "schedule fixture unexpectedly parsed"
            );
        }
    }

    #[test]
    fn main_and_unbounded_pushes_are_rejected() {
        for push in ["  push:\n    branches: [main]\n", "  push:\n"] {
            let triggers = parsed(&format!("on:\n  workflow_dispatch:\n{push}jobs:\n"));
            assert!(!validate("other.yml", &triggers).is_empty());
        }
    }

    #[test]
    fn tag_pushes_are_rejected() {
        let source = "on:\n  workflow_dispatch:\n  push:\n    branches: [integration]\n    tags: [v1]\njobs:\n";
        let error = parse_triggers(source).expect_err("tag trigger unexpectedly parsed");
        assert!(error.contains("only `branches` is permitted"), "{error}");
    }

    #[test]
    fn deeper_push_structures_are_rejected() {
        let source = "on:\n  workflow_dispatch:\n  push:\n    branches: [integration]\n      unexpected: value\njobs:\n";
        let error = parse_triggers(source).expect_err("deeper push structure unexpectedly parsed");
        assert!(error.contains("ambiguous nested structure"), "{error}");
    }

    #[test]
    fn portable_requires_both_allowed_events() {
        let triggers = parsed("on:\n  workflow_dispatch:\njobs:\n");
        assert!(!validate(PORTABLE, &triggers).is_empty());
    }

    #[test]
    fn validation_levels_quick_job_bootstraps_the_pinned_rust_script() {
        let source = include_str!("../.github/workflows/validation-levels.yml");
        assert!(
            validate_validation_levels_bootstrap(source).is_empty(),
            "checked-in workflow lost its rust-script bootstrap"
        );
    }

    #[test]
    fn validation_levels_bootstrap_rejects_missing_or_unpinned_tools() {
        let source = include_str!("../.github/workflows/validation-levels.yml");
        let premature_validation = source
            .replacen(
                "      - name: GitHub-managed portable test lane\n        run: ./scripts/validate.rs --portable-only --no-label-pr",
                "      - name: GitHub-managed portable test lane\n        run: echo skipped",
                1,
            )
            .replacen(
                "      - name: Install rust-script",
                "      - name: Premature portable validation\n        run: ./scripts/validate.rs --portable-only --no-label-pr\n      - name: Install rust-script",
                1,
            );
        for broken in [
            source.replacen("  RUST_SCRIPT_VERSION: \"0.36.0\"\n", "", 1),
            source.replacen(
                "cargo install rust-script --version \"${RUST_SCRIPT_VERSION}\" --locked",
                "cargo install rust-script",
                1,
            ),
            source.replacen(
                "rust_script_path=$(command -v rust-script)",
                "rust_script_path=rust-script",
                1,
            ),
            source.replacen(
                "actual_version=$(rust-script --version)",
                "actual_version=unknown",
                1,
            ),
            source.replacen(
                "rust_script_path=$(command -v rust-script) || {\n            echo \"::error::rust-script is not on PATH; scripts/validate.rs would die at exec with status 127.\"\n            exit 1",
                "rust_script_path=$(command -v rust-script) || {\n            echo \"::error::rust-script is not on PATH; scripts/validate.rs would die at exec with status 127.\"\n            true",
                1,
            ),
            source.replacen(
                "echo \"::error::expected ${expected_version}, found ${actual_version} at ${rust_script_path}\"\n            exit 1",
                "echo \"::error::expected ${expected_version}, found ${actual_version} at ${rust_script_path}\"\n            true",
                1,
            ),
            source.replacen(
                "[[ \"$actual_version\" != \"$expected_version\" ]]",
                "[[ \"$actual_version\" == \"$expected_version\" ]]",
                1,
            ),
            premature_validation,
        ] {
            assert!(
                !validate_validation_levels_bootstrap(&broken).is_empty(),
                "broken bootstrap unexpectedly passed"
            );
        }
    }

    #[test]
    fn buck2_nightly_binds_tools_release_event_and_public_artifact_evidence() {
        let source = include_str!("../.github/workflows/buck2-oss-nightly.yml");
        assert!(
            validate_buck2_nightly_contract(source).is_empty(),
            "checked-in Buck2 nightly lost its fail-closed release contract"
        );
    }

    #[test]
    fn buck2_nightly_contract_refuses_path_and_evidence_gate_regressions() {
        let source = include_str!("../.github/workflows/buck2-oss-nightly.yml");
        for (description, marker) in buck2_nightly_required_markers() {
            let broken = source.replacen(marker, "removed-load-bearing-contract", 1);
            assert!(
                !validate_buck2_nightly_contract(&broken).is_empty(),
                "removing {description} unexpectedly passed"
            );
        }
        let path_export = r#"echo "$RUNNER_TEMP/cargo-home/bin" >>"$GITHUB_PATH""#;
        let install = r#"cargo install rust-script --version 0.36.0 --locked --root "$CARGO_HOME""#;
        let late_path = source.replacen(path_export, "", 1).replacen(
            install,
            &format!("{install}\n          {path_export}"),
            1,
        );
        let late_upload = source
            .replacen(
                "      - name: Upload feature-complete release evidence",
                "      - name: Misplaced release evidence upload",
                1,
            )
            .replacen(
                "      - name: Build the Hermit green gate",
                "      - name: Upload feature-complete release evidence\n        run: true\n      - name: Build the Hermit green gate",
                1,
            );
        for broken in [
            source.replacen(
                path_export,
                r#"echo "$HOME/.cargo/bin" >>"$GITHUB_PATH""#,
                1,
            ),
            late_path,
            late_upload,
            source.replacen(
                "          GH_TOKEN: \"\"",
                "          GH_TOKEN: inherited",
                1,
            ),
            source.replacen(
                "          test \"$marker_count\" = 0",
                "          marker_count=0\n          test \"$marker_count\" = 0",
                1,
            ),
            source.replacen(
                "          [[ $shell_rc =~ ^[0-9]+$ ]]",
                "          shell_rc=0\n          [[ $shell_rc =~ ^[0-9]+$ ]]",
                1,
            ),
            source.replacen(
                "          summary_rc=$?",
                "          summary_rc=$?\n          summary_rc=0",
                1,
            ),
            source.replacen(
                "          reconciliation_rc=$?",
                "          reconciliation_rc=$?\n          reconciliation_rc=0",
                1,
            ),
            source.replacen(
                "          summary_rc=$?",
                "          summary_rc=$?\n          summary_rc=$((summary_rc * 0))",
                1,
            ),
            source.replacen(
                "          ./scripts/build-buck-release.rs --release-evidence-verdict \\",
                "          if false; then\n            ./scripts/build-buck-release.rs --release-evidence-verdict \\",
                1,
            ),
            source.replacen(
                "          exit \"$shell_rc\"",
                "          : \"$shell_rc\"",
                1,
            ),
            source.replacen("          set +e\n", "", 1),
            source.replacen(
                "          candidate_rc=$?\n          set -e",
                "          set -e\n          candidate_rc=$?",
                1,
            ),
            source.replacen(
                "          ./scripts/build-buck-release.rs --scan-and-package-public-evidence \\",
                "          ./scripts/build-buck-release.rs --scan-and-package-public-evidence \\ || true",
                1,
            ),
            source.replacen(
                "        if: always() && steps.release-evidence.outputs.safe-to-upload == 'true'",
                "        if: always()",
                1,
            ),
            source.replacen(
                "          echo \"safe-to-upload=true\" >>\"$GITHUB_OUTPUT\"",
                "          echo \"safe-to-upload=true\" >>\"$GITHUB_OUTPUT\"\n          echo \"safe-to-upload=true\" >>\"$GITHUB_OUTPUT\"",
                1,
            ),
            source.replacen(
                "path: ${{ steps.release-evidence.outputs.artifact-path }}",
                "path: ${{ runner.temp }}/buck2-phase1-release",
                1,
            ),
            source.replacen(
                "      - name: Upload feature-complete release evidence",
                "      - name: Interposed mutable diagnostic\n        run: true\n\n      - name: Upload feature-complete release evidence",
                1,
            ),
        ] {
            assert!(
                !validate_buck2_nightly_contract(&broken).is_empty(),
                "broken Buck2 nightly contract unexpectedly passed"
            );
        }
        for carrier in [
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "ACTIONS_RUNTIME_TOKEN",
            "ACTIONS_ID_TOKEN_REQUEST_TOKEN",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
        ] {
            let cleared = format!("          {carrier}: \"\"");
            let broken = source.replacen(&cleared, &format!("          {carrier}: inherited"), 1);
            assert_ne!(source, broken, "missing token fixture for {carrier}");
            assert!(
                !validate_buck2_nightly_contract(&broken).is_empty(),
                "inheriting {carrier} in a load-bearing step unexpectedly passed"
            );
        }
        let github_token_reintroduced = source.replacen(
            "          GITHUB_TOKEN: \"\"",
            "          GITHUB_TOKEN: ${{ github.token }}",
            1,
        );
        assert!(
            !validate_buck2_nightly_contract(&github_token_reintroduced).is_empty(),
            "reintroducing github.token into a shell step unexpectedly passed"
        );
    }

    #[test]
    fn every_single_byte_mutation_of_bound_release_blocks_is_refused() {
        let source = include_str!("../.github/workflows/buck2-oss-nightly.yml");
        for name in [
            "Capture the feature-complete Hermit release build",
            "Reconcile and scan release evidence",
            "Upload feature-complete release evidence",
            "Build the four Reverie targets",
            "Run the full third-party diagnostic",
            "Scan and package the full third-party diagnostic",
            "Upload the full third-party diagnostic log",
            "Build the Hermit green gate",
        ] {
            let block = named_step_block(source, name).unwrap();
            let start = block.as_ptr() as usize - source.as_ptr() as usize;
            for index in 0..block.len() {
                let mut bytes = block.as_bytes().to_vec();
                bytes[index] = if bytes[index] == b'x' { b'y' } else { b'x' };
                let changed = String::from_utf8(bytes).unwrap();
                let mut mutated = source.to_owned();
                mutated.replace_range(start..start + block.len(), &changed);
                assert!(
                    !validate_bound_release_blocks(&mutated).is_empty(),
                    "single-byte mutation {index} of {name} escaped the bound-block contract"
                );
            }
        }
    }

    #[test]
    fn ambiguous_trigger_shapes_fail_to_parse() {
        for source in [
            "on: [push, workflow_dispatch]\njobs:\n",
            "on:\n  workflow_dispatch: {}\njobs:\n",
            "on:\n  push:\n    branches:\n      - integration\njobs:\n",
        ] {
            assert!(
                parse_triggers(source).is_err(),
                "fixture unexpectedly parsed"
            );
        }
    }

    #[test]
    fn duplicate_alternate_on_form_is_rejected() {
        for alternate in [
            "on: [pull_request]",
            "on : [pull_request]",
            "\"on\": [pull_request]",
            "'on': [pull_request]",
            "!!str on: [pull_request]",
            "? on\n: [pull_request]",
        ] {
            let source = format!("on:\n  workflow_dispatch:\n{alternate}\njobs:\n  test:\n");
            assert!(
                parse_triggers(&source).is_err(),
                "alternate top-level on unexpectedly parsed: {alternate}"
            );
        }
    }

    #[test]
    fn quoted_top_level_on_is_rejected() {
        for spelling in ["\"on\"", "'on'"] {
            let source = format!("{spelling}:\n  workflow_dispatch:\njobs:\n");
            let error =
                parse_triggers(&source).expect_err("quoted top-level on unexpectedly parsed");
            assert!(error.contains("unquoted simple spelling"), "{error}");
        }
    }
}
