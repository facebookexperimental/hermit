/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[path = "common/hermit_binary.rs"]
mod hermit_test;
#[path = "common/prepared_tools.rs"]
mod prepared_tools;

use std::collections::BTreeMap;
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
// The strict runs are separate Hermit processes compared with each other, so
// they share one epoch. A host-drawn one differs per run, and Ruby's
// Random.new_seed mixes in the clock: three strict runs gave three seeds.
const EPOCH: &str = "--epoch=2026-01-01T00:00:00Z";

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
        .join(format!(
            "language-runtime-determinism-{}",
            std::process::id()
        ))
        .join(name);
    if directory.exists() {
        fs::remove_dir_all(&directory).expect("failed to clean runtime test build directory");
    }
    fs::create_dir_all(&directory).expect("failed to create runtime test build directory");
    directory
}

/// The executable the harness declared for `name` in `HERMIT_PREPARED_TOOLS`,
/// or else the first candidate that is an executable file.
fn executable(name: &str, candidates: &[&str]) -> PathBuf {
    if let Some(declared) = prepared_tools::prepared_tool(name) {
        return declared;
    }
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
    let output = hermit_test::hermit_output(&rendered, &mut command);
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
        .args(["run", "--strict", EPOCH, "--"])
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
    let go = executable("go", &["/usr/bin/go", "/usr/local/go/bin/go"]);
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
fn ruby_runtime_entropy_is_determinized() {
    let ruby = executable("ruby", &["/usr/bin/ruby", "/bin/ruby"]);
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
fn ruby_thread_prctls_are_supported_in_fail_closed_mode() {
    let _guard = runtime_lock();
    let ruby = executable("ruby", &["/usr/bin/ruby", "/bin/ruby"]);
    let mut command = bounded_command(env!("CARGO_BIN_EXE_hermit"));
    command
        .args([
            "run",
            "--strict",
            "--verify",
            "--panic-on-unsupported-syscalls",
            "--",
        ])
        .arg(ruby)
        .args([
            "--disable-gems",
            "-e",
            r#"thread = Thread.new { Thread.current.name = "hermit-ruby"; 42 }; raise "bad" unless thread.value == 42"#,
        ]);
    command_output(command, "Ruby fail-closed prctl verification");
}

#[test]
fn node_runtime_entropy_is_determinized() {
    let node = executable("node", &["/usr/bin/node", "/bin/node"]);
    assert_runtime_entropy_is_determinized(
        "Node.js",
        Invocation::new(node, [source("random.js").into_os_string()]),
    );
}

#[test]
#[ignore = "requires the optional OpenJDK toolchain"]
fn jvm_runtime_entropy_is_determinized() {
    let javac = executable("javac", &["/usr/bin/javac"]);
    let java = executable("java", &["/usr/bin/java"]);
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
    let ocamlopt = executable("ocamlopt", &["/usr/bin/ocamlopt"]);
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
fn python_runtime_entropy_is_determinized() {
    let python = executable("python3", &["/usr/bin/python3", "/bin/python3"]);
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

/// Writes an empty file `name` in `directory` with `mode`.
fn tool_file_with_mode(directory: &Path, name: &str, mode: u32) -> PathBuf {
    fs::create_dir_all(directory).expect("failed to create a fixture directory");
    let path = directory.join(name);
    fs::write(&path, b"").expect("failed to write a fixture file");
    fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("failed to set a mode");
    path
}

/// The HERMIT_PREPARED_TOOLS text that declares each `(name, path)`.
fn tool_declaration(entries: &[(&str, &Path)]) -> OsString {
    let declared: BTreeMap<&str, &Path> = entries.iter().copied().collect();
    serde_json::to_string(&declared)
        .expect("failed to write a tool declaration")
        .into()
}

#[test]
fn a_declared_tool_is_used_only_when_declared() {
    assert_eq!(
        prepared_tools::parse_prepared_tools(None),
        Ok(BTreeMap::new())
    );
    let root = tempfile::tempdir().expect("failed to create a fixture directory");
    let lua = tool_file_with_mode(root.path(), "lua", 0o755);
    // Unlike Redis, a tool need not keep its own file name.
    let node = tool_file_with_mode(root.path(), "node-linux-x64", 0o755);
    let declared = tool_declaration(&[("lua", &lua), ("node", &node)]);
    let expected = BTreeMap::from([("lua".to_owned(), lua), ("node".to_owned(), node)]);
    assert_eq!(
        prepared_tools::parse_prepared_tools(Some(declared)),
        Ok(expected)
    );
}

#[test]
fn a_bad_tool_declaration_is_refused_rather_than_replaced() {
    use std::os::unix::ffi::OsStringExt;

    let root = tempfile::tempdir().expect("failed to create a fixture directory");
    let lua = tool_file_with_mode(root.path(), "lua", 0o755);
    let plain = tool_file_with_mode(&root.path().join("plain"), "lua", 0o644);
    let directory = root.path().join("directory/lua");
    fs::create_dir_all(&directory).expect("failed to create a fixture directory");
    let missing = root.path().join("missing/node");
    let cases: [(OsString, &str); 11] = [
        (OsString::from_vec(vec![0xff]), "is not UTF-8"),
        ("not json".into(), "is not a JSON object of paths"),
        (r#"{"lua": 1}"#.into(), "is not a JSON object of paths"),
        ("{}".into(), "names no tool"),
        (tool_declaration(&[("", &lua)]), "which is not a file name"),
        (
            tool_declaration(&[("bin/lua", &lua)]),
            "which is not a file name",
        ),
        (
            tool_declaration(&[("lua", Path::new("lua"))]),
            "which is not absolute",
        ),
        (
            tool_declaration(&[("lua", &directory)]),
            "which is not an executable file",
        ),
        (
            tool_declaration(&[("lua", &plain)]),
            "which is not an executable file",
        ),
        (
            tool_declaration(&[("node", &missing)]),
            "No such file or directory",
        ),
        // One bad entry refuses the whole declaration, the good one included.
        (
            tool_declaration(&[("lua", &lua), ("node", &missing)]),
            "names node",
        ),
    ];
    for (declared, problem) in cases {
        let shown = format!("{declared:?}");
        let refused = prepared_tools::parse_prepared_tools(Some(declared));
        let refused = refused.expect_err(&format!("accepted {shown}"));
        assert!(refused.contains(problem), "{shown}: {refused}");
    }
}
