/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A verify run across a host action between its two runs: the guests and the
//! FIFO-driven harness that name, or refuse to name, a host file changed
//! during one run (see `hermit::host_input_change`). Shared by the ptrace
//! tests in `verify_claim_names_its_limit.rs` and the SaBRe tests in
//! `sabre_examples.rs`.

#![allow(dead_code)]

use std::path::Path;
use std::process::Command;

/// The guest: open `F`, wait for the host on `fifo`, print the line it sent,
/// open `F` again, and print the (virtual) inode number of `F`. A host
/// replacement of `F` between the opens is a new host inode, so it gets a new
/// virtual number in that run only, and the runs' outputs differ.
///
/// (This used to print `G`'s number, which differed only because one new host
/// inode renumbered every file Hermit met after it. Hermit no longer does
/// that: https://github.com/rrnewton/hermit/issues/2897.)
pub const HOST_INPUT_GUEST: &str = r#"
    cat "$1/F" > /dev/null || exit 3
    read line < "$1/fifo" || exit 3
    echo "$line"
    cat "$1/F" > /dev/null || exit 3
    stat -c %i "$1/F" || exit 3
"#;

/// [`HOST_INPUT_GUEST`] without a child process: the shell opens `F` itself
/// with a builtin's redirection (`: <`), stats it with the `[` builtin (as
/// `cat`'s `fstat` did, so that run 1 meets the original inode and gives it a
/// virtual number before the replacement), reads the FIFO and echoes with
/// builtins, and `exec`s the final `stat`, so it never forks.
///
/// For SaBRe, where a child's exit is still ordered by host timing. The
/// shell's wait for its child and the child's `SIGCHLD` reach the coordinator
/// in either order (docs/SABRE_COMPATIBILITY.md: child completion ordering
/// belongs to the SIGCHLD owner). On a GitHub-hosted runner, whose `/bin/sh` is
/// dash, the two runs of [`HOST_INPUT_GUEST`] committed them in different
/// orders at the first `cat`'s exit
/// (https://github.com/rrnewton/hermit/actions/runs/37666223480). That divergence is in the
/// coordinator's part of the log, ahead of every record the guest forwards, so
/// no host replacement after it could be reported. A guest with no child
/// gives the runs nothing to order but the opens.
pub const HOST_INPUT_GUEST_WITHOUT_CHILDREN: &str = r#"
    : < "$1/F" || exit 3
    [ -f "$1/F" ] || exit 3
    read line < "$1/fifo" || exit 3
    echo "$line"
    : < "$1/F" || exit 3
    exec stat -c %i "$1/F"
"#;

/// A guest that replaces `F` itself, in one run only: after the host's line
/// it moves `A` over `F` when the line is `moveA`, and otherwise moves `B` to
/// `H`, leaving `F` alone. Both are one `mv`, which opens no file named here,
/// so both runs make the same opens. Run 1 then finds a new host inode at its
/// second open of `F` where run 2 finds the one it found before, exactly as a
/// host replacement would show. But the runs diverged before that open: they
/// read different lines.
pub const SELF_REPLACING_GUEST: &str = r#"
    cat "$1/F" > /dev/null || exit 3
    read line < "$1/fifo" || exit 3
    echo "$line"
    if [ "$line" = moveA ]; then mv "$1/A" "$1/F"; else mv "$1/B" "$1/H"; fi || exit 3
    cat "$1/F" > /dev/null || exit 3
"#;

/// A guest that rebinds `F` itself with raw calls, in a directory that keeps
/// its state between the two runs: after the host's line, `python` opens `F`,
/// renames `A` onto it, opens it again and links `F` back to `A`. In run 1
/// the rename replaces `F` and the link succeeds, leaving both names on one
/// file. In run 2 the rename therefore succeeds without changing anything,
/// and the link fails with EEXIST. The two runs' logs agree through the
/// second open, and the opens' identities differ as a host replacement's
/// would, but the guest made the replacement.
pub fn rebinding_guest(python: &Path) -> String {
    format!(
        r#"
    read line < "$1/fifo" || exit 3
    '{}' -c 'import os, sys
d = sys.argv[1]
open(d + "/F").read()
os.rename(d + "/A", d + "/F")
open(d + "/F").read()
os.link(d + "/F", d + "/A")' "$1"
"#,
        python.display()
    )
}

/// [`rebinding_guest`]'s sequence with the rename issued past the tracer: a
/// forked child jumps to the untraced `syscall` of Reverie's private page
/// (0x7100_0000) with `rename(A, F)` in its registers. The kernel runs it
/// untraced, and the child then dies on the `ud2` that follows the stub
/// (SIGILL). Detcore never sees the rename, so the guest's rebinding of `F` is
/// absent from its records while the opens around it are not.
pub fn private_page_rebinding_guest(python: &Path) -> String {
    format!(
        r#"
    read line < "$1/fifo" || exit 3
    '{}' -c 'import ctypes, mmap, os, sys
d = sys.argv[1]
open(d + "/F").read()
pid = os.fork()
if pid == 0:
    # mov eax, 82 (rename); movabs r11, 0x71000000; jmp r11
    code = bytes([0xb8, 0x52, 0, 0, 0, 0x49, 0xbb, 0, 0, 0, 0x71, 0, 0, 0, 0, 0x41, 0xff, 0xe3])
    page = mmap.mmap(-1, mmap.PAGESIZE, prot=mmap.PROT_READ | mmap.PROT_WRITE | mmap.PROT_EXEC)
    page.write(code)
    stub = ctypes.CFUNCTYPE(ctypes.c_long, ctypes.c_char_p, ctypes.c_char_p)(
        ctypes.addressof(ctypes.c_char.from_buffer(page)))
    stub((d + "/A").encode(), (d + "/F").encode())
    os._exit(9)
_, status = os.waitpid(pid, 0)
print(status)
open(d + "/F").read()
os.link(d + "/F", d + "/A")' "$1"
"#,
        python.display()
    )
}

/// How [`private_page_rewriting_guest`] changes Reverie's private page so
/// that its untraced `syscall` is followed by a `ret` instead of the `ud2`.
#[derive(Clone, Copy, Debug)]
pub enum PrivatePageRewrite {
    /// `mprotect` the page writable and replace the `ud2` with `ret`.
    Mprotect,
    /// `mmap` a page of `syscall; ret` over it with MAP_FIXED.
    MapFixed,
    /// Replace the `ud2` with `ret` through `/proc/self/mem`, which writes
    /// through the page's protection.
    ProcSelfMem,
}

/// [`private_page_rebinding_guest`]'s sequence, with the page first changed
/// as `rewrite` says, so that the forked child calls into it, renames `A`
/// onto `F` untraced and returns without faulting, then exits 9; the parent
/// writes the child's wait status to its standard error, an open
/// descriptor, so no host input is recorded for it, and stops before its
/// second open of `F` unless the child exited 9. No SIGILL
/// occurs, so only the rewrite can tell Detcore the rename may have
/// happened.
pub fn private_page_rewriting_guest(python: &Path, rewrite: PrivatePageRewrite) -> String {
    let rewrite = match rewrite {
        PrivatePageRewrite::Mprotect => {
            "libc.mprotect.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int]
    assert libc.mprotect(0x71000000, 4096, 7) == 0, ctypes.get_errno()
    ctypes.memmove(0x71000002, b\"\\xc3\", 1)"
        }
        PrivatePageRewrite::MapFixed => {
            "libc.mmap.restype = ctypes.c_void_p
    libc.mmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_long]
    # PROT_READ | PROT_WRITE | PROT_EXEC; MAP_PRIVATE | MAP_FIXED | MAP_ANONYMOUS
    assert libc.mmap(0x71000000, 4096, 7, 0x32, -1, 0) == 0x71000000, ctypes.get_errno()
    ctypes.memmove(0x71000000, b\"\\x0f\\x05\\xc3\", 3)"
        }
        PrivatePageRewrite::ProcSelfMem => {
            "with open(\"/proc/self/mem\", \"r+b\", buffering=0) as mem:
        mem.seek(0x71000002)
        mem.write(b\"\\xc3\")"
        }
    };
    format!(
        r#"
    read line < "$1/fifo" || exit 3
    '{}' -c 'import ctypes, mmap, os, sys
d = sys.argv[1]
open(d + "/F").read()
pid = os.fork()
if pid == 0:
    libc = ctypes.CDLL(None, use_errno=True)
    {rewrite}
    # mov eax, 82 (rename); movabs r11, 0x71000000; call r11; ret
    code = bytes([0xb8, 0x52, 0, 0, 0, 0x49, 0xbb, 0, 0, 0, 0x71, 0, 0, 0, 0, 0x41, 0xff, 0xd3, 0xc3])
    page = mmap.mmap(-1, mmap.PAGESIZE, prot=mmap.PROT_READ | mmap.PROT_WRITE | mmap.PROT_EXEC)
    page.write(code)
    stub = ctypes.CFUNCTYPE(ctypes.c_long, ctypes.c_char_p, ctypes.c_char_p)(
        ctypes.addressof(ctypes.c_char.from_buffer(page)))
    assert stub((d + "/A").encode(), (d + "/F").encode()) == 0
    os._exit(9)
_, status = os.waitpid(pid, 0)
print(status)
print("child wait status %d" % status, file=sys.stderr, flush=True)
assert status == 9 << 8, status
open(d + "/F").read()
os.link(d + "/F", d + "/A")' "$1"
"#,
        python.display()
    )
}

/// A guest that tries to rename onto `F` and fails: it opens `F`, waits for
/// the host's line, tries `rename(missing, F)` (ENOENT), opens `F` again and
/// prints the inode number of `F`, as [`HOST_INPUT_GUEST`] does. A failed
/// rename changes nothing, but
/// Hermit cannot tell, before the call runs, that it will fail, so `F` counts
/// as rebound and a host replacement of it is not named.
pub fn failed_rename_guest(python: &Path) -> String {
    format!(
        r#"
    '{}' -c 'import os, sys
d = sys.argv[1]
open(d + "/F").read()
open(d + "/fifo").readline()
try:
    os.rename(d + "/missing", d + "/F")
except FileNotFoundError:
    pass
open(d + "/F").read()
print(os.stat(d + "/F").st_ino)' "$1"
"#,
        python.display()
    )
}

/// The Python interpreter itself, not a launcher that might use CLONE_VFORK.
pub fn python_interpreter() -> std::path::PathBuf {
    let output = Command::new("python3")
        .args(["-c", "import sys; print(sys.executable)"])
        .output()
        .expect("run python3");
    assert!(output.status.success(), "python3 failed: {output:?}");
    std::path::PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
}

/// Open `fifo` for writing once a reader has it open, or panic at `deadline`.
fn open_fifo_writer(fifo: &Path, deadline: std::time::Instant) -> std::fs::File {
    use std::os::unix::fs::OpenOptionsExt;
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(fifo)
        {
            Ok(file) => return file,
            Err(error) if error.raw_os_error() == Some(libc::ENXIO) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the guest never opened {} for reading",
                    fifo.display()
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(error) => panic!("cannot open {}: {error}", fifo.display()),
        }
    }
}

/// Run `guest` ([`HOST_INPUT_GUEST`] or [`SELF_REPLACING_GUEST`]) under
/// `hermit` with `global_args` (such as `--backend=sabre`) and `envs`, as
/// `run --strict --verify --verify-strict`. The host sends `lines[0]` to run
/// 1 and `lines[1]` to run 2; before sending to run 1, it replaces `F` with
/// an identical copy when `replace_in_run1` is set. Returns the input
/// directory, stderr, and the typed report.
/// [`verify_across_host_action`]'s setup for a run that hermit ends during run
/// 1: run 1 gets `line`, and run 2 must never start. Returns hermit's exit
/// status and stderr; fails fast if run 2 starts.
pub fn verify_ended_in_run_1(
    hermit: &Path,
    name: &str,
    guest: &str,
    line: &str,
) -> (Option<i32>, String) {
    use std::io::Write;

    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create test directory");
    std::fs::write(root.join("F"), "input\n").expect("write F");
    std::fs::write(root.join("A"), "input\n").expect("write A");
    std::fs::write(root.join("B"), "input\n").expect("write B");
    let fifo = root.join("fifo");
    nix::unistd::mkfifo(
        &fifo,
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .expect("mkfifo");
    let stderr_path = root.join("stderr");
    let stderr_file = std::fs::File::create(&stderr_path).expect("create stderr file");
    let mut child = Command::new("timeout")
        .args(["--kill-after", "5s", "180s"])
        .arg(hermit)
        .arg("run")
        .args(["--verify", "--verify-strict", "--verify-json"])
        .arg(root.join("verify.json"))
        .args([
            "--base-env=minimal",
            "--mount=type=tmpfs,target=/test",
            "--workdir=/test",
            "--",
            "/bin/sh",
            "-c",
            guest,
            "sh",
        ])
        .arg(&root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(stderr_file)
        .spawn()
        .expect("failed to start hermit");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(170);
    let mut writer = open_fifo_writer(&fifo, deadline);
    writeln!(writer, "{line}").expect("send run 1 its line");
    drop(writer);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll hermit") {
            break status;
        }
        let stderr = std::fs::read_to_string(&stderr_path).unwrap_or_default();
        if stderr.contains("Run2") {
            let _ = child.kill();
            let _ = child.wait();
            panic!("run 2 started after run 1 should have ended the run:\n{stderr}");
        }
        assert!(std::time::Instant::now() < deadline, "hermit did not exit");
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let stderr = std::fs::read_to_string(&stderr_path).unwrap_or_default();
    (status.code(), stderr)
}

pub fn verify_across_host_action(
    hermit: &Path,
    global_args: &[&str],
    envs: &[(&str, &Path)],
    name: &str,
    guest: &str,
    replace_in_run1: bool,
    lines: [&str; 2],
) -> (
    std::path::PathBuf,
    String,
    hermit::canonical_verdict::VerificationReport,
) {
    verify_across_host_action_with(
        hermit,
        global_args,
        &["--strict"],
        envs,
        name,
        guest,
        replace_in_run1,
        lines,
    )
}

/// [`verify_across_host_action`] with `run_args` in place of `--strict`, for
/// a run mode that `--strict` refuses (such as `--passthru-opt`).
#[allow(clippy::too_many_arguments)]
pub fn verify_across_host_action_with(
    hermit: &Path,
    global_args: &[&str],
    run_args: &[&str],
    envs: &[(&str, &Path)],
    name: &str,
    guest: &str,
    replace_in_run1: bool,
    lines: [&str; 2],
) -> (
    std::path::PathBuf,
    String,
    hermit::canonical_verdict::VerificationReport,
) {
    use std::io::Write;

    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create test directory");
    std::fs::write(root.join("F"), "input\n").expect("write F");
    std::fs::write(root.join("A"), "input\n").expect("write A");
    std::fs::write(root.join("B"), "input\n").expect("write B");
    let fifo = root.join("fifo");
    nix::unistd::mkfifo(
        &fifo,
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .expect("mkfifo");
    let report_path = root.join("verify.json");
    let stderr_path = root.join("stderr");
    let stderr_file = std::fs::File::create(&stderr_path).expect("create stderr file");
    let child = Command::new("timeout")
        .args(["--kill-after", "5s", "180s"])
        .arg(hermit)
        .args(global_args)
        .arg("run")
        .args(run_args)
        .args(["--verify", "--verify-strict", "--verify-json"])
        .arg(&report_path)
        .args([
            "--base-env=minimal",
            "--mount=type=tmpfs,target=/test",
            "--workdir=/test",
            "--",
            "/bin/sh",
            "-c",
            guest,
            "sh",
        ])
        .arg(&root)
        .envs(envs.iter().copied())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(stderr_file)
        .spawn()
        .expect("failed to start hermit");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(170);

    // Run 1: once its guest waits on the FIFO it has opened F once.
    let mut writer = open_fifo_writer(&fifo, deadline);
    if replace_in_run1 {
        let copy = root.join("F.new");
        std::fs::copy(root.join("F"), &copy).expect("copy F");
        std::fs::rename(&copy, root.join("F")).expect("replace F");
    }
    writeln!(writer, "{}", lines[0]).expect("send run 1 its line");
    drop(writer);

    // Run 2 starts only after run 1 has finished with the FIFO.
    while !std::fs::read_to_string(&stderr_path)
        .unwrap_or_default()
        .contains("Run2")
    {
        assert!(std::time::Instant::now() < deadline, "run 2 never started");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let mut writer = open_fifo_writer(&fifo, deadline);
    writeln!(writer, "{}", lines[1]).expect("send run 2 its line");
    drop(writer);

    let output = child.wait_with_output().expect("wait for hermit");
    let stderr = std::fs::read_to_string(&stderr_path).unwrap_or_default();
    assert_eq!(
        output.status.code(),
        Some(hermit::HERMIT_VERIFICATION_DIVERGENCE_EXIT),
        "a divergence exits with HERMIT_VERIFICATION_DIVERGENCE_EXIT\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    let report = hermit::canonical_verdict::VerificationReport::from_current_json_slice(
        &std::fs::read(&report_path).expect("read verify json"),
    )
    .expect("parse verify json");
    (root, stderr, report)
}
