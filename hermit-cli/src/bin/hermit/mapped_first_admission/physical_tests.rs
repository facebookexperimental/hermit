/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in LICENSE.
 */

//! Real child/socket/pidfd tests of safe admission only. No runtime image,
//! Native Setup, traced entry, Tool initialization, Ready or Guest instruction.
use std::io::Read;
use std::io::Write;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::Child;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use reverie::GlobalTool;
use reverie_rpc_transport::BlockingRpcClient;
use reverie_rpc_transport::ConnectionFailure;
use reverie_rpc_transport::RpcServer;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

use super::*;

const WAIT: Duration = Duration::from_secs(10);
const CHILD: &str = "mapped_first_admission::physical_tests::admission_socket_child";
const CHILD_ENV: &str = "HERMIT_MAPPED_ADMISSION_TEST_CONTROL_FD";
const FRAME_LIMIT: usize = 64 * 1024;

fn need(condition: bool, message: &'static str) -> io::Result<()> {
    if condition {
        Ok(())
    } else {
        Err(io::Error::other(message))
    }
}
fn rpc(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}
async fn bounded<T>(future: impl std::future::Future<Output = io::Result<T>>) -> io::Result<T> {
    tokio::time::timeout(WAIT, future).await.map_err(rpc)?
}

/// A real process handle exists before any numeric id is read. Its control
/// socket blocks it alive until instructed. Never accepts an arbitrary PID.
struct HeldChild {
    child: Child,
    pidfd: OwnedFd,
    control: tokio::net::UnixStream,
}
impl Drop for HeldChild {
    fn drop(&mut self) {
        // Actual original parent owns kill/wait even if any assertion/await fails.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl HeldChild {
    async fn spawn(detlog: &UnixStream) -> io::Result<Self> {
        let (parent, child_socket) = UnixStream::pair()?;
        let child_fd = child_socket.as_raw_fd();
        let detlog_fd = detlog.as_raw_fd();
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args([
                "--exact",
                CHILD,
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_ENV, child_fd.to_string())
            .env_remove("RUST_TEST_THREADS")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        // SAFETY: after fork these are only raw fcntl calls on live captured
        // descriptors, without allocation/locks/formatting. Parent CLOEXEC flags
        // stay unchanged, so concurrent tests cannot inherit another lease.
        unsafe {
            command.pre_exec(move || {
                for fd in [child_fd, detlog_fd] {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let child = command.spawn()?;
        // Install a cleanup owner before every later fallible operation.
        struct Spawned(Option<Child>);
        impl Drop for Spawned {
            fn drop(&mut self) {
                if let Some(child) = self.0.as_mut() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
        let mut child = Spawned(Some(child));
        drop(child_socket);
        parent.set_nonblocking(true)?;
        let mut control = tokio::net::UnixStream::from_std(parent)?;
        let mut ready = [0];
        bounded(async { control.read_exact(&mut ready).await.map(|_| ()) }).await?;
        need(
            ready == *b"B",
            "actual child did not enter held control boundary",
        )?;
        // Reuse the existing in_guest_exits test pattern: a real unreaped Child
        // is held alive here. This is not reconstruction from a request/PID mock.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, child.0.as_ref().unwrap().id(), 0) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let pidfd = unsafe { OwnedFd::from_raw_fd(raw as i32) };
        need(
            hermit::physical_exit_watch::birth_identity(&pidfd).is_some(),
            "real held child lacks kernel pidfs birth identity",
        )?;
        Ok(Self {
            child: child.0.take().unwrap(),
            pidfd,
            control,
        })
    }
    async fn start(&mut self, mode: u8, packet: &[u8]) -> io::Result<()> {
        let frame = reverie_rpc_transport::codec::encode(&(mode, packet.to_vec())).map_err(rpc)?;
        need(
            frame.len() <= FRAME_LIMIT,
            "test command exceeds finite bound",
        )?;
        bounded(async {
            reverie_rpc_transport::codec::write_message(&mut self.control, &frame)
                .await
                .map_err(rpc)
        })
        .await
    }
    async fn byte(&mut self) -> io::Result<u8> {
        let mut byte = [0];
        bounded(async { self.control.read_exact(&mut byte).await.map(|_| ()) }).await?;
        Ok(byte[0])
    }
    async fn send(&mut self, byte: u8) -> io::Result<()> {
        bounded(async { self.control.write_all(&[byte]).await }).await
    }
    async fn terminal_wait(&mut self, kill: bool) -> io::Result<ExitStatus> {
        if kill {
            let _ = self.child.kill();
        }
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "actual child wait unconfirmed",
                ));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

use super::runtime_support::DirectoryBoundServer;
use super::runtime_support::Serving;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Case {
    SameBirth,
    WrongBirth,
    WrongNonce,
    WrongDigest,
    Replay,
    Transport,
}

async fn run(case: Case) -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    // Preserve the runner-owned temp directory even when its pathname exceeds
    // Linux's sockaddr_un limit. This short path names our retained directory
    // FD; the parent stays alive through child/watch/server retirement. Its PID
    // is a filesystem coordinate, never the peer-birth admission authority.
    let directory_fd = std::fs::File::open(directory.path())?;
    let socket = std::path::PathBuf::from(format!(
        "/proc/{}/fd/{}/rpc.sock",
        std::process::id(),
        directory_fd.as_raw_fd()
    ));
    let (detlog_sender, mut detlog_receiver) = UnixStream::pair()?;
    detlog_receiver.set_read_timeout(Some(WAIT))?;
    let mut held = HeldChild::spawn(&detlog_sender).await?;
    let mut other = if case == Case::WrongBirth {
        Some(HeldChild::spawn(&detlog_sender).await?)
    } else {
        None
    };
    let expected_pid = held.child.id() as i32;
    let expected_birth = hermit::physical_exit_watch::birth_identity(&held.pidfd)
        .ok_or_else(|| io::Error::other("held birth unavailable"))?;
    if let Some(other) = &other {
        need(
            hermit::physical_exit_watch::birth_identity(&other.pidfd) != Some(expected_birth),
            "wrong-birth control did not create a different actual birth",
        )?;
    }
    let config = detcore::Config {
        no_rcb_time: true,
        max_timeslice: None,
        target_timeslice: None,
        sequentialize_threads: false,
        in_guest_detlog_forward_policy: Some("INFO".to_owned()),
        ..Default::default()
    };
    need(
        packet::config_violation(&config).is_none(),
        "test Config outside ordinary InitialImage domain",
    )?;
    // Genuine ordinary GlobalState constructor; no Default (which panics),
    // native registry, numeric mock or custom scheduler/reporter implementation.
    let global = Arc::new(<detcore::GlobalState as GlobalTool>::init_global_state(&config).await);
    let mut wire_config = config.clone();
    if case == Case::WrongDigest {
        // Digest is derived by server, not a field a client sends. Keep the SAME
        // actual GlobalState but change actual serialized wire Config.
        wire_config.in_guest_detlog_forward_policy = Some("WARN".to_owned());
        need(
            config_digest(&reverie_rpc_transport::codec::encode(&wire_config).map_err(rpc)?)
                != config_digest(&reverie_rpc_transport::codec::encode(&config).map_err(rpc)?),
            "wire Config mutation did not change full digest",
        )?;
    }
    let mut server =
        RpcServer::bind_exclusive(&socket, global.clone(), wire_config).map_err(rpc)?;
    let owner = MappedFirstAdmission::prepare(
        global,
        held.pidfd.try_clone()?,
        socket.as_os_str().as_bytes(),
        detlog_sender.as_fd(),
        detlog_sender.as_raw_fd(),
    )
    .await?;
    // It names the actual inherited sender at its actual descriptor. No record,
    // source-registration or final DETLOG EOF success is asserted by these tests.
    drop(detlog_sender);
    let setup: io::Result<(Vec<u8>, Serving)> = async {
        let bytes = owner.initial_packet()?;
        let decoded = packet::decode(&bytes).map_err(rpc)?;
        need(
            decoded.coordinator == socket.as_os_str().as_bytes(),
            "wrong real socket packet",
        )?;
        need(
            owner.watched_process().birth_identity() == expected_birth
                && owner.watched_process().process_id() == expected_pid,
            "actual watch join changed birth",
        )?;
        let issues = owner.configure_server(&mut server)?;
        let connections = server.connection_monitor();
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(DirectoryBoundServer {
            server: Box::pin(server.serve_until(async move {
                let _ = stopped.await;
            })),
            _directory_fd: directory_fd,
        });
        Ok((
            bytes,
            Serving {
                task: Some(task),
                shutdown: Some(shutdown),
                connections,
                issues,
                owner: owner.clone(),
            },
        ))
    }
    .await;
    let (bytes, mut serving) = match setup {
        Ok(setup) => setup,
        Err(error) => {
            owner.fail(&error);
            let held_wait = held.terminal_wait(true).await;
            let other_wait = if let Some(other) = other.as_mut() {
                other.terminal_wait(true).await.map(|_| ())
            } else {
                Ok(())
            };
            let drained = owner.exit_admission().drain(WAIT).await;
            return match (held_wait, other_wait, drained) {
                (Ok(_), Ok(()), Ok(())) => Err(error),
                (a, b, c) => Err(io::Error::other(format!(
                    "admission setup failed: {error}; cleanup unconfirmed: {a:?}, {b:?}, {c:?}"
                ))),
            };
        }
    };

    // Always perform explicit physical cleanup before returning any test-body
    // refusal/assertion. RAII handles construction failures and panic fallback.
    let observed = async {
        let sender = other.as_mut().unwrap_or(&mut held);
        let mode = match case {
            Case::WrongNonce => b'N',
            Case::WrongBirth => b'B',
            Case::WrongDigest => b'D',
            Case::Transport => b'T',
            _ => b'A',
        };
        sender.start(mode, &bytes).await?;
        if case == Case::SameBirth || case == Case::Replay {
            need(
                sender.byte().await? == b'A',
                "child did not observe actual ACK and Config",
            )?;
            need(
                serving.owner.initial_used.load(Ordering::Acquire),
                "success did not consume one-use",
            )?;
            need(
                !serving.owner.failed.load(Ordering::Acquire),
                "positive admission already poisoned",
            )?;
            need(
                serving.owner.watched_process().watch_is_healthy(),
                "positive lost actual live watch",
            )?;
            need(
                serving.issues.snapshot().is_empty(),
                "positive retained transport issue",
            )?;
            if case == Case::Replay {
                sender.send(b'R').await?;
            }
        }
        if case != Case::SameBirth {
            bounded(async {
                serving.issues.failed().await;
                Ok(())
            })
            .await?;
            let failures = serving.issues.snapshot();
            need(
                failures.len() == 1,
                "expected exactly one retained connection failure",
            )?;
            let ConnectionFailure::Transport(error) = &failures[0].failure else {
                return Err(io::Error::other(
                    "refusal was not an actual transport issue",
                ));
            };
            match case {
                Case::Transport => {
                    // Read exactly one truncated frame through actual server.
                    need(
                        matches!(error.as_ref(), reverie_rpc_transport::RpcError::Io(e)
                        if e.kind() == io::ErrorKind::UnexpectedEof),
                        "wrong truncated-frame failure",
                    )?;
                    // Outside admission hook: actual future host must fail this
                    // retained issue. Do not pretend server hooks already did it.
                    need(
                        !serving.owner.initial_used.load(Ordering::Acquire),
                        "transport consumed nonce",
                    )?;
                    serving.owner.fail(error.as_ref());
                }
                Case::WrongBirth => need(
                    error.to_string().contains("different or failed birth"),
                    "wrong-birth control failed at another guard",
                )?,
                _ => need(
                    error
                        .to_string()
                        .contains("owner/nonce/Config/replay refused"),
                    "binding control failed at another guard",
                )?,
            }
            need(
                serving.owner.failed.load(Ordering::Acquire),
                "refusal did not permanently poison launch",
            )?;
            need(
                serving.owner.initial_used.load(Ordering::Acquire) == (case == Case::Replay),
                "one-use bit changed on mismatch or reset after replay",
            )?;
            need(
                serving.owner.initial_packet().is_err(),
                "poisoned owner issued another packet",
            )?;
            need(
                serving.owner.exit_admission().failure().is_some(),
                "watch did not retain failure",
            )?;
        } else {
            sender.send(b'X').await?;
        }
        Ok(())
    }
    .await;

    // Refusal kills watched process via real watcher; retain original kill/wait
    // regardless. Wrong peer is a second real child and has its own wait owner.
    let held_wait = held
        .terminal_wait(observed.is_err() || case != Case::SameBirth)
        .await;
    let other_wait = if let Some(other) = other.as_mut() {
        other.terminal_wait(true).await.map(|_| ())
    } else {
        Ok(())
    };
    let joined = serving.close().await;
    // Actual sender copies existed only in the parent lease (already dropped)
    // and these held children. This checks their physical FD retirement; it is
    // not a registered DETLOG source completion receipt.
    let mut tail = [0];
    let detlog_eof = detlog_receiver.read(&mut tail).and_then(|n| {
        need(
            n == 0,
            "inherited test DETLOG sender did not reach actual EOF",
        )
    });
    drop(detlog_receiver);
    // Check ALL cleanup results even if observation failed; do not hide an
    // unconfirmed physical/connection drain behind the original test error.
    let cleanup = match (held_wait, other_wait, joined) {
        (Ok(status), Ok(()), Ok(())) => {
            if case == Case::SameBirth {
                need(status.success(), "positive child did not exit successfully")
            } else {
                Ok(())
            }
        }
        (a, b, c) => Err(io::Error::other(format!(
            "physical admission cleanup unconfirmed: held={a:?}, other={b:?}, server/watch={c:?}; observation={observed:?}"
        ))),
    };
    cleanup?;
    detlog_eof?;
    need(
        serving.owner.exit_admission().reported_exits() == [expected_pid],
        "watch did not report exactly actual held child once",
    )?;
    if case == Case::SameBirth {
        need(
            serving.issues.snapshot().is_empty(),
            "clean positive gained shutdown issue",
        )?;
        need(
            serving.owner.exit_admission().failure().is_none(),
            "clean positive gained watch failure",
        )?;
    }
    observed
}

#[tokio::test]
async fn actual_same_birth_initial_ack_config_and_physical_drain() {
    run(Case::SameBirth).await.unwrap();
}
#[tokio::test]
async fn actual_other_child_peer_birth_refuses_and_drains_original_watch() {
    run(Case::WrongBirth).await.unwrap();
}
#[tokio::test]
async fn actual_same_birth_wrong_nonce_poison_is_permanent() {
    run(Case::WrongNonce).await.unwrap();
}
#[tokio::test]
async fn actual_server_wire_config_digest_mismatch_refuses() {
    run(Case::WrongDigest).await.unwrap();
}
#[tokio::test]
async fn actual_same_birth_second_connection_cannot_redeem_twice() {
    run(Case::Replay).await.unwrap();
}
#[tokio::test]
async fn actual_truncated_request_issue_is_failed_then_physically_drained() {
    run(Case::Transport).await.unwrap();
}

/// Subprocess fixture only; not a passing hardware/admission test when ignored.
/// Every parent above runs this exact identity --ignored once in a real child.
#[test]
#[ignore = "subprocess fixture; selected explicitly by physical admission parent tests"]
fn admission_socket_child() {
    let raw: i32 = std::env::var(CHILD_ENV)
        .expect("physical parent control lease")
        .parse()
        .expect("actual inherited control FD");
    let mut control = unsafe { UnixStream::from_raw_fd(raw) };
    control.set_read_timeout(Some(WAIT)).unwrap();
    control.set_write_timeout(Some(WAIT)).unwrap();
    control.write_all(b"B").unwrap();
    let mut size = [0; 4];
    control.read_exact(&mut size).unwrap();
    let size = u32::from_be_bytes(size) as usize;
    assert!(size <= FRAME_LIMIT, "bounded parent command");
    let mut command = vec![0; size];
    control.read_exact(&mut command).unwrap();
    let (mode, bytes): (u8, Vec<u8>) = reverie_rpc_transport::codec::decode(&command).unwrap();
    let initial = packet::decode(&bytes).unwrap();
    assert_eq!(
        initial.schema_fingerprint,
        detcore::config_wire_fingerprint().as_bytes()
    );
    let inherited = detcore::detlog::tool_output_env_target(initial.detlog_identity);
    assert!(
        inherited.is_some(),
        "real inherited sender must match packet inode/device/FD"
    );
    let path = std::path::Path::new(std::ffi::OsStr::from_bytes(initial.coordinator));
    if mode == b'T' {
        let mut stream = UnixStream::connect(path).unwrap();
        stream.set_write_timeout(Some(WAIT)).unwrap();
        stream.write_all(&6_u32.to_be_bytes()).unwrap();
        stream.write_all(b"x").unwrap(); // EOF within declared frame, not clean close.
        stream.shutdown(std::net::Shutdown::Both).unwrap();
        control.write_all(b"T").unwrap();
        let mut release = [0];
        // Stay alive until parent observes the exact mid-frame issue and kills
        // the watched owner; do not race SO_PEERPIDFD with child exit.
        control.read_exact(&mut release).unwrap();
    } else {
        let mut nonce = *initial.nonce;
        if mode == b'N' {
            nonce[0] ^= 1;
        }
        let digest = *initial.config_digest;
        let connect = || {
            let stream = UnixStream::connect(path)?;
            stream.set_read_timeout(Some(WAIT))?;
            stream.set_write_timeout(Some(WAIT))?;
            BlockingRpcClient::<detcore::GlobalState>::initial_image_from_connected_stream(
                stream,
                reverie::Tid::from_raw(std::process::id() as i32),
                &nonce,
                digest,
            )
            .map_err(rpc)
        };
        match connect() {
            Ok(client) => {
                // Real library checked ACK and raw Config digest before decode.
                assert_eq!(
                    config_digest(&reverie_rpc_transport::codec::encode(client.config()).unwrap()),
                    digest
                );
                assert_eq!(
                    client.config().in_guest_detlog_forward_policy.as_deref(),
                    Some("INFO")
                );
                drop(client); // clean same-socket end; authorization stays consumed.
                control.write_all(b"A").unwrap();
                let mut action = [0];
                control.read_exact(&mut action).unwrap();
                if action == *b"R" {
                    assert!(connect().is_err(), "same-birth replay must refuse");
                } else {
                    assert_eq!(action, [b'X']);
                }
            }
            Err(_) => {
                // Only negative parent cases accept server refusal, by their
                // retained exact issue + poison + one-use checks, not this exit.
                assert!(
                    matches!(mode, b'N' | b'B' | b'D'),
                    "unexpected first admission failure"
                );
            }
        }
    }
    // Negative parent checks do not depend on this child's terminal status.
}

// Real first-exec packet lease controls; no cooperative child or runtime entry.
mod packet_held_tests;
