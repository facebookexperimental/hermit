/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// LiteInst is an optional backend: without the `liteinst` feature (which
// `third-party-backends` enables for every validation build) this binary has
// no tests, because there is no backend for them to exercise.
#![cfg(feature = "liteinst")]

#[path = "common/liteinst.rs"]
mod liteinst_runtime;

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

static LITEINST_ADVANCED_GUEST: OnceLock<PathBuf> = OnceLock::new();
static LITEINST_MMAP_GUEST: OnceLock<PathBuf> = OnceLock::new();
static LITEINST_COMPAT_FIXTURE: OnceLock<PathBuf> = OnceLock::new();
static LITEINST_SEMANTIC_FIXTURE: OnceLock<PathBuf> = OnceLock::new();
static LITEINST_COMPRESSED_FIXTURES: OnceLock<[PathBuf; 3]> = OnceLock::new();

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
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("liteinst-advanced");
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
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("liteinst-advanced");
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
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("liteinst-advanced");
        fs::create_dir_all(&build_root).expect("failed to create LiteInst fixture directory");
        let fixture = build_root.join("compatibility-fixture.txt");
        fs::write(&fixture, COMPAT_FIXTURE_CONTENT).expect("failed to write LiteInst fixture");
        fixture
    })
}

fn semantic_fixture() -> &'static Path {
    LITEINST_SEMANTIC_FIXTURE.get_or_init(|| {
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("liteinst-advanced");
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

fn compressed_fixtures() -> &'static [PathBuf; 3] {
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
            ("/usr/bin/xz", &["-c"][..], "compatibility-fixture.xz"),
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

fn run_liteinst(program: &Path, args: &[&str], verify: bool) -> Output {
    run_liteinst_with_input(program, args, verify, None, None)
}

const VIRTUAL_TIME_EPOCH: &str = "2026-01-01T00:00:00Z";

fn liteinst_command_at_epoch(log_level: &str, epoch: Option<&str>) -> Command {
    let mut command = Command::new(liteinst_runtime::hermit_binary());
    command
        .arg(format!("--log={log_level}"))
        .args(["--backend", "liteinst", "run"]);
    if let Some(epoch) = epoch {
        command.arg(format!("--epoch={epoch}"));
    }
    command.args([
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
fn liteinst_commands_use_minimal_environment_and_private_workdir() {
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
            "--strict",
            "--base-env=minimal",
            "--mount=type=tmpfs,target=/test",
            "--workdir=/test",
        ]
    );
}

fn run_liteinst_with_input(
    program: &Path,
    args: &[&str],
    verify: bool,
    input: Option<&[u8]>,
    epoch: Option<&str>,
) -> Output {
    liteinst_runtime::ensure_liteinst_runtime();
    let home = tempfile::tempdir().expect("failed to create isolated LiteInst HOME");
    let xdg_config_home = home.path().join(".config");
    fs::create_dir_all(&xdg_config_home).expect("failed to create isolated XDG config directory");
    let mut command = liteinst_command_at_epoch("info", epoch);
    if verify {
        command.arg("--verify");
    }
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
        return command.output().expect("failed to run Hermit LiteInst");
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

fn assert_liteinst_strict_verify(program: &Path, args: &[&str], expected_stdout: &[u8]) {
    let output = run_liteinst_strict_verify(program, args);
    assert_eq!(output.stdout, expected_stdout);
}

fn assert_liteinst_virtual_time_is_continuous() {
    const EPOCH_SECONDS: u64 = 1_767_225_600;
    const MAX_STARTUP_SECONDS: u64 = 60;

    // Whole seconds remain stable across verified LiteInst runs. Do not assert
    // the old exact epoch: that encoded #1095's reset-on-exec behavior and
    // rejects legitimate deterministic startup progress.
    let output = assert_liteinst_strict_verify_output(run_liteinst_with_input(
        Path::new("/usr/bin/date"),
        &["-u", "+%s"],
        true,
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
    let progress = assert_liteinst_strict_verify_output(run_liteinst_with_input(
        advanced_guest(),
        &["clock-progress"],
        true,
        None,
        Some(VIRTUAL_TIME_EPOCH),
    ));
    assert_eq!(progress.stdout, b"clock-progress-ok\n");
}

#[test]
fn liteinst_strict_verify_heap_growth_avoids_trampoline_mappings() {
    let output = run_liteinst_strict_verify(mmap_guest(), &["heap"]);
    assert!(
        output.stdout.starts_with(b"heap "),
        "heap-growth guest omitted its success marker: {}",
        String::from_utf8_lossy(&output.stdout),
    );
}

fn run_liteinst_strict_verify(program: &Path, args: &[&str]) -> Output {
    assert_liteinst_strict_verify_output(run_liteinst(program, args, true))
}

fn run_liteinst_strict_verify_with_stdin(program: &Path, args: &[&str], input: &[u8]) -> Output {
    assert_liteinst_strict_verify_output(run_liteinst_with_input(
        program,
        args,
        true,
        Some(input),
        None,
    ))
}

fn assert_liteinst_strict_verify_output(output: Output) -> Output {
    assert!(
        output.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(
            "liteinst host hybrid] activation verified (traps=1, hooks=31); Detcore Tool active in ptrace host"
        ),
        "{stderr}"
    );
    let perf_supported = reverie_ptrace::is_perf_supported();
    assert_eq!(
        stderr.contains("perf_event_open is unavailable; continuing with --max-timeslice=disabled"),
        !perf_supported,
        "perf_supported={perf_supported}\n{stderr}"
    );
    assert!(
        stderr.contains("Success: deterministic. Determinism verified."),
        "{stderr}"
    );
    assert!(
        stderr.contains(
            "LiteInst host hybrid (reverie-liteinst patch runtime + ptrace Detcore Tool)"
        ),
        "{stderr}"
    );
    output
}

#[test]
fn liteinst_detcore_strict_verify_micro_suite() {
    assert_liteinst_strict_verify(Path::new("/bin/true"), &[], b"");
    assert_liteinst_strict_verify(Path::new("/bin/echo"), &["hello"], b"hello\n");

    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let readme = repository.join("README.md");
    let expected = fs::read(&readme).expect("read README fixture");
    assert_liteinst_strict_verify(
        Path::new("/bin/cat"),
        &[readme.to_str().unwrap()],
        &expected,
    );
}

#[test]
fn liteinst_strict_verify_identity_utilities() {
    assert_liteinst_strict_verify(Path::new("/usr/bin/uname"), &["-s"], b"Linux\n");
    assert_liteinst_strict_verify(Path::new("/usr/bin/id"), &["-u"], b"0\n");
    assert_liteinst_strict_verify(Path::new("/usr/bin/whoami"), &[], b"root\n");
}

#[test]
fn liteinst_strict_verify_virtual_identity_and_time() {
    assert_liteinst_virtual_time_is_continuous();
    assert_liteinst_strict_verify(
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
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/groups"),
        &[],
        expected_groups.as_bytes(),
    );
}

#[test]
fn liteinst_strict_verify_file_and_text_utilities() {
    let fixture = compatibility_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");

    assert_liteinst_strict_verify(
        Path::new("/usr/bin/printf"),
        &["liteinst-printf-ok\n"],
        b"liteinst-printf-ok\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/grep"),
        &["^liteinst", fixture],
        COMPAT_FIXTURE_CONTENT,
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/head"),
        &["-n", "1", fixture],
        COMPAT_FIXTURE_CONTENT,
    );

    let expected_wc = format!("{} {fixture}\n", COMPAT_FIXTURE_CONTENT.len());
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/wc"),
        &["-c", fixture],
        expected_wc.as_bytes(),
    );
    let expected_sha256 = format!("{COMPAT_FIXTURE_SHA256}  {fixture}\n");
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/sha256sum"),
        &[fixture],
        expected_sha256.as_bytes(),
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/stat"),
        &["-c", "%s", fixture],
        format!("{}\n", COMPAT_FIXTURE_CONTENT.len()).as_bytes(),
    );
}

#[test]
fn liteinst_strict_verify_semantic_text_utilities() {
    let fixture = semantic_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");

    assert_liteinst_strict_verify(
        Path::new("/usr/bin/tail"),
        &["-n", "2", fixture],
        b"alpha:1\nbeta:2\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/uniq"),
        &[fixture],
        b"gamma:3\nalpha:1\nbeta:2\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/cut"),
        &["-d", ":", "-f", "1", fixture],
        b"gamma\nalpha\nalpha\nbeta\n",
    );
    assert_liteinst_strict_verify(Path::new("/usr/bin/diff"), &[fixture, fixture], b"");
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/sed"),
        &["-n", "2,3p", fixture],
        b"alpha:1\nalpha:1\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/sort"),
        &[fixture],
        b"alpha:1\nalpha:1\nbeta:2\ngamma:3\n",
    );
}

#[test]
fn liteinst_strict_verify_semantic_file_and_sqlite_utilities() {
    let fixture = semantic_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");

    assert_liteinst_strict_verify(
        Path::new("/usr/bin/find"),
        &[fixture, "-maxdepth", "0", "-type", "f", "-print"],
        format!("{fixture}\n").as_bytes(),
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/md5sum"),
        &[fixture],
        format!("{SEMANTIC_FIXTURE_MD5}  {fixture}\n").as_bytes(),
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/du"),
        &["-b", fixture],
        format!("{}\t{fixture}\n", SEMANTIC_FIXTURE_CONTENT.len()).as_bytes(),
    );
    assert_liteinst_strict_verify(
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
fn liteinst_strict_verify_encoding_and_digest_utilities() {
    let fixture = compatibility_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");

    assert_liteinst_strict_verify(
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
        assert_liteinst_strict_verify(Path::new(program), &[fixture], expected.as_bytes());
    }
    let expected_cksum = format!("2216041199 {} {fixture}\n", COMPAT_FIXTURE_CONTENT.len());
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/cksum"),
        &[fixture],
        expected_cksum.as_bytes(),
    );
}

#[test]
fn liteinst_strict_verify_formatting_and_sequence_utilities() {
    let compat_fixture = compatibility_fixture();
    let compat_fixture = compat_fixture
        .to_str()
        .expect("fixture path should be UTF-8");
    let semantic_fixture = semantic_fixture();
    let semantic_fixture = semantic_fixture
        .to_str()
        .expect("fixture path should be UTF-8");

    assert_liteinst_strict_verify(Path::new("/usr/bin/seq"), &["5"], b"1\n2\n3\n4\n5\n");
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/fmt"),
        &["--width=10", compat_fixture],
        b"liteinst\ncompatibility\nfixture\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/fold"),
        &["--width=8", compat_fixture],
        b"liteinst\n compati\nbility f\nixture\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/nl"),
        &["-ba", semantic_fixture],
        b"     1\tgamma:3\n     2\talpha:1\n     3\talpha:1\n     4\tbeta:2\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/tac"),
        &[semantic_fixture],
        b"beta:2\nalpha:1\nalpha:1\ngamma:3\n",
    );
}

#[test]
fn liteinst_strict_verify_round2_encoding_and_comparison_utilities() {
    let fixture = compatibility_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");

    assert_liteinst_strict_verify(
        Path::new("/usr/bin/base32"),
        &["--wrap=0", fixture],
        b"NRUXIZLJNZZXIIDDN5WXAYLUNFRGS3DJOR4SAZTJPB2HK4TFBI======",
    );
    let sum_output = run_liteinst_strict_verify(Path::new("/usr/bin/sum"), &[fixture]);
    let sum_stdout = String::from_utf8(sum_output.stdout).expect("sum output should be UTF-8");
    let sum_fields = sum_stdout.split_whitespace().collect::<Vec<_>>();
    match sum_fields.as_slice() {
        ["04458", "1"] => {}
        ["04458", "1", output_path] => assert_eq!(*output_path, fixture),
        _ => panic!("unexpected sum output: {sum_stdout:?}"),
    }
    assert_liteinst_strict_verify(Path::new("/usr/bin/cmp"), &[fixture, fixture], b"");
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/comm"),
        &[fixture, fixture],
        b"\t\tliteinst compatibility fixture\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/join"),
        &[fixture, fixture],
        b"liteinst compatibility fixture compatibility fixture\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/paste"),
        &[fixture, fixture],
        b"liteinst compatibility fixture\tliteinst compatibility fixture\n",
    );
}

#[test]
fn liteinst_strict_verify_round2_representation_and_path_utilities() {
    let fixture = compatibility_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");

    assert_liteinst_strict_verify(
        Path::new("/usr/bin/od"),
        &["-An", "-tx1", fixture],
        b" 6c 69 74 65 69 6e 73 74 20 63 6f 6d 70 61 74 69\n 62 69 6c 69 74 79 20 66 69 78 74 75 72 65 0a\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/pr"),
        &["-t", fixture],
        COMPAT_FIXTURE_CONTENT,
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/readlink"),
        &["-f", "/etc/../etc/hostname"],
        b"/etc/hostname\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/rev"),
        &[fixture],
        b"erutxif ytilibitapmoc tsnietil\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/strings"),
        &[fixture],
        COMPAT_FIXTURE_CONTENT,
    );
    let dd_input = format!("if={fixture}");
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/dd"),
        &[&dd_input, "bs=7", "count=2", "status=none"],
        b"liteinst compa",
    );
}

#[test]
fn liteinst_strict_verify_round2_arithmetic_and_predicate_utilities() {
    let fixture = compatibility_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");

    assert_liteinst_strict_verify(Path::new("/usr/bin/factor"), &["84"], b"84: 2 2 3 7\n");
    assert_liteinst_strict_verify(Path::new("/usr/bin/expr"), &["6", "*", "7"], b"42\n");
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/numfmt"),
        &["--to=iec", "1024"],
        b"1.0K\n",
    );
    assert_liteinst_strict_verify(Path::new("/usr/bin/test"), &["-f", fixture], b"");
    assert_liteinst_strict_verify(Path::new("/usr/bin/pathchk"), &[fixture], b"");
}

#[test]
fn liteinst_strict_verify_round3_portable_system_utilities() {
    assert_liteinst_strict_verify(Path::new("/usr/bin/arch"), &[], b"x86_64\n");
    assert_liteinst_strict_verify(Path::new("/usr/bin/getconf"), &["LONG_BIT"], b"64\n");
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/getopt"),
        &["-o", "ab:", "--", "-a", "-b", "value", "rest"],
        b" -a -b 'value' -- 'rest'\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/bin/bash"),
        &[
            "--noprofile",
            "--norc",
            "-c",
            "printf 'liteinst-bash-ok\\n'",
        ],
        b"liteinst-bash-ok\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/jq"),
        &["-nr", "[3,1,2] | sort | join(\",\")"],
        b"1,2,3\n",
    );

    let existing_directory = compatibility_fixture()
        .parent()
        .expect("compatibility fixture should have a parent directory");
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/mkdir"),
        &[
            "-p",
            existing_directory.to_str().expect("path should be UTF-8"),
        ],
        b"",
    );
}

#[test]
fn liteinst_strict_verify_round3_encoding_and_compression_utilities() {
    let fixture = compatibility_fixture();
    let fixture = fixture.to_str().expect("fixture path should be UTF-8");
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/iconv"),
        &["-f", "UTF-8", "-t", "UTF-16LE", fixture],
        b"l\0i\0t\0e\0i\0n\0s\0t\0 \0c\0o\0m\0p\0a\0t\0i\0b\0i\0l\0i\0t\0y\0 \0f\0i\0x\0t\0u\0r\0e\0\n\0",
    );

    let [gzip_fixture, bzip2_fixture, xz_fixture] = compressed_fixtures();
    for (program, compressed_fixture) in [
        ("/usr/bin/gzip", gzip_fixture),
        ("/usr/bin/bzip2", bzip2_fixture),
        ("/usr/bin/xz", xz_fixture),
    ] {
        assert_liteinst_strict_verify(
            Path::new(program),
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
fn liteinst_strict_verify_round3_stdin_filter_utilities() {
    let output = run_liteinst_strict_verify_with_stdin(
        Path::new("/usr/bin/tr"),
        &["a-z", "A-Z"],
        b"gamma\nalpha\nbeta\n",
    );
    assert_eq!(output.stdout, b"GAMMA\nALPHA\nBETA\n");

    let output =
        run_liteinst_strict_verify_with_stdin(Path::new("/usr/bin/tee"), &[], b"liteinst-tee-ok\n");
    assert_eq!(output.stdout, b"liteinst-tee-ok\n");

    let output =
        run_liteinst_strict_verify_with_stdin(Path::new("/usr/bin/tsort"), &[], b"a b\nb c\n");
    assert_eq!(output.stdout, b"a\nb\nc\n");
}

#[test]
fn liteinst_strict_verify_path_and_language_utilities() {
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/basename"),
        &["/tmp/hermit-example.txt", ".txt"],
        b"hermit-example\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/dirname"),
        &["/tmp/hermit-example.txt"],
        b"/tmp\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/realpath"),
        &["/etc/../etc/passwd"],
        b"/etc/passwd\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/ls"),
        &["-1", "/etc/hostname"],
        b"/etc/hostname\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/awk"),
        &["BEGIN { for (i = 1; i <= 10; ++i) sum += i; print sum }"],
        b"55\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/perl"),
        &["-e", r#"print join(q{,}, map { $_ * $_ } 1..5), qq{\n}"#],
        b"1,4,9,16,25\n",
    );
}

#[test]
fn liteinst_strict_verify_shell_and_entropy_consumer() {
    assert_liteinst_strict_verify(
        Path::new("/bin/sh"),
        &["-c", "printf 'liteinst-shell-ok\\n'"],
        b"liteinst-shell-ok\n",
    );
    assert_liteinst_strict_verify(
        Path::new("/usr/bin/hexdump"),
        &["/dev/urandom", "--length", "16"],
        b"0000000 7229 04bb 964d 28df ba71 4c03 de95 7027\n0000010\n",
    );
}

#[test]
fn liteinst_strict_verify_python_entropy() {
    let output = run_liteinst_strict_verify(
        Path::new("/usr/bin/python3"),
        &[
            "-c",
            "import os; print(os.getpid(), len(os.urandom(8)), os.urandom(8).hex())",
        ],
    );
    let stdout = String::from_utf8(output.stdout).expect("Python output should be UTF-8");
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
}

#[test]
fn liteinst_strict_verify_python_random_example() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let output = run_liteinst_strict_verify(&repository.join("examples/rand.py"), &[]);
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

/// Runs one task-creating guest mode under LiteInst and requires it to finish.
///
/// ⚠️ THIS REPLACED A REFUSAL ASSERTION AND IS STRICTLY STRONGER THAN IT WAS.
/// Until Reverie removed the clone boundary, `handle_new_task` returned
/// `ENOTSUPP` for every `clone`/`clone3`/`fork`/`vfork`, and this helper
/// asserted that refusal. The refusal is gone for `clone`/`clone3`/`fork`, so
/// the old assertion had to be updated rather than deleted: it now requires the
/// guest to RUN, which the refusal could never satisfy, and it still refuses to
/// accept a silent reintroduction of the boundary.
///
/// `marker` is the guest's own end-of-mode line, printed only after the created
/// task has been created, run and reaped (see `tests/c/liteinst_advanced.c`), so
/// a regression that skips the task cannot satisfy this by exiting zero. The
/// negative assertions carry the two failure shapes the old test existed for:
/// `ENOTSUPP` is the removed boundary coming back, and `Bad system call` is the
/// SIGSYS the seccomp filter must never deliver to the guest.
///
/// The ten-second child deadline is deliberately unchanged. `--verify` is
/// deliberately NOT added here: the same measurement that justified this update
/// timed `threads` under `--verify --verify-strict` at 3.32/3.37/3.47/3.71 and
/// one 14.03 seconds on a loaded host, so folding it in would either flake
/// against this bound or require widening it.
fn assert_multi_task_mode(mode: &str, marker: &str) {
    liteinst_runtime::ensure_liteinst_runtime();
    let mut command = liteinst_command("error");
    let mut child = command
        .arg("--")
        .arg(advanced_guest())
        .arg(mode)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to start Hermit LiteInst multi-task guest");
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("failed to poll Hermit LiteInst") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("Hermit LiteInst hung running {mode}");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let output = child
        .wait_with_output()
        .expect("failed to collect Hermit LiteInst multi-task output");
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
        stdout.contains(marker),
        "guest did not reach the end of {mode}\nstdout={stdout}\nstderr={stderr}",
    );
    assert!(
        !stderr.contains("ENOTSUPP (Operation is not supported)"),
        "{stderr}"
    );
    assert!(!stderr.contains("Bad system call"), "{stderr}");
}

/// Path to the freshly built `libreverie_liteinst.so` preload runtime.
///
/// [`liteinst_runtime::ensure_liteinst_runtime`] builds it beside the Hermit
/// test binary, so it lives in the same profile directory.
fn liteinst_runtime_library() -> PathBuf {
    liteinst_runtime::ensure_liteinst_runtime();
    liteinst_runtime::liteinst_runtime_library()
}

/// A bare preload must not create a second in-guest Detcore Tool.
///
/// Host mode is selected only by `run_host_with_preload`. Without that private
/// selector, even a stale legacy coordinator variable must leave the patch DSO
/// inert and let the program run normally.
#[test]
fn liteinst_preload_is_inert_without_host_runtime_selector() {
    let runtime = liteinst_runtime_library();
    assert!(
        runtime.is_file(),
        "expected LiteInst preload runtime at {}",
        runtime.display(),
    );

    let output = Command::new("/bin/true")
        .env(
            reverie_liteinst::COORDINATOR_ENV,
            "/definitely/not/a/coordinator.sock",
        )
        .env("LD_PRELOAD", &runtime)
        .output()
        .expect("failed to launch /bin/true under the LiteInst preload");

    assert_eq!(
        output.status.code(),
        Some(0),
        "bare patch preload must remain inert\nstatus={:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("reverie-liteinst initialization failed"),
        "bare preload attempted to install an in-guest Detcore Tool: stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn liteinst_thread_clone_runs_without_sigsys() {
    assert_multi_task_mode("threads", "threads-ok");
}

#[test]
fn liteinst_fork_runs_without_hanging() {
    assert_multi_task_mode("fork", "fork-ok");
}

#[test]
fn liteinst_abnormal_exit_after_registration_does_not_hang() {
    liteinst_runtime::ensure_liteinst_runtime();
    // INFO-level Detcore diagnostics can exceed a pipe's capacity before the
    // guest reaches its fatal signal. Keep draining out of the child process
    // while retaining the diagnostics for the scheduler-start assertion.
    let mut stderr = tempfile::tempfile().expect("create LiteInst diagnostic sink");
    let stderr_sink = stderr.try_clone().expect("clone LiteInst diagnostic sink");
    let mut command = liteinst_command("info");
    let mut child = command
        .args(["--", "/bin/sh", "-c", "kill -9 $$"])
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
}

// Regression coverage for https://github.com/rrnewton/hermit/issues/3338: the
// LiteInst runtime's preload constructor issues a few hundred syscalls before
// the guest's main runs. Charging them as guest syscalls pushed sysinfo(2)
// uptime from 121 to 123 under --max-timeslice=disabled, where each syscall is
// charged at the no-PMU rate. These runs disable the timeslice explicitly so
// the result does not depend on whether the host exposes a PMU.
static BOOTSTRAP_TIME_HOST_IDENTITY: OnceLock<PathBuf> = OnceLock::new();
static BOOTSTRAP_TIME_CLOCK_TRAJECTORY: OnceLock<PathBuf> = OnceLock::new();

fn compile_bootstrap_time_guest(
    cell: &'static OnceLock<PathBuf>,
    source: &str,
    name: &str,
) -> &'static Path {
    cell.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("liteinst-bootstrap-time");
        fs::create_dir_all(&build_root).expect("failed to create bootstrap-time guest directory");
        let guest = build_root.join(name);
        // -D_GNU_SOURCE matches the build flags that the c-programs/host-identity
        // cell in tests/e2e/manifests/c-programs.yaml gives host_identity.c.
        let output = Command::new("cc")
            .args(["-O2", "-g", "-Wall", "-Wextra", "-Werror", "-D_GNU_SOURCE"])
            .arg(repository.join(source))
            .arg("-o")
            .arg(&guest)
            .output()
            .unwrap_or_else(|error| panic!("failed to compile {source}: {error}"));
        assert!(
            output.status.success(),
            "{source} compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn bootstrap_time_host_identity() -> &'static Path {
    // The unchanged host-identity fixture. It asserts sysinfo.uptime == 121.
    compile_bootstrap_time_guest(
        &BOOTSTRAP_TIME_HOST_IDENTITY,
        "tests/c/host_identity.c",
        "host_identity",
    )
}

fn bootstrap_time_clock_trajectory() -> &'static Path {
    compile_bootstrap_time_guest(
        &BOOTSTRAP_TIME_CLOCK_TRAJECTORY,
        "hermit-cli/tests/fixtures/clock_trajectory.c",
        "clock_trajectory",
    )
}

fn bootstrap_time_command(backend: &str, home: &Path) -> Command {
    let mut command = Command::new(liteinst_runtime::hermit_binary());
    command
        .arg("--log=info")
        .args(["--backend", backend, "run"])
        .arg(format!("--epoch={VIRTUAL_TIME_EPOCH}"))
        .args([
            "--max-timeslice=disabled",
            "--strict",
            "--base-env=minimal",
            "--mount=type=tmpfs,target=/test",
            "--workdir=/test",
            "--env=LC_ALL=C",
            "--env=TZ=UTC",
        ])
        .arg(format!("--env=HOME={}", home.display()))
        .env("HOME", home);
    command
}

fn assert_bootstrap_time_success(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label}: status={:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn liteinst_runtime_bootstrap_is_not_charged_to_host_identity_uptime() {
    liteinst_runtime::ensure_liteinst_runtime();
    let scratch = tempfile::tempdir().expect("failed to create bootstrap-time scratch directory");
    let report = scratch.path().join("verify.json");
    let output = bootstrap_time_command("liteinst", scratch.path())
        .args(["--verify", "--verify-strict"])
        .arg(format!("--verify-json={}", report.display()))
        .arg("--")
        .arg(bootstrap_time_host_identity())
        .output()
        .expect("failed to run Hermit LiteInst on host_identity");
    assert_bootstrap_time_success("liteinst host_identity", &output);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.lines().any(|line| line == "sysinfo.uptime=121"),
        "LiteInst host_identity must observe uptime 121:\nstdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stderr.contains(
            "liteinst host hybrid] activation verified (traps=1, hooks=31); Detcore Tool active in ptrace host"
        ),
        "{stderr}"
    );
    assert!(
        stderr.contains("Success: deterministic. Determinism verified."),
        "{stderr}"
    );

    let report: serde_json::Value = serde_json::from_slice(
        &fs::read(&report).expect("failed to read the --verify-json report"),
    )
    .expect("the --verify-json report must be JSON");
    assert_eq!(report["verdict"], "matched", "{report}");
    assert_eq!(
        report["verified"],
        serde_json::Value::Bool(true),
        "{report}"
    );
    assert_eq!(
        report["bitwise_parity"],
        serde_json::Value::Bool(true),
        "{report}"
    );
    let compared = &report["compared_log_messages"];
    let left = compared["left"]
        .as_u64()
        .unwrap_or_else(|| panic!("compared_log_messages.left is missing: {report}"));
    assert!(
        left > 0,
        "strict verify compared no INFO messages: {report}"
    );
    assert_eq!(compared["right"].as_u64(), Some(left), "{report}");
}

/// clock_trajectory.c prints this many samples, the first half before it
/// execs itself and the second half after.
const CLOCK_TRAJECTORY_SAMPLES: usize = 10;
const CLOCK_TRAJECTORY_FIRST_AFTER_EXEC: usize = 5;

#[derive(Debug, PartialEq)]
struct ClockSample {
    monotonic_ns: u128,
    uptime: i64,
}

fn clock_trajectory(backend: &str) -> Vec<ClockSample> {
    if backend == "liteinst" {
        liteinst_runtime::ensure_liteinst_runtime();
    }
    let home = tempfile::tempdir().expect("failed to create clock-trajectory HOME");
    let output = bootstrap_time_command(backend, home.path())
        .arg("--")
        .arg(bootstrap_time_clock_trajectory())
        .output()
        .unwrap_or_else(|error| panic!("failed to run Hermit {backend}: {error}"));
    assert_bootstrap_time_success(backend, &output);
    let stdout = String::from_utf8(output.stdout).expect("clock trajectory output is UTF-8");
    let samples = stdout
        .lines()
        .enumerate()
        .map(|(index, line)| {
            let fields = line.split(' ').collect::<Vec<_>>();
            let [sample, monotonic, uptime] = fields.as_slice() else {
                panic!("{backend}: malformed clock sample {line:?}\n{stdout}");
            };
            assert_eq!(
                *sample,
                format!("sample={index}"),
                "{backend}: out-of-order sample\n{stdout}"
            );
            let monotonic_ns = monotonic
                .strip_prefix("monotonic_ns=")
                .and_then(|value| value.parse().ok())
                .unwrap_or_else(|| panic!("{backend}: bad monotonic field {line:?}"));
            let uptime = uptime
                .strip_prefix("uptime=")
                .and_then(|value| value.parse().ok())
                .unwrap_or_else(|| panic!("{backend}: bad uptime field {line:?}"));
            ClockSample {
                monotonic_ns,
                uptime,
            }
        })
        .collect::<Vec<_>>();
    // Five samples before the fixture execs itself and five after it.
    assert_eq!(
        samples.len(),
        CLOCK_TRAJECTORY_SAMPLES,
        "{backend}: expected ten samples\n{stdout}"
    );
    for pair in samples.windows(2) {
        assert!(
            pair[1].monotonic_ns > pair[0].monotonic_ns,
            "{backend}: CLOCK_MONOTONIC must strictly increase: {samples:?}"
        );
    }
    samples
}

/// Upper bound on how much later, in virtual nanoseconds, each LiteInst image
/// reaches main than the same image under ptrace.
///
/// The fix for https://github.com/rrnewton/hermit/issues/3338 stops charging the
/// runtime's preload constructor, which cost about 1.99 s per image here (of a
/// 2.14 s gap between the backends with the constructor charged). It does
/// not make the backends reach main at the same instant. Before the runtime's
/// begin trap, the dynamic loader maps the preloaded runtime and its libgcc_s
/// dependency as ordinary guest syscalls, and those stay charged. Scheduler
/// turns inside the bootstrap window also still advance global time. Measured
/// with these flags, LiteInst reaches main 151,137,500 ns after ptrace in the
/// first image in one build directory and 136,137,500 ns in another, at the
/// same commit, and in both the exec adds 136,137,500 ns more. The values repeat
/// across runs in one environment, but they depend on the fixture binary that
/// the host's cc produces, on the runtime's library set and on the environment
/// (likely the lengths of the mapped paths), so the test bounds them instead of
/// asserting them. 200 ms per image leaves about 49 to 64 ms of headroom and
/// fails if even a tenth of the runtime constructor is charged again. This residual is
/// not parity: it grows with every exec, and after enough execs sysinfo uptime
/// differs between the backends again. The follow-up is tracked from
/// https://github.com/rrnewton/hermit/issues/3338.
const LITEINST_PER_IMAGE_RESIDUAL_BOUND_NS: u128 = 200_000_000;

#[test]
fn liteinst_clock_trajectory_excludes_runtime_bootstrap_in_each_image() {
    let liteinst = clock_trajectory("liteinst");
    let ptrace = clock_trajectory("ptrace");

    // The fixture's last read is about 0.91 to 0.92 s past the epoch under LiteInst,
    // below the next uptime boundary, so both backends read the same uptime.
    // This is not a general guarantee; see LITEINST_PER_IMAGE_RESIDUAL_BOUND_NS.
    let liteinst_uptime = liteinst.iter().map(|s| s.uptime).collect::<Vec<_>>();
    let ptrace_uptime = ptrace.iter().map(|s| s.uptime).collect::<Vec<_>>();
    assert_eq!(
        liteinst_uptime, ptrace_uptime,
        "LiteInst and ptrace must agree on sysinfo uptime\nliteinst={liteinst:?}\nptrace={ptrace:?}"
    );

    // Inside one image, after main starts, the guest runs identical code under
    // both backends, so the time between consecutive samples must be identical
    // (6,500,000 ns with these flags). A difference means the backend charges
    // guest code differently, not just the bootstrap.
    for index in 1..CLOCK_TRAJECTORY_SAMPLES {
        if index == CLOCK_TRAJECTORY_FIRST_AFTER_EXEC {
            continue;
        }
        let liteinst_delta = liteinst[index].monotonic_ns - liteinst[index - 1].monotonic_ns;
        let ptrace_delta = ptrace[index].monotonic_ns - ptrace[index - 1].monotonic_ns;
        assert_eq!(
            liteinst_delta,
            ptrace_delta,
            "samples {} and {index} must be the same distance apart under both backends\nliteinst={liteinst:?}\nptrace={ptrace:?}",
            index - 1
        );
    }

    // LiteInst reaches main later than ptrace in each image, by the loader and
    // scheduler residual only. Bound the first image's gap, and the growth of
    // the gap across the exec, by the per-image residual bound.
    let gap_before_exec = liteinst[0]
        .monotonic_ns
        .checked_sub(ptrace[0].monotonic_ns)
        .unwrap_or_else(|| {
            panic!("LiteInst reached main before ptrace\nliteinst={liteinst:?}\nptrace={ptrace:?}")
        });
    let gap_after_exec = liteinst[CLOCK_TRAJECTORY_FIRST_AFTER_EXEC]
        .monotonic_ns
        .checked_sub(ptrace[CLOCK_TRAJECTORY_FIRST_AFTER_EXEC].monotonic_ns)
        .unwrap_or_else(|| {
            panic!(
                "LiteInst reached the exec'd main before ptrace\nliteinst={liteinst:?}\nptrace={ptrace:?}"
            )
        });
    assert!(
        gap_before_exec < LITEINST_PER_IMAGE_RESIDUAL_BOUND_NS,
        "LiteInst reaches main {gap_before_exec} ns after ptrace in the first image\nliteinst={liteinst:?}\nptrace={ptrace:?}"
    );
    let exec_growth = gap_after_exec
        .checked_sub(gap_before_exec)
        .unwrap_or_else(|| {
            panic!(
                "the LiteInst gap shrank across the exec\nliteinst={liteinst:?}\nptrace={ptrace:?}"
            )
        });
    assert!(
        exec_growth < LITEINST_PER_IMAGE_RESIDUAL_BOUND_NS,
        "the exec adds {exec_growth} ns to the LiteInst gap\nliteinst={liteinst:?}\nptrace={ptrace:?}"
    );
}
