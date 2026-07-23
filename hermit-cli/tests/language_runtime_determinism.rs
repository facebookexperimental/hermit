/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;
use std::sync::MutexGuard;

const NATIVE_ATTEMPTS: usize = 5;
const STRICT_ATTEMPTS: usize = 3;
const COMMAND_TIMEOUT: &str = "30s";

static HERMIT_RUNTIME_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug)]
struct Invocation {
    program: PathBuf,
    args: Vec<OsString>,
}

impl Invocation {
    fn new(program: PathBuf, args: impl IntoIterator<Item = impl Into<OsString>>) -> Self {
        Self {
            program,
            args: args.into_iter().map(Into::into).collect(),
        }
    }
}

fn runtime_lock() -> MutexGuard<'static, ()> {
    HERMIT_RUNTIME_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn source(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository")
        .join("tests")
        .join("runtime")
        .join(name)
}

fn fresh_build_dir(name: &str) -> PathBuf {
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("language-runtime-determinism")
        .join(name);
    if directory.exists() {
        fs::remove_dir_all(&directory).expect("failed to clean runtime test build directory");
    }
    fs::create_dir_all(&directory).expect("failed to create runtime test build directory");
    directory
}

fn executable(candidates: &[&str]) -> PathBuf {
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| {
            fs::metadata(path)
                .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
        .unwrap_or_else(|| panic!("required runtime tool is unavailable: {candidates:?}"))
}

fn bounded_command(program: impl Into<OsString>) -> Command {
    let mut command = Command::new("timeout");
    command
        .args(["--signal=TERM", "--kill-after=2s", COMMAND_TIMEOUT])
        .arg(program.into());
    command
}

fn command_output(mut command: Command, label: &str) -> Output {
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {label}: {rendered}: {error}"));
    assert!(
        output.status.success(),
        "{label} failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

fn run_native(invocation: &Invocation, label: &str) -> Vec<u8> {
    let mut command = bounded_command(invocation.program.as_os_str());
    command.args(&invocation.args);
    command_output(command, label).stdout
}

fn run_strict(invocation: &Invocation, label: &str) -> Vec<u8> {
    let mut command = bounded_command(env!("CARGO_BIN_EXE_hermit"));
    command
        .args(["run", "--strict", "--"])
        .arg(&invocation.program)
        .args(&invocation.args);
    command_output(command, label).stdout
}

fn assert_runtime_entropy_is_determinized(runtime: &str, invocation: Invocation) {
    let _guard = runtime_lock();

    let native_outputs = (0..NATIVE_ATTEMPTS)
        .map(|iteration| {
            run_native(
                &invocation,
                &format!("{runtime} native iteration {}", iteration + 1),
            )
        })
        .collect::<BTreeSet<_>>();
    assert!(
        native_outputs.len() > 1,
        "NONDET_SOURCE=os-seeded runtime entropy: {runtime} produced only one native output in {NATIVE_ATTEMPTS} attempts"
    );

    let strict_outputs = (0..STRICT_ATTEMPTS)
        .map(|iteration| {
            run_strict(
                &invocation,
                &format!("{runtime} strict iteration {}", iteration + 1),
            )
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        strict_outputs.len(),
        1,
        "NONDET_SOURCE=os-seeded runtime entropy: {runtime} changed output across {STRICT_ATTEMPTS} explicit --strict runs"
    );
}

#[test]
#[ignore = "requires the optional Go runtime matrix toolchain"]
fn go_runtime_entropy_is_determinized() {
    let go = executable(&["/usr/bin/go", "/usr/local/go/bin/go"]);
    let build = fresh_build_dir("go");
    let binary = build.join("runtime-random");
    let mut command = bounded_command(go.as_os_str());
    command
        .env("GOCACHE", build.join("cache"))
        .args(["build", "-trimpath", "-o"])
        .arg(&binary)
        .arg(source("random.go"));
    command_output(command, "compile Go runtime probe");
    assert_runtime_entropy_is_determinized("Go", Invocation::new(binary, [] as [&str; 0]));
}

#[test]
#[ignore = "requires the optional Ruby runtime"]
fn ruby_runtime_entropy_is_determinized() {
    let ruby = executable(&["/usr/bin/ruby"]);
    assert_runtime_entropy_is_determinized(
        "Ruby",
        Invocation::new(
            ruby,
            [
                OsString::from("--disable-gems"),
                source("random.rb").into_os_string(),
            ],
        ),
    );
}

#[test]
#[ignore = "requires the optional Node.js runtime"]
fn node_runtime_entropy_is_determinized() {
    let node = executable(&["/usr/bin/node"]);
    assert_runtime_entropy_is_determinized(
        "Node.js",
        Invocation::new(node, [source("random.js").into_os_string()]),
    );
}

#[test]
#[ignore = "requires the optional OpenJDK toolchain"]
fn jvm_runtime_entropy_is_determinized() {
    let javac = executable(&["/usr/bin/javac"]);
    let java = executable(&["/usr/bin/java"]);
    let build = fresh_build_dir("java");
    let mut command = bounded_command(javac.as_os_str());
    command
        .arg("-d")
        .arg(&build)
        .arg(source("RuntimeRandom.java"));
    command_output(command, "compile Java runtime probe");
    assert_runtime_entropy_is_determinized(
        "OpenJDK",
        Invocation::new(
            java,
            [
                OsString::from("-Xint"),
                OsString::from("-XX:ActiveProcessorCount=1"),
                OsString::from("-cp"),
                build.into_os_string(),
                OsString::from("RuntimeRandom"),
            ],
        ),
    );
}

#[test]
#[ignore = "requires the optional OCaml native compiler"]
fn ocaml_runtime_entropy_is_determinized() {
    let ocamlopt = executable(&["/usr/bin/ocamlopt"]);
    let build = fresh_build_dir("ocaml");
    let copied_source = build.join("random.ml");
    fs::copy(source("random.ml"), &copied_source).expect("failed to copy OCaml runtime probe");
    let binary = build.join("runtime-random");
    let mut command = bounded_command(ocamlopt.as_os_str());
    command.arg("-o").arg(&binary).arg(copied_source);
    command_output(command, "compile OCaml runtime probe");
    assert_runtime_entropy_is_determinized("OCaml", Invocation::new(binary, [] as [&str; 0]));
}

#[test]
#[ignore = "requires an OSS CPython 3 runtime"]
fn python_runtime_entropy_is_determinized() {
    let python = executable(&["/usr/bin/python3"]);
    let mut version = bounded_command(python.as_os_str());
    version.arg("-VV");
    let version = command_output(version, "inspect Python runtime");
    let version_text = format!(
        "{}{}",
        String::from_utf8_lossy(&version.stdout),
        String::from_utf8_lossy(&version.stderr)
    )
    .to_ascii_lowercase();
    assert!(
        !version_text.contains("fbpython") && !version_text.contains("+meta"),
        "language runtime coverage requires OSS CPython, found: {version_text}"
    );
    assert_runtime_entropy_is_determinized(
        "OSS CPython",
        Invocation::new(
            python,
            [
                OsString::from("-S"),
                OsString::from("-I"),
                source("random.py").into_os_string(),
            ],
        ),
    );
}
