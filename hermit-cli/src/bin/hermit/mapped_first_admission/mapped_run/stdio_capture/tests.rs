//! Pipe mechanics only; no guest, process/wait owner or positive reap receipt.
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::time::Duration;

use reverie::process::ChildStdin;
use tokio::io::AsyncWriteExt;

use super::*;

fn pipe() -> (OwnedFd, OwnedFd, usize) {
    let mut fds = [-1; 2];
    // SAFETY: two initialized scalar outputs; successful pipe2 gives two owners.
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    let (reader, writer) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    // SAFETY: inspection of this retained pipe descriptor only.
    let capacity = unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_GETPIPE_SZ) };
    assert!(capacity > 0);
    (reader, writer, capacity as usize)
}
fn fixture(limits: Limits, deadline: Instant) -> (Capture, OwnedFd, OwnedFd, usize) {
    let (out, out_write, out_cap) = pipe();
    let (err, err_write, err_cap) = pipe();
    let capture = Capture::new(
        ParentStdio {
            stdin: None,
            stdout: Some(out),
            stderr: Some(err),
        },
        limits,
        deadline,
    );
    (capture, out_write, err_write, out_cap.max(err_cap))
}
fn limits(bytes: usize) -> Limits {
    Limits {
        stdout: bytes,
        stderr: bytes,
        total: bytes.checked_mul(2).unwrap(),
    }
}
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(5)
}
fn backpressure_fixture(end: Instant) -> (Capture, OwnedFd, OwnedFd, usize) {
    let (out, out_write, out_cap) = pipe();
    let (err, err_write, err_cap) = pipe();
    let count = out_cap
        .max(err_cap)
        .checked_mul(3)
        .unwrap()
        .checked_add(17)
        .unwrap();
    let capture = Capture::new(
        ParentStdio {
            stdin: None,
            stdout: Some(out),
            stderr: Some(err),
        },
        limits(count),
        end,
    );
    (capture, out_write, err_write, count)
}

#[tokio::test(flavor = "current_thread")]
async fn both_streams_exceed_actual_pipe_capacity_without_serial_backpressure() {
    let end = deadline();
    let (mut capture, out, err, count) = backpressure_fixture(end);
    capture.register().unwrap();
    let expected_out: Vec<_> = (0..count).map(|i| (i % 251) as u8).collect();
    let expected_err: Vec<_> = (0..count).map(|i| (255 - i % 251) as u8).collect();
    let (mut out, mut err) = (ChildStdin::from(out), ChildStdin::from(err));
    let write = async {
        let (a, b) = tokio::join!(out.write_all(&expected_out), err.write_all(&expected_err));
        a.unwrap();
        b.unwrap();
        drop(out);
        drop(err);
    };
    let (read, ()) = timeout_at(end, async { tokio::join!(capture.pump(), write) })
        .await
        .unwrap();
    read.unwrap();
    assert_eq!(capture.stdout(), expected_out);
    assert_eq!(capture.stderr(), expected_err);
    assert!(capture.stdout.eof && capture.stderr.eof);
    assert!(capture.failure().is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn early_stderr_eof_does_not_cancel_large_stdout() {
    let end = deadline();
    let (mut capture, out, err, count) = backpressure_fixture(end);
    capture.register().unwrap();
    drop(err);
    let expected = vec![0xa5; count];
    let mut out = ChildStdin::from(out);
    let write = async {
        out.write_all(&expected).await.unwrap();
        drop(out);
    };
    let (read, ()) = timeout_at(end, async { tokio::join!(capture.pump(), write) })
        .await
        .unwrap();
    read.unwrap();
    assert_eq!(capture.stdout(), expected);
    assert!(capture.stderr().is_empty());
    assert!(capture.stdout.eof && capture.stderr.eof);
}

#[tokio::test(flavor = "current_thread")]
async fn exact_cap_is_complete_but_first_extra_byte_is_sticky_failure() {
    for payload in [&b"1234567"[..], &b"12345678"[..]] {
        let (mut capture, out, err, _) = fixture(
            Limits {
                stdout: 7,
                stderr: 0,
                total: 7,
            },
            deadline(),
        );
        capture.register().unwrap();
        let mut out = std::fs::File::from(out);
        out.write_all(payload).unwrap();
        drop(out);
        drop(err);
        let result = capture.pump().await;
        assert_eq!(capture.stdout(), &payload[..7]);
        if payload.len() == 7 {
            result.unwrap();
            assert!(capture.overflow().is_none());
            assert!(capture.stdout.eof && capture.stderr.eof);
        } else {
            result.unwrap_err();
            let overflow = capture.overflow().unwrap();
            assert_eq!(overflow.stream, Stream::Stdout);
            assert_eq!(overflow.read_bytes, 8);
            assert_eq!(overflow.retained_bytes, 7);
            capture.drain(true).await.unwrap_err();
            assert!(capture.stdout.eof && capture.stderr.eof);
            assert!(capture.failure().is_some());
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn shared_total_cap_refuses_an_extra_byte_without_silent_success() {
    let (mut capture, out, err, _) = fixture(
        Limits {
            stdout: 8,
            stderr: 8,
            total: 7,
        },
        deadline(),
    );
    capture.register().unwrap();
    std::fs::File::from(out).write_all(b"1234").unwrap();
    std::fs::File::from(err).write_all(b"5678").unwrap();
    capture.pump().await.unwrap_err();
    assert_eq!(capture.stdout().len() + capture.stderr().len(), 7);
    assert!(capture.overflow().is_some());
    assert!(capture.failure().is_some());
}

fn unread(fd: i32) -> i32 {
    let mut count = -1;
    // SAFETY: FIONREAD writes one integer for this still-owned pipe reader.
    assert_eq!(unsafe { libc::ioctl(fd, libc::FIONREAD, &mut count) }, 0);
    count
}
#[tokio::test(flavor = "current_thread")]
async fn cancelled_borrow_preserves_actual_prefixes_and_readers_until_eof() {
    let end = deadline();
    let (mut capture, out, err, _) = fixture(limits(128), end);
    let (out_fd, err_fd) = (
        capture.stdout.raw.as_ref().unwrap().as_raw_fd(),
        capture.stderr.raw.as_ref().unwrap().as_raw_fd(),
    );
    capture.register().unwrap();
    let (mut out, mut err) = (std::fs::File::from(out), std::fs::File::from(err));
    out.write_all(b"out\0\xff").unwrap();
    err.write_all(b"err\0\xfe").unwrap();
    assert_eq!((unread(out_fd), unread(err_fd)), (5, 5));
    {
        let reading = capture.pump();
        tokio::pin!(reading);
        timeout_at(end, async {
            tokio::select! {
                result = &mut reading => panic!("live writers must prevent EOF: {result:?}"),
                () = async {
                    while unread(out_fd) != 0 || unread(err_fd) != 0 { tokio::task::yield_now().await; }
                } => {}
            }
        }).await.unwrap();
    } // the borrowed pump is actually dropped; the sole state still owns bytes/fds
    assert_eq!(capture.cancel().kind(), io::ErrorKind::Interrupted);
    assert_eq!(capture.stdout(), b"out\0\xff");
    assert_eq!(capture.stderr(), b"err\0\xfe");
    assert!(!capture.stdout.eof && !capture.stderr.eof);
    drop(out);
    drop(err);
    timeout_at(end, capture.drain(true))
        .await
        .unwrap()
        .unwrap_err();
    assert!(capture.stdout.eof && capture.stderr.eof);
    assert_eq!(
        capture.failure().unwrap().kind(),
        io::ErrorKind::Interrupted
    );
    // This tests EOF mechanics only, not the required production reap gate.
}

#[tokio::test(flavor = "current_thread")]
async fn real_read_error_retains_descriptor_and_does_not_invent_eof() {
    // A pipe write endpoint's EPOLLERR is not READABLE in this Tokio version.
    // A retained PTY master after its last slave closes reports readable HUP
    // and an actual EIO, without closing/replacing the descriptor under test.
    // SAFETY: posix_openpt returns a new descriptor; no controlling tty is used.
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
    assert!(master >= 0);
    // SAFETY: successful posix_openpt transferred this unique descriptor.
    let wrong_source = unsafe { OwnedFd::from_raw_fd(master) };
    // SAFETY: both calls operate on the retained master, before opening its slave.
    assert_eq!(unsafe { libc::grantpt(master) }, 0);
    assert_eq!(unsafe { libc::unlockpt(master) }, 0);
    let mut name = [0 as libc::c_char; 128];
    // SAFETY: name is a writable buffer of exactly the supplied length.
    assert_eq!(
        unsafe { libc::ptsname_r(master, name.as_mut_ptr(), name.len()) },
        0
    );
    // SAFETY: successful ptsname_r wrote a terminated slave path; open returns
    // a new descriptor and O_NOCTTY prevents controlling-terminal acquisition.
    let slave = unsafe {
        libc::open(
            name.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    };
    assert!(slave >= 0);
    // SAFETY: successful open transferred this unique slave descriptor.
    drop(unsafe { OwnedFd::from_raw_fd(slave) });
    let wrong_fd = wrong_source.as_raw_fd();
    let (err, err_write, _) = pipe();
    drop(err_write);
    let mut capture = Capture::new(
        ParentStdio {
            stdin: None,
            stdout: Some(wrong_source),
            stderr: Some(err),
        },
        limits(64),
        deadline(),
    );
    capture.register().unwrap();
    capture.pump().await.unwrap_err();
    assert_eq!(capture.failure().unwrap().raw_os_error(), Some(libc::EIO));
    assert!(capture.stdout.io.is_some() && !capture.stdout.eof);
    // SAFETY: check that the errored reader's original fd is still owned.
    assert!(unsafe { libc::fcntl(wrong_fd, libc::F_GETFD) } >= 0);
    capture.drain(true).await.unwrap_err();
    assert!(capture.stderr.eof && !capture.stdout.eof);
}

#[tokio::test(flavor = "current_thread")]
async fn retained_writer_exhausts_original_deadline_without_endpoint_drop() {
    let end = Instant::now() + Duration::from_millis(100);
    let (mut capture, out, err, _) = fixture(limits(64), end);
    capture.register().unwrap();
    capture.pump().await.unwrap_err();
    assert_eq!(capture.failure().unwrap().kind(), io::ErrorKind::TimedOut);
    assert_eq!(capture.deadline, Some(end));
    assert!(capture.stdout.io.is_some() && capture.stderr.io.is_some());
    assert!(!capture.stdout.eof && !capture.stderr.eof);
    drop(out);
    drop(err);
}

#[tokio::test(flavor = "current_thread")]
async fn eof_without_actual_reap_cannot_yield_output_and_bytes_remain() {
    let (mut capture, out, err, _) = fixture(limits(64), deadline());
    capture.register().unwrap();
    std::fs::File::from(out).write_all(b"retained").unwrap();
    drop(err);
    capture.pump().await.unwrap();
    capture
        .finish_after_reap(&CleanupReceipt::default())
        .await
        .unwrap_err();
    // A synthetic status is used only to assert refusal, never to pass the gate.
    capture.finish(Ok(ExitStatus::SUCCESS)).unwrap_err();
    assert_eq!(capture.stdout(), b"retained");
    assert!(capture.reaped_status.is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn unexpected_parent_stdin_is_refused_but_all_endpoints_remain_owned() {
    let (out_read, out, _) = pipe();
    let (err_read, err, _) = pipe();
    let (stdin_peer, stdin, _) = pipe();
    let fd = stdin.as_raw_fd();
    let mut capture = Capture::new(
        ParentStdio {
            stdin: Some(stdin),
            stdout: Some(out_read),
            stderr: Some(err_read),
        },
        limits(64),
        deadline(),
    );
    capture.register().unwrap_err();
    assert_eq!(capture.retained_stdin().unwrap().as_raw_fd(), fd);
    assert!(capture.stdout.io.is_some() && capture.stderr.io.is_some());
    drop(stdin_peer);
    drop(out);
    drop(err);
}

#[test]
fn execution_error_keeps_its_original_type_after_a_capture_failure() {
    #[derive(Debug)]
    struct OriginalExecutionFailure;
    impl std::fmt::Display for OriginalExecutionFailure {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("original execution failure")
        }
    }
    impl std::error::Error for OriginalExecutionFailure {}
    let (mut capture, _out, _err, _) = fixture(limits(64), deadline());
    assert_eq!(capture.cancel().kind(), io::ErrorKind::Interrupted);
    let error = capture
        .finish(Err(io::Error::new(
            io::ErrorKind::InvalidData,
            OriginalExecutionFailure,
        )))
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.get_ref().unwrap().is::<OriginalExecutionFailure>());
    assert_eq!(
        capture.failure().unwrap().kind(),
        io::ErrorKind::Interrupted
    );
}

#[tokio::test(flavor = "current_thread")]
async fn expired_deadline_refuses_already_ready_bytes_and_eof() {
    let end = Instant::now() - Duration::from_secs(1);
    let (mut capture, out, err, _) = fixture(limits(64), end);
    capture.register().unwrap();
    std::fs::File::from(out)
        .write_all(b"already-ready")
        .unwrap();
    drop(err);
    let error = capture.pump().await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(capture.stdout.io.is_some() && capture.stderr.io.is_some());
}

#[tokio::test(flavor = "current_thread")]
async fn expired_deadline_refuses_previously_observed_ready_eof() {
    let end = Instant::now() + Duration::from_millis(100);
    let (mut capture, out, err, _) = fixture(limits(64), end);
    capture.register().unwrap();
    std::fs::File::from(out)
        .write_all(b"observed-before-deadline")
        .unwrap();
    drop(err);
    capture.pump().await.unwrap();
    assert!(capture.stdout.eof && capture.stderr.eof);
    assert_eq!(capture.stdout(), b"observed-before-deadline");
    tokio::time::sleep_until(end).await;
    let error = capture.pump().await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert_eq!(capture.stdout(), b"observed-before-deadline");
}

#[tokio::test(flavor = "current_thread")]
async fn full_capture_preserves_both_streams_beyond_qualification_caps() {
    let (out, out_write, _) = pipe();
    let (err, err_write, _) = pipe();
    let (limits, capture_deadline) = execution::CapturePolicy::Full.parts();
    assert!(capture_deadline.is_none());
    let mut capture = Capture::new(
        ParentStdio {
            stdin: None,
            stdout: Some(out),
            stderr: Some(err),
        },
        limits,
        capture_deadline,
    );
    capture.register().unwrap();
    let expected_out = vec![0x35; 128 * 1024 + 17];
    let expected_err = vec![0x97; 128 * 1024 + 31];
    let (mut out, mut err) = (ChildStdin::from(out_write), ChildStdin::from(err_write));
    let write = async {
        let (a, b) = tokio::join!(out.write_all(&expected_out), err.write_all(&expected_err));
        a.unwrap();
        b.unwrap();
        drop(out);
        drop(err);
    };
    // Test containment only; production capture itself has no implicit timer.
    let (read, ()) = timeout_at(deadline(), async { tokio::join!(capture.pump(), write) })
        .await
        .unwrap();
    read.unwrap();
    assert_eq!(capture.stdout(), expected_out);
    assert_eq!(capture.stderr(), expected_err);
    assert!(capture.stdout.eof && capture.stderr.eof);
    assert!(capture.failure().is_none() && capture.overflow().is_none());
    assert!(capture.deadline.is_none());
}

#[test]
fn finite_capture_policy_keeps_original_bounds_and_deadline() {
    let end = deadline();
    let (limits, actual) = execution::CapturePolicy::Qualification { deadline: end }.parts();
    assert_eq!(limits.stdout, 128 * 1024);
    assert_eq!(limits.stderr, 128 * 1024);
    assert_eq!(limits.total, 256 * 1024);
    assert_eq!(actual, Some(end));
    assert!(
        execution::CapturePolicy::Qualification { deadline: end }
            .observation()
            .is_qualification()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn capture_eof_waits_for_the_same_original_execution_future() {
    // Pipe/future mechanics only: no physical child or positive cleanup receipt.
    let end = deadline();
    let (capture, out, err, _) = fixture(limits(64), end);
    drop(out);
    drop(err);
    let mut request = execution::CaptureRequest::new(limits(64), Some(end));
    request.capture = Some(capture);
    request.register().unwrap();
    let (complete, completed) = tokio::sync::oneshot::channel();
    let entered = std::cell::Cell::new(0);
    let original = async {
        entered.set(entered.get() + 1);
        completed.await.map_err(io::Error::other)
    };
    {
        let driven = request.drive(original);
        tokio::pin!(driven);
        tokio::select! {
            result = &mut driven => panic!("EOF cannot complete the original execution: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
        assert_eq!(entered.get(), 1);
        complete.send(ExitStatus::SUCCESS).unwrap();
        assert_eq!(
            timeout_at(end, driven).await.unwrap().unwrap(),
            ExitStatus::SUCCESS
        );
        assert_eq!(entered.get(), 1);
    }
    let capture = request.capture.as_mut().unwrap();
    assert!(capture.stdout.eof && capture.stderr.eof);
    assert!(capture.reaped_status.is_none());
    // Completion of the borrowed future still cannot forge physical retirement.
    capture.finish(Ok(ExitStatus::SUCCESS)).unwrap_err();
}
