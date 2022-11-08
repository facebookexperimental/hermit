/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::thread;

use tempfile::tempdir;

// This test races and it is agnostic to which direction the race goes.  That is, it won't
// actually fail with a nonzero exit code when the race goes badly. The point is to assert
// determinism.  Or the caller can use chaos to ensure that both passing and reaching
// schedules are found.
fn run_test() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sock");
    let p2 = path.clone();

    let _t1 = thread::spawn(move || {
        let listener = UnixListener::bind(&p2).expect("bind to succeed");

        if let Some(stream) = listener.incoming().next() {
            match stream {
                Ok(_stream) => {
                    /* connection succeeded */
                    println!("Server: got client");
                }
                Err(err) => {
                    /* connection failed */
                    eprintln!("Server: connection failed: {:?}", err);
                }
            }
        }
    });
    let t2 = thread::spawn(move || {
        if let Ok(_stream) = UnixStream::connect(&path) {
            eprintln!("Client: connection succeeded..");
        } else {
            eprintln!("Client: connection failed.");
        }
    });

    t2.join().expect("client to be ok");
}

fn main() {
    if matches!(std::env::var("HERMIT_MODE"), Ok(x) if x == "record") {
        // TODO: Fix this test, which currently exhibits a desynchronization such as this:
        /*
        Test output:
        > from_execution_error::timeout
        :: Recording...
        :: Replaying...
        thread 'main' panicked at 'On thread 7, got unexpected syscall (count = 9):
        close(arg0: 0x3, arg1: 0x0, arg2: 0x0, arg3: 0x14, arg4: 0x0, arg5: 0x14)
        Expected:
        listen(arg0: 0x3, arg1: 0x80, arg2: 0x17, arg3: 0x14, arg4: 0x0, arg5: 0x14)

        Additional context:
        set_robust_list(0x7ffff69fe9e0, 24)
        sigaltstack(NULL, 0x7ffff69fd340)
        mmap(NULL, 12288, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON | MAP_ANONYMOUS, -1, 0)
        mprotect(0x7ffff7a05000, 4096, PROT_NONE)
        sigaltstack(0x7ffff69fd340, NULL)
        sched_getaffinity(7, 32, 0x7ffff7607c80)
        socket(1, 526337, 0)
        bind(3, 0x7ffff69fd1d8, 23)
        - listen(3, 128)  ← Expected this
        + close(3)  ← but got this instead!
        - accept4(3, 0x7ffff69fd0d0, 0x7ffff69fd06c, SOCK_CLOEXEC)
        - write(1, 0x5555555d24a0, 19)
        - close(5)
        - close(3)
         */
        eprintln!("Skipping test in record mode.");
    } else {
        run_test();
    }
}
