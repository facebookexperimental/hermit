/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Analyze's workspace: the directory of every trial's schedules, logs,
//! outputs and reproducer, removed when analyze exits unless it is kept.

use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context as _;
use colored::Colorize;

/// The name every workspace starts with.
const PREFIX: &str = "hermit_analyze";

/// What becomes of a workspace when analyze is done with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retention {
    /// Removed.
    Remove,
    /// Kept, because of the option named (`--keep-tmp`, `--verbose`,
    /// `--trial-log`), or
    /// because the command always keeps it (`hermit bisect`).
    Keep { because: Option<&'static str> },
}

/// A fresh workspace, removed or kept when this is dropped, as its
/// [`Retention`] says, and the last line about it says which.
#[derive(Debug)]
pub struct Workspace {
    path: PathBuf,
    retention: Retention,
    closed: bool,
}

impl Workspace {
    /// Create a fresh workspace in `tmp_dir` (`--tmp-dir`), creating that
    /// directory if it is missing, or else in the system's temporary
    /// directory ([`std::env::temp_dir`]: `$TMPDIR`, or `/tmp` when TMPDIR is
    /// unset). Nothing names `/tmp` itself.
    pub fn create(tmp_dir: Option<&Path>, retention: Retention) -> anyhow::Result<Self> {
        let mut builder = tempfile::Builder::new();
        builder.prefix(PREFIX);
        let dir = match tmp_dir {
            Some(parent) => {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("cannot create --tmp-dir {}", parent.display()))?;
                builder.tempdir_in(parent).with_context(|| {
                    format!(
                        "cannot create hermit analyze's workspace in --tmp-dir {}",
                        parent.display()
                    )
                })?
            }
            None => builder.tempdir_in(std::env::temp_dir()).with_context(|| {
                format!(
                    "cannot create hermit analyze's workspace in {} (TMPDIR, or /tmp when it is \
                     unset); pass --tmp-dir=DIR to create it elsewhere",
                    std::env::temp_dir().display()
                )
            })?,
        };
        Ok(Self {
            path: dir.keep(),
            retention,
            closed: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Keep the workspace, `because` (said on the line that names it), if it
    /// was to be removed.
    pub fn keep(&mut self, because: &'static str) {
        if self.retention == Retention::Remove {
            self.retention = Retention::Keep {
                because: Some(because),
            };
        }
    }

    /// Remove the workspace, or keep it, and return the line that says
    /// which, once: the paths analyze printed name the workspace.
    fn close(&mut self) -> Option<String> {
        if std::mem::replace(&mut self.closed, true) {
            return None;
        }
        let path = self.path.display();
        Some(match self.retention {
            Retention::Keep { because: None } => format!(":: Kept the workspace at {path}."),
            Retention::Keep {
                because: Some(option),
            } => format!(":: Kept the workspace at {path} ({option})."),
            Retention::Remove => match std::fs::remove_dir_all(&self.path) {
                Ok(()) => format!(
                    ":: Removed the workspace {path}, and with it every schedule, log and \
                     reproducer analyze names in it (--keep-tmp keeps it)."
                ),
                Err(error) => {
                    let quoted = shell_words::quote(&self.path.to_string_lossy()).into_owned();
                    let command = if error.kind() == std::io::ErrorKind::PermissionDenied {
                        format!("chmod -R u+w {quoted} && rm -rf {quoted}")
                    } else {
                        format!("rm -rf {quoted}")
                    };
                    format!(
                        ":: {} could not remove the workspace {path}: {error}. Remove it with: \
                         {command}",
                        "WARNING:".yellow().bold(),
                    )
                }
            },
        })
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        if let Some(line) = self.close() {
            note(&line);
        }
    }
}

/// Write `line` to stderr. Unlike `eprintln!`, this does not panic when
/// stderr's reader has gone (the terminal a SIGHUP reports), which on the way
/// out would end analyze with a panic instead of its status, or abort it
/// during an unwind.
pub fn note(line: &str) {
    let _ = writeln!(std::io::stderr(), "{line}");
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    /// The workspace and everything in it go when it is dropped; the
    /// `--tmp-dir` it was created in, and a file beside it there (such as a
    /// `--report-file`), stay.
    #[test]
    fn a_workspace_is_removed_when_dropped() {
        let parent = tempfile::tempdir().unwrap();
        let tmp_dir = parent.path().join("not-yet-created");
        let report = tmp_dir.join("report.json");
        let workspace = Workspace::create(Some(&tmp_dir), Retention::Remove).unwrap();
        let path = workspace.path().to_path_buf();
        assert_eq!(path.parent(), Some(tmp_dir.as_path()));
        assert!(
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(PREFIX),
            "{}",
            path.display()
        );
        std::fs::create_dir(path.join("run1_target_out")).unwrap();
        std::fs::write(path.join("run1_target.repro"), "hermit run\n").unwrap();
        std::fs::write(&report, "{}").unwrap();
        drop(workspace);
        assert!(!path.exists(), "{}", path.display());
        assert_eq!(std::fs::read_to_string(&report).unwrap(), "{}");
        let left: Vec<_> = std::fs::read_dir(&tmp_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(left, ["report.json"]);
    }

    /// A kept workspace stays with what it holds, and the line about it says
    /// why.
    #[test]
    fn a_kept_workspace_stays() {
        for (because, said) in [
            (Some("--keep-tmp"), " (--keep-tmp)."),
            (Some("--verbose"), " (--verbose)."),
            (None, "."),
        ] {
            let parent = tempfile::tempdir().unwrap();
            let mut workspace =
                Workspace::create(Some(parent.path()), Retention::Keep { because }).unwrap();
            let path = workspace.path().to_path_buf();
            std::fs::write(path.join("run1_target.repro"), "hermit run\n").unwrap();
            assert_eq!(
                workspace.close().unwrap(),
                format!(":: Kept the workspace at {}{said}", path.display())
            );
            assert_eq!(workspace.close(), None);
            drop(workspace);
            assert!(
                path.join("run1_target.repro").is_file(),
                "{}",
                path.display()
            );
        }
    }

    /// A workspace that cannot be removed is named, with the command that
    /// removes it, and nothing panics. Root ignores the permission that
    /// stops the removal here, so the test has nothing to check as root.
    #[test]
    fn a_workspace_that_cannot_be_removed_is_named() {
        // SAFETY: geteuid(2) has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let parent = tempfile::tempdir().unwrap();
        let mut workspace = Workspace::create(Some(parent.path()), Retention::Remove).unwrap();
        let path = workspace.path().to_path_buf();
        let locked = path.join("run1_target_out");
        std::fs::create_dir(&locked).unwrap();
        std::fs::write(locked.join("stdout"), "").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
        let line = workspace.close().unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        let quoted = shell_words::quote(&path.to_string_lossy()).into_owned();
        assert!(
            line.contains(&format!(
                "could not remove the workspace {}",
                path.display()
            )) && line.contains(&format!(
                "Remove it with: chmod -R u+w {quoted} && rm -rf {quoted}"
            )),
            "{line}"
        );
        drop(workspace);
        assert!(
            path.is_dir(),
            "a closed workspace was removed again on drop"
        );
    }

    /// A `--tmp-dir` that cannot be created is named in the error.
    #[test]
    fn an_uncreatable_tmp_dir_is_named() {
        let parent = tempfile::tempdir().unwrap();
        let file = parent.path().join("a-file");
        std::fs::write(&file, "").unwrap();
        let error = Workspace::create(Some(&file.join("below")), Retention::Remove).unwrap_err();
        assert!(
            format!("{error:#}").contains(&format!("cannot create --tmp-dir {}", file.display())),
            "{error:#}"
        );
    }
}
