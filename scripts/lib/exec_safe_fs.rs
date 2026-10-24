//! Create files that this process, or a child of it, will execute.
//!
//! Linux refuses to execute a file while any process holds it open for
//! writing (ETXTBSY). Rust opens files close-on-exec, but that does not close
//! the window: a sibling thread that spawns while this thread still holds the
//! write descriptor forks a child that owns a duplicate until the child itself
//! execs. A parallel test binary spawns constantly, so an executable written
//! and then run in-process is refused intermittently. With eight threads
//! spawning `true`, 79, 95 and 134 of 2000 freshly written executables were
//! busy; created through `install`, 0 of 6000 were.
//!
//! Nothing here opens an executable target for writing in this process. A
//! short-lived `install` child, which never forks, creates it instead, so no
//! descriptor any other child can inherit ever refers to it. GNU `install`
//! unlinks an existing destination first, so replacing an executable also
//! yields a new inode that no earlier descriptor names.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

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

/// Write `contents` to `path` as a file with `mode`, creating or replacing it
/// without this process ever holding the final inode open for writing. Only
/// test fixtures write executables from bytes, so outside a test build this
/// would be dead code, which the rust-script clippy gate refuses.
#[cfg(test)]
pub fn write_executable(
    path: impl AsRef<Path>,
    contents: impl AsRef<[u8]>,
    mode: u32,
) -> io::Result<()> {
    let path = path.as_ref();
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    use std::io::Write;
    // The staging inode is written here but never executed.
    let mut staging = tempfile::NamedTempFile::new_in(parent)?;
    staging.write_all(contents.as_ref())?;
    staging.flush()?;
    install(staging.path(), path, mode)
}

/// The executable form of `set_permissions` for a file this process has
/// already written: a child installs its bytes as a fresh sibling inode with
/// `mode`, which then replaces it. A descriptor to the written inode, held
/// here or inherited by any child, no longer names the file that runs.
pub fn set_executable(path: impl AsRef<Path>, mode: u32) -> io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = path.as_ref();
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other(format!("{} names no file", path.display())))?;
    let staging = path.with_file_name(format!(
        ".{}.exec-safe.{}.{}",
        name.to_string_lossy(),
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    install(path, &staging, mode)?;
    std::fs::rename(&staging, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&staging);
    })
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
    use std::process::Stdio;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use super::*;

    const SCRIPT: &str = "#!/bin/sh\nexit 0\n";

    /// Negative control: the hazard is real on this host. A write descriptor
    /// that a live child holds, exactly what a sibling's fork leaves behind,
    /// makes the file unexecutable until that child is gone.
    #[test]
    fn a_write_descriptor_held_by_a_child_makes_the_file_busy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("busy");
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
        let dir = tempfile::tempdir().unwrap();
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
        let source = dir.path().join("source");
        write_executable(&source, SCRIPT, 0o755).unwrap();
        let mut failures = Vec::new();
        let mut run = |path: &Path, want: i32| match Command::new(path).status() {
            Ok(status) if status.code() == Some(want) => {}
            other => failures.push(format!("{}: {other:?}", path.display())),
        };
        for index in 0..500 {
            let written = dir.path().join(format!("written-{index}"));
            write_executable(&written, SCRIPT, 0o700).unwrap();
            run(&written, 0);
            // Replacing in place must not reuse the executed inode either.
            write_executable(&written, "#!/bin/sh\nexit 3\n", 0o700).unwrap();
            run(&written, 3);
            let copied = dir.path().join(format!("copied-{index}"));
            copy(&source, &copied).unwrap();
            run(&copied, 0);
            // The in-process write is the hazard; set_executable replaces it.
            let chmodded = dir.path().join(format!("chmodded-{index}"));
            std::fs::write(&chmodded, "#!/bin/sh\nexit 4\n").unwrap();
            set_executable(&chmodded, 0o700).unwrap();
            run(&chmodded, 4);
        }
        stop.store(true, Ordering::Relaxed);
        for spawner in spawners {
            spawner.join().unwrap();
        }
        assert!(failures.is_empty(), "{failures:?}");
    }

    #[test]
    fn copies_keep_mode_and_bytes_and_plain_files_stay_in_process() {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("executable");
        write_executable(&executable, SCRIPT, 0o750).unwrap();
        let copied = dir.path().join("copied");
        assert_eq!(copy(&executable, &copied).unwrap(), SCRIPT.len() as u64);
        assert_eq!(std::fs::read(&copied).unwrap(), SCRIPT.as_bytes());
        assert_eq!(std::fs::metadata(&copied).unwrap().mode() & 0o7777, 0o750);
        let plain = dir.path().join("plain");
        std::fs::write(&plain, "data\n").unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o640)).unwrap();
        let plain_copy = dir.path().join("plain-copy");
        assert_eq!(copy(&plain, &plain_copy).unwrap(), 5);
        assert_eq!(
            std::fs::metadata(&plain_copy).unwrap().mode() & 0o7777,
            0o640
        );
        let chmodded = dir.path().join("chmodded");
        std::fs::write(&chmodded, SCRIPT).unwrap();
        let written_inode = std::fs::metadata(&chmodded).unwrap().ino();
        set_executable(&chmodded, 0o711).unwrap();
        let replaced = std::fs::metadata(&chmodded).unwrap();
        assert_ne!(
            replaced.ino(),
            written_inode,
            "the written inode must not be the one run"
        );
        assert_eq!(replaced.mode() & 0o7777, 0o711);
        assert_eq!(std::fs::read(&chmodded).unwrap(), SCRIPT.as_bytes());
        assert!(set_executable(dir.path().join("absent"), 0o700).is_err());
        // A failed install is an error, not a silent partial file.
        let missing = dir.path().join("no/such/dir/target");
        assert!(write_executable(&missing, SCRIPT, 0o700).is_err());
        assert!(!missing.exists());
        // Only the final file remains beside the target: staging is removed.
        let names = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            names,
            ["chmodded", "copied", "executable", "plain", "plain-copy"]
                .into_iter()
                .map(String::from)
                .collect()
        );
    }
}
