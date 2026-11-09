//! Scoped complete-output capture; no process, wait or RPC owner.
use std::io;
use std::os::fd::OwnedFd;
use std::sync::Arc;

use reverie::ExitStatus;
use reverie::process::ChildStderr;
use reverie::process::ChildStdout;
use reverie::process::Output;
use reverie::process::ParentStdio;
use reverie_inguest::launcher::exec_stop::CleanupReceipt;
use tokio::io::AsyncReadExt;
use tokio::time::Instant;
use tokio::time::timeout_at;

#[derive(Clone, Copy, Debug)]
pub(super) struct Limits {
    pub stdout: usize,
    pub stderr: usize,
    pub total: usize,
}
impl Limits {
    fn unbounded() -> Self {
        Self {
            stdout: usize::MAX,
            stderr: usize::MAX,
            total: usize::MAX,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stream {
    Stdout,
    Stderr,
}
#[derive(Debug)]
pub(super) struct Overflow {
    pub stream: Stream,
    pub read_bytes: usize,
    pub retained_bytes: usize,
}
struct Reader<R> {
    raw: Option<OwnedFd>,
    io: Option<R>,
    bytes: Vec<u8>,
    eof: bool,
    error: Option<Arc<io::Error>>,
}
impl<R> Reader<R> {
    fn new(raw: Option<OwnedFd>) -> Self {
        Self {
            raw,
            io: None,
            bytes: Vec::new(),
            eof: false,
            error: None,
        }
    }
    fn register(
        &mut self,
        convert: fn(OwnedFd) -> Result<R, (OwnedFd, io::Error)>,
    ) -> Option<Arc<io::Error>> {
        let error = match self.raw.take() {
            Some(fd) => match convert(fd) {
                Ok(reader) => {
                    self.io = Some(reader);
                    return None;
                }
                Err((fd, error)) => {
                    self.raw = Some(fd);
                    error
                }
            },
            None => io::Error::other("expected capture endpoint absent"),
        };
        let error = Arc::new(error);
        self.error = Some(error.clone());
        Some(error)
    }
    fn active(&self) -> bool {
        self.io.is_some() && !self.eof && self.error.is_none()
    }
}

/// Keep outside catch_unwind/block_on/select. Drop only after original physical
/// cleanup; this state has no authority to perform that cleanup itself.
#[must_use]
pub(super) struct Capture {
    stdout: Reader<ChildStdout>,
    stderr: Reader<ChildStderr>,
    stdin: Option<OwnedFd>,
    limits: Limits,
    deadline: Option<Instant>,
    registered: bool,
    failure: Option<Arc<io::Error>>,
    overflow: Option<Overflow>,
    reaped_status: Option<ExitStatus>,
    finished: bool,
}
impl Capture {
    pub fn new(parent: ParentStdio, limits: Limits, deadline: impl Into<Option<Instant>>) -> Self {
        Self {
            stdout: Reader::new(parent.stdout),
            stderr: Reader::new(parent.stderr),
            stdin: parent.stdin,
            limits,
            deadline: deadline.into(),
            registered: false,
            failure: None,
            overflow: None,
            reaped_status: None,
            finished: false,
        }
    }
    #[cfg(test)]
    pub fn stdout(&self) -> &[u8] {
        &self.stdout.bytes
    }
    #[cfg(test)]
    pub fn stderr(&self) -> &[u8] {
        &self.stderr.bytes
    }
    #[cfg(test)]
    pub fn overflow(&self) -> Option<&Overflow> {
        self.overflow.as_ref()
    }
    #[cfg(test)]
    pub fn failure(&self) -> Option<&io::Error> {
        self.failure.as_deref()
    }
    pub fn retained_stdin(&self) -> Option<&OwnedFd> {
        self.stdin.as_ref()
    }
    fn remembered(&mut self, error: Arc<io::Error>) -> io::Error {
        let first = self.failure.get_or_insert(error);
        io::Error::new(first.kind(), first.clone())
    }
    fn fail(&mut self, error: io::Error) -> io::Error {
        self.remembered(Arc::new(error))
    }
    fn result(&self) -> io::Result<()> {
        match &self.failure {
            Some(error) => Err(io::Error::new(error.kind(), error.clone())),
            None => Ok(()),
        }
    }
    /// Requires the caller's entered I/O-enabled runtime. Tokio reactor panics
    /// are not converted to success/errors. Failed endpoints stay owned here.
    pub fn register(&mut self) -> io::Result<()> {
        if self.registered {
            return self.result();
        }
        self.registered = true;
        if self.stdin.is_some() {
            self.fail(io::Error::other(
                "unexpected parent stdin: no capture feeder",
            ));
        }
        if let Some(error) = self.stdout.register(ChildStdout::try_from_owned_fd) {
            self.remembered(error);
        }
        if let Some(error) = self.stderr.register(ChildStderr::try_from_owned_fd) {
            self.remembered(error);
        }
        self.result()
    }
    fn append(&mut self, stream: Stream, bytes: &[u8]) -> io::Result<()> {
        let total = self.stdout.bytes.len().checked_add(self.stderr.bytes.len());
        let (reader_bytes, limit) = match stream {
            Stream::Stdout => (&mut self.stdout.bytes, self.limits.stdout),
            Stream::Stderr => (&mut self.stderr.bytes, self.limits.stderr),
        };
        let keep = bytes
            .len()
            .min(limit.saturating_sub(reader_bytes.len()))
            .min(total.map_or(0, |total| self.limits.total.saturating_sub(total)));
        if let Err(error) = reader_bytes.try_reserve(keep) {
            return Err(self.fail(io::Error::other(error)));
        }
        reader_bytes.extend_from_slice(&bytes[..keep]);
        if keep != bytes.len() || total.is_none() {
            let overflow = self.overflow.get_or_insert(Overflow {
                stream,
                read_bytes: bytes.len(),
                retained_bytes: keep,
            });
            let error = io::Error::other(format!(
                "capture overflow: {:?} read={} retained={}",
                overflow.stream, overflow.read_bytes, overflow.retained_bytes,
            ));
            return Err(self.fail(error));
        }
        Ok(())
    }
    async fn drain(&mut self, after_error: bool) -> io::Result<()> {
        let (mut out, mut err) = ([0_u8; 4096], [0_u8; 4096]);
        while self.stdout.active() || self.stderr.active() {
            let (stream, read) = tokio::select! {
                read = async { self.stdout.io.as_mut().unwrap().read(&mut out).await }, if self.stdout.active() => (Stream::Stdout, read),
                read = async { self.stderr.io.as_mut().unwrap().read(&mut err).await }, if self.stderr.active() => (Stream::Stderr, read),
            };
            let result = match read {
                Ok(0) => {
                    match stream {
                        Stream::Stdout => self.stdout.eof = true,
                        Stream::Stderr => self.stderr.eof = true,
                    };
                    Ok(())
                }
                Ok(count) => self.append(
                    stream,
                    match stream {
                        Stream::Stdout => &out[..count],
                        Stream::Stderr => &err[..count],
                    },
                ),
                Err(error) => {
                    let error = Arc::new(error);
                    match stream {
                        Stream::Stdout => self.stdout.error = Some(error.clone()),
                        Stream::Stderr => self.stderr.error = Some(error.clone()),
                    };
                    Err(self.remembered(error))
                }
            };
            if !after_error {
                result?;
            }
        }
        self.result()
    }
    /// Borrows only; each successful cancellation-safe read is appended before
    /// another await. No reader or vector is moved into a losing future.
    pub async fn pump(&mut self) -> io::Result<()> {
        self.result()?;
        if !self.registered {
            return Err(self.fail(io::Error::other("capture not registered")));
        }
        // Tokio polls a ready inner future before its timeout. Check the same
        // deadline explicitly on both sides; ready EOF cannot renew the bound.
        if self.deadline.is_some_and(|end| Instant::now() >= end) {
            return Err(self.fail(io::Error::new(io::ErrorKind::TimedOut, "capture deadline")));
        }
        let drained = match self.deadline {
            Some(end) => timeout_at(end, self.drain(false)).await,
            None => Ok(self.drain(false).await),
        };
        match drained {
            Ok(result) => result?,
            Err(_) => {
                return Err(self.fail(io::Error::new(io::ErrorKind::TimedOut, "capture deadline")));
            }
        }
        if self.deadline.is_some_and(|end| Instant::now() >= end) {
            return Err(self.fail(io::Error::new(io::ErrorKind::TimedOut, "capture deadline")));
        }
        Ok(())
    }
    /// Call after the original caller cancels the execution/pump borrow. Its
    /// existing physical owner still performs kill/reap and RPC cancellation.
    pub fn cancel(&mut self) -> io::Error {
        self.fail(io::Error::new(
            io::ErrorKind::Interrupted,
            "capture execution cancelled",
        ))
    }
    /// Caller supplies the SAME receipt as its retained physical owner, after
    /// original reap AND successful pending-RPC cancellation/join. Receipt alone
    /// does not prove the join. Never poll here between cancel intent and join.
    pub async fn finish_after_reap(&mut self, receipt: &CleanupReceipt) -> io::Result<()> {
        let state = receipt.snapshot();
        if !state.child_created || !state.confirmed || state.terminal_status.is_none() {
            return Err(self.fail(io::Error::other("capture physical reap unconfirmed")));
        }
        self.reaped_status = state.terminal_status;
        if !self.registered {
            let _ = self.register();
        } // failed-launch endpoints
        if self.deadline.is_some_and(|end| Instant::now() >= end) {
            return Err(self.fail(io::Error::new(
                io::ErrorKind::TimedOut,
                "capture EOF deadline",
            )));
        }
        let drained = match self.deadline {
            Some(end) => timeout_at(end, self.drain(true)).await,
            None => Ok(self.drain(true).await),
        };
        if drained.is_err() {
            self.fail(io::Error::new(
                io::ErrorKind::TimedOut,
                "capture EOF deadline",
            ));
        }
        // A ready drain can finish after the bound without yielding to Tokio.
        if self.deadline.is_some_and(|end| Instant::now() >= end) {
            self.fail(io::Error::new(
                io::ErrorKind::TimedOut,
                "capture EOF deadline",
            ));
        }
        if !self.stdout.eof || !self.stderr.eof {
            self.fail(io::Error::other("capture EOF not established"));
        }
        self.result()
    }
    /// Call only after ALL original cleanup/log/source/summary checks. Errors
    /// retain this state and its partial bytes; no partial Output is returned.
    pub fn finish(&mut self, execution: io::Result<ExitStatus>) -> io::Result<Output> {
        let status = match execution {
            Ok(status) => status,
            Err(error) => {
                // The caller keeps the typed primary. Capture failures remain
                // separate diagnostics; a failed terminal decision cannot retry
                // with a replacement success status and obtain Output.
                self.finished = true;
                return Err(error);
            }
        };
        self.result()?;
        if self.finished
            || self.reaped_status != Some(status)
            || !self.stdout.eof
            || !self.stderr.eof
        {
            return Err(self.fail(io::Error::other(
                "capture output incomplete or already taken",
            )));
        }
        self.finished = true;
        Ok(Output {
            status,
            stdout: std::mem::take(&mut self.stdout.bytes),
            stderr: std::mem::take(&mut self.stderr.bytes),
        })
    }
}
mod execution;
pub(crate) use execution::CapturePolicy;
pub(super) use execution::CaptureRequest;
pub(crate) use execution::VerificationCapture;
pub(super) use execution::check;
pub(crate) use execution::run_verification;

#[cfg(test)]
mod tests;
