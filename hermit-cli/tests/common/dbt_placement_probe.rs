/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Test-only startup observation inside the genuine, attested frontend.

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

// Library and mapping self-exec tests use this handoff; CLI tests pass --env directly.
#[allow(dead_code)]
pub(crate) const OBSERVER_ENV: &str = "HERMIT_TEST_DBT_PLACEMENT_OBSERVER";

pub(crate) struct Probe {
    _directory: tempfile::TempDir,
    root: PathBuf,
    install: PathBuf,
    record: PathBuf,
    guest_bind: Option<PathBuf>,
    real_library: PathBuf,
    staged_library: PathBuf,
    observer: PathBuf,
}

fn c_bytes(path: &Path) -> String {
    let mut bytes = path.as_os_str().as_bytes().to_vec();
    bytes.push(0);
    bytes
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn same_file(left: &Path, right: &Path) {
    let left = fs::metadata(left).expect("real SDK library metadata");
    let right = fs::metadata(right).expect("staged SDK library metadata");
    assert_eq!((left.dev(), left.ino()), (right.dev(), right.ino()));
}

impl Probe {
    pub(crate) fn new(contained_cli: bool) -> Self {
        let (drrun, client) =
            detcore_dbt::prepare_native_client().expect("real DBT launch artifacts");
        let refused = reverie_dbt::DbtRunner::new("/bin/true", &client)
            .expect_err("an unrelated frontend must remain refused");
        assert_eq!(refused.kind(), std::io::ErrorKind::Unsupported);
        assert!(
            refused
                .to_string()
                .contains("incompatible DynamoRIO frontend")
        );
        let runtime = detcore_dbt::runtime_library_path().expect("real Detcore DBT runtime");
        let genuine_drrun = fs::canonicalize(drrun).unwrap();
        let sdk = genuine_drrun
            .parent()
            .and_then(Path::parent)
            .expect("real SDK root");
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../target"));
        let directory = tempfile::Builder::new()
            .prefix("dbt-placement-")
            .tempdir_in(target)
            .expect("owned placement probe directory");
        let root = fs::canonicalize(directory.path()).unwrap();
        assert!(
            !root.starts_with("/tmp"),
            "the frontend must remain visible outside the CLI's private /tmp"
        );
        let install = root.join("install");
        let resources = install.join("rsrcs");
        fs::create_dir_all(&resources).unwrap();
        for (name, path) in [
            ("libreverie_dbt_client.so", &client),
            ("libdetcore_dbt.so", &runtime),
        ] {
            symlink(fs::canonicalize(path).unwrap(), resources.join(name)).unwrap();
        }
        let staged_sdk = resources.join("dynamorio");
        fs::create_dir_all(staged_sdk.join("bin64")).unwrap();
        for entry in fs::read_dir(sdk).expect("real staged SDK") {
            let entry = entry.unwrap();
            if entry.file_name() != "bin64" {
                symlink(
                    fs::canonicalize(entry.path()).unwrap(),
                    staged_sdk.join(entry.file_name()),
                )
                .unwrap();
            }
        }
        let real_library = fs::canonicalize(sdk.join("lib64/release/libdynamorio.so")).unwrap();
        let staged_library = staged_sdk.join("lib64/release/libdynamorio.so");
        same_file(&real_library, &staged_library);
        let staged_frontend = staged_sdk.join("bin64/drrun");
        fs::copy(&genuine_drrun, &staged_frontend).unwrap();
        assert_eq!(
            fs::read(&staged_frontend).unwrap(),
            fs::read(&genuine_drrun).unwrap(),
            "the selected frontend must retain its attested bytes"
        );
        let frontend = fs::metadata(&staged_frontend).unwrap();
        let record = root.join("argv-record");
        fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&record)
            .unwrap();
        let guest_bind = contained_cli.then(|| Path::new("/tmp").join(root.file_name().unwrap()));
        let visible_record = guest_bind
            .as_ref()
            .map_or_else(|| record.clone(), |p| p.join("argv-record"));
        let observer_file = root.join("observer.so");
        let observer = guest_bind
            .as_ref()
            .map_or_else(|| observer_file.clone(), |p| p.join("observer.so"));
        let source = root.join("observer.c");
        fs::write(
            root.join("artifacts.txt"),
            format!(
                "drrun={}\nclient={}\nruntime={}\nfrontend-device={}\nfrontend-inode={}\n",
                genuine_drrun.display(),
                fs::canonicalize(&client).unwrap().display(),
                fs::canonicalize(&runtime).unwrap().display(),
                frontend.dev(),
                frontend.ino(),
            ),
        )
        .unwrap();
        fs::write(
            &source,
            format!(
                "static const char observer[] = {{{}}};\nstatic const char record[] = {{{}}};\nstatic const unsigned long long frontend_device = {};\nstatic const unsigned long long frontend_inode = {};\n{}",
                c_bytes(&observer),
                c_bytes(&visible_record),
                frontend.dev(),
                frontend.ino(),
                OBSERVER,
            ),
        )
        .unwrap();
        let output = Command::new("cc")
            .args(["-O2", "-Wall", "-Wextra", "-Werror", "-fPIC", "-shared"])
            .arg(&source)
            .arg("-o")
            .arg(&observer_file)
            .output()
            .expect("compile frontend startup observer");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        eprintln!("DBT placement observation: {}", record.display());
        Self {
            _directory: directory,
            root,
            install,
            record,
            guest_bind,
            real_library,
            staged_library,
            observer,
        }
    }

    pub(crate) fn launch(&self) -> (&Path, Option<String>) {
        (
            &self.install,
            self.guest_bind
                .as_ref()
                .map(|bind| format!("--bind={}:{}", self.root.display(), bind.display())),
        )
    }

    pub(crate) fn observer(&self) -> &Path {
        &self.observer
    }

    pub(crate) fn assert_zero(&self, launches: usize, protected: bool) {
        // Normal tests remove the entire stage, including on assertion failure.
        // Evidence runs may retain only these bounded, credential-redacted files.
        if let Some(destination) = std::env::var_os("HERMIT_DBT_PLACEMENT_OBSERVATION_DIR") {
            let destination = PathBuf::from(destination);
            assert!(
                destination.is_absolute() && destination.is_dir(),
                "explicit evidence directory"
            );
            let destination = destination.join(self.root.file_name().unwrap());
            fs::create_dir(&destination).expect("fresh evidence directory");
            for (name, limit) in [
                ("argv-record", 1_048_576),
                ("observer.c", 65_536),
                ("artifacts.txt", 16_384),
            ] {
                let path = self.root.join(name);
                assert!(
                    fs::metadata(&path).unwrap().len() <= limit,
                    "bounded observation evidence"
                );
                let bytes = fs::read(path).unwrap();
                assert!(bytes.len() as u64 <= limit, "bounded observation evidence");
                fs::write(destination.join(name), bytes).unwrap();
            }
        }
        same_file(&self.real_library, &self.staged_library);
        let text = fs::read_to_string(&self.record).expect("read complete launch observations");
        let mut records = Vec::new();
        let mut current = None;
        let mut incoming = None;
        let mut identity = None;
        for line in text.lines() {
            match line {
                "BEGIN" => {
                    assert!(current.is_none(), "interleaved launch records");
                    current = Some(Vec::<Vec<u8>>::new());
                }
                "END" => records.push((
                    incoming.take().expect("incoming argv0"),
                    identity.take().expect("actual frontend identity"),
                    current.take().expect("record start"),
                )),
                "TOKEN64" | "SOCKET32" => {
                    current
                        .as_mut()
                        .expect("record start")
                        .push(line.as_bytes().to_vec());
                }
                _ => {
                    if let Some(value) = line.strip_prefix("IDENTITY ") {
                        let (device, inode) = value.split_once(' ').expect("device and inode");
                        assert!(current.as_ref().expect("record start").is_empty());
                        assert!(
                            identity
                                .replace((
                                    device.parse::<u64>().unwrap(),
                                    inode.parse::<u64>().unwrap()
                                ))
                                .is_none(),
                            "one frontend identity"
                        );
                        continue;
                    }
                    let is_incoming = line.starts_with('I');
                    let hex = line
                        .strip_prefix(if is_incoming { 'I' } else { 'A' })
                        .expect("known record field");
                    assert_eq!(hex.len() % 2, 0, "complete argument encoding");
                    let bytes = hex
                        .as_bytes()
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|pair| {
                            u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap()
                        })
                        .collect();
                    if is_incoming {
                        assert!(
                            current.as_ref().expect("record start").is_empty(),
                            "incoming argv0 precedes executed argv"
                        );
                        assert!(incoming.replace(bytes).is_none(), "one incoming argv0");
                    } else {
                        current.as_mut().expect("record start").push(bytes);
                    }
                }
            }
        }
        assert!(current.is_none(), "truncated launch observation");
        assert_eq!(
            records.len(),
            launches,
            "exact real launch observation count"
        );
        let frontend = fs::metadata(self.install.join("rsrcs/dynamorio/bin64/drrun")).unwrap();
        for (incoming, identity, args) in records {
            assert_eq!(identity, (frontend.dev(), frontend.ino()));
            assert_eq!(
                incoming,
                self.install
                    .join("rsrcs/dynamorio/bin64/drrun")
                    .as_os_str()
                    .as_bytes(),
                "actual frontend argv0"
            );
            assert_eq!(args.first(), Some(&incoming), "unchanged frontend argv0");
            let client = args
                .iter()
                .position(|a| a == b"-c")
                .expect("client boundary");
            let guest = args
                .iter()
                .enumerate()
                .skip(client + 2)
                .find(|(_, a)| *a == b"--")
                .map(|(i, _)| i)
                .expect("guest boundary");
            let placement = args[..client]
                .windows(2)
                .filter(|pair| pair[0] == b"-vm_max_offset")
                .collect::<Vec<_>>();
            assert_eq!(
                placement.len(),
                1,
                "Hermit must select exactly one zero offset"
            );
            assert_eq!(
                placement[0][1], b"0x0",
                "Hermit must select explicit zero offset"
            );
            let controls = &args[client + 2..guest];
            for (flag, value) in [
                (b"-evidence-token".as_slice(), b"TOKEN64".as_slice()),
                (b"-evidence-socket".as_slice(), b"SOCKET32".as_slice()),
            ] {
                assert_eq!(
                    controls.iter().filter(|a| a.as_slice() == flag).count(),
                    usize::from(protected),
                    "exact client-region protected control count",
                );
                assert_eq!(
                    controls
                        .windows(2)
                        .filter(|p| p[0] == flag && p[1] == value)
                        .count(),
                    usize::from(protected),
                    "validated client-region protected control value",
                );
            }
        }
    }
}

// No argv or environment is printed on failure. Credentials are recognized
// only inside the real client region and replaced before any record is written.
const OBSERVER: &str = r#"
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/stat.h>
#include <unistd.h>

static void fail(void) {
    static const char message[] = "DBT placement observation failed\n";
    if (write(2, message, sizeof(message) - 1) < 0) { /* Preserve the original failure. */ }
    _exit(91);
}
static void hex_arg(FILE *file, char kind, const char *value) {
    if (fputc(kind, file) == EOF) fail();
    for (const unsigned char *p = (const unsigned char *)value; *p; ++p)
        if (fprintf(file, "%02x", *p) < 0) fail();
    if (fputc('\n', file) == EOF) fail();
}
static void opaque(FILE *file, const char *value, size_t length, const char *tag) {
    if (strlen(value) != length) fail();
    for (size_t i = 0; i < length; ++i)
        if (!((value[i] >= '0' && value[i] <= '9') ||
              (value[i] >= 'a' && value[i] <= 'f'))) fail();
    if (fprintf(file, "%s\n", tag) < 0) fail();
}
/* Exported only so the additive guest control can detect accidental preload. */
int hermit_dbt_placement_observer_marker = 1;

__attribute__((constructor))
static void observe_frontend(int argc, char **argv, char **envp) {
    (void)envp;
    struct stat actual;
    const char *preload = getenv("LD_PRELOAD");
    if (stat("/proc/self/exe", &actual) != 0 ||
        (unsigned long long)actual.st_dev != frontend_device ||
        (unsigned long long)actual.st_ino != frontend_inode ||
        preload == NULL || strcmp(preload, observer) != 0 ||
        unsetenv("LD_PRELOAD") != 0)
        fail();
    int client = -1, guest = -1;
    for (int i = 1; i < argc; ++i) {
        if (client < 0 && strcmp(argv[i], "-c") == 0) client = i;
        else if (client >= 0 && i >= client + 2 && strcmp(argv[i], "--") == 0) {
            guest = i;
            break;
        }
    }
    if (client < 0 || guest < 0) fail();
    int fd = open(record, O_WRONLY | O_APPEND | O_CLOEXEC | O_NOFOLLOW);
    if (fd < 0 || flock(fd, LOCK_EX) != 0) fail();
    FILE *file = fdopen(fd, "a");
    if (!file || fputs("BEGIN\n", file) == EOF) fail();
    if (fprintf(file, "IDENTITY %llu %llu\n",
                (unsigned long long)actual.st_dev,
                (unsigned long long)actual.st_ino) < 0) fail();
    hex_arg(file, 'I', argv[0]);
    hex_arg(file, 'A', argv[0]);
    for (int i = 1; i < argc; ++i) {
        hex_arg(file, 'A', argv[i]);
        if (i >= client + 2 && i < guest &&
            (strcmp(argv[i], "-evidence-token") == 0 ||
             strcmp(argv[i], "-evidence-socket") == 0)) {
            int token = strcmp(argv[i], "-evidence-token") == 0;
            if (++i >= guest) fail();
            opaque(file, argv[i], token ? 64 : 32, token ? "TOKEN64" : "SOCKET32");
        }
    }
    if (fputs("END\n", file) == EOF || fclose(file) != 0) fail();
}
"#;
