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
use std::process::Command;

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
        // to. Runtime qualification independently checks declared source bytes
        // and the exact Cargo graph; this marker does not replace source identity.
        .env("HERMIT_LITEINST_REVERIE_PIN", env!("HERMIT_REVERIE_PIN"))
        .arg(cargo_profile)
        .arg(runtime)
        .arg(target_dir.join("liteinst-runtime-build"));
    command
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
    }
}
