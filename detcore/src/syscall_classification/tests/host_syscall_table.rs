/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Where `every_host_kernel_syscall_number_is_accounted_for` reads the
//! running kernel's syscall numbers. It uses only `std`, so `rustc --test`
//! can check this file on its own.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::Path;

/// Set in every container of the validation's pinned root by
/// `ci/hermetic/run-in-pinned-root.sh`, bound or not, to the running
/// kernel's generated uapi `asm` directory. It means only "never fall back
/// to another header": the directory itself is always derived from the
/// test's own `uname -r`, and a value naming anything else is refused.
pub(crate) const PINNED_ROOT_UAPI_ASM_VAR: &str = "HERMIT_RUNNING_KERNEL_UAPI_ASM";

/// The running kernel's generated uapi `asm` directory, from its release.
fn uapi_asm_dir(release: &str) -> String {
    format!("/lib/modules/{release}/build/arch/x86/include/generated/uapi/asm")
}

/// The headers to read, in order, or a refusal naming why none may be read.
///
/// `pinned_root_uapi_asm` is the raw value of [`PINNED_ROOT_UAPI_ASM_VAR`].
/// Set, the only candidate is the running kernel's own `unistd_64.h`,
/// derived from `release`: any header the image carries is a table baked
/// into the image, not the running kernel's, so an absent bind must fail
/// rather than fall back to it. The value must be exactly that directory; a
/// different path, a value that is not UTF-8, or a release too empty to
/// derive the directory from is refused by name, never read and never taken
/// for unset. Unset: the running kernel's build tree (when the release is
/// known), then the installed headers.
pub(crate) fn candidates(
    release: &str,
    pinned_root_uapi_asm: Option<&OsStr>,
) -> Result<Vec<String>, String> {
    let Some(value) = pinned_root_uapi_asm else {
        let mut paths = Vec::new();
        if !release.is_empty() {
            paths.push(format!("{}/unistd_64.h", uapi_asm_dir(release)));
            paths.push(format!(
                "/lib/modules/{release}/build/include/uapi/asm-generic/unistd.h"
            ));
        }
        paths.push("/usr/include/asm/unistd_64.h".to_owned());
        paths.push("/usr/include/x86_64-linux-gnu/asm/unistd_64.h".to_owned());
        return Ok(paths);
    };
    if release.is_empty() {
        return Err(format!(
            "{PINNED_ROOT_UAPI_ASM_VAR} is {value:?}, but `uname -r` gave no release to \
             derive the running kernel's header directory from"
        ));
    }
    let expected = uapi_asm_dir(release);
    match value.to_str() {
        Some(dir) if dir == expected => Ok(vec![format!("{expected}/unistd_64.h")]),
        Some(_) => Err(format!(
            "{PINNED_ROOT_UAPI_ASM_VAR} is {value:?}, not the running kernel's header \
             directory {expected:?}; only that exact path is accepted"
        )),
        None => Err(format!(
            "{PINNED_ROOT_UAPI_ASM_VAR} is not UTF-8 ({value:?}); the running kernel's \
             header directory is {expected:?}"
        )),
    }
}

/// The first candidate, read under `root` (`/` outside this file's tests),
/// that defines at least one syscall number: its path and its numbers.
pub(crate) fn read_first(
    root: &Path,
    candidates: &[String],
) -> Option<(String, BTreeMap<i32, String>)> {
    candidates.iter().find_map(|candidate| {
        let text = std::fs::read_to_string(root.join(candidate.trim_start_matches('/'))).ok()?;
        let numbers = parse_nr_defines(&text);
        (!numbers.is_empty()).then(|| (candidate.clone(), numbers))
    })
}

/// The `#define __NR_<name> <decimal>` lines of a syscall header.
fn parse_nr_defines(text: &str) -> BTreeMap<i32, String> {
    let mut numbers = BTreeMap::new();
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("#define __NR_") else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let (Some(name), Some(value)) = (fields.next(), fields.next()) else {
            continue;
        };
        // Only plain decimal definitions name a wired number; aliases
        // (`__NR_x __NR_y`) and offset expressions belong to another ABI's
        // arithmetic. `__NR_syscalls` is the table's size, not a syscall.
        if name == "syscalls" {
            continue;
        }
        if let Ok(nr) = value.parse::<i32>() {
            numbers.entry(nr).or_insert_with(|| name.to_owned());
        }
    }
    numbers
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;
    use std::process::Command;
    use std::process::Stdio;

    use super::*;

    const RELEASE: &str = "0.0.0-fixture";
    const GUEST_ASM: &str = "/lib/modules/0.0.0-fixture/build/arch/x86/include/generated/uapi/asm";

    /// A private directory standing in for a container's root.
    struct Root {
        dir: PathBuf,
        files: Vec<PathBuf>,
    }

    impl Root {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "hermit-host-syscall-table-{name}-{}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Root {
                dir,
                files: Vec::new(),
            }
        }

        fn write(&mut self, path: &str, text: &str) {
            let path = self.dir.join(path.trim_start_matches('/'));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            self.files.push(path);
        }
    }

    impl Drop for Root {
        /// Removes only what `write` created, then the directories it left
        /// empty: detcore's clippy.toml bars `remove_dir_all`, which walks
        /// the tree with fdopendir.
        fn drop(&mut self) {
            for file in &self.files {
                let _ = std::fs::remove_file(file);
                for dir in file.ancestors().skip(1) {
                    if !dir.starts_with(&self.dir) || std::fs::remove_dir(dir).is_err() {
                        break;
                    }
                }
            }
            let _ = std::fs::remove_dir(&self.dir);
        }
    }

    /// The image carries its own header and the wrapper bound nothing (a
    /// host without the running kernel's headers): nothing is read, so the
    /// syscall-table test fails by name instead of checking the image's
    /// table.
    #[test]
    fn an_image_header_never_stands_in_for_an_absent_bind() {
        let mut root = Root::new("absent-bind");
        root.write("/usr/include/asm/unistd_64.h", "#define __NR_read 0\n");
        let pinned = candidates(RELEASE, Some(OsStr::new(GUEST_ASM))).unwrap();
        assert_eq!(
            read_first(&root.dir, &pinned).map(|(source, _)| source),
            None,
            "a header other than the bound one was read in the pinned root"
        );
        assert_eq!(pinned, [format!("{GUEST_ASM}/unistd_64.h")]);
        // Control: without the wrapper's name, the same root's image header
        // is read, which is what the pinned root must never do.
        let (source, _) = read_first(&root.dir, &candidates(RELEASE, None).unwrap()).unwrap();
        assert_eq!(source, "/usr/include/asm/unistd_64.h");
    }

    /// With the bind present, its header is read even where the image also
    /// carries one.
    #[test]
    fn the_bound_header_is_the_only_one_read_in_the_pinned_root() {
        let mut root = Root::new("bound");
        root.write("/usr/include/asm/unistd_64.h", "#define __NR_read 0\n");
        root.write(
            &format!("{GUEST_ASM}/unistd_64.h"),
            "#define __NR_read 0\n#define __NR_write 1\n",
        );
        let pinned = candidates(RELEASE, Some(OsStr::new(GUEST_ASM))).unwrap();
        let (source, numbers) = read_first(&root.dir, &pinned).unwrap();
        assert_eq!(source, format!("{GUEST_ASM}/unistd_64.h"));
        assert_eq!(numbers.len(), 2);
    }

    /// The variable cannot point the test at another directory: a value that
    /// is not the directory derived from the test's own release is refused,
    /// naming the variable, the value and the expected path.
    #[test]
    fn a_value_other_than_the_running_kernels_directory_is_refused() {
        let refused = candidates(RELEASE, Some(OsStr::new("/elsewhere"))).unwrap_err();
        for part in [PINNED_ROOT_UAPI_ASM_VAR, "\"/elsewhere\"", GUEST_ASM] {
            assert!(refused.contains(part), "{part} missing from: {refused}");
        }
        // With no release to derive the directory from, even the fixture's
        // own directory is refused.
        assert!(candidates("", Some(OsStr::new(GUEST_ASM))).is_err());
    }

    /// A value that is not UTF-8 is refused by name, never read as unset
    /// (which would bring back the fallback headers).
    #[test]
    fn a_value_that_is_not_utf8_is_refused_never_taken_for_unset() {
        let value = OsStr::from_bytes(b"/lib/modules/\xff/asm");
        let refused = candidates(RELEASE, Some(value)).unwrap_err();
        for part in [PINNED_ROOT_UAPI_ASM_VAR, "not UTF-8", GUEST_ASM] {
            assert!(refused.contains(part), "{part} missing from: {refused}");
        }
    }

    /// The exact derived directory is accepted, and only it: another
    /// release's directory or the same path with a trailing slash is not.
    #[test]
    fn only_the_exact_derived_directory_is_accepted() {
        assert_eq!(
            candidates(RELEASE, Some(OsStr::new(GUEST_ASM))),
            Ok(vec![format!("{GUEST_ASM}/unistd_64.h")])
        );
        for near_miss in [
            format!("{GUEST_ASM}/"),
            "/lib/modules/9.9.9-other/build/arch/x86/include/generated/uapi/asm".to_owned(),
        ] {
            assert!(
                candidates(RELEASE, Some(OsStr::new(&near_miss))).is_err(),
                "{near_miss} was accepted"
            );
        }
    }

    #[test]
    fn outside_the_pinned_root_the_build_tree_comes_first_then_installed_headers() {
        assert_eq!(
            candidates(RELEASE, None).unwrap(),
            [
                format!("{GUEST_ASM}/unistd_64.h"),
                "/lib/modules/0.0.0-fixture/build/include/uapi/asm-generic/unistd.h".to_owned(),
                "/usr/include/asm/unistd_64.h".to_owned(),
                "/usr/include/x86_64-linux-gnu/asm/unistd_64.h".to_owned(),
            ]
        );
        assert_eq!(
            candidates("", None).unwrap(),
            [
                "/usr/include/asm/unistd_64.h",
                "/usr/include/x86_64-linux-gnu/asm/unistd_64.h"
            ]
        );
    }

    /// The repo's skip channel (hermit-cli/tests/common/test_skip.rs),
    /// mirrored here because detcore cannot depend on that test common:
    /// one `HERMIT-TEST-SKIP: <test>\t<reason>` line on stdout and, when
    /// the runner set HERMIT_TEST_SKIP_FILE, appended to it in one write,
    /// so a skip is never counted as a pass.
    fn record_test_skip(test: &str, reason: &str) {
        use std::io::Write as _;
        println!("HERMIT-TEST-SKIP: {test}\t{reason}");
        let Some(path) = std::env::var_os("HERMIT_TEST_SKIP_FILE") else {
            return;
        };
        let line = format!("HERMIT-TEST-SKIP: {test}\t{reason}\n");
        assert!(
            line.len() < 4096,
            "skip marker for {test} is {} bytes; keep the reason under one PIPE_BUF line",
            line.len()
        );
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("HERMIT_TEST_SKIP_FILE must name the runner's writable skip sidecar");
        let written = file
            .write(line.as_bytes())
            .expect("writing a skip marker to HERMIT_TEST_SKIP_FILE failed");
        assert_eq!(written, line.len(), "short write of a skip marker");
    }

    fn command_stdout(argv: &[&str]) -> Option<String> {
        let output = Command::new(argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    /// Re-runs `every_host_kernel_syscall_number_is_accounted_for` in this
    /// same test binary under a kernel release that is not the running one
    /// (`setarch --uname-2.6` changes uname(2) too, so this holds however
    /// the release is read), in a private mount namespace whose
    /// /lib/modules holds this host's live header bytes at the other
    /// release's path, with HERMIT_RUNNING_KERNEL_UAPI_ASM naming that
    /// path. The child must wire exactly as many numbers as the same bytes
    /// wire here. Where a capability is refused the case skips naming it,
    /// and a skip never counts as this evidence.
    #[test]
    fn accounted_for_binds_a_different_kernel_release() {
        for (argv, capability) in [
            (
                vec!["unshare", "-rm", "/bin/true"],
                "unprivileged user+mount namespaces (unshare -rm)",
            ),
            (
                vec!["setarch", "x86_64", "--uname-2.6", "/bin/true"],
                "setarch x86_64 --uname-2.6 personality",
            ),
        ] {
            let probe = Command::new(argv[0])
                .args(&argv[1..])
                .stdin(Stdio::null())
                .output();
            match probe {
                Ok(output) if output.status.success() => {}
                other => {
                    record_test_skip(
                        "accounted_for_binds_a_different_kernel_release",
                        &format!("{capability} refused here: {other:?}"),
                    );
                    return;
                }
            }
        }
        let other_release = command_stdout(&["setarch", "x86_64", "--uname-2.6", "uname", "-r"])
            .expect("setarch uname -r");
        let running_release = command_stdout(&["uname", "-r"]).expect("uname -r");
        assert_ne!(other_release, running_release);
        let pinned = std::env::var_os(PINNED_ROOT_UAPI_ASM_VAR);
        let live = candidates(&running_release, pinned.as_deref())
            .ok()
            .and_then(|paths| read_first(Path::new("/"), &paths));
        let Some((live_source, mut parent_numbers)) = live else {
            record_test_skip(
                "accounted_for_binds_a_different_kernel_release",
                "no live host syscall header to stage at the other release",
            );
            return;
        };
        parent_numbers.retain(|nr, _| *nr >= 0 && *nr < 0x4000_0000);
        let parent_count = parent_numbers.len();
        let live_bytes = std::fs::read(&live_source).expect("read the live header");
        assert!(parent_count > 300, "parent parse of {live_source}");

        let run_child = |stage_header: bool| -> (bool, String) {
            let mut root = Root::new(if stage_header {
                "other-release"
            } else {
                "other-release-absent"
            });
            let staged = root.dir.join("unistd_64.h");
            if stage_header {
                std::fs::write(&staged, &live_bytes).unwrap();
                root.files.push(staged.clone());
            }
            let guest_asm = uapi_asm_dir(&other_release);
            // `mount --bind` of the staged file would need the target file
            // to exist first; a tmpfs copy is simpler and just as private.
            let setup = format!(
                "mount -t tmpfs tmpfs /lib/modules || exit 125; \
                 mkdir -p {guest_asm} || exit 125; \
                 if [ -f \"$1\" ]; then cp \"$1\" {guest_asm}/unistd_64.h || exit 125; fi; \
                 shift; exec \"$@\""
            );
            let exe = std::env::current_exe().expect("current test binary");
            let output = Command::new("setarch")
                .args([
                    "x86_64",
                    "--uname-2.6",
                    "unshare",
                    "-rm",
                    "/bin/sh",
                    "-c",
                    &setup,
                    "sh",
                ])
                .arg(&staged)
                .arg(&exe)
                .args([
                    "--exact",
                    "syscall_classification::tests::every_host_kernel_syscall_number_is_accounted_for",
                    "--show-output",
                ])
                .env(PINNED_ROOT_UAPI_ASM_VAR, &guest_asm)
                .stdin(Stdio::null())
                .output()
                .expect("run the accounted-for child");
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            (output.status.success(), text)
        };

        let (ok, text) = run_child(true);
        let guest_asm = uapi_asm_dir(&other_release);
        assert!(ok, "child at release {other_release} failed: {text}");
        let expected = format!(
            "{guest_asm}/unistd_64.h (kernel release {other_release}) wires \
             {parent_count} syscall numbers; every one is in the pinned \
             table or the manifests"
        );
        let matched = text.lines().find(|line| line.trim() == expected.trim());
        assert!(
            matched.is_some(),
            "child stdout has no exact line {expected:?}: {text}"
        );
        println!("{}", matched.unwrap().trim());

        let (ok, text) = run_child(false);
        assert!(
            !ok,
            "child without the other release's header passed: {text}"
        );
        assert!(
            text.contains("no host syscall table found in")
                && text.contains(&other_release)
                && text.contains(&guest_asm),
            "child panic does not name the missing header for {other_release} \
             at {guest_asm}: {text}"
        );
    }

    #[test]
    fn only_plain_decimal_defines_name_wired_numbers() {
        let numbers = parse_nr_defines(
            "#define __NR_read 0\n#define __NR_syscalls 470\n\
             #define __NR_alias __NR_read\n  #define __NR_write 1  \n",
        );
        assert_eq!(
            numbers.into_iter().collect::<Vec<_>>(),
            [(0, "read".to_owned()), (1, "write".to_owned())]
        );
    }
}
