/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Mechanics-only integration with Hermit's existing physical container owner.
//! These tests never release a guest and do not establish Detcore readiness.

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use clap::Parser;
use reverie_inguest::launcher::exec_stop;

use super::Args;
use super::Subcommand;
use super::run::RunOpts;

fn options() -> RunOpts {
    let args = Args::parse_from(["hermit", "--backend=in-guest-trap", "run", "--", "/bin/sh"]);
    let Subcommand::Run(options) = args.command else {
        unreachable!("the fixed arguments select run");
    };
    *options
}

fn initial_strings(context: &exec_stop::EntryContext) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let word = |address: u64| {
        let offset = usize::try_from(address.checked_sub(context.stack_start()).unwrap()).unwrap();
        u64::from_ne_bytes(
            context.stack_bytes()[offset..offset + 8]
                .try_into()
                .unwrap(),
        )
    };
    let string = |address: u64| {
        let offset = usize::try_from(address.checked_sub(context.stack_start()).unwrap()).unwrap();
        let tail = &context.stack_bytes()[offset..];
        let end = tail
            .iter()
            .position(|byte| *byte == 0)
            .expect("terminated stack string");
        tail[..end].to_vec()
    };
    let mut cursor = context.regs().rsp;
    let argc = usize::try_from(word(cursor)).unwrap();
    assert!(argc < 32, "bounded test argv");
    cursor += 8;
    let mut arguments = Vec::new();
    for _ in 0..argc {
        arguments.push(string(word(cursor)));
        cursor += 8;
    }
    assert_eq!(word(cursor), 0, "argv terminator");
    cursor += 8;
    let mut environment = Vec::new();
    while word(cursor) != 0 {
        assert!(environment.len() < 32, "bounded explicit test environment");
        environment.push(string(word(cursor)));
        cursor += 8;
    }
    (arguments, environment)
}

#[test]
fn shared_exec_stop_preserves_command_inside_the_common_container() {
    use std::os::fd::AsFd;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;

    let (observed, ()) = options()
        .launch_in_container(
            (),
            "first-image stopped-child ownership test",
            Some(Duration::from_secs(15)),
            |_, _, _| {
                // These are the real common container's namespaces and /tmp.
                // The mechanics do not construct a substitute namespace/root.
                assert_eq!(std::process::id(), 1, "common private PID namespace");
                let scratch = tempfile::tempdir_in("/tmp")?;
                let original = scratch.path().join("original");
                let moved = scratch.path().join("renamed");
                std::fs::create_dir(&original)?;
                let marker = scratch.path().join("guest-must-not-run");
                let script = format!("printf escaped > '{}'", marker.display());
                let arguments = vec![
                    OsString::from("distinct-loader-argv0"),
                    OsString::from("-c"),
                    OsString::from(&script),
                ];
                let environment = vec![
                    (
                        OsString::from("LOADER_TEST_ONLY"),
                        OsString::from("exact value"),
                    ),
                    (OsString::from("EMPTY_VALUE"), OsString::new()),
                ];
                let bootstrap = tempfile::tempfile_in(scratch.path())?;
                let bootstrap_metadata = bootstrap.metadata()?;
                let request = exec_stop::PreparedExec::new(
                    Path::new("/bin/sh"),
                    &arguments,
                    &environment,
                    Some(&original),
                    vec![bootstrap.as_fd()],
                )
                .map_err(anyhow::Error::msg)?;
                // Resolution happened while preparing, so changing the old
                // pathname cannot redirect the child's working directory.
                std::fs::rename(&original, &moved)?;
                std::fs::create_dir(&original)?;
                let cleanup = exec_stop::CleanupReceipt::default();
                let stopped = exec_stop::ExecStop::spawn(request, cleanup.clone())
                    .expect("shared launcher must reach the actual initial exec stop");
                let pid = stopped.pid().as_raw();
                assert_eq!(
                    std::fs::read_link(format!("/proc/{pid}/cwd"))?,
                    moved,
                    "prepared cwd is a retained directory, not a reopened pathname"
                );
                let inherited =
                    std::fs::metadata(format!("/proc/{pid}/fd/{}", bootstrap.as_raw_fd()))?;
                assert_eq!(inherited.dev(), bootstrap_metadata.dev());
                assert_eq!(inherited.ino(), bootstrap_metadata.ino());
                for fd in 0..=2 {
                    let parent = std::fs::metadata(format!("/proc/self/fd/{fd}"));
                    let child = std::fs::metadata(format!("/proc/{pid}/fd/{fd}"));
                    match (parent, child) {
                        (Ok(parent), Ok(child)) => {
                            assert_eq!((child.dev(), child.ino()), (parent.dev(), parent.ino()));
                        }
                        (Err(parent), Err(child)) => {
                            assert_eq!(parent.kind(), std::io::ErrorKind::NotFound);
                            assert_eq!(child.kind(), std::io::ErrorKind::NotFound);
                        }
                        other => panic!("stdio inheritance differs for fd {fd}: {other:?}"),
                    }
                }
                let (actual_arguments, actual_environment) = initial_strings(stopped.entry());
                let boundary = stopped.boundary();
                assert_eq!(boundary.event_regs.rip, boundary.exit_regs.rip);
                assert_eq!(boundary.event_regs.rsp, boundary.exit_regs.rsp);
                assert_eq!(boundary.exit_regs.rax, 0);
                assert!(!marker.exists(), "no guest instruction was released");
                let retirement = match stopped.cancel() {
                    Ok(retirement) => retirement,
                    Err(_retained_owner) => panic!("physical child retirement unconfirmed"),
                };
                assert!(retirement.confirmed);
                assert!(retirement.kill_requested);
                assert_eq!(
                    retirement.confirmation,
                    Some(exec_stop::CleanupConfirmation::SafeptraceTerminalWait)
                );
                assert!(retirement.wait_error.is_none());
                assert!(cleanup.snapshot().confirmed);
                assert!(
                    !marker.exists(),
                    "cancellation must never release guest entry"
                );
                assert!(
                    !Path::new(&format!("/proc/{pid}")).exists(),
                    "the original child has been reaped inside its namespace"
                );
                Ok((actual_arguments, actual_environment, script))
            },
        )
        .unwrap();
    assert_eq!(
        observed.0,
        [
            b"distinct-loader-argv0".to_vec(),
            b"-c".to_vec(),
            observed.2.as_bytes().to_vec(),
        ]
    );
    assert_eq!(
        observed.1,
        [
            b"LOADER_TEST_ONLY=exact value".to_vec(),
            b"EMPTY_VALUE=".to_vec()
        ],
        "no ambient host environment reaches the stopped image"
    );
}
