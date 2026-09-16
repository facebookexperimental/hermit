#!/usr/bin/env -S rust-script --force
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Fail fast when a *nested* Cargo workspace's `Cargo.lock` is stale relative to
//! its own manifest, before the opaque `--locked` build failure downstream.
//!
//! `liteinst-runtime-build/` is a SEPARATE nested Cargo workspace
//! (`[workspace] members = ["runtime"]` in its own `Cargo.toml`); it is NOT a
//! member of the root Hermit workspace. Its inner `runtime` crate has a git
//! dependency on Reverie pinned by `rev`. Because it is a distinct workspace, a
//! root-level `cargo update` — or a Reverie-pin bump — refreshes the root
//! `Cargo.lock` but NOT `liteinst-runtime-build/Cargo.lock`. When the pin moves
//! and the nested lock is left behind, the staged build
//! (`scripts/stage-liteinst-runtime.sh` via `hermit-install/build.rs`) runs
//! `cargo build --locked` and fails ~78s in with the cryptic:
//!
//! ```text
//! error: cannot update the lock file liteinst-runtime-build/Cargo.lock
//!        because --locked was passed
//! ```
//!
//! `scripts/check-reverie-pin.rs` scans tracked Cargo metadata for a consistent
//! Reverie `rev` string but does NOT verify that each nested lockfile is fresh
//! versus its manifest. This checker closes that gap: for each known nested
//! workspace it runs `cargo metadata --locked` — the SAME probe that reproduces
//! the bug (rc=0 fresh / non-zero stale) — and, on failure, prints the exact
//! regenerate command and fails BEFORE the slow, opaque build.rs panic.
//!
//! `cargo metadata --locked` resolves the git dependency, so it needs network:
//! run it under the proxy on Meta hosts.
//!
//! Local use on Meta hosts:
//!
//! ```text
//! with-proxy ./scripts/check-nested-lockfiles.rs
//! ```

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::env;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

/// Nested Cargo workspaces (repo-relative directory holding a `Cargo.toml` with
/// its own `[workspace]` table and a sibling `Cargo.lock`) that the root
/// workspace does NOT include as members. Add one line per future nested
/// workspace.
const NESTED_WORKSPACES: &[&str] = &["liteinst-runtime-build"];

#[derive(Default)]
struct Config {
    repo: Option<PathBuf>,
}

fn usage() -> &'static str {
    "Usage: check-nested-lockfiles.rs [OPTIONS]\n\
     \n\
     Verify that each nested Cargo workspace's Cargo.lock is fresh versus its\n\
     manifest, using `cargo metadata --locked` (needs network for git deps; run\n\
     under with-proxy on Meta hosts).\n\
     \n\
     Options:\n\
       --repo PATH    Hermit checkout (default: git root)\n\
       -h, --help     Show this help"
}

fn take_value(args: &[String], i: &mut usize, flag: &str) -> Result<String, String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn parse_args() -> Result<Config, String> {
    let args: Vec<String> = env::args().collect();
    let mut config = Config::default();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--repo" => config.repo = Some(PathBuf::from(take_value(&args, &mut i, "--repo")?)),
            "-h" | "--help" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?}\n{}", usage())),
        }
        i += 1;
    }
    Ok(config)
}

fn git_root() -> Result<PathBuf, String> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|error| format!("could not run git rev-parse: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git rev-parse failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim(),
    ))
}

/// The actionable one-liner shown when a nested lockfile is stale. `workspace`
/// is the repo-relative nested-workspace directory (e.g. `liteinst-runtime-build`).
fn stale_message(workspace: &str) -> String {
    format!(
        "{workspace}/Cargo.lock is STALE vs its manifest. \
         Regenerate: (cd {workspace} && with-proxy cargo metadata --format-version 1 >/dev/null) \
         then commit {workspace}/Cargo.lock"
    )
}

/// Run `cargo metadata --locked` for one nested workspace. Returns Ok(()) when
/// the lockfile is fresh (rc=0), Err(captured stderr) when stale or otherwise
/// unresolvable (non-zero rc). A spawn failure (no cargo on PATH) is a hard
/// checker error.
fn probe_workspace(root: &Path, workspace: &str) -> Result<Result<(), String>, String> {
    let manifest = root.join(workspace).join("Cargo.toml");
    if !manifest.is_file() {
        return Err(format!(
            "nested workspace manifest not found: {}",
            manifest.display()
        ));
    }
    let output = Command::new("cargo")
        .args(["metadata", "--locked", "--format-version", "1"])
        .arg("--manifest-path")
        .arg(&manifest)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| {
            format!("could not run cargo metadata for {workspace}: {error} (is cargo on PATH?)")
        })?;
    if output.status.success() {
        Ok(Ok(()))
    } else {
        Ok(Err(String::from_utf8_lossy(&output.stderr)
            .trim()
            .to_string()))
    }
}

fn loud_header(title: &str) {
    eprintln!("======================================================================");
    eprintln!("NESTED LOCKFILE LINT: {title}");
    eprintln!("======================================================================");
}

fn run_with_config(config: Config) -> Result<i32, String> {
    let root = config.repo.clone().map_or_else(git_root, Ok)?;
    eprintln!(
        "Scope: checking {} nested Cargo workspace(s) for lockfile freshness ({}).",
        NESTED_WORKSPACES.len(),
        NESTED_WORKSPACES.join(", ")
    );

    let mut stale = false;
    for workspace in NESTED_WORKSPACES {
        match probe_workspace(&root, workspace)? {
            Ok(()) => {
                println!("OK: {workspace}/Cargo.lock is fresh versus its manifest.");
            }
            Err(cargo_stderr) => {
                stale = true;
                loud_header("NESTED Cargo.lock IS STALE - BLOCKED");
                eprintln!("{}", stale_message(workspace));
                if !cargo_stderr.is_empty() {
                    eprintln!("cargo metadata --locked reported:");
                    for line in cargo_stderr.lines() {
                        eprintln!("  {line}");
                    }
                }
            }
        }
    }

    if stale {
        eprintln!();
        eprintln!(
            "A nested workspace is NOT a member of the root Cargo workspace, so a root-level"
        );
        eprintln!(
            "`cargo update` or Reverie-pin bump does NOT refresh its Cargo.lock. Regenerate and"
        );
        eprintln!("commit the nested lockfile (see the per-file command above) before landing.");
        Ok(1)
    } else {
        println!(
            "All {} nested workspace lockfile(s) are fresh.",
            NESTED_WORKSPACES.len()
        );
        Ok(0)
    }
}

fn run() -> Result<i32, String> {
    run_with_config(parse_args()?)
}

fn main() {
    rust_script_prelude::init();
    match run() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            loud_header("CHECKER ERROR - BLOCKED");
            eprintln!("{error}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Output;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use super::*;

    fn split_validate_dry_run(args: &[&str]) -> std::process::Output {
        let root = git_root().unwrap_or_else(|_| {
            Path::new(file!())
                .canonicalize()
                .expect("checker source path")
                .parent()
                .and_then(Path::parent)
                .expect("checker lives under the repository scripts directory")
                .to_owned()
        });
        Command::new(root.join("ci/hermetic/run-split-validate.sh"))
            .args(args)
            .arg("--dry-run")
            .current_dir(root)
            .output()
            .expect("run split-validate dry-run")
    }

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    #[derive(Clone, Copy, Debug)]
    enum FetchMutation {
        None,
        OmitPreparation,
        OmitResolution,
        OmitLockedFetch,
        OmitAgentUtils,
        OmitAgentUtilsNeutralCwd,
        OmitAgentUtilsLockedFetch,
    }

    struct ProductionFixture {
        base: PathBuf,
        root: PathBuf,
        fake_bin: PathBuf,
        journal: PathBuf,
    }

    impl Drop for ProductionFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    fn source_repo_root() -> PathBuf {
        Path::new(file!())
            .canonicalize()
            .expect("checker source path")
            .parent()
            .and_then(Path::parent)
            .expect("checker lives under the repository scripts directory")
            .to_owned()
    }

    fn write_executable(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create executable parent");
        }
        fs::write(path, contents).expect("write executable fixture");
        let mut permissions = fs::metadata(path).expect("fixture metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("make fixture executable");
    }

    fn replace_exactly_once(source: &str, needle: &str, replacement: &str) -> String {
        assert_eq!(
            source.matches(needle).count(),
            1,
            "production mutation target must occur exactly once"
        );
        source.replacen(needle, replacement, 1)
    }

    fn production_fixture(mutation: FetchMutation) -> ProductionFixture {
        let source_root = source_repo_root();
        let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let base = env::temp_dir().join(format!(
            "split-validate-generated-fetch-{}-{sequence}",
            std::process::id()
        ));
        let root = base.join("repo");
        let fake_bin = base.join("bin");
        let journal = base.join("journal");
        fs::create_dir_all(root.join("ci/hermetic")).expect("create fixture repository");
        fs::create_dir_all(root.join("liteinst-runtime-build"))
            .expect("create nested workspace fixture");
        fs::create_dir_all(root.join("agent-utils/rs"))
            .expect("create Agent Utils workspace fixture");
        fs::create_dir_all(&fake_bin).expect("create fake binary directory");

        let mut split = fs::read_to_string(source_root.join("ci/hermetic/run-split-validate.sh"))
            .expect("read production split validator");
        let mut prepare = fs::read_to_string(source_root.join("ci/prepare-rust-scripts.sh"))
            .expect("read production rust-script producer");
        match mutation {
            FetchMutation::None => {}
            FetchMutation::OmitAgentUtils => {
                split = replace_exactly_once(&split, "    agent-utils/rs/Cargo.toml\n", "");
            }
            FetchMutation::OmitAgentUtilsNeutralCwd => {
                split = replace_exactly_once(
                    &split,
                    "                    cd /\n",
                    "                    : # mutation control: neutral Cargo cwd omitted\n",
                );
            }
            FetchMutation::OmitAgentUtilsLockedFetch => {
                split = replace_exactly_once(
                    &split,
                    "\"$cargo_bin\" fetch --locked \\\n",
                    "\"$cargo_bin\" fetch \\\n",
                );
            }
            FetchMutation::OmitPreparation => {
                split = replace_exactly_once(
                    &split,
                    "        CARGO_HOME=\"$cargo_home\" ./ci/prepare-rust-scripts.sh --fetch-only\n",
                    "        : # mutation control: generated workspace preparation omitted\n",
                );
            }
            FetchMutation::OmitResolution => {
                let block = r#"    if ! cargo generate-lockfile --manifest-path "$workspace_manifest" >"$output" 2>&1; then
        report_cargo_failure 'the generated rust-script workspace' "$workspace_manifest" "$output" \
            'resolve dependencies for' || exit $?
    fi
    cat "$output"
"#;
                prepare = replace_exactly_once(
                    &prepare,
                    block,
                    "    # mutation control: generated workspace resolution omitted\n",
                );
            }
            FetchMutation::OmitLockedFetch => {
                let block = r#"    if ! cargo fetch --locked --manifest-path "$workspace_manifest" >"$output" 2>&1; then
        report_cargo_failure 'the generated rust-script workspace' "$workspace_manifest" "$output" \
            'fetch dependencies for' || exit $?
    fi
    cat "$output"
"#;
                prepare = replace_exactly_once(
                    &prepare,
                    block,
                    "    # mutation control: generated workspace locked fetch omitted\n",
                );
            }
        }
        write_executable(&root.join("ci/hermetic/run-split-validate.sh"), &split);
        write_executable(&root.join("ci/prepare-rust-scripts.sh"), &prepare);

        fs::write(
            root.join("ci/portable-shards.json"),
            r#"{
  "preflight_nodes": [],
  "check_nodes": [],
  "build_debug_nodes": [],
  "build_dbt_nodes": [],
  "build_aux_nodes": [],
  "debug_shards": [{"slug": "unit", "nodes": ["fixture.node"]}],
  "release_shards": [],
  "strict_compat_nodes": [],
  "e2e_nodes": [],
  "final_nodes": []
}
"#,
        )
        .expect("write shard fixture");
        fs::write(root.join("ci/expected-e2e-plan.json"), "{}\n").expect("write plan fixture");
        fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = []\n")
            .expect("write root manifest fixture");
        fs::write(
            root.join("liteinst-runtime-build/Cargo.toml"),
            "[workspace]\nmembers = []\n",
        )
        .expect("write nested manifest fixture");
        fs::write(
            root.join("agent-utils/rs/Cargo.toml"),
            "[workspace]\nmembers = []\n",
        )
        .expect("write Agent Utils manifest fixture");
        fs::write(
            root.join("fixture.rs"),
            "#!/usr/bin/env -S rust-script --force\nfn main() {}\n",
        )
        .expect("write tracked rust-script fixture");

        write_executable(
            &root.join("ci/hermetic/assert-no-network.sh"),
            r#"#!/usr/bin/env bash
set -euo pipefail
printf 'network-probe\n' >>"${FIXTURE_JOURNAL:?}"
[[ ${1:-} == --expect-network ]]
"#,
        );
        write_executable(
            &root.join("ci/hermetic/run-in-pinned-root.sh"),
            r#"#!/usr/bin/env bash
set -euo pipefail
cargo_home=
while [[ $# -gt 0 ]]; do
    case $1 in
        --cargo-home) cargo_home=$2; shift 2 ;;
        *) shift ;;
    esac
done
[[ -f $cargo_home/generated-workspace-resolved ]] || {
    printf 'offline-missing-generated-resolution\n' >>"${FIXTURE_JOURNAL:?}"
    exit 91
}
[[ -f $cargo_home/generated-workspace-fetched ]] || {
    printf 'offline-missing-generated-fetch\n' >>"${FIXTURE_JOURNAL:?}"
    exit 92
}
[[ -f $cargo_home/agent-utils-workspace-fetched ]] || {
    printf 'offline-missing-agent-utils-fetch\n' >>"${FIXTURE_JOURNAL:?}"
    exit 94
}
printf 'offline-consumer\n' >>"${FIXTURE_JOURNAL:?}"
"#,
        );
        write_executable(
            &fake_bin.join("rust-script"),
            r#"#!/usr/bin/env bash
set -euo pipefail
if [[ ${1:-} == --version ]]; then
    echo 'rust-script 0.35.0'
    exit 0
fi
package_dir=
while [[ $# -gt 0 ]]; do
    case $1 in
        --package) shift ;;
        --pkg-path) package_dir=$2; shift 2 ;;
        *) shift ;;
    esac
done
[[ -n $package_dir ]]
mkdir -p "$package_dir"
printf '[package]\nname = "fixture"\nversion = "0.0.0"\nedition = "2021"\n' >"$package_dir/Cargo.toml"
printf 'fn main() {}\n' >"$package_dir/main.rs"
printf 'rust-script-package\n' >>"${FIXTURE_JOURNAL:?}"
"#,
        );
        write_executable(
            &fake_bin.join("rustc"),
            r#"#!/usr/bin/env bash
set -euo pipefail
printf 'rustc 1.90.0 (fixture)\nbinary: rustc\ncommit-hash: fixture\n'
"#,
        );
        write_executable(
            &fake_bin.join("cargo"),
            r#"#!/usr/bin/env bash
set -euo pipefail
command_name=${1:-}
shift || true
manifest=
locked=0
while [[ $# -gt 0 ]]; do
    case $1 in
        --manifest-path) manifest=$2; shift 2 ;;
        --locked) locked=1; shift ;;
        *) shift ;;
    esac
done
case $command_name in
    metadata)
        printf 'generated-metadata\n' >>"${FIXTURE_JOURNAL:?}"
        printf '{"packages":[{}]}\n'
        ;;
    generate-lockfile)
        [[ $manifest == */hermit-rust-script-packages.*/Cargo.toml ]]
        : >"${manifest%/*}/Cargo.lock"
        : >"${CARGO_HOME:?}/generated-workspace-resolved"
        printf 'generated-resolve\n' >>"${FIXTURE_JOURNAL:?}"
        ;;
    fetch)
        [[ $locked -eq 1 ]]
        mkdir -p "${CARGO_HOME:?}/registry"
        if [[ $manifest == */hermit-rust-script-packages.*/Cargo.toml ]]; then
            [[ -f ${manifest%/*}/Cargo.lock ]]
            [[ -f $CARGO_HOME/generated-workspace-resolved ]]
            : >"$CARGO_HOME/generated-workspace-fetched"
            printf 'generated-locked-fetch\n' >>"${FIXTURE_JOURNAL:?}"
        elif [[ $manifest == */agent-utils/rs/Cargo.toml ]]; then
            [[ $PWD == / ]]
            [[ $CARGO_HOME == /* ]]
            [[ -z ${CARGO_BUILD_TARGET+x} && -z ${CARGO_TARGET_DIR+x} ]]
            [[ -f $manifest ]]
            : >"$CARGO_HOME/agent-utils-workspace-fetched"
            printf 'agent-utils-neutral-locked-fetch\n' >>"${FIXTURE_JOURNAL:?}"
        else
            printf 'committed-locked-fetch:%s\n' "$manifest" >>"${FIXTURE_JOURNAL:?}"
        fi
        ;;
    *)
        printf 'unexpected fake cargo command: %s\n' "$command_name" >&2
        exit 93
        ;;
esac
"#,
        );

        let init = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status()
            .expect("initialize fixture repository");
        assert!(init.success());
        let add = Command::new("git")
            .args(["add", "fixture.rs"])
            .current_dir(&root)
            .status()
            .expect("stage rust-script fixture");
        assert!(add.success());
        let commit = Command::new("git")
            .args([
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-q",
                "-m",
                "fixture",
            ])
            .current_dir(&root)
            .status()
            .expect("commit rust-script fixture");
        assert!(commit.success());

        ProductionFixture {
            base,
            root,
            fake_bin,
            journal,
        }
    }

    fn run_production_fixture(mutation: FetchMutation) -> (Output, String) {
        let fixture = production_fixture(mutation);
        let path = env::join_paths(
            std::iter::once(fixture.fake_bin.clone())
                .chain(env::split_paths(&env::var_os("PATH").expect("test PATH"))),
        )
        .expect("join fixture PATH");
        let output = Command::new(fixture.root.join("ci/hermetic/run-split-validate.sh"))
            .args(["--out", "phase-output", "--shards", "unit"])
            .current_dir(&fixture.root)
            .env("PATH", path)
            .env(
                "HERMIT_REAL_RUST_SCRIPT",
                fixture.fake_bin.join("rust-script"),
            )
            .env("FIXTURE_JOURNAL", &fixture.journal)
            .env("CARGO_BUILD_TARGET", "fixture-consumer-target")
            .env("CARGO_TARGET_DIR", "fixture-consumer-output")
            .output()
            .expect("run production split validator fixture");
        let journal = fs::read_to_string(&fixture.journal).unwrap_or_default();
        (output, journal)
    }

    fn journal_position(journal: &str, event: &str) -> usize {
        journal
            .lines()
            .position(|line| line == event)
            .unwrap_or_else(|| panic!("missing {event:?} in fixture journal:\n{journal}"))
    }

    #[test]
    fn nested_workspace_list_is_non_empty() {
        assert!(
            !NESTED_WORKSPACES.is_empty(),
            "at least one nested workspace must be checked"
        );
    }

    #[test]
    fn liteinst_runtime_build_is_covered() {
        assert!(
            NESTED_WORKSPACES.contains(&"liteinst-runtime-build"),
            "the known nested workspace must be checked"
        );
    }

    #[test]
    fn hermetic_default_dry_run_covers_both_phases_and_nested_workspaces() {
        let output = split_validate_dry_run(&[]);
        assert!(
            output.status.success(),
            "split-validate dry-run failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let marker = "cargo fetch --locked --manifest-path ";
        let stdout = String::from_utf8_lossy(&output.stdout);
        let source_root = source_repo_root();
        let actual = stdout
            .lines()
            .filter_map(|line| {
                line.split_once(marker).map(|(_, manifest)| {
                    Path::new(manifest)
                        .strip_prefix(&source_root)
                        .unwrap_or_else(|_| Path::new(manifest))
                        .to_string_lossy()
                        .into_owned()
                })
            })
            .collect::<BTreeSet<_>>();
        let expected = std::iter::once("Cargo.toml".to_owned())
            .chain(
                NESTED_WORKSPACES
                    .iter()
                    .map(|workspace| format!("{workspace}/Cargo.toml")),
            )
            .chain(std::iter::once("agent-utils/rs/Cargo.toml".to_owned()))
            .collect::<BTreeSet<_>>();
        assert_eq!(
            expected, actual,
            "the hermetic fetch phase must cover root, checked nested workspaces and the Agent Utils launcher workspace"
        );
        assert!(stdout.contains("-- fetch phase would run"));
        assert!(stdout.contains("-- offline phase would run"));
        assert!(stdout.contains("./ci/prepare-rust-scripts.sh --fetch-only"));
    }

    #[test]
    fn hermetic_fetch_only_dry_run_does_not_show_offline() {
        let output = split_validate_dry_run(&["--fetch-only"]);
        assert!(
            output.status.success(),
            "split-validate dry-run failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("FETCH phase"));
        assert!(stdout.contains("-- fetch phase would run"));
        assert!(stdout.contains("cargo fetch --locked"));
        assert!(stdout.contains("./ci/prepare-rust-scripts.sh --fetch-only"));
        assert!(!stdout.contains("OFFLINE phase"));
        assert!(!stdout.contains("-- offline phase would run"));
    }

    #[test]
    fn hermetic_offline_dry_run_does_not_show_a_fetch() {
        let output = split_validate_dry_run(&["--offline-only"]);
        assert!(
            output.status.success(),
            "split-validate dry-run failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("OFFLINE phase"));
        assert!(stdout.contains("-- offline phase would run"));
        assert!(!stdout.contains("FETCH phase"));
        assert!(!stdout.contains("-- fetch phase would run"));
        assert!(!stdout.contains("cargo fetch --locked"));
    }

    #[test]
    fn production_split_resolves_and_fetches_generated_workspace_before_offline_use() {
        let (output, journal) = run_production_fixture(FetchMutation::None);
        assert!(
            output.status.success(),
            "production split fixture failed:\nstdout:\n{}\nstderr:\n{}\njournal:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            journal
        );

        let network = journal_position(&journal, "network-probe");
        let metadata = journal_position(&journal, "generated-metadata");
        let resolve = journal_position(&journal, "generated-resolve");
        let fetch = journal_position(&journal, "generated-locked-fetch");
        let offline = journal_position(&journal, "offline-consumer");
        assert!(
            network < metadata && metadata < resolve && resolve < fetch && fetch < offline,
            "generated workspace preparation must finish before the offline consumer:\n{journal}"
        );
    }

    #[test]
    fn production_split_fails_if_any_generated_workspace_fetch_call_is_removed() {
        for mutation in [
            FetchMutation::OmitPreparation,
            FetchMutation::OmitResolution,
            FetchMutation::OmitLockedFetch,
        ] {
            let (output, journal) = run_production_fixture(mutation);
            assert!(
                !output.status.success(),
                "{mutation:?} unexpectedly reached a successful offline consumer:\n{journal}"
            );
            assert!(
                !journal.lines().any(|line| line == "offline-consumer"),
                "{mutation:?} reached the offline consumer without complete generated-workspace preparation:\n{journal}"
            );
        }
    }

    #[test]
    fn production_split_fetches_agent_utils_in_its_launcher_config_scope() {
        let (output, journal) = run_production_fixture(FetchMutation::None);
        assert!(output.status.success(), "{output:?}\n{journal}");
        let root = journal_position(&journal, "committed-locked-fetch:Cargo.toml");
        let liteinst = journal_position(
            &journal,
            "committed-locked-fetch:liteinst-runtime-build/Cargo.toml",
        );
        let agent_utils = journal_position(&journal, "agent-utils-neutral-locked-fetch");
        let generated = journal_position(&journal, "generated-locked-fetch");
        let offline = journal_position(&journal, "offline-consumer");
        assert!(root < liteinst && liteinst < agent_utils && agent_utils < generated);
        assert!(generated < offline);
    }

    #[test]
    fn production_split_refuses_incomplete_or_misconfigured_agent_utils_fetch() {
        for mutation in [
            FetchMutation::OmitAgentUtils,
            FetchMutation::OmitAgentUtilsNeutralCwd,
            FetchMutation::OmitAgentUtilsLockedFetch,
        ] {
            let (output, journal) = run_production_fixture(mutation);
            assert!(
                !output.status.success(),
                "{mutation:?}: {output:?}\n{journal}"
            );
            assert!(
                !journal.lines().any(|line| line == "offline-consumer"),
                "{mutation:?} reached offline consumption: {journal}"
            );
        }
    }

    #[test]
    fn hermetic_dry_run_refuses_both_phase_only_flags() {
        let output = split_validate_dry_run(&["--fetch-only", "--offline-only"]);
        assert_eq!(output.status.code(), Some(2));
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("--fetch-only and --offline-only cannot be combined")
        );
    }

    #[test]
    fn stale_message_names_the_file_and_regenerate_command() {
        let message = stale_message("liteinst-runtime-build");
        assert!(message.contains("liteinst-runtime-build/Cargo.lock is STALE"));
        assert!(message.contains("cargo metadata --format-version 1"));
        assert!(message.contains("then commit liteinst-runtime-build/Cargo.lock"));
    }

    #[test]
    fn stale_message_uses_repo_relative_paths_only() {
        // Portability: no absolute/owner-specific paths in the guidance.
        let message = stale_message("liteinst-runtime-build");
        assert!(!message.contains("/home/"));
        assert!(!message.contains('\t'));
    }

    #[test]
    fn missing_manifest_is_a_hard_checker_error() {
        let root = env::temp_dir().join(format!(
            "check-nested-lockfiles-missing-{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&root);
        let result = probe_workspace(&root, "does-not-exist");
        let _ = std::fs::remove_dir_all(&root);
        assert!(
            result.is_err(),
            "a missing nested manifest must be a hard checker error, not a silent pass"
        );
    }

    #[test]
    fn help_states_the_checker_scope() {
        let help = usage();
        assert!(help.contains("nested Cargo workspace"));
        assert!(help.contains("--locked"));
    }
}
