// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

use std::fs;
use std::io;
use std::os::unix;
use std::path::Path;
use std::path::PathBuf;

use tempfile::TempDir;

/// Represents a temporary chroot environment. This shall only contain the files
/// necessary to reproduce an execution.
pub struct TempChroot {
    dir: TempDir,
}

impl TempChroot {
    /// Creates a temporary directory that the tracee will chroot to. It is here
    /// where we can hard link files needed by the tracee.
    pub fn new_in(data_dir: &Path) -> io::Result<Self> {
        let dir = TempDir::new_in(data_dir)?;
        Ok(Self { dir })
    }

    /// Returns the path to the chroot directory.
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Returns a path relative to the chroot directory.
    pub fn relpath(&self, path: &Path) -> PathBuf {
        self.path().join(path.strip_prefix("/").unwrap_or(path))
    }

    /// Hard link a file path into the chroot directory. Any intermediate
    /// directory that the path has will also be created. `link` must be an
    /// absolute path.
    ///
    /// NOTE: The `path` should be a path on the same file system. Otherwise this
    /// will fail. This should be the case if the path being hard linked has
    /// already been copied to the recording directory.
    ///
    /// NOTE: This should only be used for paths we are certain will not be
    /// modified by a replay. Otherwise, we could end up currupting a previously
    /// recorded file.
    ///
    /// NOTE: This does not handle symlinks. If `path` is a symlink, the real
    /// path is not hard linked, just the symlink itself.
    pub fn hard_link(&self, original: &Path, link: &Path) -> io::Result<()> {
        let link = self.relpath(link);

        if let Some(dir) = link.parent() {
            fs::create_dir_all(dir)?;
        }

        fs::hard_link(original, link)
    }

    /// Create symlink a file path into the chroot directory. Any intermediate
    /// directory that the path has will also be created. `link` must be an
    /// absolute path.
    ///
    /// NOTE: original path must exist in chroot directory, otherwise this
    /// could create a dead sym link.
    pub fn symlink(&self, original: &Path, link: &Path) -> io::Result<()> {
        let link = self.relpath(link);

        if let Some(dir) = link.parent() {
            fs::create_dir_all(dir)?;
        }

        unix::fs::symlink(original, link)
    }

    /// Copies a file into the chroot.
    pub fn copy(&self, from: &Path, to: &Path) -> io::Result<()> {
        let to = self.relpath(to);

        if let Some(dir) = to.parent() {
            fs::create_dir_all(dir)?;
        }

        fs::copy(from, to)?;
        Ok(())
    }

    /// Like `copy`, but copies `path` to `{chroot}/{path}`.
    pub fn copy_same(&self, path: &Path) -> io::Result<()> {
        self.copy(path, path)
    }

    /// Creates a directory in the chroot.
    pub fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        fs::create_dir_all(self.relpath(path))
    }

    /// Deletes the chroot. The `Drop` impl will also take care of this, but
    /// calling this explicitly gives a chance to catch any errors.
    pub fn remove(self) -> io::Result<()> {
        self.dir.close()
    }
}
