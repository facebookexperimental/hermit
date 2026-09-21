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

const DYNAMORIO_FILES: &[&str] = &[
    "bin64/drrun",
    "lib64/release/libdynamorio.so",
    "lib64/release/libdrpreload.so",
    "ext/lib64/release/libdrx.so",
    "ext/lib64/release/libdrmgr.so",
    "ext/lib64/release/libdrreg.so",
    "ext/lib64/release/libdrwrap.so",
];

fn run(command: &mut Command, description: &str) {
    eprintln!("hermit-install: {description}: {command:?}");
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("failed to {description}: {error}"));
    assert!(status.success(), "failed to {description}: {status}");
}

fn output(command: &mut Command, description: &str) -> String {
    let result = command
        .output()
        .unwrap_or_else(|error| panic!("failed to {description}: {error}"));
    assert!(
        result.status.success(),
        "failed to {description}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout)
        .unwrap_or_else(|error| panic!("non-UTF-8 output while trying to {description}: {error}"))
        .trim()
        .to_owned()
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

fn ensure_submodule(
    repository: &Path,
    name: &str,
    relative: &str,
    marker: &str,
) -> (PathBuf, String) {
    let source = repository.join(relative);
    if !source.join(marker).is_file() {
        run(
            Command::new("git").arg("-C").arg(repository).args([
                "-c",
                &format!("submodule.{relative}.update=checkout"),
                "submodule",
                "update",
                "--init",
                "--checkout",
                "--depth",
                "1",
                "--recursive",
                "--",
                relative,
            ]),
            &format!("initialize the pinned {name} source"),
        );
    }

    let expected = output(
        Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(["rev-parse", &format!(":{relative}")]),
        &format!("read the pinned {name} revision"),
    );
    let actual = output(
        Command::new("git")
            .arg("-C")
            .arg(&source)
            .args(["rev-parse", "HEAD"]),
        &format!("read the checked-out {name} revision"),
    );
    assert_eq!(
        actual, expected,
        "{name} source is not at the pinned revision"
    );
    (source, expected)
}

fn stage_sabre(resources: &Path, expected_reverie_revision: &str) {
    // Reverie's vendor tree includes the frame/bootstrap repairs that are not
    // in its original third-party/sabre gitlink. Use the loader built by that
    // same Cargo dependency, including its verified source and build recipe.
    let source = reverie_sabre::bundled_sabre_source_dir();
    let revision = output(
        Command::new("git")
            .arg("-C")
            .arg(source)
            .args(["rev-parse", "HEAD"]),
        "read the bundled SaBRe source's Reverie revision",
    );
    assert_eq!(
        revision, expected_reverie_revision,
        "bundled SaBRe does not match the selected Reverie revision"
    );
    copy_file(
        reverie_sabre::bundled_sabre_path(),
        &resources.join("sabre"),
    );
    // This remains a single revision line for existing diagnostic readers,
    // but now identifies the Reverie commit carrying the vendor corrections.
    fs::write(resources.join("sabre.revision"), format!("{revision}\n"))
        .expect("failed to write SaBRe revision provenance");
    copy_file(
        &source.join("REVISION"),
        &resources.join("sabre.upstream-revision"),
    );
}

fn build_e9patch(repository: &Path, build_root: &Path, resources: &Path) {
    let (source, _) = ensure_submodule(repository, "e9patch", "third-party/e9patch", "Makefile");
    let build = build_root.join("e9patch");
    if build.exists() {
        fs::remove_dir_all(&build)
            .unwrap_or_else(|error| panic!("failed to reset {}: {error}", build.display()));
    }
    fs::create_dir_all(&build)
        .unwrap_or_else(|error| panic!("failed to create {}: {error}", build.display()));
    run(
        Command::new("cp")
            .arg("-a")
            .arg(source.join("."))
            .arg(&build),
        "copy the pinned e9patch source into the target directory",
    );

    let mut command = Command::new("make");
    command.arg("-C").arg(&build).arg("release");
    if let Some(jobs) = env::var_os("NUM_JOBS") {
        command.arg(format!("--jobs={}", jobs.to_string_lossy()));
    }
    run(&mut command, "build e9patch");
    copy_file(&build.join("e9tool"), &resources.join("e9tool"));
    copy_file(&build.join("e9patch"), &resources.join("e9patch"));
}

fn copy_licenses(repository_root: &Path, reverie_root: &Path, install: &Path) {
    copy_file(&repository_root.join("LICENSE"), &install.join("LICENSE"));
    let licenses = install.join("licenses");
    for name in [
        "LICENSE",
        "LICENSE.BSD-3",
        "LICENSE.GPL-2",
        "LICENSE.GPL-3",
        "LICENSE.MIT",
    ] {
        copy_file(
            &reverie_sabre::bundled_sabre_source_dir().join(name),
            &licenses.join("sabre").join(name),
        );
    }
    // The bundled loader also links Reverie's vendored libelf statically.
    let libelf = reverie_sabre::bundled_sabre_source_dir()
        .parent()
        .expect("bundled SaBRe source has no vendor directory")
        .join("libelf");
    for name in ["COPYING", "COPYING-LGPLV3"] {
        copy_file(
            &libelf.join(name),
            &licenses.join("sabre/libelf").join(name),
        );
    }
    copy_file(
        &reverie_root.join("third-party/dynamorio/License.txt"),
        &licenses.join("dynamorio/License.txt"),
    );
    copy_file(
        &reverie_root.join("third-party/e9patch/LICENSE"),
        &licenses.join("e9patch/LICENSE"),
    );
}

fn copy_dynamorio(resources: &Path) -> PathBuf {
    let drrun = reverie_dbt::bundled_drrun_path();
    let root = drrun
        .parent()
        .and_then(Path::parent)
        .expect("bundled drrun path has no DynamoRIO root");
    for relative in DYNAMORIO_FILES {
        copy_file(
            &root.join(relative),
            &resources.join("dynamorio").join(relative),
        );
    }
    reverie_dbt::bundled_dynamorio_cmake_dir().to_path_buf()
}

fn build_dbt_client(
    manifest_dir: &Path,
    build_root: &Path,
    resources: &Path,
    dynamorio_cmake: &Path,
) {
    let source = reverie_dbt::native_client_source_dir().join("client.c");
    let build = build_root.join("dbt-client");
    run(
        Command::new("cmake")
            .arg("-S")
            .arg(manifest_dir.join("native-client"))
            .arg("-B")
            .arg(&build)
            .arg("-DCMAKE_BUILD_TYPE=Release")
            .arg(format!("-DDynamoRIO_DIR={}", dynamorio_cmake.display()))
            .arg(format!("-DREVERIE_DBT_NATIVE_SOURCE={}", source.display()))
            .arg(format!("-DHERMIT_RESOURCE_DIR={}", resources.display())),
        "configure the relocatable Detcore DBT client",
    );
    let mut command = Command::new("cmake");
    command.arg("--build").arg(&build).args([
        "--config",
        "Release",
        "--target",
        "reverie_dbt_client",
        "--parallel",
    ]);
    if let Some(jobs) = env::var_os("NUM_JOBS") {
        command.arg(jobs);
    }
    run(&mut command, "build the relocatable Detcore DBT client");
    assert!(
        resources.join("libreverie_dbt_client.so").is_file(),
        "DBT client build did not produce libreverie_dbt_client.so"
    );
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
    // That made the printed remedy -- `cargo build --release -p hermit-install` --
    // regenerate exactly the state that was already failing, so following the
    // error message looped instead of resolving.
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
    println!("cargo:rerun-if-env-changed=HERMIT_INSTALL_FORCE_RESTAGE");
    println!("cargo:rerun-if-changed=../scripts/stage-liteinst-runtime.sh");
    println!("cargo:rerun-if-changed=native-client/CMakeLists.txt");
    println!("cargo:rerun-if-changed=native-client/detcore_dbt_link_stub.c");
    // ⚠️ THE PIN FILES. Without these the staged runtime NEVER REBUILDS WHEN THE
    // REVERIE PIN MOVES: none of the triggers above changes when a pin bump
    // lands, so Cargo considers this script fresh and the stale `.so` stays.
    // A cell then measures the old binary and reports a verdict about the new
    // pin.
    println!("cargo:rerun-if-changed=../detcore/Cargo.toml");
    println!("cargo:rerun-if-changed=../liteinst-runtime-build/Cargo.lock");
    println!("cargo:rerun-if-changed=../liteinst-runtime-build/runtime/Cargo.toml");

    let profile = env::var("PROFILE");
    if profile.as_deref() != Ok("release")
        || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux")
        || env::var("CARGO_CFG_TARGET_ARCH").as_deref() != Ok("x86_64")
    {
        return;
    }
    let profile = profile.expect("Cargo did not set PROFILE");

    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let profile_dir = out_dir
        .ancestors()
        .find(|ancestor| {
            ancestor.file_name().and_then(|name| name.to_str()) == Some(profile.as_str())
        })
        .expect("Cargo OUT_DIR does not have the active profile ancestor")
        .to_path_buf();
    let target_dir = profile_dir
        .parent()
        .expect("Cargo profile directory has no target parent");
    let install = target_dir.join("install_pkg");
    let resources = install.join("rsrcs");
    let build_root = target_dir.join("install-build");

    if install.exists() {
        fs::remove_dir_all(&install)
            .unwrap_or_else(|error| panic!("failed to reset {}: {error}", install.display()));
    }
    fs::create_dir_all(&resources)
        .unwrap_or_else(|error| panic!("failed to create {}: {error}", resources.display()));
    fs::create_dir_all(&build_root)
        .unwrap_or_else(|error| panic!("failed to create {}: {error}", build_root.display()));

    for library in ["libdetcore_dbt.so", "libdetcore_sabre.so"] {
        replace_symlink(
            &resources.join(library),
            &Path::new("../../release").join(library),
        )
        .unwrap_or_else(|error| panic!("failed to link packaged {library}: {error}"));
    }

    let dynamorio_cmake = copy_dynamorio(&resources);
    build_dbt_client(&manifest_dir, &build_root, &resources, &dynamorio_cmake);

    let repository = manifest_dir
        .parent()
        .expect("hermit-install is not inside the Hermit repository");
    build_liteinst_runtime(repository, &build_root, &profile_dir, &resources);

    let reverie_root = reverie_dbt::native_client_source_dir()
        .parent()
        .and_then(Path::parent)
        .expect("reverie-dbt source is not inside the Reverie repository");
    stage_sabre(&resources, &reverie_pin(repository));
    build_e9patch(reverie_root, &build_root, &resources);
    copy_licenses(repository, reverie_root, &install);

    replace_symlink(&install.join("hermit"), Path::new("../release/hermit"))
        .unwrap_or_else(|error| panic!("failed to link install_pkg/hermit: {error}"));
    fs::write(
        install.join("README.txt"),
        "Hermit release staging package. Copy with symlink dereferencing (for example, cp -aL) to create a standalone installation.\n",
    )
    .expect("failed to write install package README");
}

include!("../hermit-cli/reverie_pin.rs");

/// The Reverie revision this tree pins, read from the canonical manifest.
fn reverie_pin(repository: &Path) -> String {
    let Ok(text) = fs::read_to_string(repository.join("detcore/Cargo.toml")) else {
        return "unknown".into();
    };
    parse_reverie_pin(&text).unwrap_or_else(|| "unknown".into())
}
