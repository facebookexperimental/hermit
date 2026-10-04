/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process;
use std::process::Command;
use std::sync::OnceLock;

static LITEINST_RUNTIME: OnceLock<()> = OnceLock::new();

pub(super) fn hermit_binary() -> PathBuf {
    std::env::var_os("HERMIT_LITEINST_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_hermit")))
}

pub(super) fn liteinst_runtime_library() -> PathBuf {
    hermit_binary()
        .parent()
        .expect("Hermit test binary should have a profile directory")
        .join("libreverie_liteinst.so")
}

pub(super) fn staged_runtime_matches_current_pin(runtime: &Path) -> bool {
    if !runtime.is_file() {
        return false;
    }
    let revision = PathBuf::from(format!("{}.revision", runtime.display()));
    fs::read_to_string(revision).is_ok_and(|staged| staged.trim() == env!("HERMIT_REVERIE_PIN"))
}

fn cargo_build_profile_and_target(compiled_hermit: &Path) -> (OsString, PathBuf) {
    let profile_dir = compiled_hermit
        .parent()
        .expect("compiled Hermit should have a Cargo profile directory");
    let profile = profile_dir
        .file_name()
        .expect("compiled Hermit profile directory should have a name");
    let cargo_profile = if profile == OsStr::new("debug") {
        OsString::from("dev")
    } else {
        profile.to_owned()
    };
    let target_dir = profile_dir
        .parent()
        .expect("compiled Hermit profile should be inside a target directory")
        .to_owned();
    (cargo_profile, target_dir)
}

/// The profile of the separate `liteinst-runtime-build` workspace that matches a
/// Hermit profile. That workspace defines only Cargo's built-in profiles, and
/// `hermit-install` builds the runtime in `release` for every release-derived
/// Hermit profile (`release`, `validate`), so only `dev` maps to itself.
fn liteinst_runtime_profile(hermit_profile: OsString) -> OsString {
    if hermit_profile == OsStr::new("dev") {
        hermit_profile
    } else {
        OsString::from("release")
    }
}

fn stage_existing_runtime(source: &Path, destination: &Path) -> bool {
    if !staged_runtime_matches_current_pin(source) {
        return false;
    }
    let source_revision = PathBuf::from(format!("{}.revision", source.display()));
    let destination_revision = PathBuf::from(format!("{}.revision", destination.display()));
    let Some(parent) = destination.parent() else {
        return false;
    };
    if fs::create_dir_all(parent).is_err() {
        return false;
    }
    let temporary = parent.join(format!(".libreverie_liteinst.so.copy.{}", process::id()));
    let temporary_revision = PathBuf::from(format!("{}.revision", temporary.display()));
    let result = (|| {
        let before = fs::read(source).ok()?;
        fs::copy(source, &temporary).ok()?;
        let after = fs::read(source).ok()?;
        let copied = fs::read(&temporary).ok()?;
        if before != after || before != copied {
            return None;
        }
        fs::copy(source_revision, &temporary_revision).ok()?;
        fs::rename(&temporary, destination).ok()?;
        fs::rename(&temporary_revision, destination_revision).ok()?;
        staged_runtime_matches_current_pin(destination).then_some(())
    })()
    .is_some();
    let _ = fs::remove_file(temporary);
    let _ = fs::remove_file(temporary_revision);
    result
}

/// The command that builds the LiteInst runtime and stages it at `runtime`.
///
/// The selected Hermit may be the validated staged artifact under target/ci.
/// That directory is not a Cargo profile. Derive build settings only from the
/// binary Cargo compiled for this test.
pub(super) fn liteinst_stage_command(runtime: &Path) -> Command {
    let compiled_hermit = PathBuf::from(env!("CARGO_BIN_EXE_hermit"));
    let (cargo_profile, target_dir) = cargo_build_profile_and_target(&compiled_hermit);
    let cargo_profile = liteinst_runtime_profile(cargo_profile);
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let mut command = Command::new(repository.join("scripts/stage-liteinst-runtime.sh"));
    command
        .current_dir(repository)
        // The marker must name the pin this Hermit binary was built with, which
        // is what `staged_runtime_matches_current_pin` and the loader compare it
        // to. Handing it over also means staging needs no git checkout
        // (https://github.com/rrnewton/hermit/issues/3419).
        .env("HERMIT_LITEINST_REVERIE_PIN", env!("HERMIT_REVERIE_PIN"))
        .arg(cargo_profile)
        .arg(runtime)
        .arg(target_dir.join("liteinst-runtime-build"));
    command
}

/// Where a release-derived build of this test's own Cargo profile staged the
/// LiteInst runtime: `hermit-install`'s build script writes it beside the
/// profile's Hermit (`target/<profile>/libreverie_liteinst.so`) for `release`
/// and for every profile that inherits it, such as `validate`.
fn profile_staged_runtime(compiled_hermit: &Path) -> PathBuf {
    compiled_hermit
        .parent()
        .expect("compiled Hermit should have a Cargo profile directory")
        .join("libreverie_liteinst.so")
}

pub(super) fn ensure_liteinst_runtime() {
    LITEINST_RUNTIME.get_or_init(|| {
        // Continue to stage the runtime beside the selected Hermit.
        let compiled_hermit = PathBuf::from(env!("CARGO_BIN_EXE_hermit"));
        let runtime = liteinst_runtime_library();
        if staged_runtime_matches_current_pin(&runtime) {
            return;
        }
        let profile_runtime = profile_staged_runtime(&compiled_hermit);
        if stage_existing_runtime(&profile_runtime, &runtime) {
            return;
        }
        let output = liteinst_stage_command(&runtime)
            .output()
            .expect("failed to build the LiteInst runtime");
        assert!(
            output.status.success(),
            "LiteInst runtime build failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            runtime.is_file(),
            "standalone LiteInst runtime build did not stage {}",
            runtime.display(),
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_ci_artifact_never_becomes_a_cargo_profile() {
        let selected = Path::new("/checkout/target/ci/hermit-strict");
        let (profile, target) =
            cargo_build_profile_and_target(Path::new("/checkout/target/debug/hermit"));
        assert_eq!(profile, OsStr::new("dev"));
        assert_eq!(target, Path::new("/checkout/target"));
        assert_eq!(
            selected.parent().unwrap().join("libreverie_liteinst.so"),
            Path::new("/checkout/target/ci/libreverie_liteinst.so")
        );

        let (profile, target) =
            cargo_build_profile_and_target(Path::new("/checkout/target/release/hermit"));
        assert_eq!(profile, OsStr::new("release"));
        assert_eq!(target, Path::new("/checkout/target"));
        let (profile, target) =
            cargo_build_profile_and_target(Path::new("/checkout/target/validate/hermit"));
        assert_eq!(profile, OsStr::new("validate"));
        assert_eq!(target, Path::new("/checkout/target"));
        assert_eq!(liteinst_runtime_profile(profile), OsStr::new("release"));
        assert_eq!(
            liteinst_runtime_profile(OsString::from("dev")),
            OsStr::new("dev")
        );
        assert_eq!(
            profile_staged_runtime(Path::new("/checkout/target/validate/hermit")),
            Path::new("/checkout/target/validate/libreverie_liteinst.so")
        );

        let fixture = tempfile::tempdir().unwrap();
        let source = fixture
            .path()
            .join("target/validate/libreverie_liteinst.so");
        let destination = fixture.path().join("target/ci/libreverie_liteinst.so");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, b"runtime-bytes\n").unwrap();
        fs::write(
            format!("{}.revision", source.display()),
            format!("{}\n", env!("HERMIT_REVERIE_PIN")),
        )
        .unwrap();
        assert!(stage_existing_runtime(&source, &destination));
        assert_eq!(fs::read(&destination).unwrap(), b"runtime-bytes\n");
        assert!(staged_runtime_matches_current_pin(&destination));
        fs::write(
            format!("{}.revision", source.display()),
            format!("{}\n", "0".repeat(40)),
        )
        .unwrap();
        fs::remove_file(&destination).unwrap();
        fs::remove_file(format!("{}.revision", destination.display())).unwrap();
        assert!(!stage_existing_runtime(&source, &destination));
    }
}
