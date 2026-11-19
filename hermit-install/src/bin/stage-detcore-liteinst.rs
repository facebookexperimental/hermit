//! Finalize the ordinary packaged Detcore preload after the outer Cargo build.
//! This executable must never be called by a Cargo build script.
//!
//! `--prebuilt <so>` stages an existing runtime file instead of the Cargo
//! output, but only after the unchanged fixed build and qualification have
//! run and the prebuilt bytes prove identical to the qualified artifact.
//! Any other bytes are refused: this receipt attests the fixed
//! `liteinst-runtime` build, and no second provenance can be recorded in it.
//!
//! `--buck` qualifies a supplied Buck `.so` directly against Buck-declared
//! sources and publishes the `builder=buck2` receipt arm that
//! `ci/verify-packaged-detcore.py` pins. This fbsource build compiles
//! `reverie-liteinst-artifact` from the sibling `//hermetic_infra/reverie`
//! target, which carries the Buck declared-content authority
//! (`resolve_buck` / `with_buck_build`), so `--buck` runs that sequence
//! here; the Hermit-pinned git build still refuses by name until a Reverie
//! pin bump carries the authority there. Like the Cargo arm runs cargo,
//! `--buck` runs the one pinned buck2 build itself, through the retained
//! command runner with `--show-full-output`, and `with_buck_build`
//! binds the runner's own last retained record of that run — there is
//! no command or digest parameter a caller could fill in: the ELF path
//! is parsed from the retained buck2 stdout and its digest computed
//! from the stdout-derived file inside `with_buck_build` (this process
//! computes the same digest again only for the receipt). A context that cannot run buck2 — for example a
//! Buck genrule action, which buck2 is already driving — cannot use this
//! mode; the genrule keeps the Cargo `--prebuilt` gate and its by-name
//! refusal instead. Still PLUMBING ONLY and
//! unqualified: the D1c-2/D1c-6 machine-code recipes refuse the real
//! Buck-built ELF by name inside qualification, and the fbcode profile
//! does not yet deliver its pinned rustc flags to the closure (see
//! `require_profile_flags_applied`), so no Buck receipt has ever been
//! emitted.
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use reverie_liteinst_artifact as proof;
use serde_json::json;

include!("../../../hermit-cli/reverie_pin.rs");
include!("../../../hermit-cli/reverie_gitlink.rs");

/// The one named Reverie repository this staging build's
/// compiled-source qualification expects (see the same-named
/// constant in liteinst-runtime-build).
const REVERIE_GIT_REPOSITORY: &str = "https://github.com/facebookexperimental/reverie.git";

/// This consumer's declared workspace facts for the compiled-source
/// factories: the Reverie tree's relative path, the leaf package, the
/// producer packages and their lockfile, and both workspace
/// inventories, spelled exactly as this tree's manifests name them.
fn consumer_source_declaration() -> proof::ConsumerSourceDeclaration<'static> {
    proof::ConsumerSourceDeclaration {
        reverie_relative: Path::new("reverie"),
        leaf: Some(proof::DeclaredLeafPackage {
            name: "detcore-liteinst",
            target: "detcore_liteinst",
            manifest_relative: "detcore-liteinst/Cargo.toml",
            version: "0.5.7",
        }),
        runtime_packages: &[
            proof::DeclaredRuntimePackage {
                name: "hermit-liteinst-runtime-build",
                manifest_relative: "liteinst-runtime-build/Cargo.toml",
                reverie_dependency: "reverie-liteinst-artifact",
                dependency_kind: Some("build"),
                target: None,
                version: None,
            },
            proof::DeclaredRuntimePackage {
                name: "hermit-liteinst-runtime-artifact",
                manifest_relative: "liteinst-runtime-build/runtime/Cargo.toml",
                reverie_dependency: "reverie-liteinst-preload",
                dependency_kind: None,
                target: Some("hermit_liteinst_runtime_artifact"),
                version: Some("0.5.1"),
            },
        ],
        runtime_lock_relative: "liteinst-runtime-build/Cargo.lock",
        standalone_workspace_files: &[
            "liteinst-runtime-build/Cargo.toml",
            "liteinst-runtime-build/Cargo.lock",
            "liteinst-runtime-build/runtime/Cargo.toml",
            "liteinst-runtime-build/runtime/src/lib.rs",
            "liteinst-runtime-build/build.rs",
            "liteinst-runtime-build/artifact.rs",
        ],
        leaf_workspace_files: &[
            "liteinst-runtime-build/Cargo.toml",
            "liteinst-runtime-build/Cargo.lock",
            "liteinst-runtime-build/runtime/Cargo.toml",
            "liteinst-runtime-build/runtime/src/lib.rs",
            "liteinst-runtime-build/build.rs",
            "liteinst-runtime-build/artifact.rs",
            "Cargo.toml",
            "Cargo.lock",
            "detcore-liteinst/Cargo.toml",
            "detcore-liteinst/src/lib.rs",
        ],
    }
}

fn reverie_repository() -> proof::private_static::CargoGitRepository<'static> {
    proof::private_static::CargoGitRepository::new(REVERIE_GIT_REPOSITORY)
        .expect("the named Reverie repository is a valid https URL")
}

/// Whether the Reverie root carries its own Git metadata: a `.git`
/// directory or file at the Reverie root itself, the same test the
/// content factory's pin proof uses for its canonical root. No
/// parent-directory discovery: a gitless copy nested in an unrelated
/// checkout must not inherit that checkout's metadata.
fn reverie_root_has_git_metadata(reverie_root: &Path) -> bool {
    reverie_root.join(".git").exists()
}

/// Whether the root manifest spells its Reverie dependencies by path
/// (`true`) or by git revision (`false`); both or neither refuses.
fn reverie_is_path_sourced(manifest_text: &str) -> proof::Result<bool> {
    let mut path_sourced: Option<bool> = None;
    for line in manifest_text.lines() {
        let line = line.trim_start();
        if line.starts_with('#') || !line.contains("reverie") {
            continue;
        }
        let vote = if line.contains("git =") {
            Some(false)
        } else if line.contains("path =") && line.contains("reverie/") {
            Some(true)
        } else {
            None
        };
        if let Some(vote) = vote {
            match path_sourced {
                Some(seen) if seen != vote => {
                    return Err("manifest mixes git and path Reverie dependencies".into());
                }
                Some(_) => {}
                None => path_sourced = Some(vote),
            }
        }
    }
    path_sourced.ok_or_else(|| "manifest names no Reverie git or path dependency".into())
}

/// The compiled-source layout for this workspace: the content-only
/// `PathCheckout` layout only for a gitless path-sourced Reverie root;
/// a root carrying its own Git metadata takes `GitlinkCheckout` even when
/// the manifests spell path dependencies, so the content factory's
/// pin proof (HEAD and gitlink equal to the pin) always runs for a
/// real checkout. This only selects the factory arm; the arm binds
/// its own shape exactly.
fn reverie_layout(
    manifest_text: &str,
    reverie_root: &Path,
) -> proof::Result<proof::CompiledSourceLayout> {
    Ok(
        if reverie_is_path_sourced(manifest_text)? && !reverie_root_has_git_metadata(reverie_root) {
            proof::CompiledSourceLayout::PathCheckout
        } else {
            proof::CompiledSourceLayout::GitlinkCheckout
        },
    )
}

fn require(ok: bool, message: &str) -> proof::Result<()> {
    if ok { Ok(()) } else { Err(message.into()) }
}

fn close<T>(result: proof::Result<T>, closing: proof::Result<()>) -> proof::Result<T> {
    match (result, closing) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(closing)) => {
            Err(format!("{error}; packaged source close failed: {closing}"))
        }
    }
}

/// One finalizer invocation: either a Cargo mode (`--git` / `--source-copy`,
/// optionally staging byte-identical `--prebuilt` output) or the `--buck`
/// mode for a supplied Buck `.so`, qualified directly against Buck-declared
/// sources. The modes never mix: the byte-identity gate belongs to
/// `--prebuilt` Cargo mode only, and Buck mode never runs a Cargo build.
#[derive(Debug, PartialEq)]
enum Mode {
    Cargo {
        authority: String,
        target: String,
        prebuilt: Option<PathBuf>,
    },
    Buck(Box<BuckArgs>),
}

/// Explicit Buck-mode inputs. Buck mode derives nothing from
/// `CARGO_MANIFEST_DIR` or any `Cargo.toml`: the roots, pins, profile,
/// target literals and declared source file lists are arguments, so the
/// fbsource import (which drops every manifest) can spell them all. The
/// artifact path and digest are not arguments: the finalizer runs the
/// pinned buck2 build itself in `--buck-cwd`, parses the ELF path from
/// the retained `--show-full-output` stdout, and computes the digest
/// from those bytes.
#[derive(Debug, PartialEq)]
struct BuckArgs {
    target: PathBuf,
    workspace_root: PathBuf,
    reverie_root: PathBuf,
    reverie_pin: String,
    imported_sha: String,
    profile_bzl: PathBuf,
    buck_target: String,
    buck_platform: String,
    buck_constraints: Vec<String>,
    buck_cwd: PathBuf,
    workspace_files: Vec<String>,
    reverie_files: Vec<String>,
}

/// Buck literals pinned with exact equality by the verifier's Buck arm in
/// `ci/verify-packaged-detcore.py`. The finalizer refuses any other spelling
/// rather than emitting a receipt its own reader would reject. The target
/// is the profiled `detcore-liteinst-liteinst-runtime` alias at its
/// `[cdylib]` output; the constraints, sorted, are the runtime target's
/// declared cpu/os compatibility plus that alias's opt build-mode
/// constraint (`fb-only/liteinst-runtime-profile.bzl`).
const BUCK_TARGET: &str = "//hermetic_infra/hermit:detcore-liteinst-liteinst-runtime[cdylib]";
const BUCK_PLATFORM: &str = "ovr_config//platform/linux:x86_64-fbcode";
const BUCK_CONSTRAINTS: [&str; 3] = [
    "ovr_config//build_mode/constraints:opt",
    "ovr_config//cpu:x86_64",
    "ovr_config//os:linux",
];
/// Workspace-relative entries `resolve_buck` requires in the pinned
/// workspace inventory beyond the CLI-declared sources: the declaring
/// `BUCK` file and the runtime profile, hashed from the pinned workspace
/// tree (never from the `--profile-bzl` argument).
const BUCK_WORKSPACE_BUCK_FILE: &str = "BUCK";
const BUCK_PROFILE_RELATIVE: &str = "fb-only/liteinst-runtime-profile.bzl";
/// The reviewed Buck `liteinst-runtime` flag set: release opt 3 with debug
/// assertions and overflow checks on, and no `-Clto` at all (matching the
/// Cargo profile's `lto = false`, which also passes no flag).
const BUCK_RUSTC_FLAGS: [&str; 8] = [
    "-Copt-level=3",
    "-Cdebuginfo=0",
    "-Cdebug-assertions=yes",
    "-Coverflow-checks=yes",
    "-Ccodegen-units=16",
    "-Cembed-bitcode=no",
    "-Cpanic=unwind",
    "-Cstrip=none",
];
/// The arguments after the program name: the mode, the target directory, and
/// the mode's own inputs.
fn parse_args(args: &[String]) -> proof::Result<Mode> {
    let mut args = args.iter();
    let authority = args
        .next()
        .ok_or("expected --git, --source-copy or --buck")?
        .clone();
    // The mode is judged before the target is consumed, so a misspelled
    // mode reports itself rather than a missing target.
    require(
        matches!(authority.as_str(), "--git" | "--source-copy" | "--buck"),
        "expected --git, --source-copy or --buck",
    )?;
    let target = args
        .next()
        .ok_or("expected existing target directory")?
        .clone();
    match authority.as_str() {
        "--git" | "--source-copy" => {
            let prebuilt = match args.next().map(String::as_str) {
                None => None,
                Some("--prebuilt") => Some(PathBuf::from(
                    args.next()
                        .ok_or("expected prebuilt runtime path after --prebuilt")?,
                )),
                Some(_) => return Err("unexpected finalizer arguments".into()),
            };
            require(args.next().is_none(), "unexpected finalizer arguments")?;
            Ok(Mode::Cargo {
                authority,
                target,
                prebuilt,
            })
        }
        "--buck" => parse_buck_args(PathBuf::from(target), &mut args),
        _ => Err("expected --git, --source-copy or --buck".into()),
    }
}

fn parse_buck_args(
    target: PathBuf,
    args: &mut std::slice::Iter<'_, String>,
) -> proof::Result<Mode> {
    let mut workspace_root = None;
    let mut reverie_root = None;
    let mut reverie_pin = None;
    let mut imported_sha = None;
    let mut profile_bzl = None;
    let mut buck_target = None;
    let mut buck_platform = None;
    let mut buck_constraints = Vec::new();
    let mut buck_cwd = None;
    let mut workspace_files = Vec::new();
    let mut reverie_files = Vec::new();
    // Every singleton may be spelled once; the list flags repeat. There
    // is no artifact or command argument: this process runs the pinned
    // buck2 build itself and selects the ELF from its retained stdout.
    fn set_once(slot: &mut Option<String>, value: String) -> proof::Result<()> {
        require(slot.is_none(), "duplicate --buck argument")?;
        *slot = Some(value);
        Ok(())
    }
    while let Some(flag) = args.next() {
        let mut value = || {
            args.next()
                .cloned()
                .ok_or_else(|| format!("expected value after {flag}"))
        };
        match flag.as_str() {
            "--workspace-root" => set_once(&mut workspace_root, value()?)?,
            "--reverie-root" => set_once(&mut reverie_root, value()?)?,
            "--reverie-pin" => set_once(&mut reverie_pin, value()?)?,
            "--imported-sha" => set_once(&mut imported_sha, value()?)?,
            "--profile-bzl" => set_once(&mut profile_bzl, value()?)?,
            "--buck-target" => set_once(&mut buck_target, value()?)?,
            "--buck-platform" => set_once(&mut buck_platform, value()?)?,
            "--buck-constraint" => buck_constraints.push(value()?),
            "--buck-cwd" => set_once(&mut buck_cwd, value()?)?,
            "--workspace-file" => workspace_files.push(value()?),
            "--reverie-file" => reverie_files.push(value()?),
            _ => return Err("unexpected --buck argument".into()),
        }
    }
    let required =
        |slot: Option<String>, flag: &str| slot.ok_or_else(|| format!("--buck requires {flag}"));
    Ok(Mode::Buck(Box::new(BuckArgs {
        target,
        workspace_root: PathBuf::from(required(workspace_root, "--workspace-root")?),
        reverie_root: PathBuf::from(required(reverie_root, "--reverie-root")?),
        reverie_pin: required(reverie_pin, "--reverie-pin")?,
        imported_sha: required(imported_sha, "--imported-sha")?,
        profile_bzl: PathBuf::from(required(profile_bzl, "--profile-bzl")?),
        buck_target: required(buck_target, "--buck-target")?,
        buck_platform: required(buck_platform, "--buck-platform")?,
        buck_constraints,
        buck_cwd: PathBuf::from(required(buck_cwd, "--buck-cwd")?),
        workspace_files,
        reverie_files,
    })))
}

fn is_full_sha(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Argument checks that need no source authority and no filesystem: the pins
/// are full lowercase-hex SHAs, the Buck literals are exactly the verifier's,
/// the profile input names the reviewed `.bzl`, and the declared file lists
/// are nonempty logical relative paths naming at least the leaf and the
/// qualifier sources the closed factory will hash.
fn validate_buck_args(args: &BuckArgs) -> proof::Result<()> {
    require(
        is_full_sha(&args.reverie_pin),
        "--buck requires a full 40-hex --reverie-pin",
    )?;
    require(
        is_full_sha(&args.imported_sha),
        "--buck requires a full 40-hex --imported-sha",
    )?;
    require(
        args.buck_target == BUCK_TARGET,
        "--buck-target is not the pinned Buck runtime target",
    )?;
    require(
        args.buck_platform == BUCK_PLATFORM,
        "--buck-platform is not the pinned Buck runtime platform",
    )?;
    require(
        args.buck_constraints == BUCK_CONSTRAINTS,
        "--buck-constraint values are not the pinned runtime constraints",
    )?;
    require(
        args.profile_bzl
            .file_name()
            .is_some_and(|name| name == "liteinst-runtime-profile.bzl"),
        "--profile-bzl is not the liteinst-runtime profile .bzl",
    )?;
    for files in [&args.workspace_files, &args.reverie_files] {
        require(!files.is_empty(), "--buck requires declared source files")?;
        for file in files {
            let path = Path::new(file);
            require(
                !file.is_empty()
                    && !path.is_absolute()
                    && path
                        .components()
                        .all(|component| matches!(component, std::path::Component::Normal(_))),
                "--buck declared files must be logical relative paths",
            )?;
        }
    }
    require(
        args.workspace_files
            .iter()
            .any(|file| file == "detcore-liteinst/src/lib.rs"),
        "--buck workspace files must declare the detcore-liteinst leaf source",
    )?;
    require(
        args.reverie_files
            .iter()
            .any(|file| file == "reverie-liteinst-artifact/src/lib.rs"),
        "--buck reverie files must declare the qualifier source",
    )?;
    Ok(())
}

/// The `LITEINST_RUNTIME_RUSTC_FLAGS` string list a profile `.bzl` actually
/// defines: the quoted literals of its first `[...]` list after that name.
/// Anything else — no such definition, or a list this cannot read — refuses
/// by name rather than trusting the file's name alone.
fn profile_bzl_flags(contents: &str) -> proof::Result<Vec<String>> {
    let missing = "--profile-bzl does not define LITEINST_RUNTIME_RUSTC_FLAGS";
    let after_name = contents
        .split_once("LITEINST_RUNTIME_RUSTC_FLAGS")
        .ok_or(missing)?
        .1;
    let list = after_name
        .split_once('[')
        .ok_or(missing)?
        .1
        .split_once(']')
        .ok_or(missing)?
        .0;
    let mut flags = Vec::new();
    let mut rest = list;
    while let Some((_, after_quote)) = rest.split_once('"') {
        let (flag, tail) = after_quote.split_once('"').ok_or(missing)?;
        flags.push(flag.to_owned());
        rest = tail;
    }
    Ok(flags)
}

/// The supplied profile must define exactly the pinned flag list, in order.
fn validate_profile_bzl(contents: &str) -> proof::Result<()> {
    let flags = profile_bzl_flags(contents)?;
    require(
        flags.iter().map(String::as_str).collect::<Vec<_>>() == BUCK_RUSTC_FLAGS,
        "--profile-bzl LITEINST_RUNTIME_RUSTC_FLAGS do not match the pinned \
         liteinst-runtime flags",
    )
}

/// The one buck2 invocation `--buck` runs and the receipt may attest:
/// the pinned build subcommand, target and platform, with
/// `--show-full-output` so the ELF is selected from the retained stdout
/// (`parse_buck_output`) rather than named by any caller. This list is a
/// validation pin for the retained runner record, never a synthesized
/// witness: the record bound into qualification is the runner's own.
fn buck_argv() -> Vec<String> {
    vec![
        "buck2".into(),
        "build".into(),
        BUCK_TARGET.into(),
        "--target-platforms".into(),
        BUCK_PLATFORM.into(),
        "--show-full-output".into(),
        "--client-metadata".into(),
        "id=stage-detcore-liteinst".into(),
    ]
}

/// The single ELF path a retained `buck2 build --show-full-output` stdout
/// names for the pinned target: exactly one nonempty line, of the form
/// `<target> <absolute output path>`, where the target is the pinned
/// literal (buck2 may print it cell-qualified, `fbcode//...`). Anything
/// else — no output, several outputs, another target, a relative path —
/// refuses by name; the ELF is never taken from a caller argument.
fn parse_buck_output(stdout: &str) -> proof::Result<PathBuf> {
    let refused = "retained buck2 --show-full-output stdout does not name exactly \
                   one absolute output for the pinned Buck runtime target";
    let mut lines = stdout.lines().filter(|line| !line.trim().is_empty());
    let line = lines.next().ok_or(refused)?;
    require(lines.next().is_none(), refused)?;
    let (target, path) = line.split_once(' ').ok_or(refused)?;
    require(
        target == BUCK_TARGET || target == format!("fbcode{BUCK_TARGET}"),
        refused,
    )?;
    let path = PathBuf::from(path.trim());
    require(path.is_absolute(), refused)?;
    Ok(path)
}

/// A receipt may list `LITEINST_RUNTIME_RUSTC_FLAGS` only when the
/// supplied profile actually delivers them to the transitive closure.
/// The profile `.bzl` states this as `LITEINST_RUNTIME_FLAGS_APPLIED`:
/// the fbcode profile sets it `False` — `fbcode_configured_alias`
/// transitions platform and constraints only, it has no rustc-flags
/// parameter, and no fbcode build-mode constraint encodes this flag
/// list (the Reverie authority also pins exactly the alias's three
/// constraints, so none can be added here) — while the OSS shim profile,
/// whose toolchain select applies the list, sets it `True`. Checked at
/// receipt publication, where the flags would be listed: anything but
/// an explicit `True` refuses by name and no receipt is emitted.
fn require_profile_flags_applied(contents: &str) -> proof::Result<()> {
    require(
        contents
            .lines()
            .any(|line| line.trim() == "LITEINST_RUNTIME_FLAGS_APPLIED = True"),
        "--profile-bzl does not apply LITEINST_RUNTIME_RUSTC_FLAGS to the Buck \
         closure (LITEINST_RUNTIME_FLAGS_APPLIED is not True); refusing to \
         emit a receipt listing unapplied flags",
    )
}

/// `DT_FLAGS` bits this finalizer names before qualification, so a
/// loader-contract refusal says which flag failed instead of only that
/// the flags were unsupported. `DF_TEXTREL` (0x4) with `DF_BIND_NOW`
/// (0x8) is `0xc`, the real fbcode-built ELF's value.
const DF_TEXTREL: u64 = 0x4;
const DF_BIND_NOW: u64 = 0x8;

/// The refusal for a `DT_FLAGS` value, if any: TEXTREL names itself.
/// This only ever refuses earlier and more specifically; the shared
/// qualifier's loader contract still decides admission.
fn dt_flags_refusal(flags: u64) -> Option<String> {
    if flags & DF_TEXTREL != 0 {
        Some(format!(
            "canonical loader contract refuses DT_FLAGS {flags:#x}: \
             TEXTREL (DF_TEXTREL) is set; the Buck-built ELF carries text \
             relocations and only eager binding (DF_BIND_NOW) is supported"
        ))
    } else {
        None
    }
}

/// `DT_FLAGS` from a `readelf -d` dump: the `(FLAGS)` entry's symbolic
/// names mapped back to their bits. No `(FLAGS)` entry is zero flags; a
/// names list this cannot read still yields the bits it does name, so a
/// TEXTREL spelling is never lost.
fn dt_flags_from_readelf(dump: &str) -> u64 {
    let mut flags = 0;
    for line in dump.lines() {
        if !line.contains("(FLAGS)") {
            continue;
        }
        for word in line.split_whitespace() {
            match word {
                "TEXTREL" => flags |= DF_TEXTREL,
                "BIND_NOW" => flags |= DF_BIND_NOW,
                _ => {}
            }
        }
    }
    flags
}

/// `DT_NEEDED` sonames the loader contract never admits on this runtime:
/// the cxx-driver and sysroot baggage the Rust closure does not
/// reference (libstdc++, libatomic, libmvec) and, since the `-u`
/// extraction from static libgcc, libgcc_s. `libm`/`libc`/`ld-linux`
/// are genuinely referenced and stay.
const FORBIDDEN_NEEDED: [&str; 4] = [
    "libgcc_s.so.1",
    "libstdc++.so.6",
    "libatomic.so.1",
    "libmvec.so.1",
];

/// The refusal for a `DT_NEEDED` list, if any: each forbidden soname
/// names itself. Like `dt_flags_refusal`, this only ever refuses
/// earlier and more specifically; the shared qualifier's loader
/// contract still decides admission.
fn needed_refusal(needed: &[String]) -> Option<String> {
    let present: Vec<&str> = FORBIDDEN_NEEDED
        .into_iter()
        .filter(|soname| needed.iter().any(|entry| entry == soname))
        .collect();
    if present.is_empty() {
        None
    } else {
        Some(format!(
            "canonical loader contract refuses DT_NEEDED {}: this runtime \
             admits no libgcc_s/libstdc++/libatomic/libmvec dependency",
            present.join(", ")
        ))
    }
}

/// The `DT_NEEDED` sonames in a `readelf -d` dump, in dump order: the
/// `[soname]` of each `(NEEDED)` entry. An entry this cannot read
/// yields no soname for that line, never a fabricated one.
fn needed_from_readelf(dump: &str) -> Vec<String> {
    dump.lines()
        .filter(|line| line.contains("(NEEDED)"))
        .filter_map(|line| {
            line.split_once('[')?
                .1
                .split_once(']')
                .map(|(s, _)| s.to_owned())
        })
        .collect()
}

fn buck_profile_objects() -> serde_json::Value {
    json!({"liteinst-runtime": {"rustc_flags": BUCK_RUSTC_FLAGS}})
}

/// The `buck_artifact` mirror of the Cargo receipt's artifact record: the
/// ordinary (empty) feature mode, the exact compiler profile the Buck flag
/// set produces, the synthesized leaf package, and the Buck output path the
/// qualification proof must name. There is no `fresh`: Buck has no such
/// notion to report honestly.
fn buck_artifact(package: serde_json::Value, reported_path: &Path) -> serde_json::Value {
    json!({
        "features": [],
        "profile": {"opt_level": "3", "debuginfo": 0, "debug_assertions": true,
                    "overflow_checks": true, "test": false},
        "package": package,
        "reported_path": reported_path.to_string_lossy(),
    })
}

/// The produced sections of the `builder=buck2` receipt: the qualification
/// evidence plus the publication outcome (the installed identity and the
/// executed `commands` — the `buck2` producer run and the qualifier's
/// readelf/objdump runs, which the verifier binds exactly).
struct BuckReceiptSections {
    profile_inputs: serde_json::Value,
    artifact: serde_json::Value,
    qualifier_source: serde_json::Value,
    compiled_sources: serde_json::Value,
    qualification: serde_json::Value,
    installed: serde_json::Value,
    commands: serde_json::Value,
}

/// The complete `builder=buck2` receipt body, including the installed
/// identity, the executed commands and the source-close outcome. It
/// carries no `cargo_*` key and synthesizes no Cargo value;
/// `profile_inputs` is the profile `.bzl` identity, which is also the Buck
/// compiled-sources manifest slot, and `rustc_version` is the ELF
/// `.comment` identity read at qualification time. `buck_argv` is the
/// retained runner record's program plus its arguments, and
/// `buck_output_sha256` the digest this process computed from the ELF
/// the retained stdout named; the verifier binds both to the installed
/// bytes.
fn buck_receipt(
    args: &BuckArgs,
    sections: BuckReceiptSections,
    retained_argv: &[String],
    output_sha256: &str,
    rustc_version: &str,
    source_close_completed: bool,
) -> serde_json::Value {
    json!({
        "schema": "hermit-packaged-detcore/v1",
        "builder": "buck2",
        "reverie_pin": args.reverie_pin,
        "profile_name": "liteinst-runtime",
        "profile_inputs": sections.profile_inputs,
        "profile_objects": buck_profile_objects(),
        "buck_target": BUCK_TARGET,
        "buck_platform": BUCK_PLATFORM,
        "buck_constraints": BUCK_CONSTRAINTS,
        "buck_argv": retained_argv,
        "buck_output_sha256": output_sha256,
        "buck_artifact": sections.artifact,
        "rustc_version": rustc_version,
        "qualifier_source": sections.qualifier_source,
        "compiled_qualifier_sha256": proof::qualifier_source_sha256(),
        "compiled_sources": sections.compiled_sources,
        "qualification": sections.qualification,
        "installed": sections.installed,
        "commands": sections.commands,
        "source_close_completed": source_close_completed,
    })
}

/// The single `rustc version ...` identity in an ELF `.comment` dump
/// (`readelf -p .comment`): the compiler that produced the Buck artifact,
/// read from its bytes rather than from this host's `rustc`. Zero or
/// several distinct identities refuse by name.
fn buck_rustc_version(comment_dump: &str) -> proof::Result<String> {
    let mut versions: Vec<&str> = comment_dump
        .lines()
        .filter_map(|line| line.split("rustc version ").nth(1))
        .map(str::trim)
        .collect();
    versions.sort_unstable();
    versions.dedup();
    match versions.as_slice() {
        [only] => Ok(format!("rustc version {only}")),
        _ => Err("Buck artifact .comment does not carry exactly one rustc version identity".into()),
    }
}

/// Pinned expectation digests for `resolve_buck`: the CLI
/// `--workspace-file` list, with `BUCK` and
/// `fb-only/liteinst-runtime-profile.bzl` appended if absent (dedup,
/// preserving order), each hashed from the pinned workspace tree. The
/// profile entry is hashed from `workspace_root`, never from the
/// `--profile-bzl` argument, so `resolve_buck` can tie that argument's
/// identity to this inventory entry and refuse a mismatch there.
fn pinned_workspace_files(
    workspace_root: &Path,
    declared: &[String],
) -> proof::Result<Vec<(String, String)>> {
    let mut relatives: Vec<String> = Vec::new();
    for relative in declared
        .iter()
        .map(String::as_str)
        .chain([BUCK_WORKSPACE_BUCK_FILE, BUCK_PROFILE_RELATIVE])
    {
        if !relatives.iter().any(|existing| existing == relative) {
            relatives.push(relative.to_owned());
        }
    }
    relatives
        .iter()
        .map(|relative| {
            Ok((
                relative.clone(),
                proof::FileIdentity::read(&workspace_root.join(relative))?.sha256,
            ))
        })
        .collect()
}

/// Buck mode: run the pinned buck2 build and qualify its ELF directly
/// against Buck-declared sources — no Cargo build and no byte-identity
/// gate. The buck2 invocation runs through the retained-command runner
/// with `--show-full-output`; `with_buck_build(&runner, elf)` binds the
/// runner's own last retained record of that run and computes the ELF
/// digest from the stdout-derived file itself. No caller-filled
/// command, artifact path or hash reaches qualification. Then
/// `qualify_detcore_constructor_with_sources`, and stage + publish
/// [`buck_receipt`] exactly as the Cargo arm publishes its receipt. The
/// sibling fbcode `reverie-liteinst-artifact` this binary compiles
/// against carries that authority; the D1c-2/D1c-6 machine-code recipes
/// still refuse the real Buck-built ELF by name inside qualification,
/// before anything is staged.
fn run_buck(args: &BuckArgs) -> proof::Result<()> {
    validate_buck_args(args)?;
    // resolve_buck requires already-canonical roots; canonicalize here so
    // every later identity (artifact, installed copy, receipt paths)
    // shares the one canonical spelling.
    let target = fs::canonicalize(&args.target).map_err(|e| e.to_string())?;
    let workspace_root = fs::canonicalize(&args.workspace_root).map_err(|e| e.to_string())?;
    let reverie_root = fs::canonicalize(&args.reverie_root).map_err(|e| e.to_string())?;
    let buck_cwd = fs::canonicalize(&args.buck_cwd).map_err(|e| e.to_string())?;
    for path in [&target, &workspace_root, &reverie_root, &buck_cwd] {
        require(path.is_dir(), "Buck input is not a directory")?;
    }
    require(
        args.profile_bzl.is_file() && !args.profile_bzl.is_symlink(),
        "Buck input must be a regular non-symlink file",
    )?;
    // The profile is judged by its contents, not its filename: only the
    // exact pinned flag list may proceed to source resolution. The same
    // contents are re-judged at publication for whether those flags are
    // actually applied, before any receipt may list them.
    let profile_contents = fs::read_to_string(&args.profile_bzl).map_err(|e| e.to_string())?;
    validate_profile_bzl(&profile_contents)?;
    let resources = target.join("install_pkg/rsrcs");
    require(
        resources.is_dir(),
        "Buck target directory lacks install_pkg/rsrcs; create the staging \
         layout before finalization",
    )?;
    let installed = resources.join("libdetcore_liteinst.so");
    let receipt = resources.join("libdetcore_liteinst.package.json");
    // A failed finalization must not advertise a previous successful preload.
    for file in [&installed, &receipt] {
        match fs::remove_file(file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    let serial = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let evidence = target.join(format!(
        "ci/packaged-detcore/{}-{serial}",
        std::process::id()
    ));
    fs::create_dir_all(&evidence).map_err(|e| e.to_string())?;
    let mut runner = proof::Runner::new(evidence.join("logs"), Duration::from_secs(900), 64 << 20)?;
    // Run the one pinned buck2 build through the runner, mirroring the
    // Cargo arm's retained cargo run. `with_buck_build` below reads the
    // runner's last retained record itself, so no runner call may
    // intervene between this run and that bind; the clone captured here
    // is only for this function's own pin check and the receipt argv.
    let buck_build_args: Vec<String> = buck_argv()[1..].to_vec();
    let buck_stdout = runner.run("buck2", &buck_build_args, &buck_cwd)?;
    let build_command = runner
        .commands()
        .last()
        .ok_or("Buck runtime build command was not retained")?
        .clone();
    require(
        build_command.status() == 0
            && Path::new(build_command.program())
                .file_name()
                .is_some_and(|name| name == "buck2")
            && build_command.arguments() == buck_build_args.as_slice(),
        "retained Buck runtime build command is not the pinned successful buck2 build",
    )?;
    // The ELF and its digest come from that retained run alone: the path
    // from its --show-full-output stdout (canonicalized through buck-out's
    // symlink to the real file) and the sha256 computed here from it.
    let artifact_path =
        parse_buck_output(std::str::from_utf8(&buck_stdout).map_err(|e| e.to_string())?)?;
    let artifact_path = fs::canonicalize(&artifact_path).map_err(|e| e.to_string())?;
    require(
        artifact_path.is_file(),
        "retained buck2 output is not a regular file",
    )?;
    let artifact = proof::FileIdentity::read(&artifact_path)?;
    let output_sha256 = artifact.sha256.clone();
    let retained_argv: Vec<String> = std::iter::once(build_command.program().to_owned())
        .chain(build_command.arguments().iter().cloned())
        .collect();
    let pinned_workspace_files = pinned_workspace_files(&workspace_root, &args.workspace_files)?;
    let workspace_files: Vec<proof::ExpectedSourceFile<'_>> = pinned_workspace_files
        .iter()
        .map(|(relative, sha256)| proof::ExpectedSourceFile {
            relative: relative.as_str(),
            sha256: sha256.as_str(),
        })
        .collect();
    let reverie_files: Vec<&str> = args.reverie_files.iter().map(String::as_str).collect();
    let constraints: Vec<&str> = args.buck_constraints.iter().map(String::as_str).collect();
    let expectation = proof::BuckCompiledSourceExpectation {
        workspace_root: &workspace_root,
        reverie_root: &reverie_root,
        reverie_pin: &args.reverie_pin,
        imported_sha: &args.imported_sha,
        profile_bzl: &args.profile_bzl,
        target: BUCK_TARGET,
        platform: BUCK_PLATFORM,
        constraints: &constraints,
        workspace_files: &workspace_files,
        reverie_files: &reverie_files,
        leaf_name: "detcore-liteinst",
        leaf_target: "detcore_liteinst",
        leaf_source_relative: "detcore-liteinst/src/lib.rs",
        workspace_extra_files: &[
            "detcore-liteinst/build.rs",
            "common/inguest/glibc_compat.rs",
            BUCK_WORKSPACE_BUCK_FILE,
            BUCK_PROFILE_RELATIVE,
        ],
        profile_relative: BUCK_PROFILE_RELATIVE,
        stage_id: "stage-detcore-liteinst",
    };
    let sources = proof::VerifiedCompiledSources::resolve_buck(&expectation)?;
    let sources = sources.with_buck_build(&runner, &artifact.path)?;
    // Name a TEXTREL loader refusal before qualification's generic one:
    // DT_FLAGS 0xc (TEXTREL|BIND_NOW) is the real Buck ELF's first
    // blocker, and which flag failed is the actionable fact.
    let dynamic = runner.text(
        "readelf",
        &[
            "-d",
            artifact
                .path
                .to_str()
                .ok_or("Buck artifact path is not UTF-8")?,
        ],
        &target,
    )?;
    if let Some(refusal) = dt_flags_refusal(dt_flags_from_readelf(&dynamic)) {
        return Err(refusal);
    }
    // Same dump, same reasoning: name a forbidden DT_NEEDED before
    // qualification's generic loader refusal.
    if let Some(refusal) = needed_refusal(&needed_from_readelf(&dynamic)) {
        return Err(refusal);
    }
    let qualification =
        proof::qualify_detcore_constructor_with_sources(&artifact.path, &mut runner, &sources)?;
    let comment = runner.text(
        "readelf",
        &[
            "-p",
            ".comment",
            artifact
                .path
                .to_str()
                .ok_or("Buck artifact path is not UTF-8")?,
        ],
        &target,
    )?;
    let rustc_version = buck_rustc_version(&comment)?;
    let record = serde_json::to_value(sources.record()).map_err(|e| e.to_string())?;
    let qualifier_source =
        proof::FileIdentity::read(&reverie_root.join("reverie-liteinst-artifact/src/lib.rs"))?;
    require(
        qualifier_source.sha256 == proof::qualifier_source_sha256(),
        "compiled qualifier differs from Buck-declared proof source",
    )?;
    let package = sources.package();
    require(
        package.name == "detcore-liteinst"
            && package.target == "detcore_liteinst"
            && package.source.is_none()
            && package.kind == ["cdylib"]
            && package.crate_types == ["cdylib"],
        "Buck declared sources selected a different Detcore runtime",
    )?;
    let package = serde_json::to_value(package).map_err(|e| e.to_string())?;
    qualification.artifact.verify()?;
    artifact.verify()?;
    let pending = resources.join(format!(".libdetcore_liteinst.so.{serial}"));
    let staged = (|| {
        proof::write_new(
            &pending,
            &fs::read(&artifact.path).map_err(|e| e.to_string())?,
        )?;
        fs::set_permissions(
            &pending,
            fs::metadata(&artifact.path)
                .map_err(|e| e.to_string())?
                .permissions(),
        )
        .map_err(|e| e.to_string())?;
        let staged = proof::FileIdentity::read(&pending)?;
        require(
            staged.sha256 == qualification.artifact.sha256,
            "installed copy differs from qualified runtime",
        )?;
        qualification.artifact.verify()?;
        Ok(())
    })();
    if let Err(error) = close(staged, sources.verify_unchanged(&mut runner)) {
        let _ = fs::remove_file(&pending);
        return Err(error);
    }
    fs::rename(&pending, &installed).map_err(|e| e.to_string())?;
    let publication = (|| {
        let installed_identity = proof::FileIdentity::read(&installed)?;
        require(
            qualification.artifact.sha256 == installed_identity.sha256,
            "installed bytes changed before receipt publication",
        )?;
        sources.verify_unchanged(&mut runner)?;
        // The receipt may list the profile's rustc flags only if the
        // profile actually applied them; checked here, at the point the
        // receipt would be written, so unapplied flags emit nothing.
        require_profile_flags_applied(&profile_contents)?;
        // Every commands entry is this process's retained runner record:
        // the buck2 build first (its stdout identity names the staged
        // artifact), then the readelf/objdump qualification runs. No
        // producer entry is synthesized.
        let profile_inputs = record["manifest"].clone();
        let record = buck_receipt(
            args,
            BuckReceiptSections {
                profile_inputs,
                artifact: buck_artifact(package, &artifact.path),
                qualifier_source: serde_json::to_value(&qualifier_source)
                    .map_err(|e| e.to_string())?,
                compiled_sources: record,
                qualification: serde_json::to_value(&qualification).map_err(|e| e.to_string())?,
                installed: serde_json::to_value(installed_identity).map_err(|e| e.to_string())?,
                commands: serde_json::to_value(runner.commands()).map_err(|e| e.to_string())?,
            },
            &retained_argv,
            &output_sha256,
            &rustc_version,
            true,
        );
        proof::write_new(
            &receipt,
            &serde_json::to_vec_pretty(&record).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    })();
    if let Err(error) = close(publication, sources.verify_unchanged(&mut runner)) {
        let _ = fs::remove_file(&installed);
        let _ = fs::remove_file(&receipt);
        return Err(error);
    }
    println!("qualified packaged Detcore: {}", installed.display());
    Ok(())
}

fn run() -> proof::Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let (authority, target, prebuilt) = match parse_args(&argv)? {
        Mode::Cargo {
            authority,
            target,
            prebuilt,
        } => (authority, target, prebuilt),
        Mode::Buck(args) => return run_buck(&args),
    };
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("Hermit root missing")?;
    let root = fs::canonicalize(root).map_err(|e| e.to_string())?;
    let target = fs::canonicalize(target).map_err(|e| e.to_string())?;
    require(target.is_dir(), "target directory does not exist")?;
    let resources = target.join("install_pkg/rsrcs");
    require(
        resources.is_dir(),
        "run the outer hermit-install build before finalization",
    )?;
    let installed = resources.join("libdetcore_liteinst.so");
    let receipt = resources.join("libdetcore_liteinst.package.json");
    // A failed finalization must not advertise a previous successful preload.
    for file in [&installed, &receipt] {
        match fs::remove_file(file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    let serial = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let evidence = target.join(format!(
        "ci/packaged-detcore/{}-{serial}",
        std::process::id()
    ));
    fs::create_dir_all(&evidence).map_err(|e| e.to_string())?;
    let mut runner = proof::Runner::new(evidence.join("logs"), Duration::from_secs(900), 64 << 20)?;
    let manifest = root.join("Cargo.toml");
    // The pin is a provenance label: submodule checkout HEAD, the Eden
    // marker, or the recorded gitlink (see reverie_pin_at). Content
    // identity rides in the resolved compiled-source record.
    let pin = reverie_pin_at(&root).ok_or(
        "no Reverie pin label: no submodule checkout, reverie.submodule.txt marker, or gitlink names one full revision",
    )?;
    let manifest_text = fs::read_to_string(&manifest).map_err(|e| e.to_string())?;
    // `--git` is judged on the manifest spelling: the Git factory
    // binds the exact Git source string and refuses path-form
    // packages, whatever layout the content route below selects.
    require(
        authority != "--git" || !reverie_is_path_sourced(&manifest_text)?,
        "--git requires git-sourced Reverie manifests; this workspace is path-sourced, use --source-copy",
    )?;
    let layout = reverie_layout(&manifest_text, &root.join("reverie"))?;
    let expectation = proof::CompiledSourceExpectation {
        layout,
        image_kind: proof::FixedImageKind::Detcore,
        workspace_root: &root,
        manifest: &manifest,
        reverie_pin: &pin,
        consumer: Some(consumer_source_declaration()),
    };
    // Explicit authority; a failed Git resolution never downgrades to content.
    let sources = if authority == "--git" {
        proof::VerifiedCompiledSources::resolve_with_repository(
            &expectation,
            reverie_repository(),
            &mut runner,
        )?
    } else {
        proof::VerifiedCompiledSources::resolve_content_with_repository(
            &expectation,
            reverie_repository(),
            &mut runner,
        )?
    };
    let pending = resources.join(format!(".libdetcore_liteinst.so.{serial}"));
    let result = (|| {
        let profile_inputs = proof::FileIdentity::read(&manifest)?;
        let profile_document: toml::Value = fs::read_to_string(&manifest)
            .map_err(|e| e.to_string())?
            .parse::<toml::Value>()
            .map_err(|e| e.to_string())?;
        let profile = serde_json::to_value(
            profile_document
                .get("profile")
                .ok_or("Cargo profiles missing")?,
        )
        .map_err(|e| e.to_string())?;
        require(
            profile["liteinst-runtime"]["inherits"].as_str() == Some("validate")
                && profile["liteinst-runtime"]["lto"].as_bool() == Some(false)
                && profile["validate"]["inherits"].as_str() == Some("release")
                && profile["validate"]["debug-assertions"].as_bool() == Some(true)
                && profile["validate"]["overflow-checks"].as_bool() == Some(true),
            "ordinary runtime must use the reviewed liteinst-runtime profile",
        )?;
        let source_record = serde_json::to_value(sources.record()).map_err(|e| e.to_string())?;
        let reverie_root = source_record["reverie_root"]
            .as_str()
            .ok_or("resolved Reverie compiler root missing")?;
        let qualifier_source = proof::FileIdentity::read(
            &Path::new(reverie_root).join("reverie-liteinst-artifact/src/lib.rs"),
        )?;
        require(
            qualifier_source.sha256 == proof::qualifier_source_sha256(),
            "compiled qualifier differs from Cargo-resolved proof source",
        )?;
        let package = sources.package();
        require(
            package.name == "detcore-liteinst"
                && package.target == "detcore_liteinst"
                && package.source.is_none()
                && package.kind == ["cdylib"]
                && package.crate_types == ["cdylib"],
            "fresh metadata selected a different Detcore runtime",
        )?;
        let cargo_args = vec![
            "build".into(),
            "--locked".into(),
            "--offline".into(),
            "--manifest-path".into(),
            manifest.to_string_lossy().into_owned(),
            "-p".into(),
            "detcore-liteinst".into(),
            "--lib".into(),
            "--no-default-features".into(),
            "--profile".into(),
            "liteinst-runtime".into(),
            "--target-dir".into(),
            target.to_string_lossy().into_owned(),
            "--message-format=json-render-diagnostics".into(),
        ];
        let cargo_version = runner.text("cargo", &["--version", "--verbose"], &root)?;
        let rustc_version = runner.text("rustc", &["--version", "--verbose"], &root)?;
        let output = runner.run("cargo", &cargo_args, &root)?;
        let expected_profile = json!({"opt_level":"3", "debuginfo":0,
            "debug_assertions":true,"overflow_checks":true,"test":false});
        let artifact = proof::select_artifact(
            std::str::from_utf8(&output).map_err(|e| e.to_string())?,
            package,
            &[],
            &expected_profile,
            &target,
        )?;
        require(
            artifact.reported_path == target.join("liteinst-runtime/libdetcore_liteinst.so"),
            "Cargo reported a different ordinary runtime output",
        )?;
        // The ordinary staged runtime is the liteinst-off image; bind the
        // actual current build as its no-scope qualification witness. The
        // retained command is captured immediately after the runtime build,
        // before any later retained command can replace it. This mirrors
        // Hermit 4704's stage-detcore-liteinst rebinding.
        let build_command = runner
            .commands()
            .last()
            .ok_or("packaged runtime build command was not retained")?
            .clone();
        let sources =
            sources.with_current_build(&build_command, &artifact.reported_path, &target)?;
        let qualification = proof::qualify_detcore_constructor_with_sources(
            &artifact.reported_path,
            &mut runner,
            &sources,
        )?;
        qualifier_source.verify()?;
        profile_inputs.verify()?;
        qualification.artifact.verify()?;
        // A prebuilt runtime is staged in place of the Cargo output only
        // when its entire digest is the qualified one. The receipt below
        // attests the fixed build that just ran, so different bytes — for
        // example another builder's profile — are refused here by name
        // rather than recorded under that build's provenance.
        let staged_from = prebuilt.as_deref().unwrap_or(&artifact.reported_path);
        let prebuilt_identity = match &prebuilt {
            Some(path) => {
                let identity = proof::FileIdentity::read(path)?;
                require(
                    identity.sha256 == qualification.artifact.sha256,
                    "prebuilt runtime differs from the qualified fixed liteinst-runtime \
                     build; only identical bytes can be staged under this receipt",
                )?;
                Some(identity)
            }
            None => None,
        };
        proof::write_new(&pending, &fs::read(staged_from).map_err(|e| e.to_string())?)?;
        fs::set_permissions(
            &pending,
            fs::metadata(staged_from)
                .map_err(|e| e.to_string())?
                .permissions(),
        )
        .map_err(|e| e.to_string())?;
        let staged = proof::FileIdentity::read(&pending)?;
        require(
            staged.sha256 == qualification.artifact.sha256,
            "installed copy differs from qualified runtime",
        )?;
        qualification.artifact.verify()?;
        if let Some(identity) = &prebuilt_identity {
            identity.verify()?;
        }
        Ok(
            json!({"schema":"hermit-packaged-detcore/v1", "reverie_pin":pin,
            "profile_name":"liteinst-runtime", "profile_inputs":profile_inputs,
            "profile_objects":profile, "cargo_argv":cargo_args,"cargo_version":cargo_version,
            "rustc_version":rustc_version,"cargo_artifact":artifact,
            "qualifier_source":qualifier_source,"compiled_qualifier_sha256":proof::qualifier_source_sha256(),
            "compiled_sources":sources.record(),"qualification":qualification}),
        )
    })();
    let result = close(result, sources.verify_unchanged(&mut runner));
    let mut record = match result {
        Ok(record) => record,
        Err(error) => {
            let _ = fs::remove_file(&pending);
            return Err(error);
        }
    };
    fs::rename(&pending, &installed).map_err(|e| e.to_string())?;
    let publication = (|| {
        let installed_identity = proof::FileIdentity::read(&installed)?;
        require(
            record["qualification"]["artifact"]["sha256"] == installed_identity.sha256,
            "installed bytes changed before receipt publication",
        )?;
        record["installed"] =
            serde_json::to_value(installed_identity).map_err(|e| e.to_string())?;
        sources.verify_unchanged(&mut runner)?;
        record["source_close_completed"] = json!(true);
        record["build_environment"] = json!(
            [
                "CARGO_BUILD_JOBS",
                "RUSTC",
                "RUSTC_WRAPPER",
                "RUSTC_WORKSPACE_WRAPPER",
                "RUSTFLAGS",
                "CARGO_ENCODED_RUSTFLAGS",
                "RUSTUP_TOOLCHAIN",
                "CARGO_HOME",
                "CC",
                "CXX",
                "AR"
            ]
            .into_iter()
            .map(|name| (name.to_owned(), std::env::var(name).ok()))
            .collect::<std::collections::BTreeMap<_, _>>()
        );
        record["commands"] = serde_json::to_value(runner.commands()).map_err(|e| e.to_string())?;
        proof::write_new(
            &receipt,
            &serde_json::to_vec_pretty(&record).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    })();
    if let Err(error) = close(publication, sources.verify_unchanged(&mut runner)) {
        let _ = fs::remove_file(&installed);
        let _ = fs::remove_file(&receipt);
        return Err(error);
    }
    println!("qualified packaged Detcore: {}", installed.display());
    Ok(())
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("stage-detcore-liteinst: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn layout_selection_requires_one_agreeing_reverie_shape() {
        // A Reverie root that does not exist carries no Git metadata:
        // the gitless route. The checkout case uses a real root below.
        let gitless = Path::new("/nonexistent-reverie-root");
        assert_eq!(
            reverie_layout("reverie-core = { path = \"reverie/reverie\" }\n", gitless).unwrap(),
            proof::CompiledSourceLayout::PathCheckout,
        );
        assert_eq!(
            reverie_layout(
                &format!(
                    "reverie-core = {{ git = \"{REVERIE_GIT_REPOSITORY}\", rev = \"0123456789012345678901234567890123456789\" }}\n"
                ),
                gitless,
            )
            .unwrap(),
            proof::CompiledSourceLayout::GitlinkCheckout,
        );
        // A path-spelled root with its own .git takes the Git layout,
        // so the pin proof runs; content alone never certifies it.
        let serial = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let checkout = std::env::temp_dir().join(format!(
            "stage-detcore-liteinst-layout-{}-{serial}",
            std::process::id()
        ));
        fs::create_dir_all(checkout.join(".git")).unwrap();
        assert_eq!(
            reverie_layout("reverie-core = { path = \"reverie/reverie\" }\n", &checkout).unwrap(),
            proof::CompiledSourceLayout::GitlinkCheckout,
        );
        fs::remove_dir_all(&checkout).unwrap();
        assert!(
            reverie_layout(
                &format!(
                    "reverie-core = {{ path = \"reverie/reverie\" }}\nreverie-ptrace = {{ git = \"{REVERIE_GIT_REPOSITORY}\" }}\n"
                ),
                gitless,
            )
            .is_err()
        );
        assert!(reverie_layout("[workspace]\n", gitless).is_err());
    }

    #[test]
    fn arguments_accept_an_optional_prebuilt_runtime() {
        assert_eq!(
            parse_args(&argv(&["--git", "target"])).unwrap(),
            Mode::Cargo {
                authority: "--git".to_string(),
                target: "target".to_string(),
                prebuilt: None,
            }
        );
        assert_eq!(
            parse_args(&argv(&[
                "--source-copy",
                "target",
                "--prebuilt",
                "buck-out/lib.so"
            ]))
            .unwrap(),
            Mode::Cargo {
                authority: "--source-copy".to_string(),
                target: "target".to_string(),
                prebuilt: Some(PathBuf::from("buck-out/lib.so")),
            }
        );
    }

    #[test]
    fn arguments_refuse_every_other_shape() {
        for (args, message) in [
            (vec![], "expected --git, --source-copy or --buck"),
            (
                vec!["--prebuilt", "x"],
                "expected --git, --source-copy or --buck",
            ),
            // Mode precedence: a misspelled mode with no target at all
            // still reports the mode, not the missing target.
            (vec!["--bogus"], "expected --git, --source-copy or --buck"),
            (vec!["--git"], "expected existing target directory"),
            (vec!["--buck"], "expected existing target directory"),
            (
                vec!["--git", "target", "extra"],
                "unexpected finalizer arguments",
            ),
            (
                vec!["--git", "target", "--prebuilt"],
                "expected prebuilt runtime path",
            ),
            (
                vec!["--git", "target", "--prebuilt", "x", "extra"],
                "unexpected finalizer arguments",
            ),
            // The modes never mix: no --prebuilt byte-identity gate in Buck
            // mode, and no Buck flags in Cargo mode.
            (
                vec!["--buck", "target", "--prebuilt", "x"],
                "unexpected --buck argument",
            ),
            (
                vec!["--git", "target", "--artifact", "x"],
                "unexpected finalizer arguments",
            ),
        ] {
            let error = parse_args(&argv(&args)).unwrap_err().to_string();
            assert!(error.contains(message), "{args:?}: {error}");
        }
    }

    fn buck_argv_fixture() -> Vec<&'static str> {
        vec![
            "--buck",
            "target",
            "--workspace-root",
            "hermit",
            "--reverie-root",
            "reverie",
            "--reverie-pin",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--imported-sha",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "--profile-bzl",
            "fb-only/liteinst-runtime-profile.bzl",
            "--buck-target",
            BUCK_TARGET,
            "--buck-platform",
            BUCK_PLATFORM,
            "--buck-constraint",
            BUCK_CONSTRAINTS[0],
            "--buck-constraint",
            BUCK_CONSTRAINTS[1],
            "--buck-constraint",
            BUCK_CONSTRAINTS[2],
            "--buck-cwd",
            "fbcode",
            "--workspace-file",
            "detcore-liteinst/src/lib.rs",
            "--workspace-file",
            "detcore-liteinst/build.rs",
            "--reverie-file",
            "reverie-liteinst-artifact/src/lib.rs",
        ]
    }

    fn buck_args_fixture() -> BuckArgs {
        let Mode::Buck(args) = parse_args(&argv(&buck_argv_fixture())).unwrap() else {
            panic!("fixture must parse as Buck mode");
        };
        *args
    }

    #[test]
    fn buck_arguments_are_all_explicit() {
        let args = buck_args_fixture();
        assert_eq!(args.target, PathBuf::from("target"));
        assert_eq!(args.buck_cwd, PathBuf::from("fbcode"));
        assert_eq!(args.workspace_root, PathBuf::from("hermit"));
        assert_eq!(args.reverie_root, PathBuf::from("reverie"));
        assert_eq!(args.reverie_pin, "a".repeat(40));
        assert_eq!(args.imported_sha, "b".repeat(40));
        assert_eq!(args.buck_constraints, BUCK_CONSTRAINTS);
        validate_buck_args(&args).unwrap();
    }

    #[test]
    fn buck_arguments_refuse_incomplete_and_misspelled_shapes() {
        let full = buck_argv_fixture();
        for (args, message) in [
            (vec!["--buck", "target"], "--buck requires --workspace-root"),
            (
                vec!["--buck", "target", "--workspace-root"],
                "expected value after --workspace-root",
            ),
            // No caller artifact or command is accepted in Buck mode.
            (
                vec!["--buck", "target", "--artifact", "x.so"],
                "unexpected --buck argument",
            ),
            (
                vec!["--buck", "target", "--unknown", "x"],
                "unexpected --buck argument",
            ),
        ] {
            let error = parse_args(&argv(&args)).unwrap_err().to_string();
            assert!(error.contains(message), "{args:?}: {error}");
        }
        // Dropping either half of a singleton pair, or repeating it, refuses.
        for cut in [2, 4, 10, 14] {
            let mut truncated = full.clone();
            truncated.drain(cut..cut + 2);
            assert!(parse_args(&argv(&truncated)).is_err(), "cut {cut}");
        }
        let mut duplicated = full.clone();
        duplicated.extend(["--buck-cwd", "other"]);
        let error = parse_args(&argv(&duplicated)).unwrap_err().to_string();
        assert!(error.contains("duplicate --buck argument"), "{error}");
    }

    #[test]
    fn buck_validation_refuses_unpinned_provenance() {
        let mut args = buck_args_fixture();
        args.reverie_pin = "abc".into();
        assert!(
            validate_buck_args(&args)
                .unwrap_err()
                .contains("full 40-hex --reverie-pin")
        );
        let mut args = buck_args_fixture();
        args.imported_sha = "B".repeat(40);
        assert!(
            validate_buck_args(&args)
                .unwrap_err()
                .contains("full 40-hex --imported-sha")
        );
        let mut args = buck_args_fixture();
        args.buck_target = "//hermetic_infra/hermit:detcore-liteinst".into();
        assert!(
            validate_buck_args(&args)
                .unwrap_err()
                .contains("pinned Buck runtime target")
        );
        let mut args = buck_args_fixture();
        args.buck_platform = "ovr_config//platform/linux:aarch64-fbcode".into();
        assert!(
            validate_buck_args(&args)
                .unwrap_err()
                .contains("pinned Buck runtime platform")
        );
        let mut args = buck_args_fixture();
        args.buck_constraints.reverse();
        assert!(
            validate_buck_args(&args)
                .unwrap_err()
                .contains("pinned runtime constraints")
        );
        let mut args = buck_args_fixture();
        args.profile_bzl = PathBuf::from("fb-only/other.bzl");
        assert!(
            validate_buck_args(&args)
                .unwrap_err()
                .contains("liteinst-runtime profile .bzl")
        );
        let mut args = buck_args_fixture();
        args.workspace_files = vec!["/absolute/lib.rs".into()];
        assert!(
            validate_buck_args(&args)
                .unwrap_err()
                .contains("logical relative paths")
        );
        let mut args = buck_args_fixture();
        args.workspace_files = vec!["detcore-liteinst/build.rs".into()];
        assert!(
            validate_buck_args(&args)
                .unwrap_err()
                .contains("leaf source")
        );
        let mut args = buck_args_fixture();
        args.reverie_files = vec!["reverie-liteinst/src/patch_alloc.rs".into()];
        assert!(
            validate_buck_args(&args)
                .unwrap_err()
                .contains("qualifier source")
        );
        let mut args = buck_args_fixture();
        args.reverie_files.clear();
        assert!(
            validate_buck_args(&args)
                .unwrap_err()
                .contains("declared source files")
        );
    }

    /// A profile `.bzl` text in the shape `fb-only/liteinst-runtime-profile.bzl`
    /// defines, over the given flag literals.
    fn profile_bzl_text(flags: &[&str]) -> String {
        let list = flags
            .iter()
            .map(|flag| format!("    \"{flag}\",\n"))
            .collect::<String>();
        format!("LITEINST_RUNTIME_RUSTC_FLAGS = [\n{list}]\n")
    }

    /// Buck arguments over real on-disk inputs, so `run_buck` passes every
    /// path and profile check and reaches the refusal each test names.
    /// Returns the scratch root for cleanup.
    fn buck_disk_fixture(profile: &str) -> (PathBuf, BuckArgs) {
        let serial = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "stage-detcore-liteinst-buck-{}-{serial}",
            std::process::id()
        ));
        let mut args = buck_args_fixture();
        args.target = root.join("target");
        args.workspace_root = root.join("hermit");
        args.reverie_root = root.join("reverie");
        args.buck_cwd = root.join("fbcode");
        args.profile_bzl = root.join("liteinst-runtime-profile.bzl");
        for dir in [
            &args.target,
            &args.workspace_root,
            &args.reverie_root,
            &args.buck_cwd,
        ] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::create_dir_all(args.target.join("install_pkg/rsrcs")).unwrap();
        fs::write(&args.profile_bzl, profile).unwrap();
        (root, args)
    }

    #[test]
    fn buck_mode_runs_buck2_and_stages_nothing_on_refusal() {
        // Valid on-disk inputs pass argument, path and profile-content
        // checks and reach the retained buck2 run: the fixture buck
        // working directory is no Buck project (and a test sandbox may
        // have no buck2 at all), so the retained run refuses and no
        // staged runtime or receipt appears. This is the honesty gate
        // in action: without a real buck2 run there is nothing to bind.
        let (root, args) = buck_disk_fixture(&profile_bzl_text(&BUCK_RUSTC_FLAGS));
        let error = run_buck(&args).unwrap_err();
        assert!(
            !error.contains("LITEINST_RUNTIME_RUSTC_FLAGS do not match"),
            "profile check must pass before the buck2 run: {error}"
        );
        let resources = args.target.join("install_pkg/rsrcs");
        assert!(!resources.join("libdetcore_liteinst.so").exists());
        assert!(!resources.join("libdetcore_liteinst.package.json").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pinned_workspace_files_hashes_buck_and_profile_from_workspace_tree() {
        let serial = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "stage-detcore-liteinst-pinned-{}-{serial}",
            std::process::id()
        ));
        let workspace = root.join("hermit");
        for (relative, contents) in [
            ("detcore-liteinst/src/lib.rs", "leaf"),
            ("BUCK", "buck-file"),
            ("fb-only/liteinst-runtime-profile.bzl", "profile"),
        ] {
            let path = workspace.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, contents).unwrap();
        }
        let sha = |relative: &str| {
            proof::FileIdentity::read(&workspace.join(relative))
                .unwrap()
                .sha256
        };
        // BUCK and the profile are appended, in order, when absent.
        let pinned =
            pinned_workspace_files(&workspace, &["detcore-liteinst/src/lib.rs".to_string()])
                .unwrap();
        assert_eq!(
            pinned,
            [
                (
                    "detcore-liteinst/src/lib.rs".to_string(),
                    sha("detcore-liteinst/src/lib.rs")
                ),
                ("BUCK".to_string(), sha("BUCK")),
                (
                    "fb-only/liteinst-runtime-profile.bzl".to_string(),
                    sha("fb-only/liteinst-runtime-profile.bzl")
                ),
            ]
        );
        // Already-declared BUCK/profile entries are not duplicated and
        // keep their declared positions.
        let pinned = pinned_workspace_files(
            &workspace,
            &[
                "BUCK".to_string(),
                "detcore-liteinst/src/lib.rs".to_string(),
                "fb-only/liteinst-runtime-profile.bzl".to_string(),
                "BUCK".to_string(),
            ],
        )
        .unwrap();
        assert_eq!(
            pinned
                .iter()
                .map(|(relative, _)| relative.as_str())
                .collect::<Vec<_>>(),
            [
                "BUCK",
                "detcore-liteinst/src/lib.rs",
                "fb-only/liteinst-runtime-profile.bzl"
            ]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn buck_rustc_version_requires_exactly_one_comment_identity() {
        let dump = "String dump of section '.comment':\n  \
            [     0]  rustc version 1.97.1 (8bab26f4f 2026-07-14)\n  \
            [    2a]  GCC: (GNU) 14.2.1\n";
        assert_eq!(
            buck_rustc_version(dump).unwrap(),
            "rustc version 1.97.1 (8bab26f4f 2026-07-14)"
        );
        assert!(
            buck_rustc_version("no compiler identity\n")
                .unwrap_err()
                .contains("exactly one rustc version identity")
        );
        let two = format!("{dump}  [    40]  rustc version 1.98.0\n");
        assert!(
            buck_rustc_version(&two)
                .unwrap_err()
                .contains("exactly one rustc version identity")
        );
    }

    #[test]
    fn buck_mode_refuses_a_profile_whose_flags_differ() {
        let mut flags = BUCK_RUSTC_FLAGS;
        flags[0] = "-Copt-level=2";
        let (root, args) = buck_disk_fixture(&profile_bzl_text(&flags));
        let error = run_buck(&args).unwrap_err();
        assert!(
            error.contains("LITEINST_RUNTIME_RUSTC_FLAGS do not match"),
            "profile refusal precedes source resolution: {error}"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn profile_bzl_contents_must_define_the_pinned_flags_exactly() {
        let exact = profile_bzl_text(&[
            "-Copt-level=3",
            "-Cdebuginfo=0",
            "-Cdebug-assertions=yes",
            "-Coverflow-checks=yes",
            "-Ccodegen-units=16",
            "-Cembed-bitcode=no",
            "-Cpanic=unwind",
            "-Cstrip=none",
        ]);
        validate_profile_bzl(&exact).unwrap();
        // Missing definition refuses by that name.
        let error = validate_profile_bzl("# no profile here\n").unwrap_err();
        assert!(
            error.contains("does not define LITEINST_RUNTIME_RUSTC_FLAGS"),
            "{error}"
        );
        // Dropped, appended, reordered and respelled lists all refuse.
        let mut dropped = BUCK_RUSTC_FLAGS.to_vec();
        dropped.pop();
        let mut appended = BUCK_RUSTC_FLAGS.to_vec();
        appended.push("-Clto=off");
        let mut reordered = BUCK_RUSTC_FLAGS.to_vec();
        reordered.reverse();
        let mut respelled = BUCK_RUSTC_FLAGS.to_vec();
        respelled[3] = "-Coverflow-checks=no";
        for flags in [&dropped, &appended, &reordered, &respelled] {
            let error = validate_profile_bzl(&profile_bzl_text(flags)).unwrap_err();
            assert!(
                error.contains("LITEINST_RUNTIME_RUSTC_FLAGS do not match"),
                "{flags:?}: {error}"
            );
        }
    }

    #[test]
    fn buck_output_selection_reads_only_the_retained_stdout() {
        let line = format!("fbcode{BUCK_TARGET} /buck-out/libdetcore_liteinst.so\n");
        assert_eq!(
            parse_buck_output(&line).unwrap(),
            PathBuf::from("/buck-out/libdetcore_liteinst.so")
        );
        let plain = format!("{BUCK_TARGET} /buck-out/lib.so\n");
        assert_eq!(
            parse_buck_output(&plain).unwrap(),
            PathBuf::from("/buck-out/lib.so")
        );
        for stdout in [
            "",
            "\n",
            "/buck-out/lib.so\n",
            "//hermetic_infra/hermit:detcore-liteinst /buck-out/lib.so\n",
            // A relative output is not --show-full-output's spelling.
            &format!("{BUCK_TARGET} buck-out/lib.so\n"),
            // Two outputs name no single ELF.
            &format!("{line}{line}"),
        ] {
            let error = parse_buck_output(stdout).unwrap_err();
            assert!(
                error.contains("exactly one absolute output"),
                "{stdout:?}: {error}"
            );
        }
    }

    #[test]
    fn dt_flags_textrel_refusal_names_textrel_at_0xc() {
        // DT_FLAGS 0xc is TEXTREL (0x4) | BIND_NOW (0x8), the real
        // fbcode-built ELF's value: the refusal must name TEXTREL, and
        // the readelf spelling of the same word must decode to 0xc.
        let refusal = dt_flags_refusal(0xc).expect("0xc carries TEXTREL");
        assert!(refusal.contains("TEXTREL"), "{refusal}");
        assert!(refusal.contains("0xc"), "{refusal}");
        let dump = "Dynamic section at offset 0x10 contains 3 entries:\n  \
            Tag        Type                         Name/Value\n \
            0x000000000000001e (FLAGS)              TEXTREL BIND_NOW\n \
            0x000000006ffffffb (FLAGS_1)            Flags: NOW\n \
            0x0000000000000016 (TEXTREL)            0x0\n";
        assert_eq!(dt_flags_from_readelf(dump), 0xc);
        assert!(
            dt_flags_refusal(dt_flags_from_readelf(dump))
                .is_some_and(|refusal| refusal.contains("TEXTREL"))
        );
        // Eager binding alone, and no FLAGS entry at all, name nothing.
        assert!(dt_flags_refusal(0x8).is_none());
        assert_eq!(dt_flags_from_readelf("(NEEDED) libc.so.6\n"), 0);
        assert!(dt_flags_refusal(0).is_none());
    }

    #[test]
    fn needed_refusal_names_each_forbidden_soname() {
        // The measured fbcode-built ELF's DT_NEEDED set (D1c-6): every
        // forbidden soname is named; libm/libc/ld-linux are admitted.
        let dump = "Dynamic section at offset 0x10 contains 8 entries:\n  \
            Tag        Type                         Name/Value\n \
            0x0000000000000001 (NEEDED)             Shared library: [libgcc_s.so.1]\n \
            0x0000000000000001 (NEEDED)             Shared library: [libm.so.6]\n \
            0x0000000000000001 (NEEDED)             Shared library: [libmvec.so.1]\n \
            0x0000000000000001 (NEEDED)             Shared library: [libc.so.6]\n \
            0x0000000000000001 (NEEDED)             Shared library: [ld-linux-x86-64.so.2]\n \
            0x0000000000000001 (NEEDED)             Shared library: [libatomic.so.1]\n \
            0x0000000000000001 (NEEDED)             Shared library: [libstdc++.so.6]\n \
            0x000000000000001e (FLAGS)              BIND_NOW\n";
        let needed = needed_from_readelf(dump);
        assert_eq!(needed.len(), 7);
        assert_eq!(needed[0], "libgcc_s.so.1");
        let refusal = needed_refusal(&needed).expect("forbidden NEEDEDs present");
        for soname in FORBIDDEN_NEEDED {
            assert!(refusal.contains(soname), "{soname} unnamed: {refusal}");
        }
        assert!(!refusal.contains("libm.so"), "{refusal}");
        // -z text's post-fix FLAGS (BIND_NOW only) name nothing.
        assert!(dt_flags_refusal(dt_flags_from_readelf(dump)).is_none());
        // The admitted set alone refuses nothing; an unreadable line
        // contributes no soname.
        let clean = "0x1 (NEEDED) Shared library: [libm.so.6]\n\
                     0x1 (NEEDED) Shared library: [libc.so.6]\n\
                     0x1 (NEEDED) garbled-without-brackets\n";
        assert!(needed_refusal(&needed_from_readelf(clean)).is_none());
        assert!(needed_refusal(&[]).is_none());
    }

    #[test]
    fn profile_flags_must_be_applied_before_any_receipt_lists_them() {
        require_profile_flags_applied("LITEINST_RUNTIME_FLAGS_APPLIED = True\n").unwrap();
        for contents in [
            "LITEINST_RUNTIME_FLAGS_APPLIED = False\n",
            "# flags are pinned but never delivered\n",
            "LITEINST_RUNTIME_FLAGS_APPLIED = TRUE\n",
        ] {
            let error = require_profile_flags_applied(contents).unwrap_err();
            assert!(
                error.contains("does not apply LITEINST_RUNTIME_RUSTC_FLAGS"),
                "{contents:?}: {error}"
            );
        }
    }

    /// The receipt body must match the verifier's Buck arm exactly: same
    /// schema, `builder=buck2`, the flag-dict profile, the Buck literals and
    /// argv, the artifact mirror, and no `cargo_*` key anywhere.
    #[test]
    fn buck_receipt_matches_the_verifier_buck_arm() {
        let args = buck_args_fixture();
        let package = json!({"name": "detcore-liteinst", "target": "detcore_liteinst",
            "source": null, "kind": ["cdylib"], "crate_types": ["cdylib"]});
        let profile_inputs = json!({"path": "/fixture/fb-only/liteinst-runtime-profile.bzl", "sha256": "1".repeat(64)});
        let record = buck_receipt(
            &args,
            BuckReceiptSections {
                profile_inputs: profile_inputs.clone(),
                artifact: buck_artifact(
                    package.clone(),
                    Path::new("/fixture/buck-out/libdetcore_liteinst.so"),
                ),
                qualifier_source: json!({"path": "/fixture/reverie/reverie-liteinst-artifact/src/lib.rs",
                       "sha256": proof::qualifier_source_sha256()}),
                compiled_sources: json!({"image_kind": "Detcore", "reverie_pin": args.reverie_pin,
                       "manifest": profile_inputs, "leaf": package,
                       "reverie_root": "/fixture/reverie"}),
                qualification: json!({"artifact": {"path": "/fixture/buck-out/libdetcore_liteinst.so"}}),
                installed: json!({"sha256": "5".repeat(64)}),
                commands: json!([
                    {"status": 0, "program": "buck2",
                     "arguments": ["build", BUCK_TARGET, "--target-platforms", BUCK_PLATFORM, "--show-full-output", "--client-metadata", "id=stage-detcore-liteinst"],
                     "stdout": {"path": "/fixture/logs/buck2-stdout", "sha256": "6".repeat(64)}},
                    {"status": 0, "program": "readelf",
                     "arguments": ["--version-info", "--wide", "/fixture/buck-out/libdetcore_liteinst.so"]},
                    {"status": 0, "program": "objdump",
                     "arguments": ["--disassemble", "/fixture/buck-out/libdetcore_liteinst.so"]},
                ]),
            },
            &buck_argv(),
            &"5".repeat(64),
            "rustc fixture",
            true,
        );
        let object = record.as_object().unwrap();
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "buck_argv",
                "buck_artifact",
                "buck_constraints",
                "buck_output_sha256",
                "buck_platform",
                "buck_target",
                "builder",
                "commands",
                "compiled_qualifier_sha256",
                "compiled_sources",
                "installed",
                "profile_inputs",
                "profile_name",
                "profile_objects",
                "qualification",
                "qualifier_source",
                "reverie_pin",
                "rustc_version",
                "schema",
                "source_close_completed",
            ]
        );
        assert!(keys.iter().all(|key| !key.starts_with("cargo_")));
        assert_eq!(record["schema"], "hermit-packaged-detcore/v1");
        assert_eq!(record["builder"], "buck2");
        assert_eq!(record["profile_name"], "liteinst-runtime");
        assert_eq!(
            record["profile_objects"],
            json!({"liteinst-runtime": {"rustc_flags": [
                "-Copt-level=3", "-Cdebuginfo=0", "-Cdebug-assertions=yes",
                "-Coverflow-checks=yes", "-Ccodegen-units=16",
                "-Cembed-bitcode=no", "-Cpanic=unwind", "-Cstrip=none"]}})
        );
        assert_eq!(
            record["buck_argv"],
            json!([
                "buck2",
                "build",
                BUCK_TARGET,
                "--target-platforms",
                BUCK_PLATFORM,
                "--show-full-output",
                "--client-metadata",
                "id=stage-detcore-liteinst"
            ])
        );
        // The output digest this process computed from the ELF the
        // retained stdout named is carried for the verifier to bind to
        // the installed bytes.
        assert_eq!(record["buck_output_sha256"], "5".repeat(64));
        assert_eq!(
            record["buck_output_sha256"], record["installed"]["sha256"],
            "fixture models a coherent publication"
        );
        // The executed producer entry is buck_argv without its program, as
        // the verifier's Buck arm binds it; the installed identity and the
        // source-close outcome are carried, not deferred to publication.
        assert_eq!(record["commands"][0]["program"], "buck2");
        assert_eq!(
            record["commands"][0]["arguments"],
            json!([
                "build",
                BUCK_TARGET,
                "--target-platforms",
                BUCK_PLATFORM,
                "--show-full-output",
                "--client-metadata",
                "id=stage-detcore-liteinst"
            ])
        );
        assert_eq!(record["installed"], json!({"sha256": "5".repeat(64)}));
        assert_eq!(record["source_close_completed"], json!(true));
        assert_eq!(record["buck_target"], BUCK_TARGET);
        assert_eq!(record["buck_platform"], BUCK_PLATFORM);
        assert_eq!(record["buck_constraints"], json!(BUCK_CONSTRAINTS));
        assert_eq!(record["buck_artifact"]["features"], json!([]));
        assert_eq!(
            record["buck_artifact"]["profile"],
            json!({"opt_level": "3", "debuginfo": 0, "debug_assertions": true,
                   "overflow_checks": true, "test": false})
        );
        assert_eq!(
            record["buck_artifact"]["reported_path"],
            "/fixture/buck-out/libdetcore_liteinst.so"
        );
        // The verifier joins profile_inputs to the compiled-sources manifest
        // slot; in the Buck arm both are the profile .bzl identity.
        assert_eq!(
            record["profile_inputs"],
            record["compiled_sources"]["manifest"]
        );
        assert_eq!(
            record["compiled_qualifier_sha256"],
            proof::qualifier_source_sha256()
        );
    }

    #[test]
    fn source_close_preserves_both_failures_and_never_publishes_success() {
        assert_eq!(
            close::<()>(Err("build refused".into()), Ok(())),
            Err("build refused".into())
        );
        assert_eq!(close(Ok(()), Err("changed".into())), Err("changed".into()));
        assert_eq!(
            close::<()>(Err("proof refused".into()), Err("changed".into())),
            Err("proof refused; packaged source close failed: changed".into())
        );
    }
}
