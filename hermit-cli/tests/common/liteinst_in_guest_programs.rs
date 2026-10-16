/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Real programs under in-guest LiteInst (`--backend=liteinst`), which runs
//! Detcore's Tool inside the guest through the `detcore-liteinst` preload
//! (https://github.com/rrnewton/hermit/issues/3520).
//!
//! These tests were `hermit-cli/tests/liteinst_advanced.rs`, which ran the same
//! programs under the ptrace-hosted LiteInst hybrid. When Hermit's LiteInst
//! became in-guest only they moved here, into the `cli` test binary, and kept
//! their programs, inputs and expected output. Three things differ, each
//! because in-guest LiteInst refuses what the hybrid accepted (see
//! `refuse_unqualified_liteinst_in_guest_options` in
//! `hermit-cli/src/bin/hermit/run.rs`):
//!
//! - Every run passes `--max-timeslice=disabled`. The in-guest Tool host
//!   cannot deliver Detcore's preemption timer yet, so a run with a maximum
//!   timeslice is refused. None of these guests depends on preemption.
//! - No run passes `--verify` yet. The exact expected output each test
//!   asserts remains; the determinism verdict does not. The in-guest Tool now
//!   forwards its records (see
//!   `liteinst_in_guest_verify_compares_the_records_the_guest_forwards` in
//!   `cli.rs`), so restoring it is the next step of
//!   https://github.com/rrnewton/hermit/issues/3520.
//! - The hybrid's activation banner and `--verify` banner are gone, so each
//!   run instead asserts Hermit's in-guest selection line and the absence of
//!   any host-hybrid line.
//!
//! The runtime library is the `libdetcore_liteinst.so` beside the Hermit this
//! binary was built with. The validation builds put it there:
//! `build.workspace_compile_in_pinned_root` and `build.workspace_on_host` run
//! `cargo build --profile validate --workspace --all-targets`, which builds the
//! `detcore-liteinst` cdylib next to `target/validate/hermit`. A plain
//! `cargo nextest run -p hermit --test cli` does not build it; run
//! `cargo build -p detcore-liteinst` (in the same profile) first, or Hermit
//! refuses the run and names that command.

use std::fs;
use std::io::Read;
use std::io::Seek;
use std::io::Write;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use super::dispatch_stats;
use super::hermit_run_guard;
use super::process_build_root;

static LITEINST_ADVANCED_GUEST: OnceLock<PathBuf> = OnceLock::new();
static LITEINST_MMAP_GUEST: OnceLock<PathBuf> = OnceLock::new();
static USERFAULTFD_GUEST: OnceLock<PathBuf> = OnceLock::new();
static CONSTRUCTOR_MUTATION_GUEST: OnceLock<PathBuf> = OnceLock::new();
static EXIT_REAPING_GUEST: OnceLock<PathBuf> = OnceLock::new();
static UNSCHEDULED_EXIT_GUEST: OnceLock<PathBuf> = OnceLock::new();
static SELF_SIGNAL_DEATH_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DUP_ALIAS_GUEST: OnceLock<PathBuf> = OnceLock::new();
static CLOSE_RANGE_PORT_GUEST: OnceLock<PathBuf> = OnceLock::new();
static IOV_OVERWRITTEN_GUEST: OnceLock<PathBuf> = OnceLock::new();
static LITEINST_COMPAT_FIXTURE: OnceLock<PathBuf> = OnceLock::new();
static LITEINST_SEMANTIC_FIXTURE: OnceLock<PathBuf> = OnceLock::new();
static LITEINST_COMPRESSED_FIXTURES: OnceLock<[PathBuf; 2]> = OnceLock::new();

const COMPAT_FIXTURE_CONTENT: &[u8] = b"liteinst compatibility fixture\n";
const COMPAT_FIXTURE_SHA256: &str =
    "e5c4447a0a9f796a0b72bb47875e9879aa7722c74e601385e74058f029ae60cd";
const COMPAT_FIXTURE_SHA1: &str = "41396e2c2d5ce6332143190b04e78ba101db58f8";
const COMPAT_FIXTURE_SHA224: &str = "344c0ace4382f9d738db9a385af4435e493e876fdc334c21485917ba";
const COMPAT_FIXTURE_SHA384: &str = "38184361b2dbdee2b75d92506acf3ab1dba402eed33cc0691841d5b33521382e5752437b3cfa2232d2241ad6baaf5fa9";
const COMPAT_FIXTURE_SHA512: &str = "2c856cc937ac0a50cedf2a3d3d0a6c10570791ace2e3cd44374a1308844bc2acca0fd6e100b38306da9844c7248bebcca3fd46a1c2651b98d36f588126925078";
const COMPAT_FIXTURE_BLAKE2: &str = "d69629a852f326482ab1e50881d63a17028e3205b66a6a54d7d85c0cb9ceff149ba03c45585a6a94e1a1edd120fe50c44e9dfce62830ffac3460a57bde29c5aa";
const SEMANTIC_FIXTURE_CONTENT: &[u8] = b"gamma:3\nalpha:1\nalpha:1\nbeta:2\n";
const SEMANTIC_FIXTURE_MD5: &str = "c61c6cb65c4b5e1a6f3eb32b601db629";

/// The line Hermit prints when it runs the guest under in-guest LiteInst
/// (`RunOpts::main` in `hermit-cli/src/bin/hermit/run.rs`).
const IN_GUEST_SELECTED: &str =
    "hermit: [liteinst in-guest] selected: the guest preload is to host the Detcore Tool";

fn hermit_binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_hermit"))
}

fn group_name_by_gid<'a>(contents: &'a str, gid: &str) -> Option<&'a str> {
    contents.lines().find_map(|line| {
        let mut fields = line.split(':');
        let name = fields.next()?;
        fields.next()?;
        (fields.next()? == gid).then_some(name)
    })
}

fn advanced_guest() -> &'static Path {
    LITEINST_ADVANCED_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("liteinst-advanced");
        fs::create_dir_all(&build_root).expect("failed to create LiteInst guest directory");
        let guest = build_root.join("liteinst_advanced");
        let output = Command::new("cc")
            .args(["-O2", "-g", "-Wall", "-Wextra", "-Werror", "-pthread"])
            .arg(repository.join("tests/c/liteinst_advanced.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile LiteInst advanced guest");
        assert!(
            output.status.success(),
            "LiteInst advanced guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn mmap_guest() -> &'static Path {
    LITEINST_MMAP_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("liteinst-advanced");
        fs::create_dir_all(&build_root).expect("failed to create LiteInst guest directory");
        let guest = build_root.join("mmap_determinism");
        let output = Command::new("cc")
            .args(["-O2", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/mmap_determinism.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile LiteInst mmap guest");
        assert!(
            output.status.success(),
            "LiteInst mmap guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn compatibility_fixture() -> &'static Path {
    LITEINST_COMPAT_FIXTURE.get_or_init(|| {
        let build_root = process_build_root("liteinst-advanced");
        fs::create_dir_all(&build_root).expect("failed to create LiteInst fixture directory");
        let fixture = build_root.join("compatibility-fixture.txt");
        fs::write(&fixture, COMPAT_FIXTURE_CONTENT).expect("failed to write LiteInst fixture");
        fixture
    })
}

fn semantic_fixture() -> &'static Path {
    LITEINST_SEMANTIC_FIXTURE.get_or_init(|| {
        let build_root = process_build_root("liteinst-advanced");
        fs::create_dir_all(&build_root).expect("failed to create LiteInst fixture directory");
        let mut fixture = tempfile::Builder::new()
            .prefix("semantic-fixture-")
            .tempfile_in(build_root)
            .expect("failed to create LiteInst semantic fixture");
        fixture
            .write_all(SEMANTIC_FIXTURE_CONTENT)
            .expect("failed to write LiteInst semantic fixture");
        let (_file, path) = fixture
            .keep()
            .expect("failed to retain LiteInst semantic fixture");
        path
    })
}

fn compressed_fixtures() -> &'static [PathBuf; 2] {
    LITEINST_COMPRESSED_FIXTURES.get_or_init(|| {
        let source = compatibility_fixture();
        let build_root = source
            .parent()
            .expect("compatibility fixture should have a parent directory");
        [
            (
                "/usr/bin/gzip",
                &["-n", "-c"][..],
                "compatibility-fixture.gz",
            ),
            ("/usr/bin/bzip2", &["-c"][..], "compatibility-fixture.bz2"),
        ]
        .map(|(program, args, filename)| {
            let output = Command::new(program)
                .args(args)
                .arg(source)
                .output()
                .unwrap_or_else(|error| panic!("failed to run {program}: {error}"));
            assert!(
                output.status.success(),
                "{program} failed:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            let path = build_root.join(filename);
            fs::write(&path, output.stdout).expect("failed to write compressed fixture");
            path
        })
    })
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(liteinst-c2-hermit): Review running a Nix wrapper's ELF directly.
/// The ELF that runs when `program` is executed. Nix installs some programs
/// (gzip in the pinned validation root) as a `#!` shell wrapper that execs
/// `.<name>-wrapped` beside it. In-guest LiteInst does not support execve yet,
/// so a wrapper would fail at that exec; run the wrapped ELF directly. An
/// ordinary ELF program is returned unchanged.
fn elf_program(program: &Path) -> PathBuf {
    fn is_elf(path: &Path) -> bool {
        let mut magic = [0_u8; 4];
        fs::File::open(path)
            .and_then(|mut file| file.read_exact(&mut magic))
            .is_ok()
            && magic == *b"\x7fELF"
    }
    if is_elf(program) {
        return program.to_path_buf();
    }
    let resolved = fs::canonicalize(program)
        .unwrap_or_else(|error| panic!("failed to resolve {}: {error}", program.display()));
    let name = resolved
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_else(|| panic!("{} has no UTF-8 file name", resolved.display()));
    let wrapped = resolved.with_file_name(format!(".{name}-wrapped"));
    assert!(
        is_elf(&wrapped),
        "{} is neither an ELF nor a Nix wrapper around {}",
        program.display(),
        wrapped.display()
    );
    wrapped
}

const VIRTUAL_TIME_EPOCH: &str = "2026-01-01T00:00:00Z";

fn liteinst_command_at_epoch(log_level: &str, epoch: Option<&str>) -> Command {
    let mut command = Command::new(hermit_binary());
    command
        .arg(format!("--log={log_level}"))
        .args(["--backend", "liteinst", "run"]);
    if let Some(epoch) = epoch {
        command.arg(format!("--epoch={epoch}"));
    }
    command.args([
        "--max-timeslice=disabled",
        "--strict",
        "--base-env=minimal",
        "--mount=type=tmpfs,target=/test",
        "--workdir=/test",
    ]);
    command
}

fn liteinst_command(log_level: &str) -> Command {
    liteinst_command_at_epoch(log_level, None)
}

#[test]
fn liteinst_in_guest_commands_use_minimal_environment_and_private_workdir() {
    let command = liteinst_command("off");
    let args = command
        .get_args()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        args,
        [
            "--log=off",
            "--backend",
            "liteinst",
            "run",
            "--max-timeslice=disabled",
            "--strict",
            "--base-env=minimal",
            "--mount=type=tmpfs,target=/test",
            "--workdir=/test",
        ]
    );
    let epoch_args = liteinst_command_at_epoch("off", Some(VIRTUAL_TIME_EPOCH))
        .get_args()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        epoch_args,
        [
            "--log=off",
            "--backend",
            "liteinst",
            "run",
            "--epoch=2026-01-01T00:00:00Z",
            "--max-timeslice=disabled",
            "--strict",
            "--base-env=minimal",
            "--mount=type=tmpfs,target=/test",
            "--workdir=/test",
        ]
    );
}

fn run_liteinst(program: &Path, args: &[&str]) -> Output {
    run_liteinst_with_input(program, args, None, None)
}

fn run_liteinst_with_input(
    program: &Path,
    args: &[&str],
    input: Option<&[u8]>,
    epoch: Option<&str>,
) -> Output {
    let home = tempfile::tempdir().expect("failed to create isolated LiteInst HOME");
    let xdg_config_home = home.path().join(".config");
    fs::create_dir_all(&xdg_config_home).expect("failed to create isolated XDG config directory");
    let mut command = liteinst_command_at_epoch("info", epoch);
    command
        .arg(format!("--env=HOME={}", home.path().display()))
        .arg(format!(
            "--env=XDG_CONFIG_HOME={}",
            xdg_config_home.display()
        ))
        .arg("--env=PYTHONDONTWRITEBYTECODE=1")
        .env("HOME", home.path())
        .env("PYTHONDONTWRITEBYTECODE", "1");
    command.arg("--").arg(program).args(args);
    let Some(input) = input else {
        return command
            .stdin(Stdio::null())
            .output()
            .expect("failed to run Hermit LiteInst");
    };

    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run Hermit LiteInst with stdin");
    child
        .stdin
        .take()
        .expect("LiteInst stdin pipe should exist")
        .write_all(input)
        .expect("failed to write LiteInst stdin");
    child
        .wait_with_output()
        .expect("failed to collect Hermit LiteInst output")
}

/// Requires that Hermit ran the guest under in-guest LiteInst and never named
/// the retired host hybrid.
fn assert_in_guest_selected(stderr: &str) {
    assert!(
        stderr.lines().any(|line| line == IN_GUEST_SELECTED),
        "{stderr}"
    );
    assert!(!stderr.contains("liteinst host hybrid"), "{stderr}");
    assert!(!stderr.contains("LiteInst host hybrid"), "{stderr}");
}

fn assert_liteinst_in_guest_output(output: Output) -> Output {
    assert!(
        output.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_in_guest_selected(&String::from_utf8_lossy(&output.stderr));
    output
}

fn run_liteinst_in_guest(program: &Path, args: &[&str]) -> Output {
    assert_liteinst_in_guest_output(run_liteinst(program, args))
}

fn run_liteinst_in_guest_with_stdin(program: &Path, args: &[&str], input: &[u8]) -> Output {
    assert_liteinst_in_guest_output(run_liteinst_with_input(program, args, Some(input), None))
}

fn assert_liteinst_in_guest(program: &Path, args: &[&str], expected_stdout: &[u8]) {
    let output = run_liteinst_in_guest(program, args);
    assert_eq!(
        output.stdout,
        expected_stdout,
        "{} {args:?}: stdout={:?}",
        program.display(),
        String::from_utf8_lossy(&output.stdout),
    );
}

fn assert_liteinst_virtual_time_is_continuous() {
    const EPOCH_SECONDS: u64 = 1_767_225_600;
    const MAX_STARTUP_SECONDS: u64 = 60;

    // Whole seconds remain stable across LiteInst runs. Do not assert the old
    // exact epoch: that encoded #1095's reset-on-exec behavior and rejects
    // legitimate deterministic startup progress.
    let output = assert_liteinst_in_guest_output(run_liteinst_with_input(
        Path::new("/usr/bin/date"),
        &["-u", "+%s"],
        None,
        Some(VIRTUAL_TIME_EPOCH),
    ));
    let timestamp = String::from_utf8(output.stdout).expect("date output should be UTF-8");
    let seconds = timestamp
        .trim()
        .parse::<u64>()
        .expect("date seconds should be numeric");

    assert!(
        seconds >= EPOCH_SECONDS,
        "guest time preceded the configured epoch: {timestamp}"
    );
    assert!(
        seconds < EPOCH_SECONDS + MAX_STARTUP_SECONDS,
        "guest startup consumed an implausible amount of virtual time: {timestamp}"
    );
    // Verify continuous progression independently of the startup offset.
    let progress = assert_liteinst_in_guest_output(run_liteinst_with_input(
        advanced_guest(),
        &["clock-progress"],
        None,
        Some(VIRTUAL_TIME_EPOCH),
    ));
    assert_eq!(progress.stdout, b"clock-progress-ok\n");
}

/// Hermit passes the runtime its configuration and clock RPC fingerprint, and a
/// runtime built from the same tree accepts it and then removes it, as it does
/// the DETLOG variables: the value is zeroed in the environment block, so
/// `/proc/self/environ` keeps only the name, and a program reading `main`'s
/// initial environment, as bash does, sees the name with an empty value
/// (https://github.com/rrnewton/hermit/issues/3986). The fingerprint itself
/// never reaches the guest. A runtime from another tree would stop before the
/// guest's `main` instead.
#[test]
fn liteinst_in_guest_checks_and_hides_the_config_fingerprint() {
    let _guard = hermit_run_guard();
    const NAME: &str = "HERMIT_LITEINST_CONFIG_FINGERPRINT";
    let fingerprint = detcore::config_wire_fingerprint();

    // The value the guest's shell imported, by parameter expansion alone (no
    // child process). PYTHONDONTWRITEBYTECODE, which run_liteinst sets, shows
    // that the shell sees the environment.
    let shell = run_liteinst_in_guest(
        Path::new("/usr/bin/bash"),
        &[
            "-c",
            "printf '[%s] %s' \"${HERMIT_LITEINST_CONFIG_FINGERPRINT-unset}\" \"${!PYTHON*}\"",
        ],
    );
    let shell = String::from_utf8_lossy(&shell.stdout);
    assert!(
        shell == "[] PYTHONDONTWRITEBYTECODE" || shell == "[unset] PYTHONDONTWRITEBYTECODE",
        "{shell}"
    );

    let environ = run_liteinst_in_guest(
        Path::new("/usr/bin/head"),
        &["-c", "1048576", "/proc/self/environ"],
    );
    let entries = environ
        .stdout
        .split(|&byte| byte == 0)
        .filter(|entry| entry.starts_with(NAME.as_bytes()))
        .collect::<Vec<_>>();
    assert_eq!(
        entries,
        [format!("{NAME}=").as_bytes()],
        "{:?}",
        String::from_utf8_lossy(&environ.stdout)
    );
    assert!(
        !String::from_utf8_lossy(&environ.stdout).contains(&fingerprint),
        "{:?}",
        String::from_utf8_lossy(&environ.stdout)
    );
}

/// The runtime refuses a coordinator whose configuration fingerprint is not its
/// own, before it connects or decodes anything
/// (https://github.com/rrnewton/hermit/issues/3986). Run without Hermit, with a
/// coordinator address nothing listens on, so the refusal must come first: a
/// check moved after the connection would report the connection instead. This
/// needs no CPUID faulting, because the runtime refuses before it installs
/// anything. An empty value, which the removal leaves in `main`'s initial
/// environment, counts as absent and is not refused.
/// The status the runtime's constructor exits with when it refuses to install
/// (`fail` in `detcore-liteinst/src/lib.rs`, `libc::_exit(127)`). It is the
/// runtime's own code, not Hermit's `GUEST_PROGRAM_NOT_FOUND_EXIT`, which has
/// the same value; this test runs no Hermit.
const LITEINST_RUNTIME_REFUSAL_EXIT: i32 = 127;

#[test]
fn liteinst_runtime_refuses_another_trees_fingerprint_before_connecting() {
    let runtime = hermit_binary()
        .parent()
        .expect("the Cargo-built Hermit has a profile directory")
        .join("libdetcore_liteinst.so");
    assert!(
        runtime.is_file(),
        "{} is missing; build it with `cargo build -p detcore-liteinst` in this profile",
        runtime.display()
    );
    let run = |fingerprint: &str| {
        Command::new("/usr/bin/test")
            .env_clear()
            .env(
                "REVERIE_LITEINST_COORDINATOR",
                "/nonexistent/coordinator.sock",
            )
            .env("HERMIT_LITEINST_CONFIG_FINGERPRINT", fingerprint)
            .env("LD_PRELOAD", &runtime)
            .stdin(Stdio::null())
            .output()
            .expect("failed to start a program under the LiteInst runtime")
    };

    let other = run("0000000000000000");
    let stderr = String::from_utf8_lossy(&other.stderr);
    assert_eq!(
        other.status.code(),
        Some(LITEINST_RUNTIME_REFUSAL_EXIT),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "the runtime at {} was built from a different tree than the hermit running it",
            runtime.display()
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "its configuration and clock RPC fingerprint is {}, Hermit's is 0000000000000000",
            detcore::config_wire_fingerprint()
        )),
        "{stderr}"
    );

    let empty = run("");
    let stderr = String::from_utf8_lossy(&empty.stderr);
    assert!(!stderr.contains("built from a different tree"), "{stderr}");
}

#[test]
fn liteinst_in_guest_heap_growth_avoids_trampoline_mappings() {
    let _guard = hermit_run_guard();
    let output = run_liteinst_in_guest(mmap_guest(), &["heap"]);
    assert!(
        output.stdout.starts_with(b"heap "),
        "heap-growth guest omitted its success marker: {}",
        String::from_utf8_lossy(&output.stdout),
    );
}

/// Signal phase 1, step I3: with guest SIGALRM handlers admitted (and site
/// patching off, as phase 1 requires), the guest's handler is installed
/// virtually and Detcore learns of it: the phase 1 table then refuses a
/// socket, a fork and rt_sigpending with EOPNOTSUPP; an expiry while SIGALRM
/// is blocked is held in the scheduler's ledger, so a change to SIG_DFL is
/// refused with EPERM; SIG_IGN then discards it and the socket succeeds. With
/// handlers not admitted (the default), the installation is refused with
/// EPERM as before and nothing changes. Both runs are verified deterministic.
/// The table admits reads of a procfs file Detcore renders, whose descriptor
/// is a sealed memfd: /proc/loadavg reads as on Linux, beside /proc/uptime,
/// the phase's own admitted capture (the captured kernel loadavg was refused
/// before https://github.com/rrnewton/hermit/issues/4213's package K
/// rendered it), while a capture that may wait, /proc/diskstats, is still
/// refused in the same process.
#[test]
fn liteinst_in_guest_sigalrm_handler_is_virtual_and_published() {
    let _guard = hermit_run_guard();
    let build_root = process_build_root("liteinst-sigalrm");
    fs::create_dir_all(&build_root).expect("failed to create the SIGALRM guest directory");
    let guest = build_root.join("sigalrm_virtual_handler");
    let compiled = Command::new("cc")
        .args(["-O2", "-g", "-Wall", "-Wextra", "-Werror"])
        .arg(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/liteinst_sigalrm_virtual_handler.c"),
        )
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("failed to compile the SIGALRM guest");
    assert!(compiled.status.success(), "{compiled:?}");
    for (admitted, expected) in [
        (
            "1",
            "install=0 errno=0\nquery_is_handler=1\nsigpending=-1 errno=95\n\
             socket=refused errno=95\nfork=refused errno=95\nto_default=-1 errno=1\n\
             ignore=0\nsocket_after=ok errno=0\n",
        ),
        (
            "0",
            "install=-1 errno=1\nquery_is_handler=0\nsigpending=0 errno=0\n\
             socket=ok errno=0\nfork=ok errno=0\nto_default=0 errno=0\n\
             ignore=0\nsocket_after=ok errno=0\n",
        ),
    ] {
        let output = super::hermit_test::hermit_output(
            "hermit run",
            liteinst_command("info")
                .arg("--verify")
                .arg("--env=REVERIE_LITEINST_SITE_PATCHING=0")
                .arg(format!(
                    "--env=REVERIE_LITEINST_SIGALRM_HANDLERS={admitted}"
                ))
                .arg("--")
                .arg(&guest)
                .stdin(Stdio::null()),
        );
        let output = assert_liteinst_in_guest_output(output);
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            expected,
            "admitted={admitted}"
        );
    }
    // A signalfd inherited on stdin (a run without --verify keeps the
    // caller's stdin) refuses the handler although Detcore's model labels
    // stdin a regular file: the run then behaves as with handlers not
    // admitted.
    let not_admitted = "install=-1 errno=1\nquery_is_handler=0\nsigpending=0 errno=0\n\
                        socket=ok errno=0\nfork=ok errno=0\nto_default=0 errno=0\n\
                        ignore=0\nsocket_after=ok errno=0\n";
    let signalfd = {
        use std::os::fd::FromRawFd;
        let mut mask: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe { libc::sigemptyset(&mut mask) };
        unsafe { libc::sigaddset(&mut mask, libc::SIGUSR2) };
        let raw = unsafe { libc::signalfd(-1, &mask, libc::SFD_CLOEXEC) };
        assert!(raw >= 0, "signalfd: {}", std::io::Error::last_os_error());
        unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) }
    };
    let output = super::hermit_test::hermit_output(
        "hermit run",
        liteinst_command("info")
            .arg("--env=REVERIE_LITEINST_SITE_PATCHING=0")
            .arg("--env=REVERIE_LITEINST_SIGALRM_HANDLERS=1")
            .arg("--")
            .arg(&guest)
            .stdin(Stdio::from(signalfd)),
    );
    let output = assert_liteinst_in_guest_output(output);
    assert_eq!(String::from_utf8_lossy(&output.stdout), not_admitted);

    let procfs_reader = sigalrm_fixture("liteinst_sigalrm_procfs_read");
    let output = liteinst_command("info")
        .arg("--verify")
        .arg("--proc=standard")
        .arg("--env=REVERIE_LITEINST_SITE_PATCHING=0")
        .arg("--env=REVERIE_LITEINST_SIGALRM_HANDLERS=1")
        .arg("--")
        .arg(&procfs_reader)
        .args(["/proc/loadavg", "/proc/uptime", "/proc/diskstats"])
        .stdin(Stdio::null())
        .output()
        .expect("failed to run Hermit LiteInst");
    let output = assert_liteinst_in_guest_output(output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 3, "{stdout}");
    assert_eq!(lines[0], "/proc/loadavg: 0.00 0.00 0.00 1/1 1", "{stdout}");
    assert!(
        lines[1].starts_with("/proc/uptime: ") && !lines[1].contains("errno"),
        "{stdout}"
    );
    assert_eq!(
        lines[2],
        format!("/proc/diskstats: read errno {}", libc::EOPNOTSUPP),
        "{stdout}"
    );
}

/// Compiles one of the SIGALRM guests in `tests/fixtures`.
fn sigalrm_fixture(name: &str) -> std::path::PathBuf {
    let build_root = process_build_root(&format!("liteinst-{name}"));
    fs::create_dir_all(&build_root).expect("failed to create the SIGALRM guest directory");
    let guest = build_root.join(name);
    let compiled = Command::new("cc")
        .args(["-O2", "-g", "-Wall", "-Wextra", "-Werror"])
        .arg(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(format!("{name}.c")),
        )
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("failed to compile the SIGALRM guest");
    assert!(compiled.status.success(), "{compiled:?}");
    guest
}

/// Runs a SIGALRM fixture with exact outputs and current typed canonical
/// verification. The official CLI runner owns these Hermit invocations.
fn strict_sigalrm_fixture_run(
    guest: &Path,
    backend: &str,
    admission: Option<&str>,
    patching: &str,
    mode: &str,
) -> Vec<u8> {
    use hermit::canonical_verdict::VerificationReport;

    use super::kvm_cancellation::bounded_command_with_timeout;
    use super::kvm_cancellation::bounded_read;

    let directory = process_build_root(&format!("sigalrm-fixture-{backend}-{mode}"));
    fs::create_dir_all(&directory).expect("SIGALRM verification directory");
    let report_path = directory.join("verify.json");
    let mut command = Command::new(hermit_binary());
    command
        .args(["--log=info", "--backend", backend, "run"])
        .arg(format!("--epoch={VIRTUAL_TIME_EPOCH}"))
        .args([
            "--strict",
            "--max-timeslice=disabled",
            "--base-env=minimal",
            "--verify",
            "--verify-strict",
        ])
        .arg(format!("--verify-json={}", report_path.display()))
        .arg(format!("--env=REVERIE_LITEINST_SITE_PATCHING={patching}"))
        .env_remove("REVERIE_LITEINST_SIGALRM_HANDLERS")
        .env_remove("RUST_LOG")
        .env_remove("HERMIT_LOG")
        .env_remove("HERMIT_LOG_FILE")
        .stdin(Stdio::null());
    if let Some(value) = admission {
        command.arg(format!("--env=REVERIE_LITEINST_SIGALRM_HANDLERS={value}"));
    }
    command.arg("--").arg(guest).arg(mode);
    let status = bounded_command_with_timeout(&mut command, &directory, Duration::from_secs(57));
    let stdout = bounded_read(&directory.join("stdout"), 1024 * 1024);
    let stderr = bounded_read(&directory.join("stderr"), 16 * 1024 * 1024);
    assert_eq!(
        status.code(),
        Some(0),
        "{backend}/{mode}: stdout={} stderr={}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr),
    );
    let report =
        VerificationReport::from_current_json_slice(&bounded_read(&report_path, 16 * 1024 * 1024))
            .expect("complete current SIGALRM verification report");
    report
        .require_canonical_match()
        .expect("nonempty canonical INFO comparison must match");
    report
        .require_exact_output_match()
        .expect("exact status/stdout/stderr comparison must match");
    assert_eq!(report.guest_exit_code, Some(0));
    assert!(report.guest_signal.is_none());
    stdout
}

/// Only four existing virtual-root scalar queries are admitted by this slice.
/// Every raw value/errno and the one-shot alarm/handler/pause lifecycle must
/// match ptrace, including queries made inside the installed handler.
#[test]
fn in_guest_trap_sigalrm_scalar_identity_matches_ptrace() {
    let _guard = hermit_run_guard();
    let guest = sigalrm_fixture("liteinst_sigalrm_scalar_identity");
    let mut expected = String::from(
        "install=0 errno=0 alarm=0 errno=0 pause=-1 errno=4 hits=1 signal=14 remove=0 errno=0\n",
    );
    for stage in [
        "before",
        "installed",
        "handler",
        "after_delivery",
        "removed",
    ] {
        for query in ["getuid", "geteuid", "getgid", "getegid"] {
            expected.push_str(&format!("{stage} {query} value=0 errno=0\n"));
        }
    }
    let mut outputs = Vec::new();
    for backend in ["ptrace", "in-guest-trap"] {
        let stdout = strict_sigalrm_fixture_run(&guest, backend, Some("1"), "0", "handled");
        assert_eq!(stdout, expected.as_bytes(), "{backend} scalar lifecycle");
        outputs.push(stdout);
    }
    assert_eq!(
        outputs[0], outputs[1],
        "same scalar guest/input under both backends"
    );
}

/// Opt-in alone is insufficient: no opt-in and true patched LiteInst both
/// retain EPERM installation refusal, leaving the scalar queries unchanged.
#[test]
fn liteinst_sigalrm_scalar_identity_keeps_handler_admission_controls() {
    let _guard = hermit_run_guard();
    let guest = sigalrm_fixture("liteinst_sigalrm_scalar_identity");
    let mut expected = String::from("install=-1 errno=1 hits=0\n");
    for stage in ["before", "after_refusal"] {
        for query in ["getuid", "geteuid", "getgid", "getegid"] {
            expected.push_str(&format!("{stage} {query} value=0 errno=0\n"));
        }
    }
    for (backend, admission, patching) in
        [("in-guest-trap", None, "0"), ("liteinst", Some("1"), "1")]
    {
        let stdout = strict_sigalrm_fixture_run(&guest, backend, admission, patching, "refused");
        assert_eq!(stdout, expected.as_bytes(), "{backend} admission control");
    }
}

/// A default-fatal virtual alarm ends the actual child by SIGALRM and reports
/// its exit callbacks. No guest handler opt-in is needed. Patched LiteInst is
/// exercised as well as the trap backend; nanosleep's inaccessible remainder
/// must not be written after the fatal wake.
#[test]
fn liteinst_in_guest_default_fatal_alarms_end_emulated_sleeps_and_verify() {
    let _guard = hermit_run_guard();
    let guest = sigalrm_fixture("liteinst_sigalrm_sleep");
    let expected = concat!(
        "before action=0 blocked=0 pending=0\n",
        "alarm_previous=0\n",
        "child_signal=14 child_exit=-1 core=0\n",
        "parent_alarm=0\n",
    );
    for (backend, patching) in [("ptrace", "0"), ("in-guest-trap", "0"), ("liteinst", "1")] {
        for mode in ["default-pause", "default-nanosleep"] {
            let stdout = strict_sigalrm_fixture_run(&guest, backend, None, patching, mode);
            assert_eq!(stdout, expected.as_bytes(), "{backend}/{mode}");
        }
    }
}

/// Ignored and blocked alarms do not interrupt a sleep. A blocked alarm stays
/// pending after cancellation; canceling an unexpired alarm prevents delivery.
/// The child inherits disposition and mask, but has its own alarm state.
#[test]
fn liteinst_in_guest_noninterrupting_alarms_preserve_sleep_and_pending_state() {
    let _guard = hermit_run_guard();
    let guest = sigalrm_fixture("liteinst_sigalrm_sleep");
    for (mode, disposition, blocked, pending, canceled) in [
        ("ignored-nanosleep", 1, 0, 0, false),
        ("blocked-nanosleep", 0, 1, 1, false),
        ("cancelled-nanosleep", 0, 0, 0, true),
    ] {
        let mut expected =
            format!("before action={disposition} blocked={blocked} pending=0\nalarm_previous=0\n");
        if canceled {
            expected.push_str("cancel_before_sleep=1\n");
        }
        expected.push_str(&format!(
            "nanosleep=0 errno=0\nafter_sleep action={disposition} blocked={blocked} pending={pending}\n\
             cancel_after_sleep=0\nafter_cancel action={disposition} blocked={blocked} pending={pending}\n\
             child_signal=0 child_exit=0 core=0\nparent_alarm=0\n"
        ));
        for (backend, patching) in [("ptrace", "0"), ("in-guest-trap", "0"), ("liteinst", "1")] {
            let stdout = strict_sigalrm_fixture_run(&guest, backend, None, patching, mode);
            assert_eq!(stdout, expected.as_bytes(), "{backend}/{mode}");
        }
    }
}

/// This bridge covers emulated sleeps, not a running process's alarm. The
/// existing missing-exit check must still refuse an alarm outside the bridge.
#[test]
fn liteinst_in_guest_alarm_outside_emulated_sleep_keeps_its_loss() {
    use super::kvm_cancellation::bounded_command_with_timeout;
    use super::kvm_cancellation::bounded_read;

    let _guard = hermit_run_guard();
    let guest = sigalrm_fixture("liteinst_sigalrm_sleep");
    for backend in ["in-guest-trap", "liteinst"] {
        let directory = process_build_root(&format!("alarm-outside-sleep-{backend}"));
        fs::create_dir_all(&directory).expect("excluded alarm directory");
        let mut command = Command::new(hermit_binary());
        command
            .args(["--log=info", "--backend", backend, "run"])
            .arg(format!("--epoch={VIRTUAL_TIME_EPOCH}"))
            .args([
                "--strict",
                "--max-timeslice=disabled",
                "--base-env=minimal",
                "--verify",
                "--verify-strict",
                "--",
            ])
            .arg(&guest)
            .arg("running-alarm")
            .env_remove("REVERIE_LITEINST_SIGALRM_HANDLERS")
            .env_remove("REVERIE_LITEINST_SITE_PATCHING")
            .stdin(Stdio::null());
        let status =
            bounded_command_with_timeout(&mut command, &directory, Duration::from_secs(57));
        let stderr = bounded_read(&directory.join("stderr"), 16 * 1024 * 1024);
        let stderr = String::from_utf8_lossy(&stderr);
        assert!(!status.success(), "{backend}: {stderr}");
        assert!(
            stderr.contains("exited without deregistering; its exit was not scheduled"),
            "{backend}: {status:?}: {stderr}",
        );
        assert!(
            !stderr.contains("Determinism verified"),
            "{backend}: {stderr}"
        );
    }
}

/// Runs a SIGALRM guest whose handler is delivered before a syscall that has
/// not run (the I4 addendum's point c), once without --verify, where its
/// output must be `expected` (the program's output on native Linux), then
/// with --verify, which must refuse the run: each such delivery records a
/// determinism loss, since the handler cannot see the interrupted rcx and r11
/// Linux would show it.
fn assert_entry_delivery_runs_and_refuses_verification(
    guest: &Path,
    extra_args: &[&str],
    expected: &str,
) {
    let output = super::hermit_test::hermit_output(
        "hermit run",
        liteinst_command("info")
            .args(extra_args)
            .arg("--env=REVERIE_LITEINST_SITE_PATCHING=0")
            .arg("--env=REVERIE_LITEINST_SIGALRM_HANDLERS=1")
            .arg("--")
            .arg(guest)
            .stdin(Stdio::null()),
    );
    let output = assert_liteinst_in_guest_output(output);
    assert_eq!(String::from_utf8_lossy(&output.stdout), expected);
    let output = super::hermit_test::hermit_output(
        "hermit run",
        liteinst_command("info")
            .arg("--verify")
            .args(extra_args)
            .arg("--env=REVERIE_LITEINST_SITE_PATCHING=0")
            .arg("--env=REVERIE_LITEINST_SIGALRM_HANDLERS=1")
            .arg("--")
            .arg(guest)
            .stdin(Stdio::null()),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "verification matched: {stderr}");
    assert!(
        stderr.contains("run 1: determinism loss recorded: ")
            && stderr.contains("before a syscall that has not run"),
        "{stderr}"
    );
    assert!(!stderr.contains("Determinism verified"), "{stderr}");
}

/// Signal phase 1, the I4 addendum: a SIGALRM that becomes pending while a
/// handler blocks it runs as soon as that handler returns, before the guest's
/// next syscall (a write, or exit_group), as Linux delivers it at the
/// handler's rt_sigreturn; never nested inside the handler. A handler that
/// adds SIGALRM to its saved mask returns with it blocked, and the next runs
/// when sigprocmask unblocks it (an ordinary completion delivery). The
/// handlers query their mask, and the guest's alarm and pause after them still
/// work. The output is the program's output on native Linux; --verify refuses
/// the run (see `assert_entry_delivery_runs_and_refuses_verification`).
#[test]
fn liteinst_in_guest_sigalrm_pending_at_handler_return_runs_before_the_next_syscall() {
    let _guard = hermit_run_guard();
    let guest = sigalrm_fixture("liteinst_sigalrm_entry_delivery");
    assert_entry_delivery_runs_and_refuses_verification(
        &guest,
        &[],
        "handler 1 start\nhandler 1 end\nhandler 2 blocked=1\nafter pause\n\
         handler 3 start\nhandler 3 end\nblocked after return\nhandler 4 blocked=1\n\
         after unblock\nhandler 5 start\nhandler 5 end\nhandler 6 blocked=1\n",
    );
}

/// Signal phase 1: a SIGALRM that expires while the guest runs only trapped
/// CPUID instructions. With a logical target timeslice the scheduler yields
/// at those traps and the alarm commits there; its handler runs before the
/// guest's next syscall (exit_group), as on Linux, where it runs during the
/// loop. The output matches native; --verify refuses the run (see
/// `assert_entry_delivery_runs_and_refuses_verification`).
#[test]
fn liteinst_in_guest_sigalrm_committed_at_an_instruction_trap_runs_before_exit() {
    let _guard = hermit_run_guard();
    let guest = sigalrm_fixture("liteinst_sigalrm_cpuid_loop_then_exit");
    assert_entry_delivery_runs_and_refuses_verification(
        &guest,
        &["--target-timeslice=100000"],
        "handler ran\n",
    );
}

/// Signal phase 1: a SIGALRM Detcore took that the runtime cannot deliver as
/// prepared. With Hermit started at RLIMIT_SIGPENDING 0 the kernel sends the
/// runtime's instance without its siginfo (a signal below SIGRTMIN is sent
/// even when its siginfo cannot be queued), so the trampoline refuses it and
/// the process ends before the guest resumes: it never prints "resumed". The
/// run records a determinism loss, which verification reports.
#[test]
fn liteinst_in_guest_an_undeliverable_sigalrm_is_a_loss_and_stops_the_guest() {
    use std::os::unix::process::CommandExt;
    let _guard = hermit_run_guard();
    let guest = sigalrm_fixture("liteinst_sigalrm_delivery_lost");
    let mut command = liteinst_command("info");
    command
        .arg("--verify")
        .arg("--env=REVERIE_LITEINST_SITE_PATCHING=0")
        .arg("--env=REVERIE_LITEINST_SIGALRM_HANDLERS=1")
        .arg("--")
        .arg(&guest)
        .stdin(Stdio::null());
    // SAFETY: setrlimit is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            let mut limit: libc::rlimit = std::mem::zeroed();
            if libc::getrlimit(libc::RLIMIT_SIGPENDING, &mut limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            limit.rlim_cur = 0;
            if libc::setrlimit(libc::RLIMIT_SIGPENDING, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        })
    };
    let output = super::hermit_test::hermit_output(&format!("{command:?}"), &mut command);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stdout} {stderr}");
    assert!(!stdout.contains("resumed"), "{stdout} {stderr}");
    assert!(
        stderr.contains("run 1: determinism loss recorded: process "),
        "{stderr}"
    );
    assert!(!stderr.contains("Determinism verified"), "{stderr}");
}

/// Characterizes syscall-completion delivery and real SA_SIGINFO return masks
/// against native Linux on LiteInst fallback, ptrace, and in-guest-trap. Each
/// backend must pass strict typed verification with preemption disabled. The
/// return modes distinguish the saved mask, sa_mask activation, an optional
/// uc_sigmask edit, and the mask observed after returning through the restorer.
#[test]
fn liteinst_in_guest_sigalrm_handler_runs_at_syscall_completion() {
    use detcore::Digest;
    use hermit::canonical_verdict::VerificationReport;

    use super::kvm_cancellation::bounded_command_with_timeout;
    use super::kvm_cancellation::bounded_read;

    let _guard = hermit_run_guard();
    let guest = sigalrm_fixture("liteinst_sigalrm_delivery");
    let completion = b"install=0 errno=0\n\
         pause=-1 errno=4 runs=1 signal=14 code=128 blocked_inside=1 blocked_after=0\n\
         unblock: before=0 after=1 blocked_after=0\n";
    let unchanged = b"install=0 errno=0 before_alrm=0 before_usr1=0\n\
         pause=-1 errno=4 runs=1 signal=14 code=128\n\
         saved_alrm=0 saved_usr1=0 active_alrm=1 active_usr1=1\n\
         return_alrm=0 return_usr1=0\n\
         after_alrm=0 after_usr1=0\n";
    let edited = b"install=0 errno=0 before_alrm=0 before_usr1=0\n\
         pause=-1 errno=4 runs=1 signal=14 code=128\n\
         saved_alrm=0 saved_usr1=0 active_alrm=1 active_usr1=1\n\
         return_alrm=0 return_usr1=1\n\
         after_alrm=0 after_usr1=1\n";
    let mut failures = Vec::new();
    for (mode, expected) in [
        ("completion", completion.as_slice()),
        ("return-unchanged", unchanged.as_slice()),
        ("return-edit-usr1", edited.as_slice()),
    ] {
        let directory = process_build_root(&format!("sigalrm-native-{mode}"));
        let mut native = Command::new(&guest);
        native.arg(mode).stdin(Stdio::null());
        let status = bounded_command_with_timeout(&mut native, &directory, Duration::from_secs(57));
        let stdout = bounded_read(&directory.join("stdout"), 1024 * 1024);
        let stderr = bounded_read(&directory.join("stderr"), 16 * 1024 * 1024);
        assert_eq!(status.code(), Some(0), "native/{mode}: {stderr:?}");
        assert_eq!(stdout, expected, "native/{mode}");
        let expected_stderr = if mode == "completion" {
            vec![b'.'; 200]
        } else {
            Vec::new()
        };
        assert_eq!(stderr, expected_stderr, "native/{mode}");
        for backend in ["liteinst", "ptrace", "in-guest-trap"] {
            let result = std::panic::catch_unwind(|| {
                let verified = strict_sigalrm_fixture_run(&guest, backend, Some("1"), "0", mode);
                let retained = process_build_root(&format!("sigalrm-fixture-{backend}-{mode}"));
                if backend == "liteinst" {
                    let stderr = bounded_read(&retained.join("stderr"), 16 * 1024 * 1024);
                    assert_in_guest_selected(&String::from_utf8_lossy(&stderr));
                }
                let report = VerificationReport::from_current_json_slice(&bounded_read(
                    &retained.join("verify.json"),
                    16 * 1024 * 1024,
                ))
                .expect("complete current SIGALRM verification report");
                let outputs = report.compared_outputs.expect("compared guest outputs");
                for operand in [&outputs.left, &outputs.right] {
                    assert_eq!(operand.exit_code, status.code(), "{backend}/{mode}");
                    assert!(operand.signal.is_none(), "{backend}/{mode}");
                    assert_eq!(
                        operand.stdout_bytes,
                        stdout.len() as u64,
                        "{backend}/{mode}"
                    );
                    assert_eq!(
                        operand.stdout_sha256,
                        Digest::new(&stdout).to_string(),
                        "{backend}/{mode}"
                    );
                    assert_eq!(
                        operand.stderr_bytes,
                        stderr.len() as u64,
                        "{backend}/{mode}"
                    );
                    assert_eq!(
                        operand.stderr_sha256,
                        Digest::new(&stderr).to_string(),
                        "{backend}/{mode}"
                    );
                }
                assert_eq!(verified, expected, "{backend}/{mode}");
                assert_eq!(verified, stdout, "{backend}/{mode} versus native");
            });
            if result.is_err() {
                failures.push(format!("{backend}/{mode}"));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "signal characterization failed: {failures:?}"
    );
}

/// In-guest LiteInst runs the Detcore Tool inside the guest, so the Tool must
/// keep out of the guest's C library heap. Installing a SIGALRM handler makes
/// Detcore list the kernel's descriptor table; listed with glibc's opendir,
/// the freed buffer kept raw /proc inode numbers that the guest's next
/// opendir got back, so the padding of its getdents64 records (which Linux
/// never writes) differed between runs (how `iostat` failed --verify). The
/// fixture prints that padding; the run must verify and match native Linux.
#[test]
fn liteinst_in_guest_tool_directory_reads_stay_out_of_the_guest_heap() {
    let _guard = hermit_run_guard();
    let build_root = process_build_root("liteinst-guest-heap");
    fs::create_dir_all(&build_root).expect("failed to create the guest directory");
    let guest = build_root.join("tool_keeps_out_of_guest_heap");
    let compiled = Command::new("cc")
        .args(["-O2", "-g", "-Wall", "-Wextra", "-Werror"])
        .arg(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/liteinst_tool_keeps_out_of_guest_heap.c"),
        )
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("failed to compile the guest");
    assert!(compiled.status.success(), "{compiled:?}");
    let output = super::hermit_test::hermit_output(
        "hermit run",
        liteinst_command("info")
            .arg("--verify")
            .arg("--env=REVERIE_LITEINST_SITE_PATCHING=0")
            .arg("--env=REVERIE_LITEINST_SIGALRM_HANDLERS=1")
            .arg("--")
            .arg(&guest)
            .stdin(Stdio::null()),
    );
    let output = assert_liteinst_in_guest_output(output);
    let mut expected = String::from("install=0\nrecord 0 pad: 00 00 00\nrecord 1 pad: 00 00\n");
    for record in 2..10 {
        expected.push_str(&format!("record {record} pad: 00 00 00 00 00 00\n"));
    }
    assert_eq!(String::from_utf8_lossy(&output.stdout), expected);
}

/// The in-guest runtime installs the Detcore Tool from a constructor that
/// runs before the program's `main`, inside the guest. Everything that
/// installation allocates (the coordinator's configuration, the Tool, the
/// runtime's tables) must stay out of the guest's C library heap: its sizes
/// follow host state, so the guest's heap layout, and every heap address its
/// syscalls pass, followed host state too. The configuration lists every mount
/// of the guest's mount namespace, which starts as a copy of the caller's.
/// Extra mounts, forced here with `--mount` (each adds one entry), move the
/// guest's heap. A host mount appearing or disappearing between the two runs
/// of `--verify` would move it the same way; that this is what made
/// compat/python3 under in-guest-trap fail 1 of 10 at its first syscall
/// (`readlink`, whose path buffer is on the heap) is inferred, not
/// observed: the failing run recorded no mount counts. The fixture
/// prints the heap at `main`'s entry and where its first allocation lands:
/// under both in-guest backends and with zero to three extra mounts, the heap
/// must hold exactly what it holds natively and the first allocation must
/// land at the same address every time.
#[test]
fn liteinst_in_guest_runtime_leaves_the_guest_heap_as_the_program_made_it() {
    let _guard = hermit_run_guard();
    let build_root = process_build_root("liteinst-guest-heap");
    fs::create_dir_all(&build_root).expect("failed to create the guest directory");
    let guest = build_root.join("runtime_leaves_guest_heap_empty");
    let compiled = Command::new("cc")
        .args(["-O2", "-g", "-Wall", "-Wextra", "-Werror"])
        .arg(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/liteinst_runtime_leaves_guest_heap_empty.c"),
        )
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("failed to compile the guest");
    assert!(compiled.status.success(), "{compiled:?}");

    let native = Command::new(&guest)
        .output()
        .expect("failed to run the guest natively");
    assert!(native.status.success(), "{native:?}");
    let native_stdout = String::from_utf8_lossy(&native.stdout).into_owned();
    let native_heap = native_stdout
        .lines()
        .next()
        .expect("the guest prints its heap at main")
        .to_owned();
    assert!(native_heap.starts_with("heap at main: "), "{native_stdout}");

    let mut first_run: Option<(String, String)> = None;
    for (backend, selected) in [
        ("liteinst", IN_GUEST_SELECTED),
        ("in-guest-trap", IN_GUEST_TRAP_SELECTED),
    ] {
        for extra_mounts in 0..=3 {
            let label = format!("--backend={backend} with {extra_mounts} extra mounts");
            let output = super::hermit_test::hermit_output(
                "hermit run",
                Command::new(hermit_binary())
                    .args(["--log=error", "--backend", backend, "run"])
                    .args([
                        "--max-timeslice=disabled",
                        "--strict",
                        "--base-env=minimal",
                        "--mount=type=tmpfs,target=/test",
                        "--workdir=/test",
                    ])
                    .args((0..extra_mounts).map(|_| "--mount=type=tmpfs,target=/test"))
                    .arg("--")
                    .arg(&guest)
                    .stdin(Stdio::null()),
            );
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "{label}: status={:?}\nstdout={stdout}\nstderr={stderr}",
                output.status,
            );
            assert!(
                stderr.lines().any(|line| line == selected),
                "{label}: {stderr}"
            );
            let mut lines = stdout.lines();
            let heap = lines.next().unwrap_or_default();
            assert_eq!(
                heap, native_heap,
                "{label}: the in-guest runtime left allocations in the guest's heap \
                 (native: {native_heap})\n{stdout}"
            );
            let first_allocation = lines.next().unwrap_or_default().to_owned();
            assert!(
                first_allocation.starts_with("first allocation: 0x"),
                "{label}: {stdout}"
            );
            match &first_run {
                None => first_run = Some((label, first_allocation)),
                Some((first_label, expected)) => assert_eq!(
                    &first_allocation, expected,
                    "{label} placed the guest's first allocation elsewhere than {first_label}"
                ),
            }
        }
    }
}

/// libc wrappers the in-guest runtime and Detcore once called while installing,
/// each replaced by a raw syscall: a preloaded definition of any of them would
/// run inside the guest before `main`, and this one allocates 4 KiB through
/// the guest's malloc and counts the call before delegating. (Wrappers that the
/// pinned LiteInst2 still calls while allocating its trampoline arenas, such as
/// `mmap`, `fcntl` and `syscall`, are a disclosed residual of that dependency
/// and are left out.)
const ALLOCATING_LIBC_WRAPPERS: &str = r#"
#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <signal.h>
#include <stdarg.h>
#include <stdlib.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <unistd.h>
void *reverie_test_kept[64];
unsigned reverie_test_interposer_calls;
static void *note(const char *name) {
    reverie_test_kept[reverie_test_interposer_calls++ % 64] = malloc(4096);
    return dlsym(RTLD_NEXT, name);
}
pid_t getpid(void) { return ((pid_t (*)(void))note("getpid"))(); }
int prctl(int option, ...) {
    va_list arguments;
    va_start(arguments, option);
    unsigned long a = va_arg(arguments, unsigned long), b = va_arg(arguments, unsigned long),
                  c = va_arg(arguments, unsigned long), d = va_arg(arguments, unsigned long);
    va_end(arguments);
    return ((int (*)(int, unsigned long, unsigned long, unsigned long, unsigned long))note(
        "prctl"))(option, a, b, c, d);
}
int sigaltstack(const stack_t *stack, stack_t *old) {
    return ((int (*)(const stack_t *, stack_t *))note("sigaltstack"))(stack, old);
}
int mprotect(void *address, size_t length, int protection) {
    return ((int (*)(void *, size_t, int))note("mprotect"))(address, length, protection);
}
int pthread_sigmask(int how, const sigset_t *set, sigset_t *old) {
    return ((int (*)(int, const sigset_t *, sigset_t *))note("pthread_sigmask"))(how, set, old);
}
int fstat(int fd, struct stat *metadata) {
    return ((int (*)(int, struct stat *))note("fstat"))(fd, metadata);
}
int open(const char *path, int flags, ...) {
    /* The mode argument exists only with O_CREAT or O_TMPFILE, whose value
     * includes O_DIRECTORY, so all of its bits must be set. */
    unsigned mode = 0;
    if ((flags & O_CREAT) != 0 || (flags & O_TMPFILE) == O_TMPFILE) {
        va_list arguments;
        va_start(arguments, flags);
        mode = va_arg(arguments, unsigned);
        va_end(arguments);
    }
    return ((int (*)(const char *, int, unsigned))note("open"))(path, flags, mode);
}
int pipe2(int fds[2], int flags) { return ((int (*)(int *, int))note("pipe2"))(fds, flags); }
int socketpair(int domain, int type, int protocol, int fds[2]) {
    return ((int (*)(int, int, int, int *))note("socketpair"))(domain, type, protocol, fds);
}
ssize_t readlink(const char *path, char *buffer, size_t size) {
    return ((ssize_t (*)(const char *, char *, size_t))note("readlink"))(path, buffer, size);
}
"#;

/// With [`ALLOCATING_LIBC_WRAPPERS`] preloaded after the runtime, the heap
/// fixture must report what it reports natively with the same preload: an
/// untouched heap, no wrapper called before `main`, and the same first
/// allocation as without the preload, under both in-guest backends.
#[test]
fn liteinst_in_guest_runtime_calls_none_of_the_converted_libc_wrappers() {
    let _guard = hermit_run_guard();
    let build_root = process_build_root("liteinst-guest-heap");
    fs::create_dir_all(&build_root).expect("failed to create the guest directory");
    let guest = build_root.join("runtime_leaves_guest_heap_empty_wrapped");
    let compiled = Command::new("cc")
        .args(["-O2", "-g", "-Wall", "-Wextra", "-Werror"])
        .arg(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/liteinst_runtime_leaves_guest_heap_empty.c"),
        )
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("failed to compile the guest");
    assert!(compiled.status.success(), "{compiled:?}");
    let source = build_root.join("allocating_libc_wrappers.c");
    fs::write(&source, ALLOCATING_LIBC_WRAPPERS).expect("failed to write the wrappers");
    let wrappers = build_root.join("liballocating_libc_wrappers.so");
    let compiled = Command::new("cc")
        // No _FORTIFY_SOURCE: its inline open would clash with the wrapper's.
        .args([
            "-O2",
            "-U_FORTIFY_SOURCE",
            "-Wall",
            "-Werror",
            "-shared",
            "-fPIC",
            "-o",
        ])
        .arg(&wrappers)
        .arg(&source)
        .arg("-ldl")
        .output()
        .expect("failed to compile the wrappers");
    assert!(compiled.status.success(), "{compiled:?}");
    let wrappers = wrappers.to_str().expect("a UTF-8 path").to_owned();
    assert!(
        !wrappers.contains([' ', ':']),
        "LD_PRELOAD cannot name {wrappers}"
    );

    let native = Command::new(&guest)
        .env("LD_PRELOAD", &wrappers)
        .output()
        .expect("failed to run the guest natively");
    assert!(native.status.success(), "{native:?}");
    let native_stdout = String::from_utf8_lossy(&native.stdout).into_owned();
    let native_lines: Vec<&str> = native_stdout.lines().collect();
    assert_eq!(native_lines.len(), 3, "{native_stdout}");
    assert_eq!(native_lines[2], "interposed calls before main: 0");

    for (backend, selected) in [
        ("liteinst", IN_GUEST_SELECTED),
        ("in-guest-trap", IN_GUEST_TRAP_SELECTED),
    ] {
        let run = |preload: Option<&str>| {
            let mut command = Command::new(hermit_binary());
            command
                .args(["--log=error", "--backend", backend, "run"])
                .args([
                    "--max-timeslice=disabled",
                    "--strict",
                    "--base-env=minimal",
                    "--mount=type=tmpfs,target=/test",
                    "--workdir=/test",
                ]);
            if let Some(preload) = preload {
                command.arg(format!("--env=LD_PRELOAD={preload}"));
            }
            let output = super::hermit_test::hermit_output(
                "hermit run",
                command.arg("--").arg(&guest).stdin(Stdio::null()),
            );
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            assert!(
                output.status.success(),
                "{backend}: status={:?}\nstdout={stdout}\nstderr={stderr}",
                output.status,
            );
            assert!(
                stderr.lines().any(|line| line == selected),
                "{backend}: {stderr}"
            );
            stdout
        };
        let wrapped = run(Some(&wrappers));
        let plain = run(None);
        let wrapped_lines: Vec<&str> = wrapped.lines().collect();
        let plain_lines: Vec<&str> = plain.lines().collect();
        assert_eq!(
            (wrapped_lines.first(), wrapped_lines.get(2)),
            (native_lines.first(), native_lines.get(2)),
            "{backend}: a preloaded libc wrapper ran inside the guest before main\n{wrapped}"
        );
        assert_eq!(
            wrapped_lines.get(1),
            plain_lines.get(1),
            "{backend}: the preload moved the guest's first allocation\n{wrapped}"
        );
    }
}

/// A fork child's runtime state is rebuilt from the fork's return path inside
/// the child, outside any Tool callback, and binding the child's branch
/// counter there allocated its box through the child's malloc whenever the
/// host's PMU let it bind (and not when it could not): the child's heap then
/// held one more chunk than the parent's, and the child's first allocation
/// landed after it. The fixture prints the child's heap after one syscall and
/// its first allocation, and the parent's after the child exits. Under both
/// in-guest backends every line must match the native run once the addresses
/// are set aside, and the child's first allocation must land where the
/// parent's does.
///
/// This host's counter binds, so this covers the successful binding; Hermit
/// gives the guest no way to make it fail (the guest's own seccomp filters are
/// refused). Reverie's `clock_allocations` test covers binding that fails, by
/// denying perf_event_open to one thread.
#[test]
fn liteinst_in_guest_fork_child_heap_is_the_parents() {
    let _guard = hermit_run_guard();
    let build_root = process_build_root("liteinst-guest-heap");
    fs::create_dir_all(&build_root).expect("failed to create the guest directory");
    let guest = build_root.join("fork_child_heap");
    let compiled = Command::new("cc")
        .args(["-O2", "-g", "-Wall", "-Wextra", "-Werror"])
        .arg(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/liteinst_fork_child_heap.c"),
        )
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("failed to compile the guest");
    assert!(compiled.status.success(), "{compiled:?}");
    // Each line without its address, and the addresses of the child's and the
    // parent's first allocations.
    fn split(stdout: &str) -> (Vec<String>, Option<String>, Option<String>) {
        let mut lines = Vec::new();
        let (mut child, mut parent) = (None, None);
        for line in stdout.lines() {
            let (text, first) = match line.split_once(" first=") {
                Some((text, first)) => (text, Some(first.to_owned())),
                None => (line, None),
            };
            if text.starts_with("child:") {
                child = first;
            } else if text.starts_with("parent after wait:") {
                parent = first;
            }
            lines.push(text.to_owned());
        }
        (lines, child, parent)
    }

    let native = Command::new(&guest)
        .output()
        .expect("failed to run the guest natively");
    assert!(native.status.success(), "{native:?}");
    let native_stdout = String::from_utf8_lossy(&native.stdout).into_owned();
    let (native_lines, native_child, native_parent) = split(&native_stdout);
    assert_eq!(native_lines.len(), 4, "{native_stdout}");
    assert_eq!(native_child, native_parent, "{native_stdout}");

    for (backend, selected) in [
        ("liteinst", IN_GUEST_SELECTED),
        ("in-guest-trap", IN_GUEST_TRAP_SELECTED),
    ] {
        let output = super::hermit_test::hermit_output(
            "hermit run",
            Command::new(hermit_binary())
                .args(["--log=error", "--backend", backend, "run"])
                .args([
                    "--max-timeslice=disabled",
                    "--strict",
                    "--base-env=minimal",
                    "--mount=type=tmpfs,target=/test",
                    "--workdir=/test",
                    "--",
                ])
                .arg(&guest)
                .stdin(Stdio::null()),
        );
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "{backend}: status={:?}\nstdout={stdout}\nstderr={stderr}",
            output.status,
        );
        assert!(
            stderr.lines().any(|line| line == selected),
            "{backend}: {stderr}"
        );
        let (lines, child, parent) = split(&stdout);
        assert_eq!(
            lines, native_lines,
            "{backend}: the in-guest runtime left allocations in a heap \
             (native:\n{native_stdout})\n{stdout}"
        );
        assert!(child.is_some(), "{backend}: {stdout}");
        assert_eq!(
            child, parent,
            "{backend}: the child's first allocation landed elsewhere than the parent's\n{stdout}"
        );
    }
}

/// With guest SIGALRM handlers admitted (REVERIE_LITEINST_SIGALRM_HANDLERS=1
/// with site patching off), installation also records which file is the C
/// library. It found it with `dlopen(RTLD_NOLOAD)`, which allocates through
/// the C library's malloc on the library's first direct open, so a chunk
/// stayed in the guest's heap before `main` (704 bytes in use instead of
/// none). The fixture allocates nothing itself; it reports the heap at
/// `main`'s entry, installs a SIGALRM handler, takes one SIGALRM and reports
/// the heap again. Under `--backend=in-guest-trap` and under
/// `--backend=liteinst` with site patching off, the handler must be admitted
/// and every line must match the native run.
#[test]
fn liteinst_in_guest_admitted_sigalrm_leaves_the_guest_heap_as_the_program_made_it() {
    let _guard = hermit_run_guard();
    let build_root = process_build_root("liteinst-guest-heap");
    fs::create_dir_all(&build_root).expect("failed to create the guest directory");
    let guest = build_root.join("sigalrm_heap");
    let compiled = Command::new("cc")
        .args(["-O2", "-g", "-Wall", "-Wextra", "-Werror"])
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/liteinst_sigalrm_heap.c"))
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("failed to compile the guest");
    assert!(compiled.status.success(), "{compiled:?}");
    let native = Command::new(&guest)
        .output()
        .expect("failed to run the guest natively");
    assert!(native.status.success(), "{native:?}");
    let native_stdout = String::from_utf8_lossy(&native.stdout).into_owned();
    assert!(
        native_stdout
            .lines()
            .any(|line| line == "install=0 errno=0 runs=1"),
        "{native_stdout}"
    );

    for (backend, selected, site_patching) in [
        ("in-guest-trap", IN_GUEST_TRAP_SELECTED, None),
        (
            "liteinst",
            IN_GUEST_SELECTED,
            Some("--env=REVERIE_LITEINST_SITE_PATCHING=0"),
        ),
    ] {
        let output = super::hermit_test::hermit_output(
            "hermit run",
            Command::new(hermit_binary())
                .args(["--log=error", "--backend", backend, "run"])
                .args([
                    "--max-timeslice=disabled",
                    "--strict",
                    "--base-env=minimal",
                    "--mount=type=tmpfs,target=/test",
                    "--workdir=/test",
                    "--env=REVERIE_LITEINST_SIGALRM_HANDLERS=1",
                ])
                .args(site_patching)
                .arg("--")
                .arg(&guest)
                .stdin(Stdio::null()),
        );
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "{backend}: status={:?}\nstdout={stdout}\nstderr={stderr}",
            output.status,
        );
        assert!(
            stderr.lines().any(|line| line == selected),
            "{backend}: {stderr}"
        );
        assert_eq!(
            stdout, native_stdout,
            "{backend}: the admitted in-guest runtime left allocations in the guest's heap"
        );
    }
}

#[test]
fn liteinst_in_guest_detcore_micro_suite() {
    let _guard = hermit_run_guard();
    assert_liteinst_in_guest(Path::new("/bin/true"), &[], b"");
    assert_liteinst_in_guest(Path::new("/bin/echo"), &["hello"], b"hello\n");

    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let readme = repository.join("README.md");
    let expected = fs::read(&readme).expect("read README fixture");
    assert_liteinst_in_guest(
        Path::new("/bin/cat"),
        &[readme.to_str().unwrap()],
        &expected,
    );
}

#[test]
fn liteinst_in_guest_identity_utilities() {
    let _guard = hermit_run_guard();
    assert_liteinst_in_guest(Path::new("/usr/bin/uname"), &["-s"], b"Linux\n");
    assert_liteinst_in_guest(Path::new("/usr/bin/id"), &["-u"], b"0\n");
    assert_liteinst_in_guest(Path::new("/usr/bin/whoami"), &[], b"root\n");
}

#[test]
fn liteinst_in_guest_virtual_identity_and_time() {
    let _guard = hermit_run_guard();
    assert_liteinst_virtual_time_is_continuous();
    assert_liteinst_in_guest(
        Path::new("/usr/bin/hostname"),
        &[],
        b"hermetic-container.local\n",
    );
    let group_file = fs::read_to_string("/etc/group").expect("failed to read host group database");
    let root_group = group_name_by_gid(&group_file, "0").expect("GID 0 should have a name");
    let overflow_group = group_name_by_gid(&group_file, "65534").unwrap_or("nobody");
    // Detcore reports both primary GIDs as zero. Container::map_root maps the
    // caller's effective GID to zero; other inherited supplementary groups
    // appear as the overflow GID. A group-file entry does not add membership.
    // The groups utility prints each distinct GID only once.
    let effective_gid = nix::unistd::getegid();
    let has_overflow_group = nix::unistd::getgroups()
        .expect("failed to read caller supplementary groups")
        .iter()
        .any(|gid| *gid != effective_gid);
    let expected_groups = if has_overflow_group {
        format!("{root_group} {overflow_group}\n")
    } else {
        format!("{root_group}\n")
    };
    assert_liteinst_in_guest(
        Path::new("/usr/bin/groups"),
        &[],
        expected_groups.as_bytes(),
    );
}

#[test]
fn liteinst_in_guest_file_and_text_utilities() {
    let _guard = hermit_run_guard();
    let fixture = compatibility_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");

    assert_liteinst_in_guest(
        Path::new("/usr/bin/printf"),
        &["liteinst-printf-ok\n"],
        b"liteinst-printf-ok\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/grep"),
        &["^liteinst", fixture],
        COMPAT_FIXTURE_CONTENT,
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/head"),
        &["-n", "1", fixture],
        COMPAT_FIXTURE_CONTENT,
    );

    let expected_wc = format!("{} {fixture}\n", COMPAT_FIXTURE_CONTENT.len());
    assert_liteinst_in_guest(
        Path::new("/usr/bin/wc"),
        &["-c", fixture],
        expected_wc.as_bytes(),
    );
    let expected_sha256 = format!("{COMPAT_FIXTURE_SHA256}  {fixture}\n");
    assert_liteinst_in_guest(
        Path::new("/usr/bin/sha256sum"),
        &[fixture],
        expected_sha256.as_bytes(),
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/stat"),
        &["-c", "%s", fixture],
        format!("{}\n", COMPAT_FIXTURE_CONTENT.len()).as_bytes(),
    );
}

#[test]
fn liteinst_in_guest_semantic_text_utilities() {
    let _guard = hermit_run_guard();
    let fixture = semantic_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");

    assert_liteinst_in_guest(
        Path::new("/usr/bin/tail"),
        &["-n", "2", fixture],
        b"alpha:1\nbeta:2\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/uniq"),
        &[fixture],
        b"gamma:3\nalpha:1\nbeta:2\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/cut"),
        &["-d", ":", "-f", "1", fixture],
        b"gamma\nalpha\nalpha\nbeta\n",
    );
    assert_liteinst_in_guest(Path::new("/usr/bin/diff"), &[fixture, fixture], b"");
    assert_liteinst_in_guest(
        Path::new("/usr/bin/sed"),
        &["-n", "2,3p", fixture],
        b"alpha:1\nalpha:1\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/sort"),
        &[fixture],
        b"alpha:1\nalpha:1\nbeta:2\ngamma:3\n",
    );
}

#[test]
fn liteinst_in_guest_semantic_file_and_sqlite_utilities() {
    let _guard = hermit_run_guard();
    let fixture = semantic_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");

    assert_liteinst_in_guest(
        Path::new("/usr/bin/find"),
        &[fixture, "-maxdepth", "0", "-type", "f", "-print"],
        format!("{fixture}\n").as_bytes(),
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/md5sum"),
        &[fixture],
        format!("{SEMANTIC_FIXTURE_MD5}  {fixture}\n").as_bytes(),
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/du"),
        &["-b", fixture],
        format!("{}\t{fixture}\n", SEMANTIC_FIXTURE_CONTENT.len()).as_bytes(),
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/sqlite3"),
        &[
            ":memory:",
            "CREATE TABLE t(v); INSERT INTO t VALUES(3),(1),(2); \
             SELECT v FROM t ORDER BY v;",
        ],
        b"1\n2\n3\n",
    );
}

#[test]
fn liteinst_in_guest_encoding_and_digest_utilities() {
    let _guard = hermit_run_guard();
    let fixture = compatibility_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");

    assert_liteinst_in_guest(
        Path::new("/usr/bin/base64"),
        &["--wrap=0", fixture],
        b"bGl0ZWluc3QgY29tcGF0aWJpbGl0eSBmaXh0dXJlCg==",
    );
    for (program, digest) in [
        ("/usr/bin/sha1sum", COMPAT_FIXTURE_SHA1),
        ("/usr/bin/sha224sum", COMPAT_FIXTURE_SHA224),
        ("/usr/bin/sha384sum", COMPAT_FIXTURE_SHA384),
        ("/usr/bin/sha512sum", COMPAT_FIXTURE_SHA512),
        ("/usr/bin/b2sum", COMPAT_FIXTURE_BLAKE2),
    ] {
        let expected = format!("{digest}  {fixture}\n");
        assert_liteinst_in_guest(Path::new(program), &[fixture], expected.as_bytes());
    }
    let expected_cksum = format!("2216041199 {} {fixture}\n", COMPAT_FIXTURE_CONTENT.len());
    assert_liteinst_in_guest(
        Path::new("/usr/bin/cksum"),
        &[fixture],
        expected_cksum.as_bytes(),
    );
}

#[test]
fn liteinst_in_guest_formatting_and_sequence_utilities() {
    let _guard = hermit_run_guard();
    let compat_fixture = compatibility_fixture();
    let compat_fixture = compat_fixture
        .to_str()
        .expect("fixture path should be UTF-8");
    let semantic_fixture = semantic_fixture();
    let semantic_fixture = semantic_fixture
        .to_str()
        .expect("fixture path should be UTF-8");

    assert_liteinst_in_guest(Path::new("/usr/bin/seq"), &["5"], b"1\n2\n3\n4\n5\n");
    assert_liteinst_in_guest(
        Path::new("/usr/bin/fmt"),
        &["--width=10", compat_fixture],
        b"liteinst\ncompatibility\nfixture\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/fold"),
        &["--width=8", compat_fixture],
        b"liteinst\n compati\nbility f\nixture\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/nl"),
        &["-ba", semantic_fixture],
        b"     1\tgamma:3\n     2\talpha:1\n     3\talpha:1\n     4\tbeta:2\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/tac"),
        &[semantic_fixture],
        b"beta:2\nalpha:1\nalpha:1\ngamma:3\n",
    );
}

#[test]
fn liteinst_in_guest_round2_encoding_and_comparison_utilities() {
    let _guard = hermit_run_guard();
    let fixture = compatibility_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");

    assert_liteinst_in_guest(
        Path::new("/usr/bin/base32"),
        &["--wrap=0", fixture],
        b"NRUXIZLJNZZXIIDDN5WXAYLUNFRGS3DJOR4SAZTJPB2HK4TFBI======",
    );
    let sum_output = run_liteinst_in_guest(Path::new("/usr/bin/sum"), &[fixture]);
    let sum_stdout = String::from_utf8(sum_output.stdout).expect("sum output should be UTF-8");
    let sum_fields = sum_stdout.split_whitespace().collect::<Vec<_>>();
    match sum_fields.as_slice() {
        ["04458", "1"] => {}
        ["04458", "1", output_path] => assert_eq!(*output_path, fixture),
        _ => panic!("unexpected sum output: {sum_stdout:?}"),
    }
    assert_liteinst_in_guest(Path::new("/usr/bin/cmp"), &[fixture, fixture], b"");
    assert_liteinst_in_guest(
        Path::new("/usr/bin/comm"),
        &[fixture, fixture],
        b"\t\tliteinst compatibility fixture\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/join"),
        &[fixture, fixture],
        b"liteinst compatibility fixture compatibility fixture\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/paste"),
        &[fixture, fixture],
        b"liteinst compatibility fixture\tliteinst compatibility fixture\n",
    );
}

#[test]
fn liteinst_in_guest_round2_representation_and_path_utilities() {
    let _guard = hermit_run_guard();
    let fixture = compatibility_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");

    assert_liteinst_in_guest(
        Path::new("/usr/bin/od"),
        &["-An", "-tx1", fixture],
        b" 6c 69 74 65 69 6e 73 74 20 63 6f 6d 70 61 74 69\n 62 69 6c 69 74 79 20 66 69 78 74 75 72 65 0a\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/pr"),
        &["-t", fixture],
        COMPAT_FIXTURE_CONTENT,
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/readlink"),
        &["-f", "/etc/../etc/hostname"],
        b"/etc/hostname\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/rev"),
        &[fixture],
        b"erutxif ytilibitapmoc tsnietil\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/strings"),
        &[fixture],
        COMPAT_FIXTURE_CONTENT,
    );
    let dd_input = format!("if={fixture}");
    assert_liteinst_in_guest(
        Path::new("/usr/bin/dd"),
        &[&dd_input, "bs=7", "count=2", "status=none"],
        b"liteinst compa",
    );
}

#[test]
fn liteinst_in_guest_round2_arithmetic_and_predicate_utilities() {
    let _guard = hermit_run_guard();
    let fixture = compatibility_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");

    assert_liteinst_in_guest(Path::new("/usr/bin/factor"), &["84"], b"84: 2 2 3 7\n");
    assert_liteinst_in_guest(Path::new("/usr/bin/expr"), &["6", "*", "7"], b"42\n");
    assert_liteinst_in_guest(
        Path::new("/usr/bin/numfmt"),
        &["--to=iec", "1024"],
        b"1.0K\n",
    );
    assert_liteinst_in_guest(Path::new("/usr/bin/test"), &["-f", fixture], b"");
    assert_liteinst_in_guest(Path::new("/usr/bin/pathchk"), &[fixture], b"");
}

#[test]
fn liteinst_in_guest_round3_portable_system_utilities() {
    let _guard = hermit_run_guard();
    assert_liteinst_in_guest(Path::new("/usr/bin/arch"), &[], b"x86_64\n");
    assert_liteinst_in_guest(Path::new("/usr/bin/getconf"), &["LONG_BIT"], b"64\n");
    assert_liteinst_in_guest(
        Path::new("/usr/bin/getopt"),
        &["-o", "ab:", "--", "-a", "-b", "value", "rest"],
        b" -a -b 'value' -- 'rest'\n",
    );
    assert_liteinst_in_guest(
        Path::new("/bin/bash"),
        &[
            "--noprofile",
            "--norc",
            "-c",
            "printf 'liteinst-bash-ok\\n'",
        ],
        b"liteinst-bash-ok\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/jq"),
        &["-nr", "[3,1,2] | sort | join(\",\")"],
        b"1,2,3\n",
    );

    let existing_directory = compatibility_fixture()
        .parent()
        .expect("compatibility fixture should have a parent directory");
    assert_liteinst_in_guest(
        Path::new("/usr/bin/mkdir"),
        &[
            "-p",
            existing_directory.to_str().expect("path should be UTF-8"),
        ],
        b"",
    );
}

#[test]
fn liteinst_in_guest_round3_encoding_and_compression_utilities() {
    let _guard = hermit_run_guard();
    let fixture = compatibility_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");
    assert_liteinst_in_guest(
        Path::new("/usr/bin/iconv"),
        &["-f", "UTF-8", "-t", "UTF-16LE", fixture],
        b"l\0i\0t\0e\0i\0n\0s\0t\0 \0c\0o\0m\0p\0a\0t\0i\0b\0i\0l\0i\0t\0y\0 \0f\0i\0x\0t\0u\0r\0e\0\n\0",
    );

    // xz is not here: it installs signal handlers before it decompresses,
    // and the in-guest runtime refuses every guest handler other than
    // SIG_DFL and SIG_IGN (https://github.com/rrnewton/reverie/issues/243).
    // It returns when that is fixed.
    let [gzip_fixture, bzip2_fixture] = compressed_fixtures();
    for (program, compressed_fixture) in [
        ("/usr/bin/gzip", gzip_fixture),
        ("/usr/bin/bzip2", bzip2_fixture),
    ] {
        assert_liteinst_in_guest(
            &elf_program(Path::new(program)),
            &[
                "-cd",
                compressed_fixture
                    .to_str()
                    .expect("compressed fixture path should be UTF-8"),
            ],
            COMPAT_FIXTURE_CONTENT,
        );
    }
}

#[test]
fn liteinst_in_guest_round3_stdin_filter_utilities() {
    let _guard = hermit_run_guard();
    let output = run_liteinst_in_guest_with_stdin(
        Path::new("/usr/bin/tr"),
        &["a-z", "A-Z"],
        b"gamma\nalpha\nbeta\n",
    );
    assert_eq!(output.stdout, b"GAMMA\nALPHA\nBETA\n");

    let output =
        run_liteinst_in_guest_with_stdin(Path::new("/usr/bin/tee"), &[], b"liteinst-tee-ok\n");
    assert_eq!(output.stdout, b"liteinst-tee-ok\n");

    let output = run_liteinst_in_guest_with_stdin(Path::new("/usr/bin/tsort"), &[], b"a b\nb c\n");
    assert_eq!(output.stdout, b"a\nb\nc\n");
}

#[test]
fn liteinst_in_guest_path_and_language_utilities() {
    let _guard = hermit_run_guard();
    assert_liteinst_in_guest(
        Path::new("/usr/bin/basename"),
        &["/tmp/hermit-example.txt", ".txt"],
        b"hermit-example\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/dirname"),
        &["/tmp/hermit-example.txt"],
        b"/tmp\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/realpath"),
        &["/etc/../etc/passwd"],
        b"/etc/passwd\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/ls"),
        &["-1", "/etc/hostname"],
        b"/etc/hostname\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/awk"),
        &["BEGIN { for (i = 1; i <= 10; ++i) sum += i; print sum }"],
        b"55\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/perl"),
        &["-e", r#"print join(q{,}, map { $_ * $_ } 1..5), qq{\n}"#],
        b"1,4,9,16,25\n",
    );
}

#[test]
fn liteinst_in_guest_shell_and_entropy_consumer() {
    let _guard = hermit_run_guard();
    assert_liteinst_in_guest(
        Path::new("/bin/sh"),
        &["-c", "printf 'liteinst-shell-ok\\n'"],
        b"liteinst-shell-ok\n",
    );
    assert_liteinst_in_guest(
        Path::new("/usr/bin/hexdump"),
        &["/dev/urandom", "--length", "16"],
        b"0000000 7229 04bb 964d 28df ba71 4c03 de95 7027\n0000010\n",
    );
}

/// Without `--verify`, the second run stands in for the determinism verdict
/// the hybrid's verified run gave: the guest prints eight bytes of
/// `/dev/urandom`, which must be the same in both runs.
#[test]
fn liteinst_in_guest_python_entropy() {
    let _guard = hermit_run_guard();
    let run = || {
        let output = run_liteinst_in_guest(
            Path::new("/usr/bin/python3"),
            &[
                "-c",
                "import os; print(os.getpid(), len(os.urandom(8)), os.urandom(8).hex())",
            ],
        );
        String::from_utf8(output.stdout).expect("Python output should be UTF-8")
    };
    let stdout = run();
    let fields = stdout.split_whitespace().collect::<Vec<_>>();
    assert_eq!(fields.len(), 3, "stdout={stdout:?}");
    // The container init is PID 1. Synchronous tracing consumes TID 2 to
    // match the other logging paths, so the root guest starts as PID 3.
    assert_eq!(fields[0], "3", "stdout={stdout:?}");
    assert_eq!(fields[1], "8", "stdout={stdout:?}");
    assert_eq!(fields[2].len(), 16, "stdout={stdout:?}");
    assert!(
        fields[2].bytes().all(|byte| byte.is_ascii_hexdigit()),
        "stdout={stdout:?}"
    );
    assert_eq!(run(), stdout, "the second run printed different entropy");
}

/// `examples/rand.py` prints ten values, each in `1..=101`.
///
/// `random` imports `hashlib`, whose `_hashlib` extension loads
/// `libcrypto.so.3`, and libcrypto's initializer executes CPUID in code mapped
/// after the in-guest runtime started. Before the pinned Reverie emulated such
/// instructions through the Tool, the guest died there with SIGSEGV (exit 139);
/// `liteinst_in_guest_cpuid_in_a_late_loaded_library_runs` covers the same path
/// without Python.
#[test]
fn liteinst_in_guest_python_random_example() {
    let _guard = hermit_run_guard();
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let output = run_liteinst_in_guest(&repository.join("examples/rand.py"), &[]);
    let stdout = String::from_utf8(output.stdout).expect("Python output should be UTF-8");
    let values = stdout
        .split_whitespace()
        .map(|field| field.parse::<u8>().expect("random value should be decimal"))
        .collect::<Vec<_>>();
    assert_eq!(values.len(), 10, "stdout={stdout:?}");
    assert!(
        values.iter().all(|value| (1..=101).contains(value)),
        "stdout={stdout:?}"
    );
}

static LATE_CPUID_GUEST: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();

/// Builds `tests/c/dlopen_cpuid_vendor.c` and the shared library
/// `tests/c/dlopen_cpuid_vendor_lib.c` it loads, under `CARGO_TARGET_TMPDIR`.
fn late_cpuid_guest() -> &'static (PathBuf, PathBuf) {
    LATE_CPUID_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("dlopen-cpuid-vendor");
        fs::create_dir_all(&build_root).expect("failed to create late-CPUID guest directory");
        let library = build_root.join("libdlopen_cpuid_vendor.so");
        let guest = build_root.join("dlopen_cpuid_vendor");
        for (source, output, flags) in [
            (
                "tests/c/dlopen_cpuid_vendor_lib.c",
                &library,
                &["-shared", "-fPIC"][..],
            ),
            ("tests/c/dlopen_cpuid_vendor.c", &guest, &[][..]),
        ] {
            let compiled = Command::new("cc")
                .args(["-O2", "-Wall", "-Wextra", "-Werror"])
                .args(flags)
                .arg(repository.join(source))
                .arg("-o")
                .arg(output)
                .arg("-ldl")
                .output()
                .unwrap_or_else(|error| panic!("failed to compile {source}: {error}"));
            assert!(
                compiled.status.success(),
                "{source} compilation failed:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&compiled.stdout),
                String::from_utf8_lossy(&compiled.stderr),
            );
        }
        (guest, library)
    })
}

/// CPUID executed by a library the guest loads with `dlopen` after start-up.
///
/// The guest executes CPUID leaf 0 in its main executable, then dlopens a
/// library whose constructor executes it again, and prints both vendor
/// strings. Both must be present and equal: Detcore answers CPUID the same way
/// wherever the instruction is. The ptrace backend runs the same guest first as
/// the control, so a failure here is not a broken guest.
///
/// The library is mapped after the in-guest runtime started, so its CPUID has
/// no patch arena; the runtime must emulate it through the Tool. Without that
/// (Reverie before the late-code CPUID fix), the guest printed the main
/// executable's line and then died with SIGSEGV (exit 139) in the constructor,
/// the defect that also broke `import hashlib`
/// (`liteinst_in_guest_python_random_example`).
#[test]
fn liteinst_in_guest_cpuid_in_a_late_loaded_library_runs() {
    let _guard = hermit_run_guard();
    let (guest, library) = late_cpuid_guest();
    let check = |output: &Output, backend: &str| {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "{backend}: status={:?}\nstdout={stdout}\nstderr={stderr}",
            output.status
        );
        let lines = stdout.lines().collect::<Vec<_>>();
        let [main, loaded] = lines[..] else {
            panic!("{backend}: expected two vendor lines, got {stdout:?}");
        };
        let main = main
            .strip_prefix("main-vendor=")
            .unwrap_or_else(|| panic!("{backend}: no main-vendor line in {stdout:?}"));
        let loaded = loaded
            .strip_prefix("library-vendor=")
            .unwrap_or_else(|| panic!("{backend}: no library-vendor line in {stdout:?}"));
        assert!(!main.is_empty(), "{backend}: empty vendor in {stdout:?}");
        assert_eq!(main, loaded, "{backend}: {stdout:?}");
    };

    let control = super::hermit_test::hermit_output(
        "hermit run",
        Command::new(hermit_binary())
            .args(["--log=error", "--backend=ptrace", "run"])
            .args([
                "--strict",
                "--max-timeslice=disabled",
                "--base-env=minimal",
                "--",
            ])
            .arg(guest)
            .arg(library)
            .stdin(Stdio::null()),
    );
    check(&control, "ptrace");

    let output = super::hermit_test::hermit_output(
        "hermit run",
        liteinst_command("error")
            .arg("--")
            .arg(guest)
            .arg(library)
            .stdin(Stdio::null()),
    );
    check(&output, "liteinst");
    assert_in_guest_selected(&String::from_utf8_lossy(&output.stderr));
}

/// A forking guest finishes under in-guest LiteInst.
///
/// `fork-ok` is the guest's own end-of-mode line, printed only after the child
/// has been created, run and reaped (see `tests/c/liteinst_advanced.c`), so a
/// regression that skips the child cannot satisfy this by exiting zero. The
/// negative assertions carry the two failure shapes the hybrid test existed
/// for: `ENOTSUPP` is a refused task creation, and `Bad system call` is a
/// SIGSYS the guest must never receive. The guest's `threads` mode is not run
/// here: in-guest LiteInst refuses thread clone ("clone injection requires
/// ptrace fallback").
#[test]
fn liteinst_in_guest_fork_runs_without_hanging() {
    let _guard = hermit_run_guard();
    let mut command = liteinst_command("error");
    let mut child = command
        .arg("--")
        .arg(advanced_guest())
        .arg("fork")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to start Hermit LiteInst fork guest");
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("failed to poll Hermit LiteInst") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("Hermit LiteInst hung running fork");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let output = child
        .wait_with_output()
        .expect("failed to collect Hermit LiteInst fork output");
    assert_eq!(output.status, status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        status.code(),
        Some(0),
        "status={:?}\nstdout={stdout}\nstderr={stderr}",
        output.status,
    );
    assert!(
        stdout.contains("fork-ok"),
        "guest did not reach the end of fork\nstdout={stdout}\nstderr={stderr}",
    );
    assert!(
        !stderr.contains("ENOTSUPP (Operation is not supported)"),
        "{stderr}"
    );
    assert!(!stderr.contains("Bad system call"), "{stderr}");
    assert_in_guest_selected(&stderr);
}

#[test]
fn liteinst_in_guest_abnormal_exit_after_registration_does_not_hang() {
    let _guard = hermit_run_guard();
    // INFO-level Detcore diagnostics can exceed a pipe's capacity before the
    // guest reaches its fatal signal. Keep draining out of the child process
    // while retaining the diagnostics for the scheduler-start assertion.
    let mut stderr = tempfile::tempfile().expect("create LiteInst diagnostic sink");
    let stderr_sink = stderr.try_clone().expect("clone LiteInst diagnostic sink");
    let mut command = liteinst_command("info");
    let mut child = command
        .args(["--", "/bin/sh", "-c", "kill -9 $$"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr_sink))
        .spawn()
        .expect("failed to start Hermit LiteInst fatal-exit guest");
    let deadline = Instant::now() + Duration::from_secs(5);

    let status = loop {
        if let Some(status) = child.try_wait().expect("failed to poll Hermit LiteInst") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("Hermit LiteInst hung after a registered guest exited by signal");
        }
        thread::sleep(Duration::from_millis(10));
    };

    let output = child
        .wait_with_output()
        .expect("failed to collect Hermit LiteInst output");
    stderr.rewind().expect("rewind LiteInst diagnostic sink");
    let mut diagnostics = String::new();
    stderr
        .read_to_string(&mut diagnostics)
        .expect("read LiteInst diagnostics");
    assert_eq!(
        status.signal(),
        Some(libc::SIGKILL),
        "{output:?}\nstderr={diagnostics}"
    );
    assert_eq!(output.status, status);
    assert!(
        diagnostics.contains("[scheduler] guest in queue"),
        "stderr={diagnostics}",
    );
    assert_in_guest_selected(&diagnostics);
}

// Regression coverage for https://github.com/rrnewton/hermit/issues/3338: the
// LiteInst runtime's preload constructor issues a few hundred syscalls before
// the guest's main runs. Charging them as guest syscalls pushed sysinfo(2)
// uptime from 121 to 123 under --max-timeslice=disabled, where each syscall is
// charged at the no-PMU rate.
static BOOTSTRAP_TIME_HOST_IDENTITY: OnceLock<PathBuf> = OnceLock::new();

fn bootstrap_time_host_identity() -> &'static Path {
    // The unchanged host-identity fixture. It asserts sysinfo.uptime == 121.
    BOOTSTRAP_TIME_HOST_IDENTITY.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("liteinst-bootstrap-time");
        fs::create_dir_all(&build_root).expect("failed to create bootstrap-time guest directory");
        let guest = build_root.join("host_identity");
        // -D_GNU_SOURCE matches the build flags that the c-programs/host-identity
        // cell in tests/e2e/manifests/c-programs.yaml gives host_identity.c.
        let output = Command::new("cc")
            .args(["-O2", "-g", "-Wall", "-Wextra", "-Werror", "-D_GNU_SOURCE"])
            .arg(repository.join("tests/c/host_identity.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile tests/c/host_identity.c");
        assert!(
            output.status.success(),
            "tests/c/host_identity.c compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

/// The runtime's own bootstrap is not charged to the guest's virtual time.
/// The hybrid version also ran `--verify --verify-strict --verify-json` and
/// required a matched, bitwise-parity report; that half is not restored yet
/// (https://github.com/rrnewton/hermit/issues/3520).
#[test]
fn liteinst_in_guest_runtime_bootstrap_is_not_charged_to_host_identity_uptime() {
    let _guard = hermit_run_guard();
    let home = tempfile::tempdir().expect("failed to create bootstrap-time HOME");
    let output = super::hermit_test::hermit_output(
        "hermit run",
        liteinst_command_at_epoch("info", Some(VIRTUAL_TIME_EPOCH))
            .args(["--env=LC_ALL=C", "--env=TZ=UTC"])
            .arg(format!("--env=HOME={}", home.path().display()))
            .env("HOME", home.path())
            .arg("--")
            .arg(bootstrap_time_host_identity())
            .stdin(Stdio::null()),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "status={:?}\nstdout={stdout}\nstderr={stderr}",
        output.status,
    );
    assert!(
        stdout.lines().any(|line| line == "sysinfo.uptime=121"),
        "LiteInst host_identity must observe uptime 121:\nstdout={stdout}\nstderr={stderr}"
    );
    assert_in_guest_selected(&stderr);
}

/// LiteInst's dispatch record: it measures its patch candidates and finds some
/// in the guest. `--max-timeslice=disabled` because in-guest LiteInst refuses
/// a maximum timeslice.
#[test]
fn liteinst_in_guest_dispatch_record_reports_patched_sites() {
    let _guard = hermit_run_guard();
    let guest = dispatch_stats::build_guest("guest-liteinst", &[]);
    let record = dispatch_stats::dispatch_record(
        "liteinst",
        hermit_binary(),
        &["--max-timeslice=disabled"],
        &[],
        &guest,
    );
    let candidates = record
        .sites
        .candidates
        .expect("LiteInst measures its candidates");
    assert!(candidates > 0, "{record}");
}

/// The line Hermit prints when it runs the guest under `--backend=in-guest-trap`
/// (`RunOpts::main` in `hermit-cli/src/bin/hermit/run.rs`).
const IN_GUEST_TRAP_SELECTED: &str = "hermit: [in-guest-trap] selected: the guest preload is to \
     host the Detcore Tool, with syscall site patching off (REVERIE_LITEINST_SITE_PATCHING=0)";

/// `--backend=in-guest-trap` is in-guest LiteInst with syscall site patching
/// off, which Hermit sets itself: its dispatch record, named `in-guest-trap`,
/// reports no site considered for patching and none patched, and every
/// dispatch arrives as an in-guest `SIGSYS` trap rather than a patched direct
/// call. The same guest under `--backend=liteinst`, with no variable
/// set by the caller in either run, patches sites: that control shows the
/// count can see patching, so the zero is the backend's doing.
#[test]
fn in_guest_trap_dispatch_record_reports_no_patched_sites() {
    let _guard = hermit_run_guard();
    let guest = dispatch_stats::build_guest("guest-in-guest-trap", &[]);
    let record = dispatch_stats::dispatch_record(
        "in-guest-trap",
        hermit_binary(),
        &["--max-timeslice=disabled"],
        &[],
        &guest,
    );
    assert_eq!(record.sites.patched, Some(0), "{record}");
    // With patching off the runtime makes no patch decision, so it records no
    // candidate either.
    assert_eq!(record.sites.candidates, Some(0), "{record}");
    assert_eq!(record.counters.patched_direct_calls, Some(0), "{record}");
    assert!(
        record
            .counters
            .signal_traps
            .is_some_and(|traps| traps >= dispatch_stats::GUEST_SYSCALLS),
        "{record}"
    );

    let control = dispatch_stats::dispatch_record(
        "liteinst",
        hermit_binary(),
        &["--max-timeslice=disabled"],
        &[],
        &guest,
    );
    assert!(
        control.sites.patched.is_some_and(|patched| patched > 0),
        "liteinst patched nothing, so a zero proves nothing about in-guest-trap: {control}"
    );
}

/// A real program verifies deterministically under `--backend=in-guest-trap`:
/// `--verify --verify-strict` matches with canonical bitwise parity over
/// nonzero INFO messages, and both runs' dispatch records report no syscall
/// site patched.
#[test]
fn in_guest_trap_verifies_a_program_with_no_site_patched() {
    let _guard = hermit_run_guard();
    let report_dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let report_path = report_dir.path().join("verify.json");
    let listed = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/common");
    let output = super::hermit_test::hermit_output(
        "hermit run",
        Command::new(hermit_binary())
            .args(["--log=info", "--backend", "in-guest-trap", "run"])
            .args([
                "--max-timeslice=disabled",
                "--strict",
                "--base-env=minimal",
                "--verify",
                "--verify-strict",
            ])
            .arg(format!("--verify-json={}", report_path.display()))
            .arg("--")
            .args(["/bin/ls", "-1"])
            .arg(&listed)
            .env_remove("RUST_LOG")
            .env_remove("HERMIT_LOG")
            .env_remove("HERMIT_LOG_FILE")
            .stdin(Stdio::null()),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "status={:?}\nstdout={}\nstderr={stderr}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
    );
    assert!(
        stderr.lines().any(|line| line == IN_GUEST_TRAP_SELECTED),
        "{stderr}"
    );
    assert!(!stderr.contains(IN_GUEST_SELECTED), "{stderr}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout
            .lines()
            .any(|line| line == "liteinst_in_guest_programs.rs"),
        "{stdout}"
    );

    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(&report_path).expect("--verify-json report")).unwrap();
    assert!(
        report["verified"] == true
            && report["bitwise_parity"] == true
            && report["verdict"] == "matched"
            && report["comparison"]["strictness"] == "canonical"
            && report["comparison"]["compare_logs"] == true,
        "{report}"
    );
    let messages = &report["compared_log_messages"];
    assert!(
        messages["left"].as_u64().is_some_and(|left| left > 0)
            && messages["left"] == messages["right"],
        "{report}"
    );
    for run in ["run1", "run2"] {
        let dispatch = &report["runtime"][run]["dispatch"];
        assert_eq!(dispatch["backend"], "in-guest-trap", "{run}: {report}");
        assert_eq!(dispatch["sites"]["patched"], 0, "{run}: {report}");
        assert_eq!(
            dispatch["counters"]["patched_direct_calls"], 0,
            "{run}: {report}"
        );
    }
}

/// Hermit, not the caller, decides site patching under `--backend=in-guest-trap`:
/// the guest sees `REVERIE_LITEINST_SITE_PATCHING=0` though nobody set it, a
/// caller value of `0` is accepted, and any other value is refused before the
/// guest starts (exit 122) with a message that names the value and the two
/// ways out.
#[test]
fn in_guest_trap_sets_site_patching_off_and_refuses_a_caller_who_turns_it_on() {
    let _guard = hermit_run_guard();
    let run = |env: Option<&str>| {
        let mut command = Command::new(hermit_binary());
        command
            .args(["--log=info", "--backend", "in-guest-trap", "run"])
            .args(["--max-timeslice=disabled", "--strict", "--base-env=minimal"]);
        if let Some(value) = env {
            command.arg(format!("--env=REVERIE_LITEINST_SITE_PATCHING={value}"));
        }
        super::hermit_test::hermit_output(
            "hermit run",
            command
                // A shell builtin prints the guest's value: no exec, and bash is
                // one of the pinned root's declared guest programs.
                .args([
                    "--",
                    "/usr/bin/bash",
                    "-c",
                    "printf '%s\\n' \"${REVERIE_LITEINST_SITE_PATCHING-unset}\"",
                ])
                .env_remove("REVERIE_LITEINST_SITE_PATCHING")
                .stdin(Stdio::null()),
        )
    };
    for env in [None, Some("0")] {
        let output = run(env);
        assert!(
            output.status.success(),
            "{env:?}: status={:?}\nstderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout), "0\n", "{env:?}");
    }
    let refused = run(Some("1"));
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(
        refused.status.code(),
        Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
        "stderr={stderr}"
    );
    assert!(refused.stdout.is_empty(), "the guest ran: {refused:?}");
    assert!(
        stderr.contains(
            "--backend=in-guest-trap (in-guest LiteInst, site patching off) refuses this run: \
             the guest environment sets REVERIE_LITEINST_SITE_PATCHING=1,"
        ),
        "{stderr}"
    );
    assert!(stderr.contains("select --backend=liteinst"), "{stderr}");
}

/// The program of `hermit-cli/tests/fixtures/in_guest_trap_constructor_main.c`,
/// linked against the shared library of
/// `in_guest_trap_constructor_lib.c`, whose constructor changes the in-guest
/// runtime's site-patching switch before the runtime's constructor runs.
fn constructor_mutation_guest() -> &'static Path {
    CONSTRUCTOR_MUTATION_GUEST.get_or_init(|| {
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let build_root = process_build_root("in-guest-trap-constructor");
        fs::create_dir_all(&build_root).expect("failed to create the fixture directory");
        let library = build_root.join("libin_guest_trap_constructor.so");
        let compiled = Command::new("cc")
            .args(["-O2", "-Wall", "-Wextra", "-Werror", "-shared", "-fPIC"])
            .arg(fixtures.join("in_guest_trap_constructor_lib.c"))
            .arg("-o")
            .arg(&library)
            .output()
            .expect("failed to compile the constructor library");
        assert!(compiled.status.success(), "{compiled:?}");
        let guest = build_root.join("in_guest_trap_constructor");
        let linked = Command::new("cc")
            .args(["-O2", "-Wall", "-Wextra", "-Werror"])
            .arg(fixtures.join("in_guest_trap_constructor_main.c"))
            .arg(&library)
            .arg(format!("-Wl,-rpath,{}", build_root.display()))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to link the constructor guest");
        assert!(linked.status.success(), "{linked:?}");
        guest
    })
}

fn run_constructor_mutation_guest(backend: &str, mode: &str) -> Output {
    super::hermit_test::hermit_output(
        "hermit run",
        Command::new(hermit_binary())
            // Quiet, so the guest's refusal message is not interleaved with
            // Detcore's own records on the same stderr.
            .args(["--log=error", "--backend", backend, "run"])
            .args(["--max-timeslice=disabled", "--strict", "--base-env=minimal"])
            .arg(format!("--env=IN_GUEST_TRAP_FIXTURE_CONSTRUCTOR={mode}"))
            .arg("--")
            .arg(constructor_mutation_guest())
            .env_remove("REVERIE_LITEINST_SITE_PATCHING")
            .stdin(Stdio::null()),
    )
}

/// A shared library's constructor runs before the in-guest runtime's and can
/// change the runtime's settings, which the runtime reads from the
/// environment while it installs. Under `--backend=in-guest-trap` each Tool
/// image compares the settings the runtime actually captured
/// (`reverie_liteinst::site_patching_enabled` and `guest_stats_enabled`) with
/// the host's request, which arrives over the coordinator connection, and
/// refuses to run the program when they differ. Refused before `main`, each
/// with its message: a constructor that sets REVERIE_LITEINST_SITE_PATCHING=1;
/// one that removes it (which turns patching on); one that sets it to 1 while
/// the library's interposed `getenv` sets it back to 0 during the runtime's
/// installation, after the runtime read 1 (a check of the environment at the
/// Tool's construction would see 0 and accept); and one that removes the
/// statistics variable. The coordinator then refuses each run too (exit 122):
/// for the sites the image's runtime patched, or because no process reported
/// statistics. Controls: the constructor leaving everything alone runs under
/// in-guest-trap; under liteinst, which patches by design, the restoring
/// constructor reaches `main` and shows its `getenv` put the variable back
/// to 0 during startup.
#[test]
fn in_guest_trap_refuses_a_guest_library_that_turns_site_patching_back_on() {
    let _guard = hermit_run_guard();
    const PATCHING: &str = "detcore-liteinst: initialization failed: --backend=in-guest-trap runs \
         with syscall site patching off, but the in-guest runtime installed this process with \
         site patching on";
    const NO_STATS: &str = "detcore-liteinst: initialization failed: --backend=in-guest-trap \
         needs every process's syscall site statistics, but the in-guest runtime installed this \
         process without collecting them";
    for (mode, image_refusal, run_refusal) in [
        ("set", PATCHING, "its guests' LiteInst statistics report "),
        ("unset", PATCHING, "its guests' LiteInst statistics report "),
        (
            "set-then-restore",
            PATCHING,
            "its guests' LiteInst statistics report ",
        ),
        (
            "no-stats",
            NO_STATS,
            "no guest process reported LiteInst statistics",
        ),
    ] {
        let output = run_constructor_mutation_guest("in-guest-trap", mode);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        // The image refuses from its constructor (exit 127, as every in-guest
        // initialization refusal does), and the coordinator's postcondition
        // refuses the run as well: the typed in-guest refusal, exit 122.
        assert_eq!(
            output.status.code(),
            Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
            "{mode}: {output:?}"
        );
        assert!(
            !stdout.contains("main ran"),
            "{mode}: the program ran: {stdout}"
        );
        assert!(stderr.contains(image_refusal), "{mode}: {stderr}");
        assert!(
            stderr.contains("run this program with --backend=liteinst"),
            "{mode}: {stderr}"
        );
        assert!(
            stderr.contains(&format!(
                "--backend=in-guest-trap (in-guest LiteInst, site patching off) refuses this \
                 run: {run_refusal}"
            )),
            "{mode}: {stderr}"
        );
        assert!(
            stderr.contains("HERMIT_POLICY_REFUSAL class=policy-refusal"),
            "{mode}: {stderr}"
        );
    }

    let control = run_constructor_mutation_guest("in-guest-trap", "none");
    assert!(control.status.success(), "{control:?}");
    assert_eq!(
        String::from_utf8_lossy(&control.stdout),
        "main ran; constructor none; restored 0; REVERIE_LITEINST_SITE_PATCHING=0\n"
    );
    let restored = run_constructor_mutation_guest("liteinst", "set-then-restore");
    assert!(restored.status.success(), "{restored:?}");
    assert_eq!(
        String::from_utf8_lossy(&restored.stdout),
        "main ran; constructor set-then-restore; restored 1; REVERIE_LITEINST_SITE_PATCHING=0\n"
    );
    let liteinst = run_constructor_mutation_guest("liteinst", "set");
    assert!(liteinst.status.success(), "{liteinst:?}");
    assert_eq!(
        String::from_utf8_lossy(&liteinst.stdout),
        "main ran; constructor set; restored 0; REVERIE_LITEINST_SITE_PATCHING=1\n"
    );
}

fn userfaultfd_guest() -> &'static Path {
    USERFAULTFD_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("liteinst-advanced");
        fs::create_dir_all(&build_root).expect("failed to create LiteInst guest directory");
        let guest = build_root.join("userfaultfd_self_service");
        let output = Command::new("cc")
            .args(["-O2", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/userfaultfd_self_service.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile the userfaultfd guest");
        assert!(
            output.status.success(),
            "tests/c/userfaultfd_self_service.c compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

/// Coord ruling D.2: userfaultfd stays supported under in-guest LiteInst,
/// whose exits hold the schedule until they are physically complete. One
/// process serves its own faults from one thread (tests/c/
/// userfaultfd_self_service.c, which c-programs.yaml runs on ptrace): it
/// resolves its registered pages ahead of use with `UFFDIO_COPY`, reads them,
/// and exits still registered. The run completes, matches under strict
/// verification, and prints what the pages hold. Skipped, with the reason
/// printed, where the kernel refuses an unprivileged userfaultfd.
#[test]
fn liteinst_in_guest_serves_its_own_userfaultfd_and_exits_registered() {
    // Skip only when the host itself refuses an unprivileged userfaultfd, so
    // a backend failure can never pass as an unavailable host.
    let native =
        super::hermit_test::hermit_output("hermit run", &mut Command::new(userfaultfd_guest()));
    let native_stdout = String::from_utf8_lossy(&native.stdout);
    assert!(native.status.success(), "native run failed: {native:?}");
    if let Some(reason) = native_stdout.strip_prefix("userfaultfd-unavailable ") {
        eprintln!(
            "skipping: this host refuses an unprivileged userfaultfd ({})",
            reason.trim()
        );
        return;
    }
    assert_eq!(native_stdout, "userfaultfd-served pages=2 sum=798720\n");
    let _guard = hermit_run_guard();
    let output = super::hermit_test::hermit_output(
        "hermit run",
        liteinst_command("info")
            .args(["--verify", "--verify-strict"])
            .arg("--")
            .arg(userfaultfd_guest())
            .stdin(Stdio::null()),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "status={:?}\nstdout={stdout}\nstderr={stderr}",
        output.status,
    );
    assert_eq!(
        stdout, "userfaultfd-served pages=2 sum=798720\n",
        "{stderr}"
    );
    assert!(
        stderr.contains("Success: deterministic. Determinism verified."),
        "{stderr}"
    );
    assert!(stderr.contains("bitwise parity established"), "{stderr}");
    assert_in_guest_selected(&stderr);
}

/// Coord ruling A: a guest that holds a FUSE device could be the server
/// another guest's exit-time flush waits for, which can never be answered
/// while Hermit holds every turn until that exit completes. In-guest LiteInst
/// refuses the open by name, before the guest's next line runs; ptrace runs
/// the same program. Skipped, with the reason printed, without /dev/fuse.
#[test]
fn liteinst_in_guest_refuses_a_guest_that_opens_dev_fuse() {
    if let Err(error) = fs::File::open("/dev/fuse") {
        eprintln!("skipping: /dev/fuse cannot be opened on this host ({error})");
        return;
    }
    let _guard = hermit_run_guard();
    let output = liteinst_command("info")
        .args(["--", "/bin/sh", "-c", ": < /dev/fuse; echo opened"])
        .stdin(Stdio::null())
        .output()
        .expect("failed to run Hermit LiteInst");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
        "stdout={stdout}\nstderr={stderr}"
    );
    assert!(!stdout.contains("opened"), "{stdout}");
    // Detcore names the capability; the in-guest runtime adds the backend.
    assert!(
        stderr.contains("refusing a guest that holds a FUSE device (/dev/fuse)"),
        "{stderr}"
    );
    assert!(
        stderr.contains("this backend completes process exits asynchronously"),
        "{stderr}"
    );
    assert!(
        stderr.contains(
            "in-guest LiteInst (--backend=liteinst or --backend=in-guest-trap) cannot run \
             this program; run it with --backend=ptrace."
        ),
        "{stderr}"
    );
    assert_in_guest_selected(&stderr);

    // ptrace runs the same program: the refusal is in-guest LiteInst's.
    let output = super::hermit_test::hermit_output(
        "hermit run",
        Command::new(hermit_binary())
            .args(["--log=info", "run", "--strict", "--"])
            .args(["/bin/sh", "-c", ": < /dev/fuse; echo opened"])
            .stdin(Stdio::null()),
    );
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "opened\n");
}

/// The other half of ruling A: a plain file on a FUSE filesystem is not
/// refused. Its server is outside the guest set, so no exit waits for a guest.
/// Uses the first regular file at the root of a FUSE mount this host has;
/// skipped, with the reason printed, on a host with none.
#[test]
fn liteinst_in_guest_reads_a_plain_file_on_a_fuse_filesystem() {
    let mounts = fs::read_to_string("/proc/self/mounts").expect("reading /proc/self/mounts");
    let file = mounts
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(' ');
            let mount = fields.nth(1)?;
            let kind = fields.next()?;
            (kind.starts_with("fuse") && kind != "fusectl").then(|| PathBuf::from(mount))
        })
        .find_map(|mount| {
            fs::read_dir(&mount).ok()?.flatten().find_map(|entry| {
                let path = entry.path();
                let metadata = fs::metadata(&path).ok()?;
                (metadata.is_file() && metadata.len() > 0 && fs::File::open(&path).is_ok())
                    .then_some(path)
            })
        });
    let Some(file) = file else {
        eprintln!("skipping: this host has no readable regular file at a FUSE mount's root");
        return;
    };
    let mut expected = vec![0u8; 16];
    let read = fs::File::open(&file)
        .and_then(|mut opened| opened.read(&mut expected))
        .expect("reading the FUSE file natively");
    expected.truncate(read);
    let _guard = hermit_run_guard();
    let output = super::hermit_test::hermit_output(
        "hermit run",
        liteinst_command("info")
            .args(["--", "/usr/bin/head", "-c", "16"])
            .arg(&file)
            .stdin(Stdio::null()),
    );
    let output = assert_liteinst_in_guest_output(output);
    assert_eq!(output.stdout, expected, "{}", file.display());
}

/// Compiles `tests/c/<name>.c` once into this test process's private directory.
/// Nextest processes must not replace the executable another test is running.
fn c_guest(cell: &'static OnceLock<PathBuf>, name: &str) -> &'static Path {
    cell.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("liteinst-advanced");
        fs::create_dir_all(&build_root).expect("failed to create LiteInst guest directory");
        let guest = build_root.join(name);
        let source = repository.join(format!("tests/c/{name}.c"));
        let output = Command::new("cc")
            .args(["-O2", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(&source)
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to run cc");
        assert!(
            output.status.success(),
            "{} compilation failed:\nstdout:\n{}\nstderr:\n{}",
            source.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

/// The C3.5 auto-reap and SIGCHLD cases (dev-hermit
/// ai_docs/transient/liteinst-inguest-exit-control-design-20261006.md,
/// "Evidence planned"), under strict verification on ptrace and on in-guest
/// LiteInst, whose exits hold the schedule until they are physically
/// complete. The parent blocks SIGCHLD and learns of each exit from EOF on a
/// pipe the child held. Every run is checked against Linux's output, per
/// backend; the two backends print the same lines in the first two modes and
/// differ, by one named LiteInst deviation, in the third:
///
/// - SA_NOCLDWAIT: wait reports ECHILD (auto-reaped) and SIGCHLD is pending,
///   as on Linux, on both backends.
/// - a raw exit system call: SIGCHLD is pending and wait returns status 7, as
///   on Linux, on both backends.
/// - SIGCHLD ignored: wait reports ECHILD, as on Linux. Linux sends no
///   SIGCHLD to a parent that ignores it (`do_notify_parent`), so nothing is
///   pending. ptrace prints that since SIGCHLD Phase A; in-guest LiteInst
///   still leaves SIGCHLD pending, a known deviation pinned at its current
///   value (dev-hermit
///   ignored/parity-green-issues/liteinst-sigign-sigchld-pending.md). This
///   mode is not a parity check; the name predates it.
#[test]
fn liteinst_in_guest_exit_reaping_matches_ptrace() {
    let _guard = hermit_run_guard();
    let guest = c_guest(&EXIT_REAPING_GUEST, "exit_reaping_probe");
    // Each backend's expected SIGCHLD-pending value, which is Linux's (the
    // fixture's header) except where a backend's deviation is named.
    //
    // sigign: Linux sends no SIGCHLD when the parent explicitly ignores it,
    // even while it is blocked (do_notify_parent sets sig = 0), so nothing is
    // pending. ptrace matches Linux since SIGCHLD Phase A stopped its
    // scheduler sending a synthetic SIGCHLD there. In-guest LiteInst still
    // leaves SIGCHLD pending: a known deviation, dev-hermit
    // ignored/parity-green-issues/liteinst-sigign-sigchld-pending.md. It is
    // pinned at its current value, which is what this test enforced before
    // through ptrace's (then equally wrong) output.
    for (mode, ptrace_pending, liteinst_pending, wait, status) in [
        ("sigign", "0", "1", "ECHILD", -1),
        ("nocldwait", "1", "1", "ECHILD", -1),
        ("rawexit", "1", "1", "child", 7),
    ] {
        let mut outputs = Vec::new();
        for backend in ["ptrace", "liteinst"] {
            let pending = if backend == "ptrace" {
                ptrace_pending
            } else {
                liteinst_pending
            };
            let mut command = Command::new(hermit_binary());
            command.args(["--log=info", "--backend", backend, "run"]);
            if backend == "liteinst" {
                command.arg("--max-timeslice=disabled");
            }
            let output = super::hermit_test::hermit_output(
                "hermit run",
                command
                    .args(["--strict", "--verify", "--verify-strict", "--"])
                    .arg(guest)
                    .arg(mode)
                    .stdin(Stdio::null()),
            );
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "{backend} {mode}: status={:?}\nstdout={stdout}\nstderr={stderr}",
                output.status
            );
            assert!(
                stderr.contains("bitwise parity established"),
                "{backend} {mode}: {stderr}"
            );
            let lines: Vec<&str> = stdout.lines().collect();
            assert_eq!(lines.len(), 3, "{backend} {mode}: {stdout}");
            for (round, line) in lines.iter().enumerate() {
                let prefix = format!("{mode} round={round} read=0 sigchld_pending=");
                let suffix = format!(" wait={wait} status={status}");
                assert!(
                    line.starts_with(&prefix) && line.ends_with(&suffix),
                    "{backend} {mode}: {line}"
                );
                assert_eq!(
                    *line,
                    format!("{prefix}{pending}{suffix}"),
                    "{backend} {mode}"
                );
            }
            outputs.push(stdout);
        }
        if ptrace_pending == liteinst_pending {
            assert_eq!(
                outputs[0], outputs[1],
                "{mode}: ptrace and in-guest LiteInst differ"
            );
        }
    }
}

/// What `self_signal_death_probe children` prints, as Linux runs it: six
/// children end themselves with a signal they send themselves, and three send
/// themselves one that does not end them (blocked, then two ignored) and exit.
const SELF_SIGNAL_DEATH_CHILDREN: &str = "abort: signal 6\n\
    raise-sigterm: signal 15\n\
    kill-sigkill: signal 9\n\
    tgkill-sigusr1: signal 10\n\
    tkill-sigquit: signal 3\n\
    raise-sigtrap: signal 5\n\
    blocked-sigusr2: exit 3\n\
    ignored-sigusr2: exit 4\n\
    ignored-sigtrap: exit 5\n";

/// One in-guest run of `self_signal_death_probe` (`tests/c`) under `backend`,
/// at log level `info`, with `extra` run options.
fn run_self_signal_death_probe(backend: &str, extra: &[&str], mode: &str) -> Output {
    let guest = c_guest(&SELF_SIGNAL_DEATH_GUEST, "self_signal_death_probe");
    super::hermit_test::hermit_output(
        "hermit run",
        Command::new(hermit_binary())
            .args(["--log=info", "--backend", backend, "run"])
            .args([
                "--max-timeslice=disabled",
                "--strict",
                "--base-env=minimal",
                "--mount=type=tmpfs,target=/test",
                "--workdir=/test",
            ])
            .args(extra)
            .arg("--")
            .arg(guest)
            .arg(mode)
            .stdin(Stdio::null()),
    )
}

/// A process that ends itself with a signal it sends itself (`abort`, `raise`,
/// `kill` of its own pid, raw `tgkill` and `tkill` of its own thread) dies at
/// a point of its own program order, in its own turn. The in-guest runtime
/// runs the Tool's signal and exit callbacks before sending such a signal, so
/// the process deregisters, and reports its statistics, before it dies, as
/// for an exit. Before, it died without deregistering: Detcore recorded a
/// determinism loss ("process N exited without deregistering; its exit was
/// not scheduled"), and `--backend=in-guest-trap` refused every run because
/// no process had reported LiteInst statistics. That ended the verify cells
/// of every program that aborts (the sigtimedwait tests and ruby, whose
/// signal-handler installs in-guest LiteInst refuses) with that loss instead
/// of the program's own status.
///
/// Both in-guest backends must now verify the probe, print what Linux prints,
/// and record no loss. The three control children prove the prediction is
/// not "every self-sent signal": a blocked and an ignored signal leave the
/// child running to its own exit code. The SIGTRAP child's death is judged by
/// its own action (the default), not by the handler LiteInst installs for
/// SIGTRAP to route its own breakpoints: before Reverie judged it so, that
/// child still died without deregistering.
#[test]
fn liteinst_in_guest_self_signal_deaths_are_scheduled_and_verify() {
    let _guard = hermit_run_guard();
    for (backend, selected) in [
        ("liteinst", IN_GUEST_SELECTED),
        ("in-guest-trap", IN_GUEST_TRAP_SELECTED),
    ] {
        let output =
            run_self_signal_death_probe(backend, &["--verify", "--verify-strict"], "children");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "{backend}: status={:?}\nstdout={stdout}\nstderr={stderr}",
            output.status
        );
        assert!(
            stderr.lines().any(|line| line == selected),
            "{backend}: {stderr}"
        );
        assert_eq!(stdout, SELF_SIGNAL_DEATH_CHILDREN, "{backend}");
        assert!(
            stderr.contains("Determinism verified"),
            "{backend}: {stderr}"
        );
        assert!(!stderr.contains("determinism loss"), "{backend}: {stderr}");
        assert!(
            !stderr.contains("exited without deregistering"),
            "{backend}: {stderr}"
        );
    }
}

/// A root process that aborts ends the run with its own status, SIGABRT, on
/// both in-guest backends, and verification refuses it for that status,
/// as on ptrace: never for a recorded determinism loss, and never, under
/// `--backend=in-guest-trap`, because no process reported LiteInst statistics
/// (which is what both did before the runtime ran the exit callbacks of a
/// process that ends itself with a signal).
#[test]
fn liteinst_in_guest_a_root_that_aborts_ends_with_its_own_status() {
    let _guard = hermit_run_guard();
    for backend in ["liteinst", "in-guest-trap"] {
        let output = run_self_signal_death_probe(backend, &[], "root-abort");
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Hermit ends itself with the signal that ended its guest.
        assert_eq!(
            output.status.signal(),
            Some(libc::SIGABRT),
            "{backend}: status={:?}\n{stderr}",
            output.status
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout), "root aborts\n");
        assert!(
            !stderr.contains("no guest process reported LiteInst statistics"),
            "{backend}: {stderr}"
        );

        let output =
            run_self_signal_death_probe(backend, &["--verify", "--verify-strict"], "root-abort");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{backend}: {stderr}");
        assert!(
            stderr.contains("First run during --verify terminated by signal 6 (SIGABRT)"),
            "{backend}: {stderr}"
        );
        assert!(!stderr.contains("determinism loss"), "{backend}: {stderr}");
        assert!(
            !stderr.contains("no guest process reported LiteInst statistics"),
            "{backend}: {stderr}"
        );
    }
}

/// The self-signal contract excludes the runtime-owned SIGSEGV and SIGSYS
/// handlers. Run both excluded cases on both in-guest backends: a parent's
/// normal exit must not hide its child's missing exit callbacks or allow
/// strict verification to report a match.
#[test]
fn liteinst_in_guest_runtime_owned_self_signals_refuse_verification() {
    let _guard = hermit_run_guard();
    for backend in ["liteinst", "in-guest-trap"] {
        for mode in ["excluded-sigsegv", "excluded-sigsys"] {
            let output =
                run_self_signal_death_probe(backend, &["--verify", "--verify-strict"], mode);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                !output.status.success(),
                "{backend} {mode}: verification matched: {stderr}"
            );
            assert!(
                stderr.contains("run 1: determinism loss recorded: process ")
                    && stderr.contains(" exited without deregistering; its exit was not scheduled"),
                "{backend} {mode}: {stderr}"
            );
            assert!(
                !stderr.contains("Determinism verified"),
                "{backend} {mode}: {stderr}"
            );
        }
    }
}

/// Processes that die at a moment the host chooses, without an exit system
/// call of their own: a child killed by PR_SET_PDEATHSIG while its parent
/// exits, and a vfork parent killed by its child while the grandparent goes
/// on. In-guest LiteInst cannot schedule such a death, so it retires the
/// process when its exit completes and records a determinism loss (the C3.5
/// design's consumption case). Each run completes with the program's output,
/// never a hang, and verification refuses to compare it, never a match.
#[test]
fn liteinst_in_guest_unscheduled_deaths_complete_and_refuse_verification() {
    let _guard = hermit_run_guard();
    let guest = c_guest(&UNSCHEDULED_EXIT_GUEST, "unscheduled_exit_probe");
    for (mode, expected) in [
        (
            "pdeathsig",
            "parent exits with its child armed\npdeathsig parent-status=0 armed-child-gone=1\n",
        ),
        (
            "vfork-parent-killed",
            "vfork-parent-killed parent-signal=9 surviving-work-done=1\n",
        ),
    ] {
        let output = super::hermit_test::hermit_output(
            "hermit run",
            liteinst_command("info")
                .arg("--")
                .arg(guest)
                .arg(mode)
                .stdin(Stdio::null()),
        );
        let output = assert_liteinst_in_guest_output(output);
        assert_eq!(String::from_utf8_lossy(&output.stdout), expected, "{mode}");

        let output = super::hermit_test::hermit_output(
            "hermit run",
            liteinst_command("info")
                .args(["--verify", "--verify-strict", "--"])
                .arg(guest)
                .arg(mode)
                .stdin(Stdio::null()),
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "{mode}: verification matched: {stderr}"
        );
        assert!(
            stderr.contains("run 1: determinism loss recorded: process ")
                && stderr.contains(" exited without deregistering; its exit was not scheduled"),
            "{mode}: {stderr}"
        );
        assert!(!stderr.contains("Determinism verified"), "{mode}: {stderr}");
    }
}

/// Under `--max-log-bytes`, Hermit writes its in-guest selection line
/// ([`IN_GUEST_SELECTED`]) before the guest starts, so before the cap or the
/// guest's exit can end the run. With stderr a full pipe nobody reads, the run
/// must still end within the bound of
/// `max_log_bytes_exits_promptly_when_stderr_is_a_full_pipe_nobody_reads`.
/// In-guest LiteInst refuses `--timeout`, so the run has none.
#[test]
fn liteinst_in_guest_max_log_bytes_exits_promptly_when_stderr_is_a_full_pipe() {
    super::CappedRun {
        build: |args| {
            let mut command = Command::new(hermit_binary());
            command.args(args);
            command
        },
        global: &["--backend", "liteinst"],
        run_options: &[
            "--max-timeslice=disabled",
            "--strict",
            "--base-env=minimal",
            "--mount=type=tmpfs,target=/test",
            "--workdir=/test",
        ],
        timeout: false,
        guest: &["/bin/true"],
        exit_code: 0,
        prints: Some(IN_GUEST_SELECTED),
        ..super::CappedRun::default()
    }
    .assert_exits_promptly_with_full_unread_stderr(|_| {});
}

/// Two descriptors aliasing one open file description (after dup) still
/// share it in a forked child. In-guest LiteInst serializes the parent's
/// Detcore state into the child, and before interning by OpenFileId the
/// child's aliases came back as two copies: both read the same random bytes.
/// The child's two reads must continue one stream, exactly as under ptrace.
#[test]
fn liteinst_in_guest_dup_aliases_share_one_cursor_after_fork() {
    let _guard = hermit_run_guard();
    let guest = c_guest(&DUP_ALIAS_GUEST, "dup_alias_after_fork");
    let mut outputs = Vec::new();
    for backend in ["ptrace", "liteinst"] {
        let mut command = Command::new(hermit_binary());
        command.args(["--log=info", "--backend", backend, "run"]);
        if backend == "liteinst" {
            command.arg("--max-timeslice=disabled");
        }
        let output = super::hermit_test::hermit_output(
            "hermit run",
            command
                .arg(format!("--epoch={VIRTUAL_TIME_EPOCH}"))
                .args(["--strict", "--"])
                .arg(guest)
                .stdin(Stdio::null()),
        );
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(output.status.success(), "{backend}: {output:?}");
        assert!(
            stdout.contains("aliases continue one stream=1"),
            "{backend}: {stdout}"
        );
        outputs.push(stdout);
    }
    assert_eq!(
        outputs[0], outputs[1],
        "in-guest LiteInst differs from ptrace"
    );
}

/// close_range(3, ~0U, 0) closes a bound socket, and Detcore must account
/// for it like a close: the socket's port goes back to the allocator, so the
/// next bind to port 0 gets the same port. Under in-guest LiteInst the range
/// also covers the runtime's coordinator connection, and the runtime once
/// answered such a call itself, before Detcore saw it: the port was never
/// released and the second bind got the next port. Both backends must reuse
/// the port and print the same output.
#[test]
fn liteinst_in_guest_close_range_releases_ports_like_ptrace() {
    let _guard = hermit_run_guard();
    let guest = c_guest(&CLOSE_RANGE_PORT_GUEST, "close_range_releases_port");
    let mut outputs = Vec::new();
    for backend in ["ptrace", "liteinst"] {
        let mut command = Command::new(hermit_binary());
        command.args(["--log=info", "--backend", backend, "run"]);
        if backend == "liteinst" {
            command.arg("--max-timeslice=disabled");
        }
        let output = super::hermit_test::hermit_output(
            "hermit run",
            command
                .arg(format!("--epoch={VIRTUAL_TIME_EPOCH}"))
                .args(["--strict", "--"])
                .arg(guest)
                .stdin(Stdio::null()),
        );
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(output.status.success(), "{backend}: {output:?}");
        assert_eq!(stdout, "port reused=1\n", "{backend}: {output:?}");
        outputs.push(stdout);
    }
    assert_eq!(
        outputs[0], outputs[1],
        "in-guest LiteInst differs from ptrace"
    );
}

/// Under `--verify` a recvmsg whose buffers cannot be observed after the call
/// still returns the kernel's result to the guest: the buffer digest records
/// an explicit, compared `unobserved` entry instead of hashing, and the verify
/// report counts it. A plain receive is hashed as before. In the overwritten
/// mode the guest's iovec array is its own receive buffer, so the payload
/// overwrites the iovec with one byte at an unmapped address; before the
/// entry, the failed read turned the guest's completed recvmsg into EFAULT
/// under --verify.
#[test]
fn verify_digest_records_an_unobservable_recvmsg_and_keeps_the_kernel_result() {
    let _guard = hermit_run_guard();
    let guest = c_guest(&IOV_OVERWRITTEN_GUEST, "recvmsg_iov_overwritten_by_payload");
    let logs = tempfile::tempdir().expect("failed to create the verify-log directory");
    for (mode, unobservable) in [("plain", false), ("overwritten", true)] {
        let log_dir = logs.path().join(mode);
        fs::create_dir(&log_dir).expect("failed to create a verify-log directory");
        let output = super::hermit_test::hermit_output(
            "hermit run",
            Command::new(hermit_binary())
                .args([
                    "--log",
                    "info",
                    "--backend",
                    "ptrace",
                    "run",
                    "--strict",
                    "--verify",
                ])
                .arg(format!("--epoch={VIRTUAL_TIME_EPOCH}"))
                .arg("--keep-logs")
                .arg("--verify-log-dir")
                .arg(&log_dir)
                .arg("--")
                .arg(guest)
                .arg(mode)
                .stdin(Stdio::null()),
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{mode}: {output:?}");
        assert!(stderr.contains("Determinism verified"), "{mode}: {stderr}");
        let buffer = stdout
            .split_whitespace()
            .find_map(|field| field.strip_prefix("buffer="))
            .unwrap_or_else(|| panic!("{mode}: {stdout}"));
        let expected_stdout = format!(
            "buffer={buffer} received=16 overwritten={}\n",
            u8::from(unobservable)
        );
        assert_eq!(stdout, expected_stdout, "{mode}");
        let hashed = format!("recvmsg in fd=4 {buffer}+16->");
        let marker = "recvmsg in fd=4 unobserved ret=16 reason=buffer-unreadable at=";
        let counted = "unobservable after the call (compared as 'unobserved' buffer-digest entries): run1=1, run2=1";
        let golden: Vec<PathBuf> = fs::read_dir(&log_dir)
            .expect("failed to read the retained verify logs")
            .map(|entry| entry.expect("failed to read a retained log").path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("run1_log_"))
            })
            .collect();
        assert_eq!(golden.len(), 1, "{mode}: {golden:?}");
        let log = fs::read_to_string(&golden[0]).expect("failed to read the retained log");
        let digests: Vec<&str> = log
            .lines()
            .filter(|line| line.contains("[iobuf]") && line.contains("recvmsg in"))
            .collect();
        if unobservable {
            assert!(
                digests.len() == 1 && digests[0].contains(marker),
                "{mode}: expected one {marker:?} entry: {digests:?}"
            );
            assert!(stderr.contains(counted), "{mode}: {stderr}");
        } else {
            assert!(
                digests.len() == 1 && digests[0].contains(&hashed),
                "{mode}: expected {hashed:?}: {digests:?}"
            );
            assert!(
                !stderr.contains("unobservable after the call"),
                "{mode}: {stderr}"
            );
        }
    }
}

static USER_ADDRESS_ERRNO_GUEST: OnceLock<PathBuf> = OnceLock::new();

/// The guest for
/// `liteinst_in_guest_user_address_limit_queries_keep_the_guest_errno`. Its
/// probes all go through one raw `read` instruction (`raw_read`, written in
/// assembly so the compiler cannot copy it), which sets no `errno`, so any
/// change to `errno` across a probe was made by whoever served it. The
/// function has unwind information (`.cfi_startproc`) and room after the
/// instruction, which LiteInst needs before it patches a site. The guest asks
/// the in-guest LiteInst runtime, when it is loaded, how often that
/// instruction's installed hook was entered.
const USER_ADDRESS_ERRNO_GUEST_SOURCE: &str = r#"
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <unistd.h>

long raw_read(long fd, void *buf, unsigned long count);
/* SYS_read is 0; the five-byte mov puts the syscall at raw_read + 5. */
#define READ_SITE_OFFSET 5
__asm__(".text\n"
        ".p2align 4\n"
        ".globl raw_read\n"
        ".type raw_read, @function\n"
        "raw_read:\n"
        "\t.cfi_startproc\n"
        "\tmovl $0, %eax\n"
        "\tsyscall\n"
        "\tnop\n\tnop\n\tnop\n\tnop\n\tnop\n\tnop\n\tnop\n\tnop\n"
        "\tret\n"
        "\t.cfi_endproc\n"
        ".size raw_read, .-raw_read\n");

typedef uint64_t (*hook_count_fn)(uint64_t);

int main(void) {
    hook_count_fn hook_count =
        (hook_count_fn)dlsym(RTLD_DEFAULT, "reverie_liteinst_site_hook_count");
    const unsigned char *site_bytes = (const unsigned char *)raw_read + READ_SITE_OFFSET;
    uint64_t site = (uint64_t)(uintptr_t)site_bytes;
    if (site_bytes[0] != 0x0f || site_bytes[1] != 0x05) {
        printf("raw_read + %d is not a syscall instruction\n", READ_SITE_OFFSET);
        return 1;
    }
    uint64_t hooks[4] = {0, 0, 0, 0};
    char byte = 0;
    int rng = open("/dev/urandom", O_RDONLY | O_CLOEXEC);
    if (rng < 0) {
        perror("open /dev/urandom");
        return 1;
    }
    if (hook_count) hooks[0] = hook_count(site);
    /* An invalid descriptor: Detcore does not ask for the user address
       limit, and the instruction becomes a patched site. */
    long warm = raw_read(-1, &byte, 1);
    if (hook_count) hooks[1] = hook_count(site);
    /* A zero-length random-device read: Detcore checks the buffer against
       the user address limit, which this process has not measured yet. */
    errno = 4242;
    long cold = raw_read(rng, &byte, 0);
    int cold_errno = errno;
    if (hook_count) hooks[2] = hook_count(site);
    /* The same read of a buffer beyond the limit, now cached: EFAULT. */
    errno = 4243;
    long cached = raw_read(rng, (void *)UINTPTR_MAX, 0);
    int cached_errno = errno;
    if (hook_count) hooks[3] = hook_count(site);
    printf("warm=%ld\n", warm);
    printf("cold=%ld errno=%d\n", cold, cold_errno);
    printf("cached=%ld errno=%d\n", cached, cached_errno);
    if (hook_count) {
        printf("hook entries per probe: warm=%lu cold=%lu cached=%lu\n",
               (unsigned long)(hooks[1] - hooks[0]), (unsigned long)(hooks[2] - hooks[1]),
               (unsigned long)(hooks[3] - hooks[2]));
    } else {
        printf("hook entries per probe: no in-guest runtime\n");
    }
    return 0;
}
"#;

fn user_address_errno_guest() -> &'static Path {
    USER_ADDRESS_ERRNO_GUEST.get_or_init(|| {
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("liteinst-advanced");
        fs::create_dir_all(&build_root).expect("failed to create LiteInst guest directory");
        let source = build_root.join("user_address_errno.c");
        fs::write(&source, USER_ADDRESS_ERRNO_GUEST_SOURCE)
            .expect("failed to write the user-address errno guest");
        let guest = build_root.join("user_address_errno");
        let output = Command::new("cc")
            .args(["-O2", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(&source)
            .arg("-o")
            .arg(&guest)
            .arg("-ldl")
            .output()
            .expect("failed to run cc");
        assert!(
            output.status.success(),
            "{} compilation failed:\nstdout:\n{}\nstderr:\n{}",
            source.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

/// In-guest LiteInst runs Detcore on the guest's own thread, so the guest and
/// Detcore share one C `errno`. Detcore asks the backend for the user address
/// limit (`Guest::user_address_limit`) before it checks a random-device read's
/// buffer; on this backend the answer is measured in the guest process with
/// failing `process_vm_readv` range checks, which set `errno` to `EFAULT`.
/// The query must leave the guest's `errno` as it found it, also when Detcore
/// is entered through a patched site's installed hook, which, unlike the
/// SIGSYS fallback, does not restore `errno` itself.
///
/// One raw `read` instruction serves every probe: an invalid descriptor first
/// (no query; the instruction becomes a patched site), then a zero-length
/// random-device read (the first query, which measures), then the same read
/// of a buffer at the top of the address space (the cached answer, EFAULT).
/// Before each probe the guest sets a sentinel `errno` that no system call
/// sets. Linux, ptrace and in-guest LiteInst must print the same results with
/// the sentinel kept, and LiteInst must have entered the instruction's
/// installed hook once per probe.
#[test]
fn liteinst_in_guest_user_address_limit_queries_keep_the_guest_errno() {
    const PROBES: &str = "warm=-9\ncold=0 errno=4242\ncached=-14 errno=4243\n";
    let guest = user_address_errno_guest();
    let native = Command::new(guest)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run the user-address errno guest natively");
    assert!(native.status.success(), "native: {native:?}");
    assert_eq!(
        String::from_utf8_lossy(&native.stdout),
        format!("{PROBES}hook entries per probe: no in-guest runtime\n"),
        "native: {native:?}"
    );
    let _guard = hermit_run_guard();
    for (backend, hooks) in [
        ("ptrace", "no in-guest runtime"),
        ("liteinst", "warm=1 cold=1 cached=1"),
    ] {
        let mut command = Command::new(hermit_binary());
        command.args(["--log=off", "--backend", backend, "run"]);
        if backend == "liteinst" {
            command.arg("--max-timeslice=disabled");
        }
        let output = super::hermit_test::hermit_output(
            "hermit run",
            command
                .args(["--strict", "--"])
                .arg(guest)
                .stdin(Stdio::null()),
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{backend}: {output:?}");
        assert_eq!(
            stdout,
            format!("{PROBES}hook entries per probe: {hooks}\n"),
            "{backend}: {output:?}"
        );
        if backend == "liteinst" {
            assert_in_guest_selected(&String::from_utf8_lossy(&output.stderr));
        }
    }
}

/// Full Linux siginfo survives both standard-signal coalescing orders.
#[test]
fn sigalrm_first_pending_metadata_matches_native_linux() {
    use detcore::Digest;
    use hermit::canonical_verdict::VerificationReport;

    use super::kvm_cancellation::bounded_command_with_timeout;
    use super::kvm_cancellation::bounded_read;

    let _guard = hermit_run_guard();
    let guest = sigalrm_fixture("sigalrm_first_copy");
    let mut failures = Vec::new();
    for (mode, expired, code, errno, value) in [
        ("guest-first", 1, -1, 73, 0x13579bdf),
        ("timer-first", 1, 128, 0, 0),
        ("guest-only", 0, -1, 73, 0x13579bdf),
        ("self-si-user", 0, 0, 73, 0x13579bdf),
    ] {
        // Attempt all four mandatory modes, retain every panic as failure,
        // and fail the body if any mode fails; catch_unwind is not a skip.
        let result = std::panic::catch_unwind(|| {
            let expected = format!(
                "mode={mode} hits=1 pending_before=1 pending_after=0 expired={expired} \
                 signo=14 code={code} errno={errno} value={value} pid_ok=1 uid_ok=1 raw128=1\n"
            );
            let directory = process_build_root(&format!("sigalrm-first-copy-native-{mode}"));
            let mut native = Command::new(&guest);
            native.arg(mode).stdin(Stdio::null());
            let status =
                bounded_command_with_timeout(&mut native, &directory, Duration::from_secs(57));
            let stdout = bounded_read(&directory.join("stdout"), 1024 * 1024);
            let stderr = bounded_read(&directory.join("stderr"), 16 * 1024 * 1024);
            assert_eq!(status.code(), Some(0), "native/{mode}: {stderr:?}");
            assert_eq!(stdout, expected.as_bytes(), "native/{mode}");
            assert!(stderr.is_empty(), "native/{mode}: {stderr:?}");

            // Existing helper keeps strict, verify-strict, nonempty INFO,
            // exact guest outputs, current typed schema, and the 57s cap.
            let verified = strict_sigalrm_fixture_run(&guest, "ptrace", Some("1"), "0", mode);
            let retained = process_build_root(&format!("sigalrm-fixture-ptrace-{mode}"));
            let report = VerificationReport::from_current_json_slice(&bounded_read(
                &retained.join("verify.json"),
                16 * 1024 * 1024,
            ))
            .expect("complete current first-copy verification report");
            // The helper already requires these; spell them out to prevent
            // accidental reuse of a looser output-only helper at integration.
            report
                .require_canonical_match()
                .expect("nonempty canonical INFO match");
            report
                .require_exact_output_match()
                .expect("exact guest output match");
            let outputs = report.compared_outputs.expect("compared guest outputs");
            for operand in [&outputs.left, &outputs.right] {
                assert_eq!(operand.exit_code, status.code(), "ptrace/{mode}");
                assert!(operand.signal.is_none(), "ptrace/{mode}");
                assert_eq!(operand.stdout_bytes, stdout.len() as u64, "ptrace/{mode}");
                assert_eq!(
                    operand.stdout_sha256,
                    Digest::new(&stdout).to_string(),
                    "ptrace/{mode}"
                );
                assert_eq!(operand.stderr_bytes, 0, "ptrace/{mode}");
                assert_eq!(
                    operand.stderr_sha256,
                    Digest::new(&stderr).to_string(),
                    "ptrace/{mode}"
                );
            }
            assert_eq!(verified, expected.as_bytes(), "ptrace/{mode}");
            assert_eq!(verified, stdout, "ptrace/{mode} versus native");
        });
        if result.is_err() {
            failures.push(mode);
        }
    }
    assert!(
        failures.is_empty(),
        "first-copy metadata modes failed: {failures:?}"
    );
}
