// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the LICENSE file.

//! `hermit verify-inputs`, the internal native-exec pairing seam: a pair of
//! `hermit run --verify-run` processes started under it adopts one sealed
//! input set, and each run reads the frozen stdin from its start.

use std::fs;
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
    Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(["verify-inputs", "--manifest-fd", MANIFEST_FD, "--input"])
        .arg(format!("data:{}", data.display()))
        .args(["--", "bash", "-c", script])
        .env("HERMIT_EPOCH", EPOCH)
        .stdin(fs::File::open(stdin).unwrap())
        .output()
        .expect("failed to run hermit verify-inputs")
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
    for (label, tamper, expected) in [
        // Hermit's own startup may reuse the freed number before adoption, so
        // the refusal names the descriptor whatever now holds it.
        ("lost", "eval \"exec $fd<&-\"", "is not a sealed object"),
        (
            "substituted",
            // The same bytes from an unsealed file are not the sealed object.
            &format!("eval \"exec $fd<{}\"", other.display()) as &str,
            "is not a sealed object",
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
            "{label}: {}",
            stderr(&output)
        );
        assert!(
            !run.join("verify-run.json").exists(),
            "{label} must not start"
        );
    }
}
