/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Exercise the actual CMake producer while its real linker has unlinked its
//! output. Neither a mocked publisher nor a retry can satisfy this oracle.

use std::io::Read;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::thread::JoinHandle;
use std::time::Duration;
use std::time::Instant;

use super::*;

struct Directory(PathBuf);

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// Closing the gate releases the real linker even when the old implementation
// fails the loader assertion. Join before removing any of its input files.
struct Builder {
    gate: Option<UnixStream>,
    listener: Option<UnixListener>,
    worker: Option<JoinHandle<io::Result<PathBuf>>>,
}

impl Builder {
    fn finish(&mut self) -> io::Result<PathBuf> {
        drop(self.gate.take());
        drop(self.listener.take());
        let result = self.worker.take().unwrap().join().unwrap();
        eprintln!(
            "publication phase: released and joined writer, success={}",
            result.is_ok()
        );
        result
    }
}

impl Drop for Builder {
    fn drop(&mut self) {
        if self.worker.is_some() {
            let _ = self.finish();
        }
    }
}

fn load_client(client: &Path) -> std::process::Output {
    Command::new("timeout")
        .args(["--kill-after=2s", "10s"])
        .arg(reverie_dbt::bundled_drrun_path())
        .arg("-c")
        .arg(client)
        .args(["--", "/bin/true"])
        .output()
        .expect("execute the genuine DynamoRIO loader")
}

fn require_generation(output: &std::process::Output, generation: u32) {
    assert!(
        output.status.success()
            && String::from_utf8_lossy(&output.stderr)
                .contains(&format!("published-client-generation={generation}\n")),
        "published generation {generation} must remain loadable: status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn native_client_publication_survives_a_concurrent_relink() {
    // A short host temp path is needed for the linker's Unix socket. No global
    // environment mutation or shared build directory is used by this fixture.
    let directory = Directory(std::env::temp_dir().join(format!(
        "dbt-publish-{}-{:x}",
        std::process::id(),
        rand::random::<u64>()
    )));
    let source = directory.0.join("source");
    let publication = directory.0.join("published");
    fs::create_dir_all(&source).unwrap();
    for (name, contents) in [
        (
            "CMakeLists.txt",
            include_str!("native_client_publication_test/CMakeLists.txt"),
        ),
        (
            "client.c",
            include_str!("native_client_publication_test/client.c"),
        ),
        (
            "link_gate.c",
            include_str!("native_client_publication_test/link_gate.c"),
        ),
    ] {
        fs::write(source.join(name), contents).unwrap();
    }
    fs::write(source.join("generation.h"), "#define GENERATION 1\n").unwrap();
    let sdk = reverie_dbt::bundled_dynamorio_cmake_dir();
    // This tiny client has no Detcore dependency. The actual producer still
    // receives and forwards its normal runtime configuration argument.
    let runtime = directory.0.join("unused-runtime.so");
    let client = build_native_client(&source, &publication, sdk, &runtime).unwrap();
    require_generation(&load_client(&client), 1);
    eprintln!("publication phase: initial generation 1 loaded");

    let compile = Command::new("cc")
        .args(["-shared", "-fPIC", "-O2", "-Wall", "-Wextra", "-Werror"])
        .arg(source.join("link_gate.c"))
        .args(["-ldl", "-o"])
        .arg(source.join("link_gate.so"))
        .output()
        .unwrap();
    assert!(
        compile.status.success(),
        "{}",
        String::from_utf8_lossy(&compile.stderr)
    );
    fs::write(source.join("generation.h"), "#define GENERATION 2\n").unwrap();
    fs::write(source.join("gate.enabled"), "").unwrap();
    let listener = UnixListener::bind(source.join("gate.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let builder_source = source.clone();
    let mut builder = Builder {
        gate: None,
        listener: Some(listener),
        worker: Some(std::thread::spawn(move || {
            build_native_client(&builder_source, &publication, sdk, &runtime)
        })),
    };
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        match builder.listener.as_ref().unwrap().accept() {
            Ok((mut gate, _)) => {
                gate.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut event = [0];
                gate.read_exact(&mut event).unwrap();
                assert_eq!(
                    event,
                    [b'U'],
                    "the actual output unlink must precede the gate"
                );
                builder.gate = Some(gate);
                eprintln!("publication phase: actual successful unlink acknowledged; writer held");
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                assert!(
                    !builder.worker.as_ref().unwrap().is_finished(),
                    "linker completed without the required real-unlink gate"
                );
                assert!(Instant::now() < deadline, "linker did not reach its unlink");
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("accept linker gate: {error}"),
        }
    }
    // Keep this assertion before release: it is the observed reader/writer
    // boundary. On failure Builder::drop releases and joins the writer.
    require_generation(&load_client(&client), 1);
    eprintln!("publication phase: generation 1 loaded while writer held");
    let new_client = builder.finish().unwrap();
    assert_eq!(new_client, client, "the public filename is stable");
    require_generation(&load_client(&client), 2);
    eprintln!("publication phase: generation 2 loaded after publication");

    // A real subsequent compiler error must not destroy the last good client.
    fs::remove_file(source.join("gate.enabled")).unwrap();
    fs::write(
        source.join("generation.h"),
        "#error deliberate build failure\n",
    )
    .unwrap();
    assert!(
        build_native_client(
            &source,
            client.parent().unwrap(),
            sdk,
            &directory.0.join("unused-runtime.so"),
        )
        .is_err()
    );
    require_generation(&load_client(&client), 2);
    eprintln!("publication phase: generation 2 preserved after failed build");
}
