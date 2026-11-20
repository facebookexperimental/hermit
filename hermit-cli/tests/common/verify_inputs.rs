// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the LICENSE file.

//! `hermit verify-inputs`, the internal native-exec pairing seam: a pair of
//! `hermit run --verify-run` processes started under it adopts one sealed
//! input set, and each run reads the frozen stdin from its start.

use std::ffi::CString;
use std::fs;
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::Command;
use std::process::Output;

use hermit::HERMIT_INTERNAL_FAILURE_EXIT;

use super::append_hermit_args;
use super::hermit_run_guard;
use super::stderr;

const EPOCH: &str = "2026-01-01T00:00:00Z";
const MANIFEST_FD: &str = "9";

/// The words of one `hermit run --verify-run DIR --native-exec-shared-inputs`
/// of `guest`, with the execution root this test process must give a guest.
fn run_words(directory: &Path, guest: &[&str]) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .args(["run", "--strict", "--verify-run"])
        .arg(directory)
        .arg(format!("--native-exec-shared-inputs={MANIFEST_FD}"));
    let mut rest = vec!["--"];
    rest.extend_from_slice(guest);
    append_hermit_args(&mut command, &rest);
    std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|word| shell_words::quote(word.to_str().unwrap()).into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Run `script` with bash under `hermit verify-inputs`, sealing `data` as one
/// input and feeding `stdin` as its stdin.
fn under_verify_inputs(data: &Path, stdin: &Path, script: &str) -> Output {
    // A fresh stdin per attempt, should a typed skid refusal run it again.
    super::hermit_test::hermit_output_with("hermit verify-inputs", || {
        let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
            .args(["verify-inputs", "--manifest-fd", MANIFEST_FD, "--input"])
            .arg(format!("data:{}", data.display()))
            .args(["--", "bash", "-c", script])
            .env("HERMIT_EPOCH", EPOCH)
            .stdin(fs::File::open(stdin).unwrap())
            .output()
            .expect("failed to run hermit verify-inputs");
        (output, ())
    })
    .0
}

#[test]
fn a_pair_under_verify_inputs_adopts_one_sealed_input_set() {
    let _guard = hermit_run_guard();
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    fs::write(&data, "original\n").unwrap();
    let stdin = dir.path().join("stdin");
    fs::write(&stdin, "hello\n").unwrap();
    // The guest also lists what Hermit, its PID 1, holds of the shared set:
    // those entries must be the same in both runs.
    let guest = [
        "/bin/sh",
        "-c",
        "/usr/bin/head -c 64; /usr/bin/ls /proc/self/fd; /usr/bin/readlink /proc/1/fd/9 /proc/1/fd/3",
    ];
    let [run1, run2] = [dir.path().join("run1"), dir.path().join("run2")];
    // The original file changes after sealing and before either run; each run
    // gets its own open file description of the frozen stdin.
    let script = format!(
        "set -e\necho changed > {data}\nprintf %s \"$(</proc/self/fd/{MANIFEST_FD})\" > {manifest}\n\
         {first} </proc/self/fd/0\n{second} </proc/self/fd/0\n",
        data = shell_words::quote(data.to_str().unwrap()),
        manifest = shell_words::quote(dir.path().join("manifest.json").to_str().unwrap()),
        first = run_words(&run1, &guest),
        second = run_words(&run2, &guest),
    );
    let output = under_verify_inputs(&data, &stdin, &script);
    assert!(output.status.success(), "{}", stderr(&output));

    // The manifest names the bytes as they were sealed, and each run adopted
    // the object only after checking its digest against the manifest.
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.path().join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["schema"], 1);
    assert_eq!(manifest["producer"], "hermit-sealed-files");
    let object = &manifest["objects"][0];
    assert_eq!(object["role"], "data");
    assert_eq!(object["size"], 9);
    assert_eq!(object["sha256"], format!("{:x}", sha256_hex(b"original\n")));
    assert_eq!(fs::read_to_string(&data).unwrap(), "changed\n");

    // Both guests read the whole stdin and hold no shared descriptor, and
    // Hermit holds the shared set at the same descriptors in both runs. (Its
    // other descriptors already differ between any two runs: random /tmp
    // names and a pidfd inode, https://github.com/rrnewton/hermit/issues/1067.)
    // The guest reads only those two links: reading the others would itself
    // make the runs diverge.
    assert_eq!(object["fd"], 3, "the guest command reads descriptor 3");
    for run in [&run1, &run2] {
        assert_eq!(
            fs::read_to_string(run.join("stdout")).unwrap(),
            "hello\n0\n1\n2\n3\n/memfd:hermit-shared-inputs-manifest (deleted)\n\
             /memfd:hermit-shared-input (deleted)\n",
            "{}",
            run.display()
        );
    }
    let report = dir.path().join("report.json");
    let compared = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(["log-diff", "--verify-json"])
        .arg(&report)
        .arg(&run1)
        .arg(&run2)
        .output()
        .unwrap();
    assert!(compared.status.success(), "{}", stderr(&compared));
}

/// The sha256 of `bytes`, as the manifest writes it.
fn sha256_hex(bytes: &[u8]) -> impl std::fmt::LowerHex {
    struct Hex([u8; 32]);
    impl std::fmt::LowerHex for Hex {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
        }
    }
    use sha2::Digest;
    Hex(sha2::Sha256::digest(bytes).into())
}

/// A `--verify-run` under a holder that sealed the host's /etc/ld.so.cache
/// binds that copy (https://github.com/rrnewton/hermit/issues/4693): the run
/// succeeds, its guest reads the sealed bytes, and a run that did not adopt
/// the role would refuse to set up its container.
#[test]
fn a_verify_run_binds_the_ld_so_cache_its_holder_sealed() {
    let Ok(host) = fs::read("/etc/ld.so.cache") else {
        return;
    };
    let _guard = hermit_run_guard();
    let dir = tempfile::tempdir().unwrap();
    let run = dir.path().join("run");
    let guest = ["/usr/bin/wc", "-c", "/etc/ld.so.cache"];
    let script = format!("set -e\n{} </dev/null\n", run_words(&run, &guest));
    let output = super::hermit_test::hermit_output_with("hermit verify-inputs", || {
        let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
            .args(["verify-inputs", "--manifest-fd", MANIFEST_FD])
            .args(["--input", "ld-so-cache:/etc/ld.so.cache"])
            .args(["bash", "-c", &script])
            .env("HERMIT_EPOCH", EPOCH)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("failed to run hermit verify-inputs");
        (output, ())
    })
    .0;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        fs::read_to_string(run.join("stdout")).unwrap(),
        format!("{} /etc/ld.so.cache\n", host.len())
    );
}

/// The seals this kernel gives a sealed shared object: the memfd that
/// `hermit verify-inputs` makes with MFD_ALLOW_SEALING, after it adds the four
/// seals every object must carry. Where vm.memfd_noexec is 1 or more, the
/// kernel also seals against exec every new memfd made with neither MFD_EXEC
/// nor MFD_NOEXEC_SEAL (F_SEAL_EXEC, mm/memfd.c `memfd_create`), so the object
/// reads 0x2f rather than 0xf.
fn sealed_object_seals() -> u32 {
    let required = libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
    // SAFETY: memfd_create takes a NUL-terminated name and returns a new
    // descriptor or -1.
    let raw = unsafe {
        libc::memfd_create(
            c"verify-inputs-test-seals".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    assert!(
        raw >= 0,
        "memfd_create: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: `raw` is the new descriptor, owned by nothing else.
    let memfd = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: fcntl on a descriptor this function owns.
    let added = unsafe { libc::fcntl(memfd.as_raw_fd(), libc::F_ADD_SEALS, required) };
    assert_eq!(added, 0, "F_ADD_SEALS: {}", std::io::Error::last_os_error());
    // SAFETY: fcntl only queries the descriptor.
    let seals = unsafe { libc::fcntl(memfd.as_raw_fd(), libc::F_GET_SEALS) };
    assert!(
        seals >= 0,
        "F_GET_SEALS: {}",
        std::io::Error::last_os_error()
    );
    let noexec = fs::read_to_string("/proc/sys/vm/memfd_noexec").map_or(0, |value| {
        value
            .trim()
            .parse::<u32>()
            .expect("vm.memfd_noexec is a number")
    });
    let exec = if noexec >= 1 { libc::F_SEAL_EXEC } else { 0 };
    assert_eq!(
        seals,
        required | exec,
        "a sealed memfd's seals with vm.memfd_noexec = {noexec}"
    );
    seals as u32
}

/// How a run refuses an unsealed regular file at `path` in place of a sealed
/// object. Outside tmpfs, Linux answers F_GET_SEALS on it with EINVAL
/// (mm/memfd.c `memfd_get_seals`). A tmpfs inode that memfd_create did not
/// make starts with F_SEAL_SEAL and can take no other seal (mm/shmem.c
/// `__shmem_get_inode`), so there F_GET_SEALS reports exactly 0x1 and the run
/// refuses those seals, naming the seals the object carries, instead.
fn substituted_refusal(path: &Path) -> String {
    let name = CString::new(path.as_os_str().as_bytes()).unwrap();
    let mut filesystem = MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `name` is NUL-terminated and statfs fills `filesystem` before
    // it is read.
    let status = unsafe { libc::statfs(name.as_ptr(), filesystem.as_mut_ptr()) };
    assert_eq!(
        status,
        0,
        "statfs {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
    // SAFETY: statfs returned 0, so it filled `filesystem`.
    if unsafe { filesystem.assume_init() }.f_type == libc::TMPFS_MAGIC {
        format!("has seals 0x1, not the sealed {:#x}", sealed_object_seals())
    } else {
        "is not a sealed object".to_owned()
    }
}

#[test]
fn a_run_refuses_lost_substituted_or_unsealed_shared_inputs() {
    let _guard = hermit_run_guard();
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    fs::write(&data, "original\n").unwrap();
    let other = dir.path().join("other");
    fs::write(&other, "original\n").unwrap();
    let stdin = dir.path().join("stdin");
    fs::write(&stdin, "").unwrap();
    // The object's descriptor, as the manifest records it.
    let object_fd = format!(
        "fd=$(grep -o '\"fd\":[0-9]*' /proc/self/fd/{MANIFEST_FD} | head -1 | cut -d: -f2)"
    );
    let substituted = substituted_refusal(&other);
    for (label, tamper, expected) in [
        // Hermit's own startup may reuse the freed number before adoption, so
        // the refusal names the descriptor whatever now holds it.
        ("lost", "eval \"exec $fd<&-\"", "is not a sealed object"),
        (
            "substituted",
            // The same bytes from an unsealed file are not the sealed object.
            &format!("eval \"exec $fd<{}\"", other.display()) as &str,
            substituted.as_str(),
        ),
        (
            "unsealed manifest",
            &format!("exec {MANIFEST_FD}<{}", other.display()) as &str,
            "is not sealed",
        ),
    ] {
        let run = dir.path().join(label.replace(' ', "-"));
        let script = format!(
            "{object_fd}\n{tamper}\n{run} </dev/null\n",
            run = run_words(&run, &["/bin/sh", "-c", ":"])
        );
        let output = under_verify_inputs(&data, &stdin, &script);
        assert_eq!(
            output.status.code(),
            Some(HERMIT_INTERNAL_FAILURE_EXIT),
            "{label}: {}",
            stderr(&output)
        );
        assert!(
            stderr(&output).contains(expected),
            "{label}: expected {expected:?}: {}",
            stderr(&output)
        );
        assert!(
            !run.join("verify-run.json").exists(),
            "{label} must not start"
        );
    }
}
