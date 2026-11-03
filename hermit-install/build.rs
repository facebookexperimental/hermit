/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::env;
use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

fn run(command: &mut Command, description: &str) {
    eprintln!("hermit-install: {description}: {command:?}");
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("failed to {description}: {error}"));
    assert!(status.success(), "failed to {description}: {status}");
}

fn copy_file(source: &Path, destination: &Path) {
    assert!(
        source.is_file(),
        "required installation resource is missing: {}",
        source.display()
    );
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .unwrap_or_else(|error| panic!("failed to create {}: {error}", parent.display()));
    }
    fs::copy(source, destination).unwrap_or_else(|error| {
        panic!(
            "failed to copy {} to {}: {error}",
            source.display(),
            destination.display()
        )
    });
}

fn replace_symlink(destination: &Path, target: &Path) -> io::Result<()> {
    match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(destination)?,
        Ok(_) => fs::remove_file(destination)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    symlink(target, destination)
}

fn replace_copy(source: &Path, destination: &Path) {
    match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.is_dir() => {
            panic!(
                "refusing to replace directory {} with a runtime file",
                destination.display()
            )
        }
        Ok(_) => fs::remove_file(destination)
            .unwrap_or_else(|error| panic!("failed to remove {}: {error}", destination.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => panic!("failed to inspect {}: {error}", destination.display()),
    }
    copy_file(source, destination);
}

fn build_liteinst_runtime(
    repository: &Path,
    build_root: &Path,
    profile_dir: &Path,
    resources: &Path,
) {
    // stage-liteinst-runtime.sh appends the canonical Reverie pin to this stable
    // root, so cache invalidation has the same source of truth as the pin gate.
    let target = build_root.join("liteinst-runtime");
    let runtime = profile_dir.join("libreverie_liteinst.so");
    run(
        Command::new(repository.join("scripts/stage-liteinst-runtime.sh"))
            .current_dir(repository)
            // This caller derives the pin through the pin gate; an exported
            // test override must not choose its marker.
            .env_remove("HERMIT_LITEINST_REVERIE_PIN")
            .arg("release")
            .arg(&runtime)
            .arg(&target),
        "build the constructor-enabled LiteInst runtime",
    );
    assert!(
        runtime.is_file(),
        "standalone build did not stage {}",
        runtime.display()
    );
    replace_copy(&runtime, &resources.join("libreverie_liteinst.so"));
    // RECORD THE PIN THE ARTIFACT WAS BUILT FROM, AND MEAN IT.
    //
    // `sabre.revision` established this shape and is read by nothing, so it is
    // provenance rather than authority. This one IS read: hermit compares it
    // against the pin compiled into the binary when it resolves the staged
    // runtime, and refuses a mismatch. That is what makes staleness loud instead
    // of silent -- see the loader in hermit-cli/src/lib.rs.
    //
    // ⚠️ WRITE IT BESIDE **EVERY** STAGED COPY, NOT JUST THIS ONE. The runtime is
    // staged twice -- once at `profile_dir` by the script above and once here in
    // `resources` -- and the loader reads `<the path it resolved>.revision`. When
    // the sidecar existed only here, a correct restage produced a correct pin in
    // `rsrcs/` while the loader resolved `target/release/libreverie_liteinst.so`,
    // found no sidecar beside it, and refused with "records no Reverie revision".
    // The artifact was never stale; only one of its two homes carried provenance.
    // That made the printed LiteInst remedy --
    // `cargo build --release -p hermit-install` -- regenerate exactly the state
    // that was already failing, so following the error message looped instead
    // of resolving.
    // The core installer stages LiteInst with that command.
    let pin = reverie_pin(repository);
    for staged in [&runtime, &resources.join("libreverie_liteinst.so")] {
        let marker = PathBuf::from(format!("{}.revision", staged.display()));
        fs::write(&marker, format!("{pin}\n")).unwrap_or_else(|error| {
            panic!(
                "failed to record the LiteInst runtime revision at {}: {error}",
                marker.display()
            )
        });
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=reverie-LICENSE");
    println!("cargo:rerun-if-env-changed=HERMIT_INSTALL_FORCE_RESTAGE");
    println!("cargo:rerun-if-changed=../scripts/stage-liteinst-runtime.sh");
    // ⚠️ THE PIN FILES. Without these the staged runtime NEVER REBUILDS WHEN THE
    // REVERIE PIN MOVES: none of the triggers above changes when a pin bump
    // lands, so Cargo considers this script fresh and the stale `.so` stays.
    // A cell then measures the old binary and reports a verdict about the new
    // pin.
    println!("cargo:rerun-if-changed=../detcore/Cargo.toml");
    println!("cargo:rerun-if-changed=../liteinst-runtime-build/Cargo.lock");
    println!("cargo:rerun-if-changed=../liteinst-runtime-build/Cargo.toml");
    println!("cargo:rerun-if-changed=../liteinst-runtime-build/build.rs");
    println!("cargo:rerun-if-changed=../liteinst-runtime-build/artifact.rs");
    println!("cargo:rerun-if-changed=../liteinst-runtime-build/runtime/Cargo.toml");
    println!("cargo:rerun-if-changed=../liteinst-runtime-build/runtime/src/lib.rs");

    // Cargo sets PROFILE to "release" for `release` and for every custom
    // profile that inherits from it, such as the validation profile
    // `validate`; it is "debug" otherwise. Staging follows every
    // release-derived profile.
    if env::var("PROFILE").as_deref() != Ok("release")
        || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux")
        || env::var("CARGO_CFG_TARGET_ARCH").as_deref() != Ok("x86_64")
    {
        return;
    }

    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let profile_dir = profile_dir_of(&out_dir);
    // The artifact directory's own name (`release`, `validate`, ...): the
    // packaged symlinks below point at the libraries and binary this same
    // Cargo invocation places there.
    let profile_dir_name = profile_dir
        .file_name()
        .expect("Cargo profile directory has no name")
        .to_owned();
    let target_dir = profile_dir
        .parent()
        .expect("Cargo profile directory has no target parent");
    let install = target_dir.join("install_pkg");
    let resources = install.join("rsrcs");
    let build_root = target_dir.join("install-build");
    // Record the core package selection beside its staged resources.
    let selection = build_root.join("package-selection");
    println!("cargo:rerun-if-changed={}", selection.display());

    if install.exists() {
        fs::remove_dir_all(&install)
            .unwrap_or_else(|error| panic!("failed to reset {}: {error}", install.display()));
    }
    fs::create_dir_all(&resources)
        .unwrap_or_else(|error| panic!("failed to create {}: {error}", resources.display()));
    fs::create_dir_all(&build_root)
        .unwrap_or_else(|error| panic!("failed to create {}: {error}", build_root.display()));

    // Ordinary Detcore LiteInst is finalized AFTER outer Cargo exits, using
    // the existing liteinst-runtime profile and shared physical/source proof.
    // Never expose this invocation's validate/LTO-off DSO as an installed one.
    // The absent resource makes an unfinalized installation fail closed.
    println!(
        "cargo:warning=finish LiteInst packaging with target/<profile>/stage-detcore-liteinst --git target (make install-deps does this automatically)"
    );

    let repository = manifest_dir
        .parent()
        .expect("hermit-install is not inside the Hermit repository");
    build_liteinst_runtime(repository, &build_root, &profile_dir, &resources);

    copy_file(&repository.join("LICENSE"), &install.join("LICENSE"));
    // Core LiteInst also ships Reverie code; keep its notice without requiring
    // a third-party backend dependency merely to locate the license.
    copy_file(
        &manifest_dir.join("reverie-LICENSE"),
        &install.join("licenses/LICENSE"),
    );

    replace_symlink(
        &install.join("hermit"),
        &Path::new("..").join(&profile_dir_name).join("hermit"),
    )
    .unwrap_or_else(|error| panic!("failed to link install_pkg/hermit: {error}"));
    fs::write(
        install.join("README.txt"),
        "Hermit release staging package. Copy with symlink dereferencing (for example, cp -aL) to create a standalone installation.\n",
    )
    .expect("failed to write install package README");
    let selected = "core\n";
    let previous = match fs::read_to_string(&selection) {
        Ok(previous) => Some(previous),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => panic!("failed to read install package selection: {error}"),
    };
    if previous.as_deref() != Some(selected) {
        fs::write(selection, selected).expect("failed to record install package selection");
    }
}

/// The Cargo artifact directory of the active profile: `OUT_DIR` is
/// `<target>[/<triple>]/<profile-dir>/build/<package>-<hash>/out`, and the
/// profile directory's name is the profile's own name except that `dev` uses
/// `debug`. Derived structurally, because a custom profile's directory is
/// named after the profile while `PROFILE` only says `release` or `debug`.
fn profile_dir_of(out_dir: &Path) -> PathBuf {
    let build = out_dir
        .parent()
        .and_then(Path::parent)
        .expect("Cargo OUT_DIR is not <profile>/build/<package>/out");
    assert_eq!(
        build.file_name().and_then(|name| name.to_str()),
        Some("build"),
        "Cargo OUT_DIR {} is not <profile>/build/<package>/out",
        out_dir.display()
    );
    build
        .parent()
        .expect("Cargo build directory has no profile parent")
        .to_path_buf()
}

include!("../hermit-cli/reverie_pin.rs");

/// The Reverie revision this tree pins, read from the canonical manifest.
fn reverie_pin(repository: &Path) -> String {
    let Ok(text) = fs::read_to_string(repository.join("detcore/Cargo.toml")) else {
        return "unknown".into();
    };
    parse_reverie_pin(&text).unwrap_or_else(|| "unknown".into())
}
