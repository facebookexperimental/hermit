/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#![deny(clippy::all)]

use std::env;
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;

const REVERSE_DAP: &str = include_str!("hermit-dap-reverse.py");

#[derive(Debug, PartialEq)]
struct ReplayOptions {
    id: OsString,
    data_dir: Option<OsString>,
    gdbserver_port: u16,
}

#[derive(Debug, PartialEq)]
struct Options {
    gdb: OsString,
    replay: Option<ReplayOptions>,
}

fn usage() {
    println!(
        "Usage: hermit-dap [--gdb PATH] [--replay ID [--data-dir DIR] [--gdbserver-port PORT]]\n\n\
         Start a Debug Adapter Protocol server for a Hermit GDB remote target.\n\
         The DAP attach request must include the guest executable as 'program'\n\
         and the Hermit GDB server address as 'target'.\n\n\
         With --replay, hermit-dap starts and manages `hermit replay --serve-only`,\n\
         enabling source-level stepBack and reverseContinue by replaying from the beginning."
    );
}

fn default_gdb() -> OsString {
    let system_gdb = Path::new("/usr/bin/gdb");
    if system_gdb.is_file() {
        system_gdb.as_os_str().to_owned()
    } else {
        OsString::from("gdb")
    }
}

fn parse_options<I>(arguments: I) -> Result<Option<Options>, String>
where
    I: IntoIterator<Item = OsString>,
{
    let mut gdb = None;
    let mut replay = None;
    let mut data_dir = None;
    let mut gdbserver_port = 1234;
    let mut gdbserver_port_set = false;
    let mut args = arguments.into_iter();

    while let Some(argument) = args.next() {
        if argument == "--gdb" {
            let path = args
                .next()
                .ok_or_else(|| "--gdb requires a path".to_owned())?;
            if gdb.replace(path).is_some() {
                return Err("--gdb may only be specified once".to_owned());
            }
        } else if argument == "--replay" {
            let id = args
                .next()
                .ok_or_else(|| "--replay requires a recording ID".to_owned())?;
            if replay.replace(id).is_some() {
                return Err("--replay may only be specified once".to_owned());
            }
        } else if argument == "--data-dir" {
            let path = args
                .next()
                .ok_or_else(|| "--data-dir requires a path".to_owned())?;
            if data_dir.replace(path).is_some() {
                return Err("--data-dir may only be specified once".to_owned());
            }
        } else if argument == "--gdbserver-port" {
            let port = args
                .next()
                .ok_or_else(|| "--gdbserver-port requires a port".to_owned())?;
            gdbserver_port_set = true;
            gdbserver_port = port
                .to_string_lossy()
                .parse()
                .map_err(|_| "--gdbserver-port requires a valid port".to_owned())?;
        } else if argument == "-h" || argument == "--help" {
            usage();
            return Ok(None);
        } else {
            return Err(format!(
                "unrecognized argument: {}",
                argument.to_string_lossy()
            ));
        }
    }

    if replay.is_none() && data_dir.is_some() {
        return Err("--data-dir requires --replay".to_owned());
    }
    if replay.is_none() && gdbserver_port_set {
        return Err("--gdbserver-port requires --replay".to_owned());
    }

    Ok(Some(Options {
        gdb: gdb.unwrap_or_else(default_gdb),
        replay: replay.map(|id| ReplayOptions {
            id,
            data_dir,
            gdbserver_port,
        }),
    }))
}

fn replay_command(options: &ReplayOptions) -> Result<Vec<String>, String> {
    let hermit = env::current_exe()
        .map_err(|error| format!("failed to locate hermit-dap: {error}"))?
        .with_file_name("hermit");
    if !hermit.is_file() {
        return Err(format!(
            "could not find the hermit executable beside hermit-dap: {}",
            hermit.display()
        ));
    }

    let mut args = vec![
        "replay".to_owned(),
        options.id.to_string_lossy().into_owned(),
        "--serve-only".to_owned(),
        format!("--gdbserver-port={}", options.gdbserver_port),
    ];
    if let Some(data_dir) = &options.data_dir {
        args.push("--data-dir".to_owned());
        args.push(data_dir.to_string_lossy().into_owned());
    }

    let mut serialized = vec![hermit.to_string_lossy().into_owned()];
    serialized.extend(args);
    Ok(serialized)
}

fn main() -> ExitCode {
    let options = match parse_options(env::args_os().skip(1)) {
        Ok(Some(options)) => options,
        Ok(None) => return ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("hermit-dap: {error}");
            eprintln!("Try 'hermit-dap --help' for more information.");
            return ExitCode::FAILURE;
        }
    };

    let mut gdb_command = Command::new(&options.gdb);
    gdb_command
        .arg("--quiet")
        .arg("--nx")
        .arg("--init-eval-command=set debuginfod enabled off")
        .arg("--init-eval-command=set sysroot /");

    if let Some(replay) = &options.replay {
        let replay_command = match replay_command(replay).and_then(|command| {
            serde_json::to_string(&command)
                .map_err(|error| format!("failed to encode hermit replay command: {error}"))
        }) {
            Ok(command) => command,
            Err(error) => {
                eprintln!("hermit-dap: {error}");
                return ExitCode::FAILURE;
            }
        };
        let replay_target = format!("127.0.0.1:{}", replay.gdbserver_port);
        let extension = format!(
            "python HERMIT_REPLAY_COMMAND = {replay_command}; \
             HERMIT_REPLAY_TARGET = {replay_target:?}; exec({REVERSE_DAP:?})"
        );
        gdb_command.arg(format!("--init-eval-command={extension}"));
    }

    let error = gdb_command.arg("--interpreter=dap").exec();
    eprintln!(
        "hermit-dap: failed to execute {}: {error}",
        PathBuf::from(&options.gdb).display()
    );
    ExitCode::FAILURE
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_options_use_existing_command_vocabulary() {
        let options = parse_options([
            "--replay".into(),
            "recording-id".into(),
            "--data-dir".into(),
            "/tmp/recordings".into(),
            "--gdbserver-port".into(),
            "2345".into(),
        ])
        .unwrap()
        .unwrap();

        assert_eq!(
            options.replay,
            Some(ReplayOptions {
                id: "recording-id".into(),
                data_dir: Some("/tmp/recordings".into()),
                gdbserver_port: 2345,
            })
        );
    }

    #[test]
    fn gdbserver_port_without_replay_is_rejected() {
        let error = parse_options(["--gdbserver-port".into(), "2345".into()]).unwrap_err();
        assert_eq!(error, "--gdbserver-port requires --replay");
    }

    #[test]
    fn data_dir_without_replay_is_rejected() {
        let error = parse_options(["--data-dir".into(), "/tmp/recordings".into()]).unwrap_err();
        assert_eq!(error, "--data-dir requires --replay");
    }
}
