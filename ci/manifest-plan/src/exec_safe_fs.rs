//! Create files that this process, or a child of it, will execute.
//!
//! Linux refuses to execute a file while any process holds it open for
//! writing (ETXTBSY). Rust opens files close-on-exec, but that does not close
//! the window: a sibling thread that spawns while this thread still holds the
//! write descriptor forks a child that owns a duplicate until the child itself
//! execs. The parallel libtest process for this crate spawns constantly, so a
//! fixture written and then run in-process is refused intermittently
//! (dh-lnslwh: `apply_harvest_refuses_case_set_and_name_set_violations` failed
//! listing its freshly written Cargo twin). scripts/lib/exec_safe_fs.rs
//! measured the same hazard: with eight threads spawning `true`, 79, 95 and
//! 134 of 2000 freshly written executables were busy.
//!
//! Writing to a temporary name and renaming does not help: the rename keeps
//! the inode, and the stray child's descriptor names the inode. Nothing here
//! opens an executable target for writing in this process. A short-lived
//! `install` child, which never forks, creates it instead, so no descriptor
//! any other child can inherit ever refers to it. GNU `install` unlinks an
//! existing destination first, so replacing an executable also yields a new
//! inode that no earlier descriptor names.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

/// Write `contents` to `path` as a file with `mode`, creating or replacing it
/// without this process ever holding the final inode open for writing.
pub fn write_executable(
    path: impl AsRef<Path>,
    contents: impl AsRef<[u8]>,
    mode: u32,
) -> io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = path.as_ref();
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other(format!("{} names no file", path.display())))?;
    // The staging inode is written here but never executed.
    let staging = path.with_file_name(format!(
        ".{}.staging.{}.{}",
        name.to_string_lossy(),
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&staging, contents)?;
    let installed = install(&staging, path, mode);
    let removed = std::fs::remove_file(&staging);
    installed?;
    removed
}

/// `std::fs::copy`, except that an executable source is copied by a child.
pub fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<u64> {
    let (from, to) = (from.as_ref(), to.as_ref());
    let metadata = std::fs::metadata(from)?;
    let mode = metadata.permissions().mode() & 0o7777;
    if mode & 0o111 == 0 {
        return std::fs::copy(from, to);
    }
    install(from, to, mode)?;
    Ok(metadata.len())
}

fn install(from: &Path, to: &Path, mode: u32) -> io::Result<()> {
    let output = Command::new("install")
        .arg("-m")
        .arg(format!("{mode:o}"))
        .arg("-T")
        .arg("--")
        .arg(from)
        .arg(to)
        .output()?;
    if output.status.success() {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "install {} -> {} failed ({}): {}",
        from.display(),
        to.display(),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::Command;
    use std::process::Stdio;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use super::*;

    const SCRIPT: &str = "#!/bin/sh\nexit 0\n";

    /// A fresh directory under the temporary root, removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "exec-safe-fs-{label}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Negative control: the hazard is real on this host. A write descriptor
    /// that a live child holds, exactly what a sibling's fork leaves behind,
    /// makes the file unexecutable until that child is gone.
    #[test]
    fn a_write_descriptor_held_by_a_child_makes_the_file_busy() {
        let dir = Scratch::new("busy");
        let path = dir.0.join("busy");
        let file = std::fs::File::create(&path).unwrap();
        (&file).write_all(SCRIPT.as_bytes()).unwrap();
        file.set_permissions(std::fs::Permissions::from_mode(0o700))
            .unwrap();
        let mut holder = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::from(file))
            .spawn()
            .unwrap();
        let busy = Command::new(&path).status();
        holder.kill().unwrap();
        holder.wait().unwrap();
        assert_eq!(
            busy.map_err(|error| error.kind()).err(),
            Some(io::ErrorKind::ExecutableFileBusy)
        );
        // With the holder gone the file runs. This test wrote it in-process,
        // so a sibling test's spawn may still hold a copy for the instant
        // before that child execs; that is the hazard itself, so retry briefly.
        let mut last = None;
        for _ in 0..200 {
            match Command::new(&path).status() {
                Err(error) if error.kind() == io::ErrorKind::ExecutableFileBusy => {
                    last = Some(error);
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                other => {
                    assert!(other.unwrap().success());
                    return;
                }
            }
        }
        panic!("still busy after the holder exited: {last:?}");
    }

    /// Regression: while other threads spawn continuously, every file created
    /// by these helpers executes at once. Each fresh executable is run
    /// immediately, which is where the in-process write failed.
    #[test]
    fn helpers_create_executables_that_are_never_busy_under_concurrent_spawns() {
        let dir = Scratch::new("regression");
        let stop = Arc::new(AtomicBool::new(false));
        let spawners = (0..8)
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
        for index in 0..200 {
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

    #[test]
    fn helpers_keep_mode_and_bytes_and_leave_nothing_beside_the_target() {
        let dir = Scratch::new("shape");
        let executable = dir.0.join("executable");
        write_executable(&executable, SCRIPT, 0o750).unwrap();
        assert_eq!(std::fs::read(&executable).unwrap(), SCRIPT.as_bytes());
        assert_eq!(
            std::fs::metadata(&executable).unwrap().mode() & 0o7777,
            0o750
        );
        // Replacing an executable yields a new inode, so no descriptor to the
        // old one, held here or inherited by any child, names the new file.
        let first_inode = std::fs::metadata(&executable).unwrap().ino();
        write_executable(&executable, "#!/bin/sh\nexit 3\n", 0o755).unwrap();
        let replaced = std::fs::metadata(&executable).unwrap();
        assert_ne!(replaced.ino(), first_inode, "replacement reused the inode");
        assert_eq!(replaced.mode() & 0o7777, 0o755);
        write_executable(&executable, SCRIPT, 0o750).unwrap();
        let copied = dir.0.join("copied");
        assert_eq!(copy(&executable, &copied).unwrap(), SCRIPT.len() as u64);
        assert_eq!(std::fs::read(&copied).unwrap(), SCRIPT.as_bytes());
        assert_eq!(std::fs::metadata(&copied).unwrap().mode() & 0o7777, 0o750);
        let plain = dir.0.join("plain");
        std::fs::write(&plain, "data\n").unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o640)).unwrap();
        let plain_copy = dir.0.join("plain-copy");
        assert_eq!(copy(&plain, &plain_copy).unwrap(), 5);
        assert_eq!(
            std::fs::metadata(&plain_copy).unwrap().mode() & 0o7777,
            0o640
        );
        // A failed write is an error, not a silent partial file.
        let missing = dir.0.join("no/such/dir/target");
        assert!(write_executable(&missing, SCRIPT, 0o700).is_err());
        assert!(!missing.exists());
        // Only the final files remain: staging is removed.
        let names = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            names,
            ["copied", "executable", "plain", "plain-copy"]
                .into_iter()
                .map(String::from)
                .collect()
        );
    }
}
