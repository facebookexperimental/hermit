/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs::File;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Error;
use clap::Parser;
use hermit::Backend;
use tracing::metadata::LevelFilter;

use super::tracing::CappedWriter;
use super::tracing::LogBudget;
use super::tracing::TracingGuard;
use super::tracing::init_file_tracing;
use super::tracing::init_stderr_tracing;
use super::tracing::log_max_bytes;
use super::tracing::parse_max_log_bytes;

/// The target named by controller diagnostics written to `--log-file`. It is
/// not a tracing target: these lines are written directly, before tracing
/// starts, so `--log`/`RUST_LOG` filtering does not apply to them.
const CONTROLLER_TARGET: &str = "hermit::controller";

/// Hermit provides a sandbox for deterministic and reproducible execution.
/// Arbitrary programs run inside (guests) become deterministic
/// functions of their controlled inputs. Configuration flags control the
/// initial environment.
///
/// See the "run" and "record" subcommands to run programs within hermit.
/// In both modes, the host file system is visible
/// to the command run inside hermit, and the results will depend on the contents
/// (but not timestamps or inode numbers) of those inputs.
///
/// In run mode, the guest's clock is virtual and starts at the epoch given by
/// `--epoch` or `HERMIT_EPOCH`. If neither is given, `hermit run` starts it at
/// the host's current time, so time values differ from one invocation to the
/// next. Pass a fixed epoch (for example `--epoch=2026-01-01T00:00:00Z`) when
/// two runs must produce identical time values.
///
/// In run and record mode, by default (`--network=local`), the guest gets its
/// own network namespace with only a loopback interface. `--network=host`
/// exposes the host network, which gives up isolation and reproducibility. A
/// recording stores its choice, and `hermit replay --autopilot` replays in the
/// same network; `hermit record start --verify-with-gdbex` defaults to, and
/// requires, `--network=host`. The DBT backend does not apply `--network` yet:
/// its guest sees the host network
/// (https://github.com/rrnewton/hermit/issues/3543).
///
/// In record mode, the guest's system call results are saved so that
/// `hermit replay` can reproduce the execution. Recordings are stored in
/// `$XDG_CACHE_HOME/hermit`, which is `~/.cache/hermit` when XDG_CACHE_HOME is
/// unset; `--data-dir` or `HERMIT_DATA_DIR` selects another directory.
///
/// Below are options common to all subcommands.
#[derive(Debug, Parser, Clone)]
pub struct GlobalOpts {
    /// The verbosity level of log output.
    #[clap(short, long, value_name = "LEVEL", env = "HERMIT_LOG")]
    pub log: Option<LevelFilter>,

    /// Log to a file instead of the terminal. The path is resolved on the HOST,
    /// exactly like a shell redirect, not inside the container the guest runs in.
    #[clap(long, value_name = "FILE", env = "HERMIT_LOG_FILE")]
    pub log_file: Option<PathBuf>,

    /// Stop the run (exit 123) once hermit's tracing output exceeds SIZE bytes.
    ///
    /// Counted: every byte hermit's tracing writes to stderr, to --log-file, or
    /// to the per-run logs of `run --verify` and `record --verify`, as one total
    /// for the whole invocation, measured before the HERMIT_LOG_MAX_BYTES file
    /// truncation.
    ///
    /// Not counted: stdout; the guest's own output, including the DETLOG
    /// records the SaBRe backend writes from inside the guest; hermit messages
    /// that do not go through tracing, such as error reports, the `--verify`
    /// comparison and retention report, and everything `log-diff` prints.
    ///
    /// At the crossing hermit drops the write that crossed and every tracing
    /// write after it. It waits at most 50 ms for tracing writes already in
    /// progress to finish, then tries once, without waiting, to write "log
    /// output exceeded --max-log-bytes=SIZE" followed by "HERMIT_LOG_CAP
    /// class=log-cap" to stderr and to --log-file, and exits 123. When that
    /// message is written, no tracing output follows it; the guest's output
    /// and hermit's error report, which are not tracing output, can. The
    /// message is best effort: it is missing when, for example, stderr is a
    /// full pipe or has no reader, or a tracing write is still in progress
    /// after the 50 ms. A hermit process killed in the middle of a tracing
    /// write never finishes it, so if the cap is crossed later in that
    /// invocation, the message is missing. The exit status is then the only
    /// signal. Where the flag is accepted the guest dies with hermit, because
    /// it runs inside hermit's PID namespace, which dies with hermit.
    ///
    /// Refused with exit 122, before any guest starts, where the guest could
    /// outlive hermit: --backend=dbt; --no-namespace with every backend,
    /// --gdbserver runs included, and so the trials of `analyze` and `bisect`
    /// when their run arguments include --no-namespace; and `hermit
    /// --backend=sabre strace`.
    ///
    /// SIZE is a byte count with an optional K/M/G/T suffix in powers of 1024,
    /// e.g. 8G. Omit the flag for no cap.
    #[clap(long, value_name = "SIZE", value_parser = parse_max_log_bytes)]
    pub max_log_bytes: Option<u64>,

    /// Process-shared byte total for `--max-log-bytes`, created by
    /// [`GlobalOpts::prepare_log_budget`] before any container fork so the cap
    /// is one total for the invocation. See [`LogBudget`].
    #[clap(skip)]
    pub(crate) log_budget: Option<LogBudget>,

    /// The log file, already opened in the HOST's filename namespace.
    ///
    /// WHY THE FILE IS CARRIED INSTEAD OF RE-OPENED FROM THE PATH. Tracing has to be
    /// initialized INSIDE the container -- see `init_tracing`, whose reason is that
    /// the tracer may create a thread -- and the container mounts a fresh writable
    /// /tmp over its root (container.rs: "A fresh writable /tmp is mounted separately
    /// for ordinary scratch files"). So opening the path at that point resolves it in
    /// the GUEST namespace. For `--log-file /tmp/x.log` the create then SUCCEEDS, into
    /// the guest's tmpfs, and the file dies with the container: exit 0, no log, no
    /// warning. Measured 2026-08-20; one debugging session was lost to it.
    ///
    /// An open file descriptor is unaffected by a later mount-namespace change, so
    /// opening on the host and carrying the handle in is mechanically what a shell
    /// redirect does. Only the OPEN moves out of the container; tracing itself is
    /// still initialized inside, so the stated reason for that is untouched.
    ///
    /// `Arc` because `GlobalOpts` is `Clone` (verify clones it per run) and because
    /// each `init_tracing` needs its own owned `File`, produced with `try_clone`.
    #[clap(skip)]
    pub log_file_handle: Option<Arc<File>>,

    /// Select the process instrumentation backend. This is a global option and
    /// must come before the subcommand, e.g. `hermit --backend ptrace run ...`.
    #[clap(long, value_enum, value_name = "BACKEND")]
    pub backend: Option<Backend>,

    /// UNSAFE: start the guest even though hermit inherited a seccomp filter from whatever
    /// launched it, such as a container runtime's default profile. Without this option hermit
    /// refuses (exit 122) when /proc/self/status reports such a filter: the filter is an input
    /// hermit does not record, and it can make system calls fail in ways the guest observes, so
    /// the run may not reproduce where the filter differs. With it, hermit prints a warning and
    /// records the filter's mode and count in the INFO log. It may be given before or after the subcommand; a run config holds it as
    /// `global: {unsafe-ignore-host-seccomp: true}`.
    #[clap(long, global = true)]
    pub unsafe_ignore_host_seccomp: bool,
}

impl GlobalOpts {
    /// Open `--log-file` in the HOST's filename namespace.
    ///
    /// Call this from `main`, before any container exists. That placement is the
    /// whole point: it is the moment a shell would perform `> file`.
    ///
    /// Reports the path on failure instead of proceeding. A run that was asked for a
    /// log and produces neither the log nor an error is indistinguishable from a run
    /// whose log was legitimately empty, and the person debugging cannot tell which.
    pub fn open_log_file(&mut self) -> Result<(), Error> {
        if let Some(path) = &self.log_file {
            let file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
                .open(path)
                .with_context(|| {
                    format!("cannot open --log-file {} for writing", path.display())
                })?;
            self.log_file_handle = Some(Arc::new(file));
        }
        Ok(())
    }

    /// Create the shared `--max-log-bytes` counter. Call from `main` before any
    /// container exists, for the same fork reason as `open_log_file`.
    pub fn prepare_log_budget(&mut self) -> Result<(), Error> {
        if let (Some(limit), None) = (self.max_log_bytes, &self.log_budget) {
            self.log_budget = Some(
                LogBudget::new(limit).context("cannot map the shared --max-log-bytes counter")?,
            );
        }
        Ok(())
    }

    /// The budget public log sinks charge, or `None` when uncapped.
    ///
    /// A caller that never ran `prepare_log_budget` still gets the cap it asked
    /// for, counted per process rather than per invocation: silently dropping a
    /// requested bound is the failure mode this flag exists to remove.
    pub(crate) fn log_budget(&self) -> Option<LogBudget> {
        match (&self.log_budget, self.max_log_bytes) {
            (Some(budget), _) => Some(budget.clone()),
            (None, Some(limit)) => Some(
                LogBudget::new(limit)
                    .unwrap_or_else(|e| panic!("cannot map the --max-log-bytes counter: {e}")),
            ),
            (None, None) => None,
        }
    }

    /// Report controller context before tracing starts, using the selected host
    /// destination without reopening its path or creating a tracing thread.
    ///
    /// In `--log-file` the line is written in the shape of a DEBUG tracing
    /// event (wall-clock timestamp, level, target), because that file is a
    /// record stream: `hermit log-diff` splits it on timestamps and refuses any
    /// record without a level tag, so a bare line made every comparison of two
    /// such files stop at line 0
    /// (<https://github.com/rrnewton/hermit/issues/3410>).
    ///
    /// DEBUG, not INFO, on purpose. The INFO stream and the DETLOG/COMMIT
    /// subset are the guest and Detcore evidence that comparisons count toward
    /// "nonzero compared messages"; a harness line written into every log must
    /// not satisfy that, or two logs holding nothing but this line would be
    /// reported as a match. Plain stderr keeps the bare line.
    pub(crate) fn write_controller_diagnostic(
        &self,
        message: std::fmt::Arguments<'_>,
    ) -> Result<(), Error> {
        if let Some(handle) = &self.log_file_handle {
            use tracing_subscriber::fmt::format::Writer;
            use tracing_subscriber::fmt::time::FormatTime;
            let mut timestamp = String::new();
            tracing_subscriber::fmt::time::SystemTime
                .format_time(&mut Writer::new(&mut timestamp))
                .map_err(|_| anyhow::anyhow!("cannot format the controller diagnostic time"))?;
            writeln!(
                &**handle,
                "{timestamp} DEBUG {CONTROLLER_TARGET}: {message}"
            )
            .context("cannot write to the host log file")?;
        } else if hermit::nonwaiting_write::diagnostics_must_not_wait() {
            // Under --max-log-bytes, termination must not depend on this
            // write: RetryingStderr below waits as long as a full stderr pipe
            // stays unread. One attempt that cannot block; the line is dropped
            // when stderr cannot take it at once.
            super::tracing::write_stderr_diagnostic(&format!("{message}\n"));
        } else {
            // A stopped stderr reader must not replace the command's primary
            // exit status. This shares the existing invocation-wide deadline
            // with later error reports and preserves the inherited fd flags.
            let _ = writeln!(detcore::util::RetryingStderr, "{message}");
        }
        Ok(())
    }

    /// Initalizes tracing. If using a container, this must be done *inside* of
    /// the container because the tracer may create a new thread.
    ///
    /// The file itself is NOT opened here when it came from `--log-file`; see
    /// `log_file_handle` for why opening at this point resolves the path in the
    /// guest namespace and silently loses it.
    #[must_use = "This function returns a guard that should not be immediately dropped"]
    pub fn init_tracing(&self) -> Option<TracingGuard> {
        self.init_tracing_for_backend(self.backend.unwrap_or_default())
    }

    /// Initialize tracing with the backend that will actually execute the guest.
    ///
    /// This can differ from the global `--backend` selection: e9patch is
    /// preprocessing that runs on the ptrace runtime. `run` passes its runtime
    /// backend here rather than the raw global value when deciding whether a
    /// Linux PID slot is needed.
    #[must_use = "This function returns a guard that should not be immediately dropped"]
    pub fn init_tracing_for_backend(&self, backend: Backend) -> Option<TracingGuard> {
        let guard = self.install_tracing_for_backend(backend);
        // A run admitted under --unsafe-ignore-host-seccomp says so in every
        // log it writes, the run-evidence log included, from its first record.
        if let Some(host) = crate::host_seccomp::unverified() {
            ::tracing::info!(
                target: "hermit::host_seccomp",
                "unverified host seccomp filter: {} (admitted by --unsafe-ignore-host-seccomp)",
                host.describe()
            );
        }
        guard
    }

    fn install_tracing_for_backend(&self, backend: Backend) -> Option<TracingGuard> {
        if let Some(handle) = &self.log_file_handle {
            // Each subscriber needs an owned File; `try_clone` dups the descriptor,
            // so every run writes through the same host-side open file.
            let file_writer = handle
                .try_clone()
                .expect("cannot duplicate the host log file descriptor");
            let limit = log_max_bytes().unwrap_or_else(|e| panic!("{e}"));
            let file_writer = CappedWriter::file(file_writer, limit, self.log_budget());
            Some(init_file_tracing(self.log, file_writer))
        } else if let Some(path) = &self.log_file {
            // An internal caller set the path directly rather than going through
            // `open_log_file` -- today that is verify's double-run setup, which
            // creates its own temp files and is measured NOT to hit the namespace
            // problem. Keep the historical behaviour for those, rather than changing
            // a path this task did not investigate.
            let file_writer = File::create(path).expect("Failed to open log file");
            // Bounded so a run that makes no progress cannot fill the disk: a
            // livelocked guest logged 928.8 GiB over 11.7 hours before this.
            // The bound is on the LOG only; the run is unaffected.
            // A malformed bound is fatal rather than silently defaulted, so a
            // typo in the value meant to DISABLE the bound cannot quietly
            // re-enable it.
            let limit = log_max_bytes().unwrap_or_else(|e| panic!("{e}"));
            let file_writer = CappedWriter::file(file_writer, limit, self.log_budget());
            Some(init_file_tracing(self.log, file_writer))
        } else {
            init_stderr_tracing(self.log, backend, self.log_budget());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn log_options(path: PathBuf) -> GlobalOpts {
        GlobalOpts {
            log: None,
            log_file: Some(path),
            max_log_bytes: None,
            log_budget: None,
            log_file_handle: None,
            unsafe_ignore_host_seccomp: false,
            backend: None,
        }
    }

    #[test]
    fn host_log_open_refuses_a_symlink_without_changing_its_target() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.log");
        let link = directory.path().join("requested.log");
        std::fs::write(&target, b"do not truncate").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let mut options = log_options(link);

        let error = options
            .open_log_file()
            .expect_err("--log-file must never follow a symlink");
        assert!(error.to_string().contains("cannot open --log-file"));
        assert_eq!(std::fs::read(target).unwrap(), b"do not truncate");
    }

    #[test]
    fn host_log_open_creates_a_regular_file_and_keeps_the_opened_object() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("requested.log");
        let opened_path = directory.path().join("opened.log");
        let mut options = log_options(path.clone());

        options.open_log_file().unwrap();
        assert!(std::fs::metadata(&path).unwrap().is_file());
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        let cloned_options = options.clone();
        drop(options);
        std::fs::rename(&path, &opened_path).unwrap();
        std::fs::write(&path, b"replacement must not change").unwrap();
        cloned_options
            .write_controller_diagnostic(format_args!("controller context"))
            .unwrap();
        let mut held = cloned_options
            .log_file_handle
            .as_ref()
            .unwrap()
            .try_clone()
            .unwrap();
        drop(cloned_options);
        held.write_all(b"written through the held descriptor")
            .unwrap();

        let written = String::from_utf8(std::fs::read(opened_path).unwrap()).unwrap();
        let (timestamp, rest) = written.split_once(" DEBUG ").unwrap();
        assert!(
            regex::Regex::new(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d+Z$")
                .unwrap()
                .is_match(timestamp),
            "{written:?}"
        );
        assert_eq!(
            rest,
            "hermit::controller: controller context\nwritten through the held descriptor"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"replacement must not change"
        );

        // An explicit log destination that cannot accept the report must refuse,
        // rather than silently falling back to the compared guest stderr.
        let mut read_only_options = log_options(path.clone());
        read_only_options.log_file_handle = Some(Arc::new(File::open(&path).unwrap()));
        let error = read_only_options
            .write_controller_diagnostic(format_args!("must not disappear"))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot write to the host log file")
        );
        assert_eq!(std::fs::read(path).unwrap(), b"replacement must not change");
    }

    #[test]
    fn host_log_open_truncates_an_existing_regular_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("existing.log");
        std::fs::write(&path, b"old log contents that must be truncated").unwrap();
        let mut options = log_options(path.clone());

        options.open_log_file().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        let mut held = options
            .log_file_handle
            .as_ref()
            .unwrap()
            .try_clone()
            .unwrap();
        held.write_all(b"new log").unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"new log");
    }
}
