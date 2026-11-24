/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! x86-64-v2 stays usable by a real v2 userland, on any host CPU.
//!
//! The synthetic fixtures in `cli.rs` (the `isa_fixture` family) prove
//! Hermit's admission policy for images declaring an ISA level, and the
//! host-userland guests elsewhere in this crate's tests prove the v3
//! profile against the host's own loader. Neither can fail when the
//! *v2 dynamic path* rots: once CI hosts run a v3-built userland
//! (CentOS 10), every host dynamic guest under the named v2 profile is
//! refused or auto-upgraded, so no test exercises a real ld.so and libc
//! computing their CPU level from Hermit's v2 CPUID table.
//!
//! This test pins the userland instead. It assembles the minimum
//! CentOS Stream 9 rootfs (real `ld-linux` + `libc.so.6`, nothing else
//! from the distribution) as an OCI directory, in the style of
//! `tests/image_archive.rs`, and runs it through Hermit's own
//! `--image=oci-dir:` container path under the explicitly named
//! `--cpu-profile=x86-64-v2`:
//!
//! - the fixture loader's own diagnostics report exactly x86-64-v2
//!   (`isa_1=0x3`, v2 "supported, searched", v3/v4 unsupported) — the
//!   dynamic-loader ISA enforcement a static guest cannot exercise;
//! - a CPUID probe guest (`fixtures/isa/cpuid_v2_probe.c`, dynamically
//!   linked against the fixture libc) asserts every v2 feature
//!   present and every above-v2 feature absent — AVX, BMI1, HLE,
//!   AVX2, BMI2, RTM and ADX included, the seven the pre-psABI
//!   table advertised until
//!   https://github.com/facebookexperimental/hermit/issues/124
//!   was fixed — and runs at bitwise parity under
//!   `--strict --verify --verify-strict` on the ptrace backend (L2);
//! - a library declaring x86-64-v3 is refused by the *guest's ld.so*
//!   ("CPU ISA level is lower than required", no Hermit policy
//!   refusal), and a v3-declaring executable gets Hermit's named
//!   policy refusal (exit 122) before it runs;
//! - an image that *executes* a v3 instruction while declaring
//!   nothing is not refused: admission is declaration-based and the
//!   presented CPUID is not an instruction sandbox on a v3 host.
//!   That known limit is asserted as observed behavior, not
//!   discovered later as a surprise.
//!
//! The rootfs bytes are never committed. The two distribution files
//! come from the directory named by `HERMIT_TEST_V2_ROOTFS` (in
//! fbsource, an antlir2 layer resource; in OSS, the extraction
//! documented in `fixtures/x86-64-v2-rootfs/README.md`):
//!
//! ```text
//! $HERMIT_TEST_V2_ROOTFS/lib64/ld-linux-x86-64.so.2
//! $HERMIT_TEST_V2_ROOTFS/lib64/libc.so.6
//! ```
//!
//! Without them the tests skip through `HERMIT-TEST-SKIP` — unless
//! `HERMIT_TEST_REQUIRE_V2_ROOTFS` is set, the must-run lane's switch
//! (the pattern of `HERMIT_TEST_REQUIRE_CPU_PROFILE_V3`), in which
//! case every skip is a failure so this coverage cannot rot silently.
//!
//! Red twins (run when this test was introduced, recorded with the
//! change): forcing `--cpu-profile=x86-64-v3` makes the probe fail
//! its hidden-feature checks and the loader report `isa_1=0x7`; a
//! fixture whose `libc.so.6` declares x86-64-v3 makes the loader
//! refuse to start any guest. Either drift turns these tests red.

#![cfg(target_arch = "x86_64")]

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;

use hermit_test::test_skip;

/// Names the directory holding the fixture's distribution files.
const FIXTURE_ENV: &str = "HERMIT_TEST_V2_ROOTFS";
/// Set on the lane that must run these tests: a skip becomes a failure.
const REQUIRE_ENV: &str = "HERMIT_TEST_REQUIRE_V2_ROOTFS";
/// Hermit's policy-refusal exit status (`HERMIT_POLICY_REFUSAL_EXIT`
/// in `tests/cli.rs`).
const POLICY_REFUSAL_EXIT: i32 = 122;

const LOADER: &str = "/lib64/ld-linux-x86-64.so.2";
const PROBE: &str = "/usr/bin/cpuid-v2-probe";
const NEEDS_V3_DEP: &str = "/usr/bin/needs-v3-dep";
const NEEDS_V3: &str = "/usr/bin/needs-v3";
const UNDECLARED_V3: &str = "/usr/bin/undeclared-v3";

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Skip `test` for `reason`, or fail when the must-run lane required it.
fn skip(test: &str, reason: &str) {
    assert!(
        std::env::var_os(REQUIRE_ENV).is_none(),
        "skipping {test} ({reason}) but {REQUIRE_ENV} is set"
    );
    test_skip::record_test_skip(test, reason);
}

/// The fixture input directory, when it names both distribution files.
fn fixture_input() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os(FIXTURE_ENV)?);
    if dir.join("lib64/ld-linux-x86-64.so.2").is_file() && dir.join("lib64/libc.so.6").is_file() {
        Some(dir)
    } else {
        None
    }
}

fn sha256_hex(data: &[u8]) -> String {
    detcore::Digest::new(data).to_string()
}

struct Entry {
    path: &'static str,
    mode: u32,
    data: Vec<u8>,
    dir: bool,
}

fn file_entry(path: &'static str, mode: u32, data: Vec<u8>) -> Entry {
    Entry {
        path,
        mode,
        data,
        dir: false,
    }
}

fn dir_entry(path: &'static str) -> Entry {
    Entry {
        path,
        mode: 0o755,
        data: Vec::new(),
        dir: true,
    }
}

/// One plain tar layer with fixed metadata, `tests/image_archive.rs` style.
fn build_tar(entries: &[Entry]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for e in entries {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(if e.dir {
            tar::EntryType::Directory
        } else {
            tar::EntryType::Regular
        });
        header.set_mode(e.mode);
        header.set_mtime(1_700_000_000);
        header.set_uid(0);
        header.set_gid(0);
        header.set_size(e.data.len() as u64);
        builder
            .append_data(&mut header, e.path, e.data.as_slice())
            .unwrap();
    }
    builder.into_inner().unwrap()
}

fn write_blob(root: &Path, bytes: &[u8]) -> String {
    let hex = sha256_hex(bytes);
    let path = root.join("blobs").join("sha256").join(&hex);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();
    hex
}

/// A one-layer OCI layout directory holding `rootfs_tar`.
fn write_oci_dir(root: &Path, rootfs_tar: &[u8]) {
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(
        root.join("oci-layout"),
        br#"{"imageLayoutVersion":"1.0.0"}"#,
    )
    .unwrap();
    let config = serde_json::to_vec(&serde_json::json!({
        "architecture": "amd64",
        "os": "linux",
        "created": "2024-01-01T00:00:00Z",
        "config": {
            "Env": ["PATH=/usr/bin"],
            "WorkingDir": "/",
        },
        "rootfs": {
            "type": "layers",
            "diff_ids": [format!("sha256:{}", sha256_hex(rootfs_tar))],
        }
    }))
    .unwrap();
    let config_hex = write_blob(root, &config);
    let layer_hex = write_blob(root, rootfs_tar);
    let manifest = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": format!("sha256:{config_hex}"),
            "size": config.len(),
        },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar",
            "digest": format!("sha256:{layer_hex}"),
            "size": rootfs_tar.len(),
        }],
    }))
    .unwrap();
    let manifest_hex = write_blob(root, &manifest);
    let index = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": format!("sha256:{manifest_hex}"),
            "size": manifest.len(),
            "platform": {"architecture": "amd64", "os": "linux"},
        }],
    }))
    .unwrap();
    std::fs::write(root.join("index.json"), index).unwrap();
}

/// Compile one fixture guest; `Err` carries the compiler output so the
/// caller can skip (or, on the must-run lane, fail) with it named.
fn compile(build: &Path, name: &str, args: &[&str]) -> Result<PathBuf, String> {
    let out = build.join(name);
    let output = Command::new("cc")
        .args(args)
        .arg("-o")
        .arg(&out)
        .output()
        .map_err(|e| format!("cc could not run: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "compiling {name} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(out)
}

/// The guest binaries that `build_fixture` packs into the image.
struct Guests {
    probe: Vec<u8>,
    dep: Vec<u8>,
    twin: Vec<u8>,
    lib: Vec<u8>,
    undeclared: Vec<u8>,
}

/// The guests, compiled against no host libc symbols (see the probe's
/// comment), so the same binaries run against the fixture libc
/// whatever the build host's distribution is. The sources travel with
/// this test binary (`include_str!`), so neither Cargo's
/// `CARGO_MANIFEST_DIR` nor a Buck staging layout decides whether
/// they are found.
fn build_guests(build: &Path) -> Result<Guests, String> {
    let write_src = |name: &str, text: &str| -> Result<String, String> {
        let path = build.join(name);
        std::fs::write(&path, text).map_err(|e| format!("writing {name}: {e}"))?;
        Ok(path.to_str().unwrap().to_owned())
    };
    let probe_src = write_src(
        "cpuid_v2_probe.c",
        include_str!("fixtures/isa/cpuid_v2_probe.c"),
    )?;
    let probe_src = probe_src.as_str();
    let lib_src = write_src(
        "needs_v3_lib.c",
        include_str!("fixtures/isa/needs_v3_lib.c"),
    )?;
    let lib_src = lib_src.as_str();
    let isa_src = write_src("isa_needed.c", include_str!("fixtures/isa/isa_needed.c"))?;
    let isa_src = isa_src.as_str();
    let undeclared_src = write_src(
        "undeclared_v3.c",
        include_str!("fixtures/isa/undeclared_v3.c"),
    )?;
    let undeclared_src = undeclared_src.as_str();
    let dynamic = [
        "-O1",
        "-nostdlib",
        "-fPIE",
        "-pie",
        "-march=x86-64-v2",
        "-Wall",
        "-Wextra",
        "-Werror",
        "-Wl,--dynamic-linker=/lib64/ld-linux-x86-64.so.2",
    ];
    let lib = compile(
        build,
        "libneeds-v3.so",
        &[
            "-O1",
            "-shared",
            "-fPIC",
            "-march=x86-64-v3",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-Wl,-soname,libneeds-v3.so",
            lib_src,
        ],
    )?;
    let mut probe_args: Vec<&str> = dynamic.to_vec();
    probe_args.extend([probe_src, "-Wl,--no-as-needed", "-lc"]);
    let probe = compile(build, "cpuid-v2-probe", &probe_args)?;
    let lib_dir = format!("-L{}", build.display());
    let mut dep_args: Vec<&str> = dynamic.to_vec();
    dep_args.extend([
        probe_src,
        "-Wl,--no-as-needed",
        &lib_dir,
        "-lneeds-v3",
        "-lc",
    ]);
    let dep = compile(build, "needs-v3-dep", &dep_args)?;
    let twin = compile(
        build,
        "needs-v3",
        &[
            "-O1",
            "-nostdlib",
            "-fPIE",
            "-pie",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-DISA_NEEDED=\"0x7\"",
            "-DISA_EXIT=42",
            "-Wl,--dynamic-linker=/lib64/ld-linux-x86-64.so.2",
            isa_src,
        ],
    )?;
    // Baseline -march and a byte-encoded PDEP, so neither the
    // compiler's target features nor its property notes declare v3.
    let undeclared = compile(
        build,
        "undeclared-v3",
        &[
            "-O1",
            "-nostdlib",
            "-fPIE",
            "-pie",
            "-march=x86-64",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-Wl,--dynamic-linker=/lib64/ld-linux-x86-64.so.2",
            undeclared_src,
            "-Wl,--no-as-needed",
            "-lc",
        ],
    )?;
    let read = |p: PathBuf| std::fs::read(p).map_err(|e| e.to_string());
    Ok(Guests {
        probe: read(probe)?,
        dep: read(dep)?,
        twin: read(twin)?,
        lib: read(lib)?,
        undeclared: read(undeclared)?,
    })
}

/// Build the OCI fixture under `build` (an `oci` directory plus a
/// per-test XDG cache), or `None` after recording why `test` skips.
fn build_fixture(test: &str, build: &Path) -> Option<PathBuf> {
    let Some(input) = fixture_input() else {
        skip(
            test,
            &format!(
                "{FIXTURE_ENV} does not name a directory holding lib64/ld-linux-x86-64.so.2 and lib64/libc.so.6"
            ),
        );
        return None;
    };
    std::fs::create_dir_all(build).unwrap();
    let Guests {
        probe,
        dep,
        twin,
        lib,
        undeclared,
    } = match build_guests(build) {
        Ok(guests) => guests,
        Err(why) => {
            skip(test, &why);
            return None;
        }
    };
    let read = |rel: &str| std::fs::read(input.join(rel)).unwrap();
    let tar = build_tar(&[
        dir_entry("lib64"),
        dir_entry("usr"),
        dir_entry("usr/bin"),
        dir_entry("proc"),
        dir_entry("dev"),
        dir_entry("tmp"),
        file_entry(
            "lib64/ld-linux-x86-64.so.2",
            0o755,
            read("lib64/ld-linux-x86-64.so.2"),
        ),
        file_entry("lib64/libc.so.6", 0o755, read("lib64/libc.so.6")),
        file_entry("lib64/libneeds-v3.so", 0o755, lib),
        file_entry("usr/bin/cpuid-v2-probe", 0o755, probe),
        file_entry("usr/bin/needs-v3-dep", 0o755, dep),
        file_entry("usr/bin/needs-v3", 0o755, twin),
        file_entry("usr/bin/undeclared-v3", 0o755, undeclared),
    ]);
    let oci = build.join("oci");
    write_oci_dir(&oci, &tar);
    Some(oci)
}

fn build_root(name: &str) -> PathBuf {
    // Cargo sets CARGO_TARGET_TMPDIR; a Buck-built binary gets it from
    // its target's env, or falls back to the system temp dir. Only
    // host-side scratch lives here (the OCI dir Hermit materializes
    // and the XDG cache); guests run from inside the image.
    let base = match option_env!("CARGO_TARGET_TMPDIR") {
        Some(dir) => PathBuf::from(dir),
        None => std::env::temp_dir(),
    };
    base.join(format!("{name}-{}", std::process::id()))
}

/// One Hermit run of `guest` (with `guest_args`) in the fixture image,
/// under the named v2 profile, with `prefix` options before `run` and
/// `options` between the image and the guest.
fn run_guest(
    build: &Path,
    oci: &Path,
    prefix: &[&str],
    options: &[String],
    guest: &str,
    guest_args: &[&str],
) -> Output {
    let mut command = Command::new(hermit_test::hermit_binary());
    // Hermit writes its private per-run summaries under its cwd
    // (reached from the container through /proc/self/fd). Keep that
    // cwd in this test's host scratch, never the source checkout:
    // on a read-only EdenFS CI checkout the verify run's summary
    // write fails in cleanup with Permission denied after the
    // guest itself exited 0 (D132128626's first CI run).
    command
        .current_dir(build)
        .args(prefix)
        .arg("run")
        .arg(format!("--image=oci-dir:{}", oci.display()))
        .args(["--cpu-profile", "x86-64-v2"])
        .args(options)
        .arg("--")
        .arg(guest)
        .args(guest_args)
        .env("XDG_CACHE_HOME", build.join("xdg-cache"))
        .stdin(std::process::Stdio::null());
    let label = format!("hermit run --image v2-rootfs {guest}");
    hermit_test::hermit_output(&label, &mut command)
}

/// The probe's exact verdict lines after its `v2` header, in order.
/// The seven `* hidden: ok` lines for AVX, BMI1, HLE, AVX2, BMI2,
/// RTM and ADX are what facebookexperimental/hermit#124 fixed;
/// they were strict expected failures until that change, and are
/// ordinary assertions now.
const PROBE_EXPECTED: [&str; 14] = [
    "sse2 advertised: ok",
    "v2 features advertised: ok",
    "v3 leaf1 features hidden: ok",
    "rdrand masked: ok",
    "avx hidden: ok",
    "bmi1 hidden: ok",
    "hle hidden: ok",
    "avx2 hidden: ok",
    "bmi2 hidden: ok",
    "rtm hidden: ok",
    "adx hidden: ok",
    "avx512f and rdseed hidden: ok",
    "lahf advertised: ok",
    "lzcnt hidden: ok",
];

fn assert_probe_output(output: &Output) {
    let stdout = stdout_of(output);
    assert_eq!(
        output.status.code(),
        Some(0),
        "probe run failed\nstdout:\n{stdout}\nstderr:\n{}",
        stderr_of(output)
    );
    let mut lines = stdout.lines();
    assert_eq!(lines.next(), Some("v2"), "{stdout}");
    assert_eq!(lines.collect::<Vec<_>>(), PROBE_EXPECTED, "{stdout}");
}

/// The fixture loader, run as the guest, computes exactly x86-64-v2
/// from Hermit's intercepted CPUID: this is the real glibc ISA
/// enforcement (feature detection, glibc-hwcaps search order) that a
/// host-userland test stops covering once hosts ship a v3 glibc.
#[test]
fn v2_rootfs_loader_sees_exactly_x86_64_v2() {
    let test = "v2_rootfs_loader_sees_exactly_x86_64_v2";
    let build = build_root("v2-rootfs-loader");
    let Some(oci) = build_fixture(test, &build) else {
        return;
    };
    let diagnostics = run_guest(&build, &oci, &[], &[], LOADER, &["--list-diagnostics"]);
    let stdout = stdout_of(&diagnostics);
    assert_eq!(
        diagnostics.status.code(),
        Some(0),
        "loader diagnostics failed\nstdout:\n{stdout}\nstderr:\n{}",
        stderr_of(&diagnostics)
    );
    let isa = stdout
        .lines()
        .find_map(|line| line.strip_prefix("x86.cpu_features.isa_1="))
        .unwrap_or_else(|| panic!("isa_1 missing from loader diagnostics:\n{stdout}"));
    assert_eq!(isa, "0x3", "loader ISA level is not exactly v2:\n{stdout}");

    let help = run_guest(&build, &oci, &[], &[], LOADER, &["--help"]);
    let stdout = stdout_of(&help);
    assert_eq!(
        help.status.code(),
        Some(0),
        "loader --help failed\nstdout:\n{stdout}\nstderr:\n{}",
        stderr_of(&help)
    );
    assert!(
        stdout.contains("x86-64-v2 (supported, searched)"),
        "{stdout}"
    );
    for unsupported in ["x86-64-v3", "x86-64-v4"] {
        assert!(
            stdout.lines().any(|line| line.trim() == unsupported),
            "{unsupported} must be listed unsupported:\n{stdout}"
        );
    }
}

/// A dynamic guest linked against the fixture libc sees the v2 CPU
/// (with the seven formerly leaked features now hidden) and runs
/// deterministically: `--strict --verify --verify-strict`
/// on the ptrace backend with `bitwise_parity: true` (L2), the v2
/// counterpart of `cli.rs`'s v3 parity run but with its userland
/// pinned instead of borrowed from the host.
#[test]
fn v2_rootfs_probe_runs_at_bitwise_parity() {
    let test = "v2_rootfs_probe_runs_at_bitwise_parity";
    let build = build_root("v2-rootfs-probe");
    let Some(oci) = build_fixture(test, &build) else {
        return;
    };
    assert_probe_output(&run_guest(&build, &oci, &[], &[], PROBE, &[]));

    let build_attempts = build.clone();
    let oci_attempts = oci.clone();
    let (output, verdict) = hermit_test::hermit_output_with(test, move || {
        let verdict = build_attempts.join("verify.json");
        let output = run_guest(
            &build_attempts,
            &oci_attempts,
            &[],
            &[
                "--strict".to_string(),
                "--verify".to_string(),
                "--verify-strict".to_string(),
                format!("--verify-json={}", verdict.display()),
            ],
            PROBE,
            &[],
        );
        (output, verdict)
    });
    assert_probe_output(&output);
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&verdict).unwrap_or_default())
            .expect("the verdict must be JSON");
    assert_eq!(report["bitwise_parity"], true, "{report}");
}

/// The negative twins. A guest depending on a library that declares
/// x86-64-v3 is refused by the fixture's ld.so — proof the loader
/// itself enforces the level Hermit presents, on a path (a DT_NEEDED
/// dependency) Hermit's exec-time admission does not scan. A
/// v3-declaring executable gets Hermit's named policy refusal before
/// its first instruction.
#[test]
fn v2_rootfs_refuses_v3_images_at_loader_and_policy() {
    let test = "v2_rootfs_refuses_v3_images_at_loader_and_policy";
    let build = build_root("v2-rootfs-twins");
    let Some(oci) = build_fixture(test, &build) else {
        return;
    };
    let dep = run_guest(&build, &oci, &[], &[], NEEDS_V3_DEP, &[]);
    let (stdout, stderr) = (stdout_of(&dep), stderr_of(&dep));
    assert_ne!(dep.status.code(), Some(0), "{stdout}\n{stderr}");
    assert!(stdout.is_empty(), "the refused guest ran: {stdout}");
    assert!(
        stderr.contains("CPU ISA level is lower than required"),
        "not the loader's refusal:\n{stderr}"
    );
    assert!(
        !stderr.contains("HERMIT_POLICY_REFUSAL"),
        "must be refused by ld.so, not Hermit policy:\n{stderr}"
    );

    let twin = run_guest(&build, &oci, &[], &[], NEEDS_V3, &[]);
    let (stdout, stderr) = (stdout_of(&twin), stderr_of(&twin));
    assert_eq!(twin.status.code(), Some(POLICY_REFUSAL_EXIT), "{stderr}");
    assert!(!stdout.contains("ran"), "the refused image ran: {stdout}");
    assert!(
        stderr.contains("HERMIT_POLICY_REFUSAL")
            && stderr.contains("--cpu-profile x86-64-v2 refused:")
            && stderr.contains("declares that it needs x86-64-v3"),
        "{stderr}"
    );
}

/// The known limit beside the twins: an image that executes a v3
/// instruction (a byte-encoded PDEP) while declaring no ISA level
/// is *not* refused. Admission reads declarations
/// (`GNU_PROPERTY_X86_ISA_1_NEEDED` on the executable and its
/// interpreter, plus the loader's own check on its dependencies);
/// nothing traps the instruction itself, so on a host whose CPU
/// implements BMI2 the PDEP simply runs under the v2 profile —
/// the presented CPUID is not an instruction sandbox. On a host
/// without BMI2 the same bytes take the hardware's #UD instead:
/// also not a policy refusal, and no marker is printed. Either
/// way this test names the limit instead of discovering it later.
#[test]
fn v2_rootfs_undeclared_v3_instruction_is_not_refused() {
    let test = "v2_rootfs_undeclared_v3_instruction_is_not_refused";
    let build = build_root("v2-rootfs-undeclared");
    let Some(oci) = build_fixture(test, &build) else {
        return;
    };
    let out = run_guest(&build, &oci, &[], &[], UNDECLARED_V3, &[]);
    let (stdout, stderr) = (stdout_of(&out), stderr_of(&out));
    assert!(
        !stderr.contains("HERMIT_POLICY_REFUSAL"),
        "an undeclared image must not take the declaration refusal:\n{stderr}"
    );
    if std::is_x86_feature_detected!("bmi2") {
        assert_eq!(
            (out.status.code(), stdout.as_str()),
            (Some(0), "undeclared-v3: ran pdep\n"),
            "the host has BMI2, so the undeclared PDEP runs:\n{stderr}"
        );
    } else {
        assert_ne!(out.status.code(), Some(0), "{stdout}\n{stderr}");
        assert_ne!(
            out.status.code(),
            Some(POLICY_REFUSAL_EXIT),
            "{stdout}\n{stderr}"
        );
        assert!(!stdout.contains("ran pdep"), "{stdout}");
    }
}
