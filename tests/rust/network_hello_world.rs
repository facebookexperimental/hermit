/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::io::Read;
use std::io::Write;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::mpsc::SyncSender;
use std::thread;

use tempfile::tempdir;

const BUFFER_SIZE: usize = 128;

fn main() {
    if matches!(std::env::var("HERMIT_MODE"), Ok(mode) if mode == "record") {
        // TODO: Record mode currently hangs for this test
        eprintln!("Skipping test in record mode.");
        return;
    }

    let temp_dir = tempdir().unwrap();
    let socket_path = temp_dir.path().join("net-hello-world.sock");
    let socket_path2 = socket_path.clone();

    let (ready_tx, ready_rx) = mpsc::sync_channel(0);
    let server_thread = run_server(socket_path, ready_tx);

    ready_rx
        .recv()
        .expect("server thread exited before reporting startup")
        .expect("server to bind its socket");

    let client_thread = run_client(socket_path2);

    client_thread.join().expect("client to be ok");
    server_thread.join().expect("server to be ok");
}

fn run_server(
    socket_path: PathBuf,
    ready_tx: SyncSender<Result<(), String>>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let listener = match UnixListener::bind(&socket_path) {
            Ok(listener) => listener,
            Err(error) => {
                ready_tx.send(Err(error.to_string())).unwrap();
                return;
            }
        };
        ready_tx.send(Ok(())).unwrap();

        println!("[Server] Listening...");
        let mut incoming = listener.incoming().next().unwrap().unwrap();

        let mut buffer = [0; BUFFER_SIZE];
        let num_bytes_read = incoming.read(&mut buffer).unwrap();
        let request = String::from_utf8(buffer[..num_bytes_read].to_vec()).unwrap();
        println!("[Server] Got request: {}", request);

        println!("[Server] Sending response...");
        incoming.write_all(b"Goodbye, moon!").unwrap();
    })
}

fn run_client(socket_path: PathBuf) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        println!("[Client] Connecting...");
        let mut stream = UnixStream::connect(socket_path).unwrap();

        println!("[Client] Sending request...");
        stream.write_all(b"Hello, world!").unwrap();

        let mut buffer = [0; BUFFER_SIZE];
        let num_bytes_read = stream.read(&mut buffer).unwrap();
        let response = String::from_utf8(buffer[..num_bytes_read].to_vec()).unwrap();
        println!("[Client] Got response: {}", response);
    })
}
