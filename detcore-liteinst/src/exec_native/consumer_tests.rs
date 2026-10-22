//! Real forwarding/socket/codec controls with explicitly modeled restored state.
//! These do not authenticate a watched birth, perform exec, or release Guest.

use std::fmt;
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::Ordering;
use std::time::Duration;

use detcore::detlog;
use detcore::native_exec_control::ExecControlError;
use detcore::types::DetTid;
use detcore::types::DetTime;
use detcore::types::MmId;
use detcore_model::fd::FilesId;
use reverie::GlobalTool;
use reverie::Pid;
use reverie_inguest::guest::rpc::GuestRpcStream;
use reverie_rpc_transport::BlockingRpcClient;
use reverie_rpc_transport::RequestEnvelope;
use reverie_rpc_transport::codec;

use super::*;

type Request = <detcore::GlobalState as GlobalTool>::Request;
type Response = <detcore::GlobalState as GlobalTool>::Response;
const FRAME_LIMIT: usize = 1024 * 1024;
const RECORD: &[u8] = b"INFO detcore::random: DETLOG post-exec-record\n";
static FORWARD_FD: AtomicI32 = AtomicI32::new(-1);

fn forward(target: &str, suffix: &str, index: u64, message: fmt::Arguments<'_>) {
    detlog::send_forwarded_record(
        FORWARD_FD.load(Ordering::Acquire),
        target,
        suffix,
        index,
        message,
    );
}

fn emit_record() {
    // Uses the production counter and sender; no supplied or reset count.
    detlog::emit_forwarded("detcore::random", "", format_args!("post-exec-record"));
}

// Global forwarding state/OnceLock cannot be reset to manufacture a fresh cut.
// Each case runs in a fresh copy of the maintained CLI test executable.
fn isolated(name: &str, case: &str, body: impl FnOnce()) {
    const KEY: &str = "HERMIT_NATIVE_CONSUMER_COMPLETE_TEST";
    let key = format!("{name}:{case}");
    if let Ok(active) = std::env::var(KEY) {
        if active == key {
            body();
        }
        return;
    }
    let full_name = format!("native_exec_consumer::tests::{name}");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &full_name, "--nocapture", "--test-threads=1"])
        .env(KEY, key)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{full_name} ({case}): {:?}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("running 1 test") && stdout.contains("1 passed; 0 failed"),
        "the exact child test must execute: {stdout}"
    );
}

fn seqpacket() -> (OwnedFd, OwnedFd) {
    let mut fds = [-1; 2];
    assert_eq!(
        unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        },
        0
    );
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

fn write_frame<T: serde::Serialize>(stream: &mut UnixStream, value: &T) {
    let bytes = codec::encode_bounded::<_, FRAME_LIMIT>(value).unwrap();
    stream
        .write_all(&u32::try_from(bytes.len()).unwrap().to_be_bytes())
        .unwrap();
    stream.write_all(&bytes).unwrap();
}

fn read_frame(stream: &mut UnixStream) -> Option<RequestEnvelope<Request>> {
    let mut header = [0; 4];
    match stream.read(&mut header[..1]) {
        Ok(0) => return None,
        Ok(1) => {}
        result => panic!("request header first byte: {result:?}"),
    }
    stream.read_exact(&mut header[1..]).unwrap();
    let length = u32::from_be_bytes(header) as usize;
    assert!(length > 0 && length <= FRAME_LIMIT);
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).unwrap();
    Some(codec::decode_exact_bounded::<_, FRAME_LIMIT>(&bytes).unwrap())
}

fn modeled_consumer(tid: i32) -> Consumer {
    // Name the real constructor so this path inclusion type-checks every
    // production dependency, but never call its unsafe admission API here.
    let _constructor = Consumer::new;
    let owner = DetTid::from_raw(tid);
    let old_mm = MmId::initial(owner);
    let config = detcore::Config::default();
    Consumer {
        ticket: ExecTicket([19; 32]),
        admitted: SuccessorBinding {
            generation: 1,
            ticket: [23; 32],
            state: [29; 32],
            config: [31; 32],
        },
        cut: frame::CutBinding {
            dettid: owner,
            detpid: owner,
            physical_tid: tid,
            old_mm,
            old_files: FilesId::initial(owner),
            prepared_mm: old_mm.for_exec(owner),
            config_digest: [31; 32],
        },
        stats: None,
        sequence: None,
        restored: true,
        completion_attempted: false,
        cut_clock: Some(DetTime::new(&config)),
    }
}

#[derive(Clone, Copy)]
enum Ack {
    Exact,
    WrongVariant,
    Negative,
    WithLateRecord,
}

struct Observed {
    request_seen: bool,
    drained: Vec<Vec<u8>>,
}

fn exchange_case(
    records: u64,
    ack: Ack,
    body: impl FnOnce(
        &mut Consumer,
        &CoordinatorRpc<detcore::GlobalState>,
    ) -> io::Result<SuccessorBinding>,
) -> (io::Result<SuccessorBinding>, SuccessorBinding, Observed) {
    let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
    let mut consumer = modeled_consumer(tid);
    let admitted = consumer.admitted;
    let ticket = consumer.ticket;
    let mm = consumer.cut.prepared_mm;
    let clock = consumer.cut_clock.as_ref().unwrap().as_nanos();
    let (send, receive) = seqpacket();
    FORWARD_FD.store(send.as_raw_fd(), Ordering::Release);
    assert!(detlog::set_forwarder(forward, detlog::ForwardPolicy::decode("1").unwrap()).is_ok());
    let written = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let sink = Arc::clone(&written);
    detlog::set_forwarded_source(
        receive,
        Box::new(move |bytes| sink.lock().unwrap().push(bytes.to_vec())),
    );
    detlog::write_forwarded_for(tid, true);
    // Bind the actual source before any post-exec record is produced.
    let receipt = detlog::check_native_exec_forwarding_cut(tid, 0, None).unwrap();
    let (client, mut server) = UnixStream::pair().unwrap();
    for stream in [&client, &server] {
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
    }
    let server = std::thread::spawn(move || {
        write_frame(&mut server, &detcore::Config::default());
        let Some(envelope) = read_frame(&mut server) else {
            return false;
        };
        assert_eq!(envelope.from, Pid::from_raw(tid));
        let (actual_clock, actual_mm, request) = envelope.request;
        assert_eq!(actual_clock.as_nanos(), clock);
        assert_eq!(actual_mm, mm);
        let Q::WithForwardedCount(produced, request) = request else {
            panic!("Complete lacks its required single counted envelope");
        };
        assert_eq!(
            produced, records,
            "count must describe the actual emitted interval"
        );
        assert!(matches!(*request, Q::CompleteNativeExec { ticket: actual } if actual == ticket));
        // Real socket records, indexes and retained source are validated before
        // the test peer sends its typed ACK. Registry/native authority is modeled.
        detlog::check_native_exec_forwarding_cut(tid, produced, Some(&receipt)).unwrap();
        let response: Response = match ack {
            Ack::WrongVariant => (None, R::CancelNativeExec(Ok(()))),
            Ack::Negative => (
                None,
                R::CompleteNativeExec(Err(ExecControlError::Transition)),
            ),
            Ack::Exact | Ack::WithLateRecord => (None, R::CompleteNativeExec(Ok(()))),
        };
        if matches!(ack, Ack::WithLateRecord) {
            // A different real thread leaves a retained slot after the validated
            // interval, before ACK. The consumer must inspect all slots again.
            emit_record();
        }
        write_frame(&mut server, &response);
        assert!(
            read_frame(&mut server).is_none(),
            "no second Complete may be sent"
        );
        true
    });
    let client =
        BlockingRpcClient::from_connected_stream(GuestRpcStream::from(client), Pid::from_raw(tid))
            .unwrap();
    let rpc = CoordinatorRpc::from_live_client("unused-completion-fixture", client, |_, _| {
        panic!("completion fixture must not reconnect")
    })
    .unwrap();
    for _ in 0..records {
        emit_record();
    }
    if records != 0 {
        assert!(
            matches!(detlog::check_empty_forwarding_cut(), Err(detlog::ForwardingCutError::PendingRecords { tid: owner, count, .. }) if owner == tid && count == records)
        );
    }
    let result = body(&mut consumer, &rpc);
    // Close before joining even when old production refuses before the request;
    // the server then records EOF, rather than hanging or fabricating success.
    drop(rpc);
    let request_seen = server.join().expect("real framed peer must not panic");
    let drained = written.lock().unwrap().clone();
    drop(send);
    (
        result,
        admitted,
        Observed {
            request_seen,
            drained,
        },
    )
}

#[test]
fn model_consumer_complete_accounts_real_post_exec_record() {
    isolated(
        "model_consumer_complete_accounts_real_post_exec_record",
        "one",
        || {
            let (result, binding, observed) = exchange_case(1, Ack::Exact, |consumer, rpc| {
                consumer.complete(rpc, consumer.admitted)
            });
            assert!(
                observed.request_seen,
                "actual Complete must reach the framed peer; old production refuses before sending it: {result:?}"
            );
            assert_eq!(observed.drained, vec![RECORD.to_vec()]);
            assert_eq!(result.unwrap(), binding);
            assert_eq!(detlog::check_empty_forwarding_cut(), Ok(()));
        },
    );
}

#[test]
fn model_consumer_complete_zero_interval_and_second_attempt() {
    isolated(
        "model_consumer_complete_zero_interval_and_second_attempt",
        "zero",
        || {
            let (result, binding, observed) = exchange_case(0, Ack::Exact, |consumer, rpc| {
                let result = consumer.complete(rpc, consumer.admitted);
                assert!(consumer.completion_attempted);
                assert!(consumer.complete(rpc, consumer.admitted).is_err());
                result
            });
            assert!(observed.request_seen);
            assert!(observed.drained.is_empty());
            assert_eq!(result.unwrap(), binding);
            assert_eq!(detlog::check_empty_forwarding_cut(), Ok(()));
        },
    );
}

#[test]
fn model_consumer_complete_wrong_binding_preserves_the_attempt() {
    isolated(
        "model_consumer_complete_wrong_binding_preserves_the_attempt",
        "binding",
        || {
            let (result, binding, observed) = exchange_case(0, Ack::Exact, |consumer, rpc| {
                let mut wrong = consumer.admitted;
                wrong.state[0] ^= 1;
                assert!(consumer.complete(rpc, wrong).is_err());
                assert!(!consumer.completion_attempted);
                consumer.complete(rpc, consumer.admitted)
            });
            assert!(observed.request_seen);
            assert!(observed.drained.is_empty());
            assert_eq!(result.unwrap(), binding);
        },
    );
}

#[test]
fn model_consumer_complete_refuses_bad_and_negative_ack() {
    for (case, ack) in [("wrong", Ack::WrongVariant), ("negative", Ack::Negative)] {
        isolated(
            "model_consumer_complete_refuses_bad_and_negative_ack",
            case,
            || {
                let (result, _, observed) = exchange_case(0, ack, |consumer, rpc| {
                    let result = consumer.complete(rpc, consumer.admitted);
                    assert!(consumer.completion_attempted);
                    assert!(consumer.complete(rpc, consumer.admitted).is_err());
                    result
                });
                assert!(observed.request_seen);
                assert!(observed.drained.is_empty());
                assert_eq!(
                    result.unwrap_err().to_string(),
                    "native successor completion ACK differs"
                );
            },
        );
    }
}

#[test]
fn model_consumer_complete_refuses_other_slot_record_at_ack() {
    isolated(
        "model_consumer_complete_refuses_other_slot_record_at_ack",
        "late",
        || {
            let (result, _, observed) = exchange_case(0, Ack::WithLateRecord, |consumer, rpc| {
                consumer.complete(rpc, consumer.admitted)
            });
            assert!(observed.request_seen);
            assert!(observed.drained.is_empty());
            assert!(
                result.is_err(),
                "ACK cannot erase a new retained other-thread record"
            );
            assert!(matches!(
                detlog::check_empty_forwarding_cut(),
                Err(detlog::ForwardingCutError::PendingRecords { count: 1, .. })
            ));
        },
    );
}

fn configured_rpc(tid: i32) -> (CoordinatorRpc<detcore::GlobalState>, UnixStream) {
    let (stream, mut peer) = UnixStream::pair().unwrap();
    for socket in [&stream, &peer] {
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
    }
    write_frame(&mut peer, &detcore::Config::default());
    let client =
        BlockingRpcClient::from_connected_stream(GuestRpcStream::from(stream), Pid::from_raw(tid))
            .unwrap();
    let rpc = CoordinatorRpc::from_live_client("unused-transport-fixture", client, |_, _| {
        panic!("completion transport fixture must not reconnect")
    })
    .unwrap();
    (rpc, peer)
}

#[test]
fn model_consumer_complete_refuses_actual_in_flight_request() {
    isolated(
        "model_consumer_complete_refuses_actual_in_flight_request",
        "busy",
        || {
            let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
            let mut consumer = modeled_consumer(tid);
            let clock = consumer.cut_clock.as_ref().unwrap().clone();
            let expected_clock = clock.as_nanos();
            let mm = consumer.cut.prepared_mm;
            let marker = ExecTicket([97; 32]);
            let (rpc, mut peer) = configured_rpc(tid);
            let (arrived, received) = std::sync::mpsc::sync_channel(0);
            let (release, released) = std::sync::mpsc::sync_channel(0);
            let server = std::thread::spawn(move || {
                // This separate test-marked exchange holds the real shared
                // client lock. It does not use or admit the tested Consumer.
                let envelope = read_frame(&mut peer).unwrap();
                assert_eq!(envelope.from, Pid::from_raw(tid));
                let (actual_clock, actual_mm, request) = envelope.request;
                assert_eq!(actual_clock.as_nanos(), expected_clock);
                assert_eq!(actual_mm, mm);
                assert!(matches!(request, Q::WithForwardedCount(0, request)
                    if matches!(*request, Q::CompleteNativeExec { ticket } if ticket == marker)));
                arrived.send(()).unwrap();
                released.recv_timeout(Duration::from_secs(3)).unwrap();
                let response: Response = (None, R::CompleteNativeExec(Ok(())));
                write_frame(&mut peer, &response);
                assert!(
                    read_frame(&mut peer).is_none(),
                    "busy Complete and its retry must not send another request"
                );
            });
            std::thread::scope(|scope| {
                let blocker = scope.spawn(|| {
                    rpc.send_blocking_unless_in_flight((
                        clock,
                        mm,
                        Q::WithForwardedCount(
                            0,
                            Box::new(Q::CompleteNativeExec { ticket: marker }),
                        ),
                    ))
                });
                // Receipt of the full first frame, with no response sent,
                // proves the actual connection is held; no timing assumption.
                received.recv_timeout(Duration::from_secs(3)).unwrap();
                let result = consumer.complete(&rpc, consumer.admitted);
                assert_eq!(
                    result.unwrap_err().to_string(),
                    "native successor completion transport failed"
                );
                assert!(consumer.completion_attempted);
                assert!(consumer.cut_clock.is_none());
                assert_eq!(
                    consumer
                        .complete(&rpc, consumer.admitted)
                        .unwrap_err()
                        .to_string(),
                    "native successor completion is out of order"
                );
                release.send(()).unwrap();
                assert_eq!(
                    blocker.join().unwrap().unwrap(),
                    (None, R::CompleteNativeExec(Ok(())))
                );
            });
            drop(rpc);
            server.join().unwrap();
            assert_eq!(detlog::check_empty_forwarding_cut(), Ok(()));
        },
    );
}

#[test]
fn model_consumer_complete_disconnected_peer_is_terminal() {
    const KEY: &str = "HERMIT_NATIVE_CONSUMER_CLOSED_SOCKET";
    const NAME: &str =
        "native_exec_consumer::tests::model_consumer_complete_disconnected_peer_is_terminal";
    if let Some(path) = std::env::var_os(KEY) {
        let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let mut consumer = modeled_consumer(tid);
        let stream = UnixStream::connect(path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let client = BlockingRpcClient::from_connected_stream(
            GuestRpcStream::from(stream),
            Pid::from_raw(tid),
        )
        .unwrap();
        let rpc = CoordinatorRpc::from_live_client("unused-closed-peer", client, |_, _| {
            panic!("closed completion fixture must not reconnect")
        })
        .unwrap();
        let _ = consumer.complete(&rpc, consumer.admitted);
        // A returned result cannot masquerade as the expected real fatal123.
        unsafe { libc::_exit(73) };
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("complete.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
        .env(KEY, &path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut peer = loop {
        match listener.accept() {
            Ok((peer, _)) => break peer,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                assert!(
                    child.try_wait().unwrap().is_none(),
                    "child exited before connect"
                );
                assert!(
                    std::time::Instant::now() < deadline,
                    "child did not connect"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("accept completion child: {error}"),
        }
    };
    peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    peer.set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    write_frame(&mut peer, &detcore::Config::default());
    // Keep Read open to observe the exact request and avoid inducing SIGPIPE;
    // the real configured client encounters clean EOF only on its ACK read.
    peer.shutdown(std::net::Shutdown::Write).unwrap();
    let envelope =
        read_frame(&mut peer).expect("child must submit actual Complete before fatal123");
    assert!(envelope.from.as_raw() > 0);
    let expected = modeled_consumer(envelope.from.as_raw());
    let (clock, mm, request) = envelope.request;
    assert_eq!(
        clock.as_nanos(),
        expected.cut_clock.as_ref().unwrap().as_nanos()
    );
    assert_eq!(mm, expected.cut.prepared_mm);
    assert!(matches!(request, Q::WithForwardedCount(0, request)
        if matches!(*request, Q::CompleteNativeExec { ticket } if ticket == expected.ticket)));
    assert!(
        read_frame(&mut peer).is_none(),
        "no second Complete after EOF"
    );
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(123),
        "shared CoordinatorRpc must terminal on missing ACK, not return or signal: {:?}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // Terminal state cannot be read from the dead child. The busy case proves
    // actual attempt consumption; this peer proves request/no-ACK/no-retry.
}
