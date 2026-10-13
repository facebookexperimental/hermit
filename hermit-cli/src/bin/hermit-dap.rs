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
use std::io;
use std::io::Write;
use std::net::TcpListener;
use std::os::fd::AsFd;
use std::os::fd::BorrowedFd;
use std::os::fd::IntoRawFd;
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
    listen: Option<String>,
}

fn usage() {
    println!(
        "Usage: hermit-dap [--gdb PATH] [--listen [HOST:]PORT] [--replay ID [--data-dir DIR] [--gdbserver-port PORT]]\n\n\
         Start a Debug Adapter Protocol server for a Hermit GDB remote target.\n\
         The DAP attach request must include the guest executable as 'program'\n\
         and the Hermit GDB server address as 'target'.\n\n\
         By default the DAP client speaks to hermit-dap on stdin and stdout.\n\
         With --listen, hermit-dap instead waits for one DAP client on a TCP port\n\
         (HOST defaults to 127.0.0.1; port 0 picks a free one), writes\n\
         `hermit-dap: listening on HOST:PORT` to stderr, and serves that client.\n\n\
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
    let mut listen = None;
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
        } else if argument == "--listen" {
            let address = args
                .next()
                .ok_or_else(|| "--listen requires [HOST:]PORT".to_owned())?;
            let address = listen_address(&address.to_string_lossy())?;
            if listen.replace(address).is_some() {
                return Err("--listen may only be specified once".to_owned());
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
        listen,
        replay: replay.map(|id| ReplayOptions {
            id,
            data_dir,
            gdbserver_port,
        }),
    }))
}

/// The socket address `--listen` binds: `PORT` alone means loopback, so the
/// adapter is not reachable from other hosts unless HOST says so.
fn listen_address(argument: &str) -> Result<String, String> {
    let (host, port) = match argument.rsplit_once(':') {
        Some((host, port)) => (host, port),
        None => ("127.0.0.1", argument),
    };
    if host.is_empty() {
        return Err("--listen requires a host before ':'".to_owned());
    }
    let port: u16 = port
        .parse()
        .map_err(|_| "--listen requires a valid port".to_owned())?;
    Ok(format!("{host}:{port}"))
}

/// Waits for one DAP client on `address` and makes its connection this
/// process's stdin and stdout, which GDB's DAP interpreter then serves.
///
/// Only the first client is accepted: GDB serves one session and exits when
/// it ends. The listening socket is closed before GDB starts (it is also
/// close-on-exec), so a second client is refused rather than left waiting.
fn accept_client(address: &str) -> Result<(), String> {
    let listener = TcpListener::bind(address).map_err(|error| {
        format!(
            "could not listen on {address}: {error}; pass another port, or port 0 \
             to let the system pick a free one"
        )
    })?;
    let local = listener
        .local_addr()
        .map_err(|error| format!("could not read the address of {address}: {error}"))?;
    // Tools that pass port 0 read the chosen port from this line.
    eprintln!("hermit-dap: listening on {local}");
    let _ = io::stderr().flush();
    let (client, peer) = listener
        .accept()
        .map_err(|error| format!("could not accept a DAP client on {local}: {error}"))?;
    drop(listener);
    nix::unistd::dup2_stdin(&client)
        .and_then(|()| nix::unistd::dup2_stdout(&client))
        .map_err(|error| format!("could not serve the DAP client {peer}: {error}"))?;
    eprintln!("hermit-dap: serving the DAP client at {peer}");
    Ok(())
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

/// Inheritable duplicates of this process's stdin, stdout and stderr, as a
/// Python tuple literal, or `None` if any duplicate fails.
///
/// GDB's DAP interpreter redirects its own descriptors 0, 1 and 2 to internal
/// pipes before it runs the replay extension, and drains them only after
/// startup. A startup refusal written through them never reaches the client,
/// so the extension writes it through these duplicates instead. They survive
/// the exec below on purpose; the extension closes them once startup
/// succeeds. Without them the extension can only write a refusal to GDB's own
/// stderr, so a failed duplicate is reported here, while this process's
/// stderr still reaches the client's log.
fn client_fds() -> String {
    let duplicate = |name: &'static str, fd: BorrowedFd<'_>| {
        nix::unistd::dup(fd).map_err(|error| format!("{name} ({error})"))
    };
    match (
        duplicate("stdin", io::stdin().as_fd()),
        duplicate("stdout", io::stdout().as_fd()),
        duplicate("stderr", io::stderr().as_fd()),
    ) {
        (Ok(input), Ok(output), Ok(error)) => format!(
            "({}, {}, {})",
            input.into_raw_fd(),
            output.into_raw_fd(),
            error.into_raw_fd()
        ),
        (input, output, error) => {
            // Dropping the duplicates that did succeed closes them.
            let failed: Vec<String> = [input.err(), output.err(), error.err()]
                .into_iter()
                .flatten()
                .collect();
            eprintln!(
                "hermit-dap: could not duplicate {}; if managed replay refuses to \
                 start, its reason goes only to GDB's stderr, which GDB's DAP \
                 interpreter does not pass to the client",
                failed.join(", ")
            );
            "None".to_string()
        }
    }
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

    if let Some(address) = &options.listen {
        // Before client_fds() below, which duplicates the client's stdin
        // and stdout for managed replay's refusals.
        if let Err(error) = accept_client(address) {
            eprintln!("hermit-dap: {error}");
            return ExitCode::FAILURE;
        }
    }

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
        let client_fds = client_fds();
        let extension = format!(
            "python HERMIT_REPLAY_COMMAND = {replay_command}; \
             HERMIT_REPLAY_TARGET = {replay_target:?}; \
             HERMIT_CLIENT_FDS = {client_fds}; exec({REVERSE_DAP:?})"
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
    fn listen_defaults_to_loopback() {
        let listen = |argument: &str| {
            parse_options(["--listen".into(), argument.into()])
                .map(|options| options.unwrap().listen)
        };
        assert_eq!(listen("4711"), Ok(Some("127.0.0.1:4711".to_owned())));
        assert_eq!(listen("0.0.0.0:0"), Ok(Some("0.0.0.0:0".to_owned())));
        assert_eq!(listen("[::1]:4711"), Ok(Some("[::1]:4711".to_owned())));
        assert_eq!(
            listen(":4711"),
            Err("--listen requires a host before ':'".to_owned())
        );
        assert_eq!(
            listen("70000"),
            Err("--listen requires a valid port".to_owned())
        );
        assert_eq!(
            parse_options(["--listen".into(), "1".into(), "--listen".into(), "2".into()]),
            Err("--listen may only be specified once".to_owned())
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
