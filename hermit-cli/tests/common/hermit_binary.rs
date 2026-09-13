/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::ffi::OsStr;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

/// Resolve the Hermit executable selected for this test process.
///
/// Validation exports `HERMIT_BIN` only after verifying the content-addressed
/// artifact. Ordinary `cargo test` invocations retain Cargo's compile-time
/// binary path as their fallback.
pub fn hermit_binary() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        std::env::var_os("HERMIT_BIN")
            .filter(|path| !path.is_empty())
            .or_else(|| option_env!("CARGO_BIN_EXE_hermit").map(OsString::from))
            .map(PathBuf::from)
            .expect("HERMIT_BIN and CARGO_BIN_EXE_hermit are both unavailable")
    })
    .as_path()
}

// Each integration target includes this module independently; identity-only
// consumers use `hermit_binary` without constructing a guest command.
#[allow(dead_code)]
const ISOLATED_WORKDIR_ENV: &str = "HERMIT_E2E_EMPTY_WORKDIR";
#[allow(dead_code)]
const HERMETIC_TEST_WORKDIR: &str = "/test";

#[allow(dead_code)]
pub fn guest_args_for<I, S>(args: I, requested: Option<&OsStr>) -> Result<Vec<OsString>, String>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut args = args.into_iter().map(Into::into).collect::<Vec<_>>();
    let Some(requested) = requested else {
        return Ok(args);
    };
    if requested != OsStr::new(HERMETIC_TEST_WORKDIR) {
        return Err(format!(
            "{ISOLATED_WORKDIR_ENV} must be {HERMETIC_TEST_WORKDIR}, got {requested:?}"
        ));
    }
    let Some(separator) = args.iter().position(|arg| arg == OsStr::new("--")) else {
        return Ok(args);
    };
    let options = &args[..separator];
    let uses_outer_mount = options
        .iter()
        .any(|arg| arg == OsStr::new("--no-namespace"))
        || options.iter().any(|arg| arg == OsStr::new("--backend=dbt"))
        || options
            .windows(2)
            .any(|pair| pair[0] == OsStr::new("--backend") && pair[1] == OsStr::new("dbt"));
    let explicit_base_env = options.iter().enumerate().find_map(|(index, arg)| {
        arg.to_str()
            .and_then(|arg| arg.strip_prefix("--base-env="))
            .map(OsString::from)
            .or_else(|| {
                (arg == OsStr::new("--base-env"))
                    .then(|| options.get(index + 1).cloned())
                    .flatten()
            })
    });
    if explicit_base_env
        .as_deref()
        .is_some_and(|base_env| base_env != OsStr::new("minimal"))
    {
        return Err(format!(
            "{ISOLATED_WORKDIR_ENV} requires --base-env=minimal, got {explicit_base_env:?}"
        ));
    }
    let has_test_mount = options.iter().any(|arg| {
        arg.to_str()
            .is_some_and(|arg| arg.starts_with("--mount=") && arg.contains("target=/test"))
    });
    let explicit_workdir = options.iter().enumerate().find_map(|(index, arg)| {
        arg.to_str()
            .and_then(|arg| arg.strip_prefix("--workdir="))
            .map(OsString::from)
            .or_else(|| {
                (arg == OsStr::new("--workdir"))
                    .then(|| options.get(index + 1).cloned())
                    .flatten()
            })
    });
    if explicit_workdir
        .as_deref()
        .is_some_and(|workdir| workdir != OsStr::new(HERMETIC_TEST_WORKDIR))
    {
        return Err(format!(
            "{ISOLATED_WORKDIR_ENV} requires --workdir={HERMETIC_TEST_WORKDIR}, got {explicit_workdir:?}"
        ));
    }
    let mut execution_root = Vec::with_capacity(3);
    if explicit_base_env.is_none() {
        execution_root.push(OsString::from("--base-env=minimal"));
    }
    if !uses_outer_mount && !has_test_mount {
        execution_root.push(OsString::from("--mount=type=tmpfs,target=/test"));
    }
    if explicit_workdir.is_none() {
        execution_root.push(OsString::from("--workdir=/test"));
    }
    args.splice(separator..separator, execution_root);
    Ok(args)
}

/// Insert the validate v3 guest arguments into a direct or timeout-wrapped
/// Hermit command. Call this before configuring standard I/O.
#[allow(dead_code)]
pub fn configure_guest_execution(command: &mut Command) {
    let requested = std::env::var_os(ISOLATED_WORKDIR_ENV);
    configure_guest_execution_for(command, requested.as_deref())
        .unwrap_or_else(|error| panic!("PATH-CONTRACT: {error}"));
}

#[allow(dead_code)]
pub fn configure_guest_execution_for(
    command: &mut Command,
    requested: Option<&OsStr>,
) -> Result<(), String> {
    if requested.is_none() {
        return Ok(());
    }

    let program = command.get_program().to_os_string();
    let args = command
        .get_args()
        .map(OsStr::to_os_string)
        .collect::<Vec<_>>();
    let hermit = hermit_binary().as_os_str();
    let hermit_args_start = if program == hermit {
        0
    } else {
        let Some(index) = args.iter().position(|arg| arg == hermit) else {
            return Ok(());
        };
        index + 1
    };
    let adjusted = guest_args_for(args[hermit_args_start..].iter().cloned(), requested)?;
    let prefix = args[..hermit_args_start].to_vec();
    let current_dir = command.get_current_dir().map(Path::to_path_buf);
    let env = command
        .get_envs()
        .map(|(key, value)| (key.to_os_string(), value.map(OsStr::to_os_string)))
        .collect::<Vec<_>>();

    let mut rebuilt = Command::new(program);
    rebuilt.args(prefix).args(adjusted);
    if let Some(current_dir) = current_dir {
        rebuilt.current_dir(current_dir);
    }
    for (key, value) in env {
        if let Some(value) = value {
            rebuilt.env(key, value);
        } else {
            rebuilt.env_remove(key);
        }
    }
    *command = rebuilt;
    Ok(())
}
