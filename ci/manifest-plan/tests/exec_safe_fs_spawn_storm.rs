//! Regression for hermit_manifest_plan::exec_safe_fs under a spawn storm
//! (dh-lnslwh): while other threads spawn continuously, every executable the
//! helpers create runs at once. Each one is run immediately, which is where an
//! in-process write was refused with ETXTBSY.
//!
//! This is its own test binary because the storm must not share a process
//! with the library's tests. Inside the parallel libtest process it failed
//! the runner's CPU-budget tests in 7 of 20 full `cargo test --lib` runs, and
//! still in 5 of 8 when the storm ran in a child of that process; with the
//! case left out, 0 of 10 failed. Cargo runs test binaries one after another,
//! and Nextest runs each test in a process of its own, so neither runs this
//! storm beside those tests' in-process parallel load.
//!
//! The storm costs CPU on purpose, and Nextest charges a test for its
//! children against the default 22 CPU-s cap. Six spawners and 100 iterations
//! measured 7.2 to 7.6 CPU-s in about 1.2 s of wall time. 8 and 200 measured
//! 17 to 18 CPU-s. Against an in-process write or copy, 6 and 100 still found
//! 6 to 23 busy executables of 300 in each of 13 runs per mutant; one fails
//! the test.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use hermit_manifest_plan::exec_safe_fs::copy;
use hermit_manifest_plan::exec_safe_fs::write_executable;

const SCRIPT: &str = "#!/bin/sh\nexit 0\n";

/// A fresh directory under the temporary root, removed when dropped.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn helpers_create_executables_that_are_never_busy_under_concurrent_spawns() {
    let dir = Scratch(
        std::env::temp_dir().join(format!("exec-safe-fs-spawn-storm-{}", std::process::id())),
    );
    std::fs::create_dir(&dir.0).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let spawners = (0..6)
        .map(|_| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let _ = Command::new("true").status();
                }
            })
        })
        .collect::<Vec<_>>();
    let source = dir.0.join("source");
    write_executable(&source, SCRIPT, 0o755).unwrap();
    let mut failures = Vec::new();
    let mut run = |path: &Path, want: i32| match Command::new(path).status() {
        Ok(status) if status.code() == Some(want) => {}
        other => failures.push(format!("{}: {other:?}", path.display())),
    };
    for index in 0..100 {
        let written = dir.0.join(format!("written-{index}"));
        write_executable(&written, SCRIPT, 0o700).unwrap();
        run(&written, 0);
        // Replacing in place must not reuse the executed inode either.
        write_executable(&written, "#!/bin/sh\nexit 3\n", 0o700).unwrap();
        run(&written, 3);
        let copied = dir.0.join(format!("copied-{index}"));
        copy(&source, &copied).unwrap();
        run(&copied, 0);
    }
    stop.store(true, Ordering::Relaxed);
    for spawner in spawners {
        spawner.join().unwrap();
    }
    assert!(failures.is_empty(), "{} busy: {failures:?}", failures.len());
}
