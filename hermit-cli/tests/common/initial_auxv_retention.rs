/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A DT_NEEDED constructor may replace an already-retired bootstrap page.
//! Redemption must leave the guest's mapping and its contents intact.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::sync::OnceLock;

static FIXTURE: OnceLock<PathBuf> = OnceLock::new();

fn fixture() -> &'static Path {
    FIXTURE.get_or_init(|| {
        let root = super::process_build_root("initial-auxv-retention");
        fs::create_dir_all(&root).expect("create replacement fixture directory");
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let library = root.join("libscratch-replacement.so");
        let guest = root.join("program");
        let dso = Command::new("cc")
            .args([
                "-O2",
                "-fPIC",
                "-fno-stack-protector",
                "-nostdlib",
                "-shared",
                "-Wl,--build-id=none",
                "-Wl,-soname,libscratch-replacement.so",
            ])
            .arg(source.join("initial_auxv_replacement_dso.c"))
            .arg("-o")
            .arg(&library)
            .output()
            .expect("compile replacement DSO");
        assert!(dso.status.success(), "{dso:?}");
        let main = Command::new("cc")
            .args(["-O2", "-fPIE", "-pie", "-fno-stack-protector"])
            .arg(source.join("initial_auxv_replacement_main.c"))
            .arg(format!("-L{}", root.display()))
            .args([
                "-Wl,--no-as-needed",
                "-lscratch-replacement",
                "-Wl,-rpath,$ORIGIN",
                "-o",
            ])
            .arg(&guest)
            .output()
            .expect("compile replacement main");
        assert!(main.status.success(), "{main:?}");
        let dynamic = Command::new("readelf")
            .arg("-dW")
            .arg(&guest)
            .arg(&library)
            .output()
            .expect("inspect replacement fixture ELF");
        assert!(dynamic.status.success(), "{dynamic:?}");
        let layout = std::str::from_utf8(&dynamic.stdout).unwrap();
        assert!(!layout.contains("PREINIT_ARRAY"), "{layout}");
        assert!(
            layout.contains("Shared library: [libscratch-replacement.so]"),
            "{layout}"
        );
        root
    })
}

fn assert_replacement_survives(backend: &str) {
    let _guard = super::hermit_run_guard();
    let binding = format!("--bind={}:/tmp/scratch-fixture", fixture().display());
    let mut command = super::hermit_command(&[
        "--log=error",
        "--backend",
        backend,
        "run",
        "--strict",
        "--seed=0",
        "--max-timeslice=disabled",
        "--epoch=2026-01-01T00:00:00Z",
        "--base-env=minimal",
        "--mount=type=tmpfs,target=/test",
        "--workdir=/test",
        &binding,
        "--env=LC_ALL=C",
        "--env=TZ=UTC",
        "--",
        "/tmp/scratch-fixture/program",
    ]);
    command.stdin(Stdio::null());
    let output = super::hermit_test::hermit_output_attempts(
        "initial auxv constructor replacement",
        1,
        &mut command,
    );
    assert!(output.status.success(), "{backend}: {output:?}");
    assert_eq!(
        output.stdout,
        b"DSO constructor stored scratch-tail cookie\nmain read scratch-tail cookie\n",
        "{backend}: {output:?}"
    );
    let selected = match backend {
        "liteinst" => {
            "hermit: [liteinst in-guest] selected: the guest preload is to host the Detcore Tool"
        }
        "in-guest-trap" => {
            "hermit: [in-guest-trap] selected: the guest preload is to host the Detcore Tool, with syscall site patching off (REVERIE_LITEINST_SITE_PATCHING=0)"
        }
        _ => panic!("unreviewed backend {backend}"),
    };
    assert!(
        std::str::from_utf8(&output.stderr)
            .unwrap()
            .lines()
            .any(|line| line == selected),
        "{backend}: {output:?}"
    );
}

#[cfg(feature = "liteinst")]
#[test]
fn liteinst_keeps_the_constructor_replacement_of_retired_seed_storage() {
    assert_replacement_survives("liteinst");
}

#[cfg(feature = "liteinst")]
#[test]
fn in_guest_trap_keeps_the_constructor_replacement_of_retired_seed_storage() {
    assert_replacement_survives("in-guest-trap");
}
