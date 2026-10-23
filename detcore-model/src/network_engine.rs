/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The pure state machine behind network record and replay.
//!
//! The engine models the guest-visible state of each outbound TCP channel:
//! which inbound bytes have been released to the guest, whether the peer
//! closed or reset the connection, how many bytes the guest transmitted, and
//! the receive low-water mark. It performs no I/O. The runtime asks it what a
//! guest `recv`, `poll` or `send` observes at a given global time, and in
//! record mode feeds it the bytes it pulled from the host.
//!
//! Determinism rests on the release rule of [`NetworkReleaseV1`]: an input
//! becomes visible once global time has reached the time at which the
//! recording first observed it *and* the guest has transmitted at least as
//! many bytes on that channel as it had by then. Both quantities are
//! functions of the guest's own deterministic execution, so a replay never
//! depends on host timing, and a replay that follows the recorded schedule
//! observes every input at the same check that the recording did.
//!
//! A blocking receive completes once enough bytes are available to satisfy
//! its target, all at once. Linux may copy a partial segment into the buffer
//! of a waiting reader before the rest arrives; the engine instead hands out
//! the bytes as if they had arrived together, which is one of the outcomes
//! Linux permits.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::error::Error;
use std::fmt;

use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;

use crate::fd::OpenFileId;
use crate::network_trace::NetworkAddressV1;
use crate::network_trace::NetworkChannelV2;
use crate::network_trace::NetworkEndpointRoleV1;
use crate::network_trace::NetworkInputEventV1;
use crate::network_trace::NetworkInputKindV1;
use crate::network_trace::NetworkOutputV4;
use crate::network_trace::NetworkReleaseV1;
use crate::network_trace::NetworkSendWaitV4;
use crate::network_trace::NetworkTraceV2;
use crate::network_trace::NetworkTraceValidationError;
use crate::network_trace::NetworkTransportV1;
use crate::time::LogicalTime;

/// Serde for the guest bytes that network requests and replies carry, as one
/// byte string rather than serde's default for `Vec<u8>`, a sequence of
/// separately serialized `u8` elements.
///
/// Requests cross a serializer on their way to the global state: the RPC
/// transport of a backend whose tool runs in the guest process, and, in a build
/// with debug assertions (the `dev` and `validate` profiles), reverie-ptrace's
/// round trip of every request through bincode. In replay, a blocking send that
/// waits for buffer space passes its whole unsent remainder on every wait, so the
/// per-element cost of a sequence is paid again on every wait: a 252 KB
/// remainder took about 21 ms per bincode round trip in a `dev` build, against
/// 0.02 ms as a byte string, and the replay of a send that had waited a few
/// thousand times outran its test's wall bound. Bincode writes both forms
/// identically, a length and then the raw bytes, so the encoding does not
/// change; decoding still accepts a sequence, which is how self-describing
/// formats such as JSON write bytes.
mod guest_bytes {
    use std::fmt;

    use serde::Deserializer;
    use serde::Serializer;
    use serde::de::Error;
    use serde::de::SeqAccess;
    use serde::de::Visitor;

    pub(super) fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(bytes)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<u8>, D::Error> {
        deserializer.deserialize_byte_buf(GuestBytes)
    }

    struct GuestBytes;

    impl<'de> Visitor<'de> for GuestBytes {
        type Value = Vec<u8>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("guest bytes")
        }

        fn visit_bytes<E: Error>(self, bytes: &[u8]) -> Result<Vec<u8>, E> {
            Ok(bytes.to_vec())
        }

        fn visit_byte_buf<E: Error>(self, bytes: Vec<u8>) -> Result<Vec<u8>, E> {
            Ok(bytes)
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Vec<u8>, A::Error> {
            // Never trust a length hint for an allocation.
            let mut bytes = Vec::with_capacity(sequence.size_hint().unwrap_or(0).min(1 << 16));
            while let Some(byte) = sequence.next_element()? {
                bytes.push(byte);
            }
            Ok(bytes)
        }
    }
}

/// Something the recorder pulled from a host socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkArrival {
    Bytes(#[serde(with = "guest_bytes")] Vec<u8>),
    /// The peer shut down its write side (the host `recv` returned 0).
    PeerWriteClosed,
    /// The host socket reported this positive errno.
    Error(i32),
}

/// What a guest receive observes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkRecvOutcome {
    Data(#[serde(with = "guest_bytes")] Vec<u8>),
    /// End of stream: `recv` returns 0.
    Eof,
    /// A pending socket error, consumed by this receive.
    Error(i32),
    /// Not enough bytes for the receive's target yet.
    WouldBlock,
}

/// A failure that ends the run: the guest asked for something the recording
/// cannot answer, or that this version does not model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkEngineError {
    /// The operation needs a different mode (for example a replay-only call
    /// during record).
    WrongMode,
    UnknownChannel(OpenFileId),
    /// A channel connected twice.
    ChannelExists(OpenFileId),
    /// Replay connected a socket the recording never connected.
    NotInTrace(OpenFileId),
    PeerMismatch {
        channel: OpenFileId,
        recorded: NetworkAddressV1,
        attempted: NetworkAddressV1,
    },
    /// Replay transmitted bytes that differ from the recording.
    OutboundMismatch {
        channel: OpenFileId,
        offset: u64,
    },
    /// Replay transmitted more bytes than the recording holds.
    OutboundBeyondRecording {
        channel: OpenFileId,
        offset: u64,
    },
    /// Replay made a blocking send where the recording holds a nonblocking
    /// send that Linux refused with `EAGAIN`.
    BlockingSendAtRefusal {
        channel: OpenFileId,
        offset: u64,
    },
    /// Replay made a nonblocking send where the recording holds a fragment
    /// that a blocking send waited for buffer space to send.
    NonblockingSendAtWait {
        channel: OpenFileId,
        offset: u64,
    },
    /// Record: another send, accepted or refused, reached the stream while a
    /// blocking send waited for buffer space, so the stream interleaves the
    /// two.
    InterleavedSend {
        channel: OpenFileId,
        offset: u64,
    },
    /// The recorder fed an arrival after the stream ended, or an empty one.
    InvalidArrival(OpenFileId),
    /// The finished recording failed validation.
    Trace(NetworkTraceValidationError),
    /// Record connected to a peer a trace cannot hold.
    UntraceablePeer(NetworkAddressV1),
    /// Replay waited [`REPLAY_STALL_LIMIT`] past the recorded arrival of an
    /// input that the recording released only after the guest had sent
    /// `needed` bytes; this replay has sent `sent`.
    ReplayStalled {
        channel: OpenFileId,
        needed: u64,
        sent: u64,
    },
    /// Replay ended without connecting a socket the recording connected.
    ReplayUnconnected(OpenFileId),
    /// Replay ended having sent `sent` of the `recorded` bytes on a channel.
    ReplayUnsent {
        channel: OpenFileId,
        sent: u64,
        recorded: u64,
    },
    /// Replay ended without the nonblocking send that the recording holds,
    /// refused with `EAGAIN`, at stream offset `offset`.
    ReplayUnrefused {
        channel: OpenFileId,
        offset: u64,
    },
}

/// How long past an input's recorded arrival, in global (virtual) time,
/// replay waits for the guest to send the bytes that gate it before declaring
/// that the guest diverged. It has the same length as the host waits of record
/// mode, but those are wall-clock waits.
pub const REPLAY_STALL_LIMIT: std::time::Duration = std::time::Duration::from_secs(60);

/// The remedy for a replay whose guest did something other than what the
/// recording holds.
const DIVERGED_REMEDY: &str = "The replayed program diverged from the recording. Replay the same \
     program with the same arguments, files and environment, or record it again with \
     --record-networking.";

impl NetworkEngineError {
    /// What the user can do about this error.
    pub fn remedy(&self) -> &'static str {
        match self {
            Self::NotInTrace(_)
            | Self::PeerMismatch { .. }
            | Self::OutboundMismatch { .. }
            | Self::OutboundBeyondRecording { .. }
            | Self::BlockingSendAtRefusal { .. }
            | Self::NonblockingSendAtWait { .. }
            | Self::ReplayStalled { .. }
            | Self::ReplayUnconnected(_)
            | Self::ReplayUnsent { .. }
            | Self::ReplayUnrefused { .. } => DIVERGED_REMEDY,
            Self::InterleavedSend { .. } => {
                "Network record/replay does not model two threads sending on one socket \
                 while one waits for buffer space; send from one thread at a time, or run \
                 without --record-networking."
            }
            Self::UntraceablePeer(_) => {
                "Connect to a specific host address and nonzero port, for example \
                 127.0.0.1 instead of 0.0.0.0, or run without --record-networking."
            }
            Self::ChannelExists(_) => {
                "Network record/replay supports one connect per socket; run this program \
                 without --record-networking."
            }
            Self::Trace(_) => {
                "The recording is outside what network record/replay supports; run this \
                 program without --record-networking."
            }
            Self::WrongMode | Self::UnknownChannel(_) | Self::InvalidArrival(_) => {
                "This is a hermit defect; report it with the command line that produced it."
            }
        }
    }
}

impl fmt::Display for NetworkEngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongMode => write!(f, "network engine called in the wrong mode"),
            Self::UnknownChannel(id) => write!(f, "network trace: {id} is not connected"),
            Self::ChannelExists(id) => write!(
                f,
                "network trace: {id} connected twice, which network record/replay does not support"
            ),
            Self::NotInTrace(id) => write!(f, "network trace has no recorded connection for {id}"),
            Self::PeerMismatch {
                channel,
                recorded,
                attempted,
            } => write!(
                f,
                "network trace mismatch: {channel} connected to {attempted:?}, but the recording connected it to {recorded:?}"
            ),
            Self::OutboundMismatch { channel, offset } => write!(
                f,
                "network outbound mismatch: {channel} sent different bytes than the recording at stream offset {offset}"
            ),
            Self::OutboundBeyondRecording { channel, offset } => write!(
                f,
                "network outbound mismatch: {channel} sent bytes past the end of the recording at stream offset {offset}"
            ),
            Self::BlockingSendAtRefusal { channel, offset } => write!(
                f,
                "network outbound mismatch: {channel} made a blocking send at stream offset \
                 {offset}, where the recording holds a nonblocking send refused with EAGAIN"
            ),
            Self::NonblockingSendAtWait { channel, offset } => write!(
                f,
                "network outbound mismatch: {channel} made a nonblocking send at stream \
                 offset {offset}, where the recording holds a blocking send that waited for \
                 buffer space"
            ),
            Self::InterleavedSend { channel, offset } => write!(
                f,
                "network record: another send on {channel}, accepted or refused, reached \
                 stream offset {offset} while a blocking send waited for buffer space"
            ),
            Self::InvalidArrival(id) => write!(
                f,
                "network record received input for {id} after its stream ended"
            ),
            Self::Trace(error) => write!(f, "network trace: {error}"),
            Self::UntraceablePeer(peer) => write!(
                f,
                "network record cannot trace a connection to {peer:?}, which names no single host"
            ),
            Self::ReplayStalled {
                channel,
                needed,
                sent,
            } => write!(
                f,
                "network replay stalled: {channel} is waiting for input that the recording \
                 delivered after {needed} bytes were sent, but this replay has sent {sent} \
                 after {} seconds of virtual time past the recorded arrival",
                REPLAY_STALL_LIMIT.as_secs()
            ),
            Self::ReplayUnconnected(id) => write!(
                f,
                "network replay ended without connecting {id}, which the recording connected"
            ),
            Self::ReplayUnsent {
                channel,
                sent,
                recorded,
            } => write!(
                f,
                "network replay ended after {channel} sent {sent} of the {recorded} bytes the \
                 recording sent"
            ),
            Self::ReplayUnrefused { channel, offset } => write!(
                f,
                "network replay ended before {channel} made the nonblocking send at stream \
                 offset {offset} that the recording holds, refused with EAGAIN"
            ),
        }
    }
}

impl Error for NetworkEngineError {}

/// One engine operation, carried from a guest thread to the global state.
///
/// Every request that observes inbound state carries the arrivals the
/// recorder pulled from the host just before it, so that one round trip
/// both records the pull and answers the guest at the same global time.
/// Replay sends no arrivals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkRequest {
    /// See [`NetworkEngine::record_connect`].
    RecordConnect {
        id: OpenFileId,
        peer: NetworkAddressV1,
        local: Option<NetworkAddressV1>,
        errno: i32,
        nonblocking: bool,
    },
    /// See [`NetworkEngine::replay_connect`].
    ReplayConnect {
        id: OpenFileId,
        peer: NetworkAddressV1,
        nonblocking: bool,
    },
    /// See [`NetworkEngine::recv`].
    Recv {
        id: OpenFileId,
        arrivals: Vec<NetworkArrival>,
        max_len: usize,
        target: usize,
        peek: bool,
    },
    /// The readiness of each channel given its `SO_RCVLOWAT`; see
    /// [`NetworkEngine::readiness`].
    Readiness(Vec<(OpenFileId, usize, Vec<NetworkArrival>)>),
    /// See [`NetworkEngine::take_error`].
    TakeError(OpenFileId),
    /// See [`NetworkEngine::send_failure`].
    SendFailure(OpenFileId),
    /// See [`NetworkEngine::record_send`].
    RecordSend {
        id: OpenFileId,
        #[serde(with = "guest_bytes")]
        bytes: Vec<u8>,
        at: Option<SendMark>,
        waits: u64,
    },
    /// See [`NetworkEngine::record_refused_send`].
    RecordRefusedSend(OpenFileId),
    /// See [`NetworkEngine::replay_send`].
    ReplaySend {
        id: OpenFileId,
        #[serde(with = "guest_bytes")]
        bytes: Vec<u8>,
        nonblocking: bool,
        waits: u64,
    },
    /// See [`NetworkEngine::shutdown`].
    Shutdown { id: OpenFileId, how: i32 },
    /// See [`NetworkEngine::addresses`].
    Addresses(OpenFileId),
}

/// Where a recorded send left a channel's outbound stream: its offset, and
/// how many output events (accepted chunks and refused sends) the channel
/// has recorded. A blocking send that waits for buffer space passes its mark
/// back with its next chunk; any output event in between changes `events`,
/// even a refusal, which does not move `offset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendMark {
    pub offset: u64,
    pub events: u64,
}

/// What [`NetworkEngine::replay_send`] did with a guest send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaySendOutcome {
    /// The send transmits this many bytes.
    Sent(usize),
    /// A nonblocking send that the recording refused with `EAGAIN`.
    WouldBlock,
    /// A blocking send whose next fragment is not yet due; it waits again.
    NotYet,
}

/// The answer to a [`NetworkRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkReply {
    /// `0` or a positive errno.
    Errno(i32),
    /// A positive errno, or `None` for success.
    Failure(Option<i32>),
    Recv(NetworkRecvOutcome),
    /// Poll events, one per requested channel.
    Events(Vec<i16>),
    Sent(usize),
    /// Where a recorded send left the outbound stream.
    Recorded(SendMark),
    /// A nonblocking send that the recording refused with `EAGAIN`.
    WouldBlock,
    /// A blocking send that has not yet waited as long as the recording did
    /// before Linux accepted the next fragment; it waits again.
    NotYet,
    /// Local (if connected) and peer address, or `None` for an unknown channel.
    Addresses(Option<(Option<NetworkAddressV1>, NetworkAddressV1)>),
}

#[derive(Debug)]
enum Mode {
    Record {
        epoch: DateTime<Utc>,
        inputs: Vec<NetworkInputEventV1>,
        outputs: Vec<NetworkOutputV4>,
    },
    Replay {
        /// Recorded channels the guest has not connected yet, each holding
        /// its input and output streams.
        unconnected: BTreeMap<OpenFileId, Channel>,
    },
}

#[derive(Debug)]
struct Channel {
    record: NetworkChannelV2,
    /// Released bytes the guest has not read yet.
    rx: VecDeque<u8>,
    /// Stream offset just past the last released byte.
    rx_end: u64,
    peer_closed: bool,
    /// The connection was reset or failed: both directions are shut down.
    reset: bool,
    /// A socket error the guest has not consumed yet.
    pending_error: Option<i32>,
    /// Bytes the guest has transmitted.
    tx: u64,
    /// Record: output events recorded, accepted chunks and refused sends.
    output_events: u64,
    shut_rd: bool,
    shut_wr: bool,
    /// Replay: inputs not yet released, in order.
    pending_inputs: VecDeque<NetworkInputEventV1>,
    /// Replay: outbound fragments not yet fully matched, in order.
    expected_outputs: VecDeque<NetworkOutputV4>,
}

impl Channel {
    fn new(record: NetworkChannelV2) -> Self {
        Channel {
            reset: record.connect_errno != 0,
            pending_error: None,
            record,
            rx: VecDeque::new(),
            rx_end: 0,
            peer_closed: false,
            tx: 0,
            output_events: 0,
            shut_rd: false,
            shut_wr: false,
            pending_inputs: VecDeque::new(),
            expected_outputs: VecDeque::new(),
        }
    }

    fn send_mark(&self) -> SendMark {
        SendMark {
            offset: self.tx,
            events: self.output_events,
        }
    }

    fn input_ended(&self) -> bool {
        self.peer_closed || self.reset
    }

    fn apply(&mut self, event: NetworkInputKindV1) {
        match event {
            NetworkInputKindV1::InboundBytes { bytes, .. } => {
                self.rx_end += bytes.len() as u64;
                self.rx.extend(bytes);
            }
            NetworkInputKindV1::PeerWriteClosed { .. } => self.peer_closed = true,
            NetworkInputKindV1::SocketError { errno, .. } => {
                self.reset = true;
                self.pending_error = Some(errno);
            }
        }
    }

    /// Replay: fail if the next input has been due for [`REPLAY_STALL_LIMIT`]
    /// and is still held back only by bytes the guest has not sent.
    fn check_stalled(&self, now: LogicalTime) -> Result<(), NetworkEngineError> {
        let Some(input) = self.pending_inputs.front() else {
            return Ok(());
        };
        let release = &input.release;
        if self.tx < release.after_transmitted_offset
            && now >= release.not_before_global_time + REPLAY_STALL_LIMIT
        {
            return Err(NetworkEngineError::ReplayStalled {
                channel: self.record.id,
                needed: release.after_transmitted_offset,
                sent: self.tx,
            });
        }
        Ok(())
    }

    fn release(&mut self, now: LogicalTime) {
        while let Some(input) = self.pending_inputs.front() {
            if !input.release.is_eligible(now, self.tx) {
                break;
            }
            let input = self.pending_inputs.pop_front().expect("front exists");
            self.apply(input.event);
        }
    }
}

/// The guest-visible network state of one record or replay run.
#[derive(Debug)]
pub struct NetworkEngine {
    mode: Mode,
    channels: BTreeMap<OpenFileId, Channel>,
}

impl NetworkEngine {
    /// Start recording; `epoch` is the run's configured epoch.
    pub fn new_record(epoch: DateTime<Utc>) -> Self {
        NetworkEngine {
            mode: Mode::Record {
                epoch,
                inputs: Vec::new(),
                outputs: Vec::new(),
            },
            channels: BTreeMap::new(),
        }
    }

    /// Replay `trace`, which must already have passed validation.
    pub fn new_replay(trace: NetworkTraceV2) -> Self {
        let mut unconnected: BTreeMap<OpenFileId, Channel> = trace
            .channels
            .into_iter()
            .map(|record| (record.id, Channel::new(record)))
            .collect();
        for input in trace.inputs {
            if let Some(channel) = unconnected.get_mut(&input.channel) {
                channel.pending_inputs.push_back(input);
            }
        }
        for output in trace.outputs {
            if let Some(channel) = unconnected.get_mut(&output.channel) {
                channel.expected_outputs.push_back(output);
            }
        }
        NetworkEngine {
            mode: Mode::Replay { unconnected },
            channels: BTreeMap::new(),
        }
    }

    pub fn is_record(&self) -> bool {
        matches!(self.mode, Mode::Record { .. })
    }

    /// Whether the guest connected `id` as a network channel.
    pub fn is_channel(&self, id: OpenFileId) -> bool {
        self.channels.contains_key(&id)
    }

    fn channel(&mut self, id: OpenFileId) -> Result<&mut Channel, NetworkEngineError> {
        self.channels
            .get_mut(&id)
            .ok_or(NetworkEngineError::UnknownChannel(id))
    }

    fn check_new(&self, id: OpenFileId) -> Result<(), NetworkEngineError> {
        if self.channels.contains_key(&id) {
            return Err(NetworkEngineError::ChannelExists(id));
        }
        Ok(())
    }

    /// Start the guest-visible life of `channel` and return what the guest
    /// `connect` returns: `0` or a positive errno. A nonblocking connect
    /// always returns `EINPROGRESS` and leaves the outcome for `SO_ERROR`,
    /// which is one of the behaviours Linux permits even on loopback.
    fn connected(&mut self, mut channel: Channel, nonblocking: bool) -> i32 {
        let errno = channel.record.connect_errno;
        let result = if nonblocking {
            channel.pending_error = (errno != 0).then_some(errno);
            libc::EINPROGRESS
        } else {
            errno
        };
        self.channels.insert(channel.record.id, channel);
        result
    }

    /// Record a connect the runtime performed on the host. `errno` is `0` on
    /// success, and `local` is the host-assigned local address.
    pub fn record_connect(
        &mut self,
        id: OpenFileId,
        peer: NetworkAddressV1,
        local: Option<NetworkAddressV1>,
        errno: i32,
        nonblocking: bool,
    ) -> Result<i32, NetworkEngineError> {
        if !self.is_record() {
            return Err(NetworkEngineError::WrongMode);
        }
        self.check_new(id)?;
        if !peer.is_traceable_peer() {
            return Err(NetworkEngineError::UntraceablePeer(peer));
        }
        let record = NetworkChannelV2 {
            id,
            transport: NetworkTransportV1::Tcp,
            role: NetworkEndpointRoleV1::OutboundClient,
            local_address: if errno == 0 { local } else { None },
            peer_address: peer,
            connect_errno: errno,
        };
        Ok(self.connected(Channel::new(record), nonblocking))
    }

    /// Replay the recorded connect of `id`, which must target the recorded peer.
    pub fn replay_connect(
        &mut self,
        id: OpenFileId,
        peer: &NetworkAddressV1,
        nonblocking: bool,
    ) -> Result<i32, NetworkEngineError> {
        self.check_new(id)?;
        let Mode::Replay { unconnected } = &mut self.mode else {
            return Err(NetworkEngineError::WrongMode);
        };
        let recorded = &unconnected
            .get(&id)
            .ok_or(NetworkEngineError::NotInTrace(id))?
            .record
            .peer_address;
        if recorded != peer {
            return Err(NetworkEngineError::PeerMismatch {
                channel: id,
                recorded: recorded.clone(),
                attempted: peer.clone(),
            });
        }
        let channel = unconnected.remove(&id).expect("checked above");
        Ok(self.connected(channel, nonblocking))
    }

    /// The local and peer addresses of a connected channel, for
    /// `getsockname` and `getpeername`.
    pub fn addresses(
        &self,
        id: OpenFileId,
    ) -> Option<(Option<&NetworkAddressV1>, &NetworkAddressV1)> {
        self.channels.get(&id).map(|channel| {
            (
                channel.record.local_address.as_ref(),
                &channel.record.peer_address,
            )
        })
    }

    /// Whether the recorder should pull more input for `id` from the host.
    pub fn needs_pull(&self, id: OpenFileId) -> bool {
        self.is_record()
            && self
                .channels
                .get(&id)
                .is_some_and(|channel| !channel.input_ended())
    }

    /// Record what the runtime pulled from the host for `id` at global time
    /// `now`, and make it visible to the guest at once.
    pub fn record_arrival(
        &mut self,
        id: OpenFileId,
        now: LogicalTime,
        arrival: NetworkArrival,
    ) -> Result<(), NetworkEngineError> {
        let channel = self
            .channels
            .get_mut(&id)
            .ok_or(NetworkEngineError::UnknownChannel(id))?;
        let Mode::Record { inputs, .. } = &mut self.mode else {
            return Err(NetworkEngineError::WrongMode);
        };
        if channel.input_ended() || matches!(&arrival, NetworkArrival::Bytes(b) if b.is_empty()) {
            return Err(NetworkEngineError::InvalidArrival(id));
        }
        let stream_offset = channel.rx_end;
        let event = match arrival {
            NetworkArrival::Bytes(bytes) => NetworkInputKindV1::InboundBytes {
                stream_offset,
                bytes,
            },
            NetworkArrival::PeerWriteClosed => {
                NetworkInputKindV1::PeerWriteClosed { stream_offset }
            }
            NetworkArrival::Error(errno) => NetworkInputKindV1::SocketError {
                stream_offset,
                errno,
            },
        };
        inputs.push(NetworkInputEventV1 {
            ordinal: inputs.len() as u64,
            channel: id,
            release: NetworkReleaseV1 {
                not_before_global_time: now,
                after_transmitted_offset: channel.tx,
            },
            event: event.clone(),
        });
        channel.apply(event);
        Ok(())
    }

    /// A guest receive of at most `max_len` bytes at global time `now`.
    ///
    /// `target` is the byte count that completes a blocking receive:
    /// `min(SO_RCVLOWAT, max_len)`, or `max_len` under `MSG_WAITALL`. A
    /// nonblocking receive, or one whose timeout expired, passes `1`. With
    /// `peek` the bytes stay queued.
    pub fn recv(
        &mut self,
        id: OpenFileId,
        now: LogicalTime,
        max_len: usize,
        target: usize,
        peek: bool,
    ) -> Result<NetworkRecvOutcome, NetworkEngineError> {
        let channel = self.channel(id)?;
        channel.release(now);
        let available = channel.rx.len();
        // After `SHUT_RD` Linux still delivers the bytes already queued, then
        // reports end of file without waiting.
        if available > 0
            && (available >= target.max(1)
                || channel.input_ended()
                || channel.shut_rd
                || channel.pending_error.is_some())
        {
            let len = available.min(max_len);
            let bytes = if peek {
                channel.rx.iter().take(len).copied().collect()
            } else {
                channel.rx.drain(..len).collect()
            };
            return Ok(NetworkRecvOutcome::Data(bytes));
        }
        if available == 0 {
            // Linux consumes a pending error even under MSG_PEEK.
            if let Some(errno) = channel.pending_error.take() {
                return Ok(NetworkRecvOutcome::Error(errno));
            }
            if channel.input_ended() || channel.shut_rd {
                return Ok(NetworkRecvOutcome::Eof);
            }
        }
        channel.check_stalled(now)?;
        Ok(NetworkRecvOutcome::WouldBlock)
    }

    /// The `poll` events `id` reports at global time `now`, before masking
    /// with the requested events. Mirrors Linux `tcp_poll`; `lowat` is the
    /// socket's `SO_RCVLOWAT`, which the guest may set before it connects.
    pub fn readiness(
        &mut self,
        id: OpenFileId,
        now: LogicalTime,
        lowat: usize,
    ) -> Result<i16, NetworkEngineError> {
        let channel = self.channel(id)?;
        channel.release(now);
        channel.check_stalled(now)?;
        let rcv_shutdown = channel.shut_rd || channel.input_ended();
        let snd_shutdown = channel.shut_wr || channel.reset;
        let mut events = 0;
        if (rcv_shutdown && snd_shutdown) || channel.reset {
            events |= libc::POLLHUP;
        }
        if rcv_shutdown {
            events |= libc::POLLIN | libc::POLLRDNORM | libc::POLLRDHUP;
        } else if channel.rx.len() >= lowat.max(1) {
            events |= libc::POLLIN | libc::POLLRDNORM;
        }
        // Linux reports a socket shut down for writing as writable, so that
        // the write fails instead of blocking.
        events |= libc::POLLOUT | libc::POLLWRNORM;
        if channel.pending_error.is_some() {
            events |= libc::POLLERR;
        }
        Ok(events)
    }

    /// Consume and return the pending socket error at global time `now`, as
    /// `SO_ERROR` does; `0` when there is none. Inputs due by `now` are
    /// released first, so the first observer of a recorded error sees it
    /// whichever operation it uses.
    pub fn take_error(
        &mut self,
        id: OpenFileId,
        now: LogicalTime,
    ) -> Result<i32, NetworkEngineError> {
        let channel = self.channel(id)?;
        channel.release(now);
        Ok(channel.pending_error.take().unwrap_or(0))
    }

    /// The errno a guest send at global time `now` fails with before
    /// transmitting anything, or `None` when the send may proceed. Inputs due
    /// by `now` are released first. A pending error is consumed first;
    /// otherwise a socket shut down for writing fails with `EPIPE`.
    pub fn send_failure(
        &mut self,
        id: OpenFileId,
        now: LogicalTime,
    ) -> Result<Option<i32>, NetworkEngineError> {
        let channel = self.channel(id)?;
        channel.release(now);
        if channel.reset || channel.shut_wr {
            return Ok(Some(channel.pending_error.take().unwrap_or(libc::EPIPE)));
        }
        Ok(None)
    }

    /// Record the bytes a host send accepted at `now` and return where they
    /// left the stream. Empty `sent` records nothing and returns the current
    /// mark, which a blocking send takes before its first wait. `at` is the
    /// mark of a send that then waited for buffer space; any output event on
    /// the channel in the meantime, accepted or refused, is refused, because
    /// replay would order the two the other way. `waits` counts the send's
    /// waits since it started or since its previous accepted chunk; when it
    /// is nonzero the fragment records that count and `now`, so that replay
    /// accepts it only after waiting as long.
    pub fn record_send(
        &mut self,
        id: OpenFileId,
        sent: &[u8],
        at: Option<SendMark>,
        now: LogicalTime,
        waits: u64,
    ) -> Result<SendMark, NetworkEngineError> {
        let channel = self
            .channels
            .get_mut(&id)
            .ok_or(NetworkEngineError::UnknownChannel(id))?;
        let Mode::Record { outputs, .. } = &mut self.mode else {
            return Err(NetworkEngineError::WrongMode);
        };
        if let Some(at) = at
            && at != channel.send_mark()
        {
            return Err(NetworkEngineError::InterleavedSend {
                channel: id,
                offset: at.offset,
            });
        }
        if sent.is_empty() {
            return Ok(channel.send_mark());
        }
        outputs.push(NetworkOutputV4 {
            channel: id,
            stream_offset: channel.tx,
            bytes: sent.to_vec(),
            wait: (waits > 0).then_some(NetworkSendWaitV4 {
                waits,
                not_before_global_time: now,
            }),
        });
        channel.tx += sent.len() as u64;
        channel.output_events += 1;
        Ok(channel.send_mark())
    }

    /// Record a nonblocking send that the host refused with `EAGAIN`, as an
    /// empty fragment at the current stream offset.
    pub fn record_refused_send(&mut self, id: OpenFileId) -> Result<(), NetworkEngineError> {
        let channel = self
            .channels
            .get_mut(&id)
            .ok_or(NetworkEngineError::UnknownChannel(id))?;
        let Mode::Record { outputs, .. } = &mut self.mode else {
            return Err(NetworkEngineError::WrongMode);
        };
        outputs.push(NetworkOutputV4 {
            channel: id,
            stream_offset: channel.tx,
            bytes: Vec::new(),
            wait: None,
        });
        channel.output_events += 1;
        Ok(())
    }

    /// Match a guest send at `now` against the recording and return how
    /// many bytes it transmits: the bytes left in the current recorded
    /// fragment, at most `bytes.len()`. A recorded short write therefore
    /// replays as the same short write. [`ReplaySendOutcome::WouldBlock`] is
    /// a nonblocking send that the recording refused with `EAGAIN`; a
    /// blocking send at that point diverged.
    ///
    /// A fragment that the recording accepted only after its send waited is
    /// due once this send has waited `waits` times as many and `now` has
    /// reached the recorded acceptance time; before that the send gets
    /// [`ReplaySendOutcome::NotYet`] and waits again. The check and the
    /// consumption happen in this one call, so nothing can run between them.
    pub fn replay_send(
        &mut self,
        id: OpenFileId,
        bytes: &[u8],
        nonblocking: bool,
        now: LogicalTime,
        waits: u64,
    ) -> Result<ReplaySendOutcome, NetworkEngineError> {
        if self.is_record() {
            return Err(NetworkEngineError::WrongMode);
        }
        let channel = self.channel(id)?;
        if bytes.is_empty() {
            return Ok(ReplaySendOutcome::Sent(0));
        }
        let offset = channel.tx;
        let fragment = channel.expected_outputs.front().ok_or(
            NetworkEngineError::OutboundBeyondRecording {
                channel: id,
                offset,
            },
        )?;
        if fragment.bytes.is_empty() {
            if !nonblocking {
                return Err(NetworkEngineError::BlockingSendAtRefusal {
                    channel: id,
                    offset,
                });
            }
            channel.expected_outputs.pop_front();
            return Ok(ReplaySendOutcome::WouldBlock);
        }
        let start = (offset - fragment.stream_offset) as usize;
        if start == 0
            && let Some(wait) = &fragment.wait
        {
            if nonblocking {
                return Err(NetworkEngineError::NonblockingSendAtWait {
                    channel: id,
                    offset,
                });
            }
            if waits < wait.waits || now < wait.not_before_global_time {
                return Ok(ReplaySendOutcome::NotYet);
            }
        }
        let remaining = &fragment.bytes[start..];
        let len = remaining.len().min(bytes.len());
        if remaining[..len] != bytes[..len] {
            let first_difference = remaining
                .iter()
                .zip(bytes)
                .position(|(expected, actual)| expected != actual)
                .unwrap_or(len);
            return Err(NetworkEngineError::OutboundMismatch {
                channel: id,
                offset: offset + first_difference as u64,
            });
        }
        channel.tx += len as u64;
        if len == remaining.len() {
            channel.expected_outputs.pop_front();
        }
        Ok(ReplaySendOutcome::Sent(len))
    }

    /// Apply `shutdown(how)`; returns `0` or the errno the guest sees.
    pub fn shutdown(&mut self, id: OpenFileId, how: i32) -> Result<i32, NetworkEngineError> {
        let channel = self.channel(id)?;
        if channel.record.connect_errno != 0 {
            return Ok(libc::ENOTCONN);
        }
        match how {
            libc::SHUT_RD => channel.shut_rd = true,
            libc::SHUT_WR => channel.shut_wr = true,
            libc::SHUT_RDWR => {
                channel.shut_rd = true;
                channel.shut_wr = true;
            }
            _ => return Ok(libc::EINVAL),
        }
        Ok(0)
    }

    /// Record pulled arrivals at `now`. Pulls that follow the end of the
    /// stream are dropped: a drained host socket keeps reporting it.
    fn record_arrivals(
        &mut self,
        id: OpenFileId,
        now: LogicalTime,
        arrivals: Vec<NetworkArrival>,
    ) -> Result<(), NetworkEngineError> {
        for arrival in arrivals {
            if !self.needs_pull(id) {
                break;
            }
            self.record_arrival(id, now, arrival)?;
        }
        Ok(())
    }

    /// Perform `request` at global time `now`.
    pub fn apply(
        &mut self,
        now: LogicalTime,
        request: NetworkRequest,
    ) -> Result<NetworkReply, NetworkEngineError> {
        Ok(match request {
            NetworkRequest::RecordConnect {
                id,
                peer,
                local,
                errno,
                nonblocking,
            } => NetworkReply::Errno(self.record_connect(id, peer, local, errno, nonblocking)?),
            NetworkRequest::ReplayConnect {
                id,
                peer,
                nonblocking,
            } => NetworkReply::Errno(self.replay_connect(id, &peer, nonblocking)?),
            NetworkRequest::Recv {
                id,
                arrivals,
                max_len,
                target,
                peek,
            } => {
                self.record_arrivals(id, now, arrivals)?;
                NetworkReply::Recv(self.recv(id, now, max_len, target, peek)?)
            }
            NetworkRequest::Readiness(channels) => {
                let mut events = Vec::with_capacity(channels.len());
                for (id, lowat, arrivals) in channels {
                    self.record_arrivals(id, now, arrivals)?;
                    events.push(self.readiness(id, now, lowat)?);
                }
                NetworkReply::Events(events)
            }
            NetworkRequest::TakeError(id) => NetworkReply::Errno(self.take_error(id, now)?),
            NetworkRequest::SendFailure(id) => NetworkReply::Failure(self.send_failure(id, now)?),
            NetworkRequest::RecordSend {
                id,
                bytes,
                at,
                waits,
            } => NetworkReply::Recorded(self.record_send(id, &bytes, at, now, waits)?),
            NetworkRequest::RecordRefusedSend(id) => {
                self.record_refused_send(id)?;
                NetworkReply::WouldBlock
            }
            NetworkRequest::ReplaySend {
                id,
                bytes,
                nonblocking,
                waits,
            } => match self.replay_send(id, &bytes, nonblocking, now, waits)? {
                ReplaySendOutcome::Sent(sent) => NetworkReply::Sent(sent),
                ReplaySendOutcome::WouldBlock => NetworkReply::WouldBlock,
                ReplaySendOutcome::NotYet => NetworkReply::NotYet,
            },
            NetworkRequest::Shutdown { id, how } => NetworkReply::Errno(self.shutdown(id, how)?),
            NetworkRequest::Addresses(id) => NetworkReply::Addresses(
                self.addresses(id)
                    .map(|(local, peer)| (local.cloned(), peer.clone())),
            ),
        })
    }

    /// End a replay: fail if the guest left part of the recording unused, by
    /// never connecting a recorded socket, by sending fewer bytes than the
    /// recording holds, or by never making a recorded refused send. Inputs the guest never read are not checked: when an
    /// input is released depends on the schedule, so a replay may end before
    /// a final input the recording happened to observe.
    pub fn finish_replay(self) -> Result<(), NetworkEngineError> {
        let Mode::Replay { unconnected } = self.mode else {
            return Err(NetworkEngineError::WrongMode);
        };
        if let Some(id) = unconnected.into_keys().next() {
            return Err(NetworkEngineError::ReplayUnconnected(id));
        }
        for (id, channel) in self.channels {
            if let Some(last) = channel.expected_outputs.back() {
                let recorded = last.stream_offset + last.bytes.len() as u64;
                return Err(if recorded > channel.tx {
                    NetworkEngineError::ReplayUnsent {
                        channel: id,
                        sent: channel.tx,
                        recorded,
                    }
                } else {
                    NetworkEngineError::ReplayUnrefused {
                        channel: id,
                        offset: channel.tx,
                    }
                });
            }
        }
        Ok(())
    }

    /// The trace of a record run. Fails if the run was a replay or the
    /// recording does not validate.
    pub fn finish(mut self) -> Result<NetworkTraceV2, NetworkEngineError> {
        let Mode::Record {
            epoch,
            inputs,
            outputs,
        } = self.mode
        else {
            return Err(NetworkEngineError::WrongMode);
        };
        let trace = NetworkTraceV2 {
            epoch,
            channels: std::mem::take(&mut self.channels)
                .into_values()
                .map(|channel| channel.record)
                .collect(),
            inputs,
            outputs,
        };
        trace.validate().map_err(NetworkEngineError::Trace)?;
        Ok(trace)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use chrono::TimeZone;

    use super::*;
    use crate::pid::DetTid;

    fn epoch() -> DateTime<Utc> {
        Utc.timestamp_opt(1_790_000_000, 0).unwrap()
    }

    /// Global time `n` microseconds after the epoch.
    fn at(n: u64) -> LogicalTime {
        LogicalTime::from_nanos(1_790_000_000 * 1_000_000_000 + n * 1_000)
    }

    fn sock(n: u64) -> OpenFileId {
        OpenFileId::new_socket(DetTid::from_raw(1), n)
    }

    fn mark(offset: u64, events: u64) -> SendMark {
        SendMark { offset, events }
    }

    /// Round-trips `request` through bincode in `config`, and checks that its
    /// guest `bytes` are written exactly as a plain `Vec<u8>` would be: their
    /// length, then the raw bytes.
    fn assert_bincode_round_trip<C: bincode::config::Config>(
        request: &NetworkRequest,
        bytes: &[u8],
        config: C,
    ) {
        let encoded = bincode::serde::encode_to_vec(request, config).unwrap();
        let (decoded, read): (NetworkRequest, usize) =
            bincode::serde::decode_from_slice(&encoded, config).unwrap();
        assert_eq!((&decoded, read), (request, encoded.len()));
        let plain = bincode::serde::encode_to_vec(bytes.to_vec(), config).unwrap();
        assert!(
            encoded.windows(plain.len()).any(|window| window == plain),
            "{request:?} does not carry its bytes as bincode writes a Vec<u8>"
        );
    }

    /// Round-trips `request` through bincode's legacy configuration, which the
    /// RPC transports and reverie-ptrace's debug round trip use, and, as
    /// compatibility checks, through bincode's standard configuration and
    /// through JSON, which writes guest bytes as a sequence.
    fn assert_survives_rpc_encodings(request: &NetworkRequest, bytes: &[u8]) {
        assert_bincode_round_trip(request, bytes, bincode::config::legacy());
        assert_bincode_round_trip(request, bytes, bincode::config::standard());
        let json = serde_json::to_string(request).unwrap();
        assert_eq!(
            &serde_json::from_str::<NetworkRequest>(&json).unwrap(),
            request
        );
    }

    fn peer() -> NetworkAddressV1 {
        NetworkAddressV1::Inet4 {
            address: [127, 0, 0, 1],
            port: 8080,
        }
    }

    fn local() -> Option<NetworkAddressV1> {
        Some(NetworkAddressV1::Inet4 {
            address: [127, 0, 0, 1],
            port: 40_000,
        })
    }

    fn data(bytes: &[u8]) -> NetworkRecvOutcome {
        NetworkRecvOutcome::Data(bytes.to_vec())
    }

    /// A request/response exchange: the response is pulled at time 20,
    /// after the guest sent "req", and the peer closes at time 30.
    fn recorded_exchange() -> NetworkTraceV2 {
        let mut engine = NetworkEngine::new_record(epoch());
        let id = sock(0);
        assert_eq!(engine.record_connect(id, peer(), local(), 0, false), Ok(0));
        assert!(engine.needs_pull(id));
        assert_eq!(
            engine.recv(id, at(10), 64, 1, false),
            Ok(NetworkRecvOutcome::WouldBlock)
        );
        engine.record_send(id, b"req", None, at(0), 0).unwrap();
        engine
            .record_arrival(id, at(20), NetworkArrival::Bytes(b"abcdef".to_vec()))
            .unwrap();
        assert_eq!(engine.recv(id, at(20), 3, 1, false), Ok(data(b"abc")));
        engine
            .record_arrival(id, at(30), NetworkArrival::PeerWriteClosed)
            .unwrap();
        assert!(!engine.needs_pull(id));
        assert_eq!(engine.recv(id, at(30), 64, 1, false), Ok(data(b"def")));
        assert_eq!(
            engine.recv(id, at(31), 64, 1, false),
            Ok(NetworkRecvOutcome::Eof)
        );
        engine.finish().unwrap()
    }

    #[test]
    fn record_produces_a_valid_trace_that_round_trips() {
        let trace = recorded_exchange();
        assert_eq!(trace.inputs.len(), 2);
        assert_eq!(
            trace.inputs[0].release,
            NetworkReleaseV1 {
                not_before_global_time: at(20),
                after_transmitted_offset: 3,
            }
        );
        let mut bytes = Vec::new();
        trace.write_framed(&mut bytes).unwrap();
        assert_eq!(
            NetworkTraceV2::read_framed(Cursor::new(bytes)).unwrap(),
            trace
        );
    }

    #[test]
    fn replay_with_the_recorded_schedule_observes_the_same_outcomes() {
        let mut engine = NetworkEngine::new_replay(recorded_exchange());
        let id = sock(0);
        assert_eq!(engine.replay_connect(id, &peer(), false), Ok(0));
        assert_eq!(
            engine.recv(id, at(10), 64, 1, false),
            Ok(NetworkRecvOutcome::WouldBlock)
        );
        assert_eq!(
            engine.replay_send(id, b"req", false, at(0), 0),
            Ok(ReplaySendOutcome::Sent(3))
        );
        assert_eq!(engine.recv(id, at(20), 3, 1, false), Ok(data(b"abc")));
        assert_eq!(engine.recv(id, at(30), 64, 1, false), Ok(data(b"def")));
        assert_eq!(
            engine.recv(id, at(31), 64, 1, false),
            Ok(NetworkRecvOutcome::Eof)
        );
    }

    #[test]
    fn replay_input_waits_for_both_time_and_transmission() {
        let mut engine = NetworkEngine::new_replay(recorded_exchange());
        let id = sock(0);
        engine.replay_connect(id, &peer(), false).unwrap();
        // Late enough, but the request has not been sent.
        assert_eq!(
            engine.recv(id, at(1_000), 64, 1, false),
            Ok(NetworkRecvOutcome::WouldBlock)
        );
        assert_eq!(
            engine.replay_send(id, b"req", false, at(0), 0),
            Ok(ReplaySendOutcome::Sent(3))
        );
        // Sent, but too early.
        assert_eq!(
            engine.recv(id, at(19), 64, 1, false),
            Ok(NetworkRecvOutcome::WouldBlock)
        );
        // Both: everything due by now is released together.
        assert_eq!(
            engine.recv(id, at(1_000), 64, 1, false),
            Ok(data(b"abcdef"))
        );
        assert_eq!(
            engine.recv(id, at(1_000), 64, 1, false),
            Ok(NetworkRecvOutcome::Eof)
        );
    }

    #[test]
    fn receive_target_follows_the_low_water_mark() {
        let mut engine = NetworkEngine::new_record(epoch());
        let id = sock(0);
        engine
            .record_connect(id, peer(), local(), 0, false)
            .unwrap();
        engine
            .record_arrival(id, at(1), NetworkArrival::Bytes(b"ab".to_vec()))
            .unwrap();
        assert_eq!(
            engine.readiness(id, at(1), 3),
            Ok(libc::POLLOUT | libc::POLLWRNORM)
        );
        assert_eq!(
            engine.recv(id, at(1), 3, 3, false),
            Ok(NetworkRecvOutcome::WouldBlock)
        );
        assert_eq!(engine.recv(id, at(1), 3, 1, true), Ok(data(b"ab")));
        engine
            .record_arrival(id, at(2), NetworkArrival::Bytes(b"cd".to_vec()))
            .unwrap();
        assert_eq!(
            engine.readiness(id, at(2), 3),
            Ok(libc::POLLIN | libc::POLLRDNORM | libc::POLLOUT | libc::POLLWRNORM)
        );
        assert_eq!(engine.recv(id, at(2), 3, 3, false), Ok(data(b"abc")));
        // With the stream ended, fewer bytes than the target complete the receive.
        engine
            .record_arrival(id, at(3), NetworkArrival::PeerWriteClosed)
            .unwrap();
        assert_eq!(engine.recv(id, at(3), 3, 3, false), Ok(data(b"d")));
        assert_eq!(
            engine.readiness(id, at(3), 3),
            Ok(libc::POLLIN
                | libc::POLLRDNORM
                | libc::POLLRDHUP
                | libc::POLLOUT
                | libc::POLLWRNORM)
        );
    }

    #[test]
    fn a_reset_delivers_queued_bytes_then_the_error_once_then_eof() {
        let mut engine = NetworkEngine::new_record(epoch());
        let id = sock(0);
        engine
            .record_connect(id, peer(), local(), 0, false)
            .unwrap();
        engine
            .record_arrival(id, at(1), NetworkArrival::Bytes(b"x".to_vec()))
            .unwrap();
        engine
            .record_arrival(id, at(1), NetworkArrival::Error(libc::ECONNRESET))
            .unwrap();
        assert_eq!(
            engine.record_arrival(id, at(2), NetworkArrival::PeerWriteClosed),
            Err(NetworkEngineError::InvalidArrival(id))
        );
        let readiness = engine.readiness(id, at(1), 1).unwrap();
        assert_eq!(readiness & libc::POLLERR, libc::POLLERR);
        assert_eq!(readiness & libc::POLLHUP, libc::POLLHUP);
        assert_eq!(engine.recv(id, at(1), 64, 64, false), Ok(data(b"x")));
        assert_eq!(
            engine.recv(id, at(1), 64, 1, true),
            Ok(NetworkRecvOutcome::Error(libc::ECONNRESET))
        );
        assert_eq!(
            engine.recv(id, at(1), 64, 1, false),
            Ok(NetworkRecvOutcome::Eof)
        );
        assert_eq!(engine.send_failure(id, at(1)), Ok(Some(libc::EPIPE)));
        engine.finish().unwrap();
    }

    /// A reset pulled at time 20, after the guest sent "req".
    fn recorded_reset() -> NetworkTraceV2 {
        let mut engine = NetworkEngine::new_record(epoch());
        let id = sock(0);
        engine
            .record_connect(id, peer(), local(), 0, false)
            .unwrap();
        engine.record_send(id, b"req", None, at(0), 0).unwrap();
        engine
            .record_arrival(id, at(20), NetworkArrival::Error(libc::ECONNRESET))
            .unwrap();
        assert_eq!(engine.take_error(id, at(20)), Ok(libc::ECONNRESET));
        engine.finish().unwrap()
    }

    #[test]
    fn so_error_and_send_release_a_due_error_as_its_first_observer() {
        let id = sock(0);
        // SO_ERROR first: not before the recorded time, then exactly once.
        let mut engine = NetworkEngine::new_replay(recorded_reset());
        engine.replay_connect(id, &peer(), false).unwrap();
        assert_eq!(
            engine.replay_send(id, b"req", false, at(0), 0),
            Ok(ReplaySendOutcome::Sent(3))
        );
        assert_eq!(engine.take_error(id, at(19)), Ok(0));
        assert_eq!(
            engine.apply(at(20), NetworkRequest::TakeError(id)),
            Ok(NetworkReply::Errno(libc::ECONNRESET))
        );
        assert_eq!(engine.take_error(id, at(21)), Ok(0));

        // A send first: not before "req" is sent, then the error once, then
        // EPIPE, without matching any byte against the recording.
        let mut engine = NetworkEngine::new_replay(recorded_reset());
        engine.replay_connect(id, &peer(), false).unwrap();
        assert_eq!(engine.send_failure(id, at(99)), Ok(None));
        assert_eq!(
            engine.replay_send(id, b"req", false, at(0), 0),
            Ok(ReplaySendOutcome::Sent(3))
        );
        assert_eq!(engine.send_failure(id, at(19)), Ok(None));
        assert_eq!(
            engine.apply(at(20), NetworkRequest::SendFailure(id)),
            Ok(NetworkReply::Failure(Some(libc::ECONNRESET)))
        );
        assert_eq!(engine.send_failure(id, at(21)), Ok(Some(libc::EPIPE)));
        assert_eq!(engine.take_error(id, at(21)), Ok(0));
    }

    #[test]
    fn failed_connects_replay_without_the_host() {
        let mut engine = NetworkEngine::new_record(epoch());
        assert_eq!(
            engine.record_connect(sock(0), peer(), None, libc::ECONNREFUSED, false),
            Ok(libc::ECONNREFUSED)
        );
        assert_eq!(
            engine.record_connect(sock(1), peer(), None, libc::ECONNREFUSED, true),
            Ok(libc::EINPROGRESS)
        );
        let trace = engine.finish().unwrap();

        let mut engine = NetworkEngine::new_replay(trace);
        assert_eq!(
            engine.replay_connect(sock(0), &peer(), false),
            Ok(libc::ECONNREFUSED)
        );
        assert_eq!(engine.take_error(sock(0), at(1)), Ok(0));
        assert_eq!(
            engine.replay_connect(sock(1), &peer(), true),
            Ok(libc::EINPROGRESS)
        );
        let readiness = engine.readiness(sock(1), at(1), 1).unwrap();
        assert_eq!(
            readiness & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP),
            libc::POLLOUT | libc::POLLERR | libc::POLLHUP
        );
        assert_eq!(engine.take_error(sock(1), at(1)), Ok(libc::ECONNREFUSED));
        assert_eq!(engine.take_error(sock(1), at(1)), Ok(0));
        assert_eq!(engine.shutdown(sock(1), libc::SHUT_WR), Ok(libc::ENOTCONN));
    }

    #[test]
    fn replay_refuses_connections_the_recording_lacks() {
        let mut engine = NetworkEngine::new_replay(recorded_exchange());
        assert_eq!(
            engine.replay_connect(sock(5), &peer(), false),
            Err(NetworkEngineError::NotInTrace(sock(5)))
        );
        let other = NetworkAddressV1::Inet4 {
            address: [127, 0, 0, 1],
            port: 9,
        };
        assert!(matches!(
            engine.replay_connect(sock(0), &other, false),
            Err(NetworkEngineError::PeerMismatch { .. })
        ));
        engine.replay_connect(sock(0), &peer(), false).unwrap();
        assert_eq!(
            engine.replay_connect(sock(0), &peer(), false),
            Err(NetworkEngineError::ChannelExists(sock(0)))
        );
    }

    #[test]
    fn replay_send_matches_bytes_and_preserves_fragments() {
        let mut engine = NetworkEngine::new_record(epoch());
        let id = sock(0);
        engine
            .record_connect(id, peer(), local(), 0, false)
            .unwrap();
        // A short write: the host accepted 4 of the guest's 8 bytes.
        engine.record_send(id, b"requ", None, at(0), 0).unwrap();
        engine.record_send(id, b"est\n", None, at(0), 0).unwrap();
        let trace = engine.finish().unwrap();

        let mut engine = NetworkEngine::new_replay(trace.clone());
        engine.replay_connect(id, &peer(), false).unwrap();
        assert_eq!(
            engine.replay_send(id, b"request\n", false, at(0), 0),
            Ok(ReplaySendOutcome::Sent(4))
        );
        assert_eq!(
            engine.replay_send(id, b"es", false, at(0), 0),
            Ok(ReplaySendOutcome::Sent(2))
        );
        assert_eq!(
            engine.replay_send(id, b"t\n", false, at(0), 0),
            Ok(ReplaySendOutcome::Sent(2))
        );
        assert_eq!(
            engine.replay_send(id, b"more", false, at(0), 0),
            Err(NetworkEngineError::OutboundBeyondRecording {
                channel: id,
                offset: 8
            })
        );

        let mut engine = NetworkEngine::new_replay(trace);
        engine.replay_connect(id, &peer(), false).unwrap();
        let error = engine
            .replay_send(id, b"reqX", false, at(0), 0)
            .unwrap_err();
        assert_eq!(
            error,
            NetworkEngineError::OutboundMismatch {
                channel: id,
                offset: 3
            }
        );
        let message = error.to_string();
        for word in ["network", "outbound", "mismatch"] {
            assert!(message.contains(word), "{message}");
        }
    }

    #[test]
    fn refused_nonblocking_sends_replay_as_eagain_at_the_same_offset() {
        let mut engine = NetworkEngine::new_record(epoch());
        let id = sock(0);
        engine
            .record_connect(id, peer(), local(), 0, false)
            .unwrap();
        // The host accepted a prefix of a nonblocking send, then refused
        // the next two attempts with EAGAIN before accepting the rest.
        assert_eq!(
            engine.record_send(id, b"re", None, at(0), 0),
            Ok(mark(2, 1))
        );
        engine.record_refused_send(id).unwrap();
        engine.record_refused_send(id).unwrap();
        assert_eq!(engine.record_send(id, b"q", None, at(0), 0), Ok(mark(3, 4)));
        engine.record_refused_send(id).unwrap();
        let trace = engine.finish().unwrap();
        assert!(trace.records_refused_sends());

        let mut engine = NetworkEngine::new_replay(trace.clone());
        engine.replay_connect(id, &peer(), false).unwrap();
        assert_eq!(
            engine.replay_send(id, b"req", true, at(0), 0),
            Ok(ReplaySendOutcome::Sent(2))
        );
        assert_eq!(
            engine.replay_send(id, b"q", true, at(0), 0),
            Ok(ReplaySendOutcome::WouldBlock)
        );
        assert_eq!(
            engine.replay_send(id, b"q", true, at(0), 0),
            Ok(ReplaySendOutcome::WouldBlock)
        );
        assert_eq!(
            engine.replay_send(id, b"q", true, at(0), 0),
            Ok(ReplaySendOutcome::Sent(1))
        );
        // The final refusal is part of the recording, too.
        let unfinished = NetworkEngineError::ReplayUnrefused {
            channel: id,
            offset: 3,
        };
        assert_eq!(unfinished.remedy(), DIVERGED_REMEDY);
        assert!(unfinished.to_string().contains("EAGAIN"), "{unfinished}");
        let mut finished = NetworkEngine::new_replay(trace.clone());
        finished.replay_connect(id, &peer(), false).unwrap();
        for (bytes, expected) in [
            (&b"req"[..], ReplaySendOutcome::Sent(2)),
            (b"q", ReplaySendOutcome::WouldBlock),
            (b"q", ReplaySendOutcome::WouldBlock),
        ] {
            assert_eq!(
                finished.replay_send(id, bytes, true, at(0), 0),
                Ok(expected)
            );
        }
        assert_eq!(
            finished.replay_send(id, b"q", true, at(0), 0),
            Ok(ReplaySendOutcome::Sent(1))
        );
        assert_eq!(
            finished.replay_send(id, b"x", true, at(0), 0),
            Ok(ReplaySendOutcome::WouldBlock)
        );
        assert_eq!(finished.finish_replay(), Ok(()));

        // A blocking send cannot reproduce a refusal: it diverged.
        let mut engine = NetworkEngine::new_replay(trace);
        engine.replay_connect(id, &peer(), false).unwrap();
        assert_eq!(
            engine.replay_send(id, b"req", false, at(0), 0),
            Ok(ReplaySendOutcome::Sent(2))
        );
        let error = engine.replay_send(id, b"q", false, at(0), 0).unwrap_err();
        assert_eq!(
            error,
            NetworkEngineError::BlockingSendAtRefusal {
                channel: id,
                offset: 2
            }
        );
        assert_eq!(error.remedy(), DIVERGED_REMEDY);
        assert_eq!(
            engine.finish_replay(),
            Err(NetworkEngineError::ReplayUnsent {
                channel: id,
                sent: 2,
                recorded: 3,
            })
        );
    }

    #[test]
    fn replay_accepts_a_waited_fragment_only_after_as_many_waits_and_as_long() {
        let mut engine = NetworkEngine::new_record(epoch());
        let id = sock(0);
        engine
            .record_connect(id, peer(), local(), 0, false)
            .unwrap();
        // Accepted on its first attempt: no wait.
        assert_eq!(
            engine.record_send(id, b"ab", None, at(1), 0),
            Ok(mark(2, 1))
        );
        // The send then took its mark, waited three times and had "cd"
        // accepted at time 50; then waited once more, and "ef" went at 60.
        let waiting = engine.record_send(id, b"", None, at(2), 0).unwrap();
        assert_eq!(waiting, mark(2, 1));
        assert_eq!(
            engine.record_send(id, b"cd", Some(waiting), at(50), 3),
            Ok(mark(4, 2))
        );
        assert_eq!(
            engine.record_send(id, b"ef", Some(mark(4, 2)), at(60), 1),
            Ok(mark(6, 3))
        );
        let trace = engine.finish().unwrap();
        let waits: Vec<_> = trace.outputs.iter().map(|output| output.wait).collect();
        let wait = |waits, time| {
            Some(NetworkSendWaitV4 {
                waits,
                not_before_global_time: at(time),
            })
        };
        assert_eq!(waits, vec![None, wait(3, 50), wait(1, 60)]);
        assert!(trace.records_send_waits());

        let mut engine = NetworkEngine::new_replay(trace.clone());
        engine.replay_connect(id, &peer(), false).unwrap();
        let sent = ReplaySendOutcome::Sent(2);
        assert_eq!(engine.replay_send(id, b"abcdef", false, at(1), 0), Ok(sent));
        // Not yet due: no wait, too few waits, or too early.
        for (time, waits) in [(10, 0), (60, 2), (49, 3)] {
            assert_eq!(
                engine.replay_send(id, b"cdef", false, at(time), waits),
                Ok(ReplaySendOutcome::NotYet),
                "time {time}, waits {waits}"
            );
        }
        assert_eq!(engine.replay_send(id, b"cdef", false, at(50), 3), Ok(sent));
        assert_eq!(
            engine.replay_send(id, b"ef", false, at(70), 0),
            Ok(ReplaySendOutcome::NotYet)
        );
        // A nonblocking send never waited in the recording, so one here
        // diverged rather than waiting.
        let error = engine.replay_send(id, b"ef", true, at(70), 0).unwrap_err();
        assert_eq!(
            error,
            NetworkEngineError::NonblockingSendAtWait {
                channel: id,
                offset: 4
            }
        );
        assert_eq!(error.remedy(), DIVERGED_REMEDY);
        assert_eq!(engine.replay_send(id, b"ef", false, at(70), 1), Ok(sent));
        assert_eq!(engine.finish_replay(), Ok(()));

        // The wait gates only the start of the fragment: a shorter guest
        // send that took part of it continues without waiting again.
        let mut engine = NetworkEngine::new_replay(trace);
        engine.replay_connect(id, &peer(), false).unwrap();
        let one = ReplaySendOutcome::Sent(1);
        assert_eq!(engine.replay_send(id, b"ab", false, at(1), 0), Ok(sent));
        assert_eq!(engine.replay_send(id, b"c", false, at(50), 3), Ok(one));
        assert_eq!(engine.replay_send(id, b"d", false, at(50), 0), Ok(one));
    }

    #[test]
    fn apply_stamps_a_waited_send_with_the_global_time_of_its_request() {
        let mut engine = NetworkEngine::new_record(epoch());
        let id = sock(0);
        engine
            .record_connect(id, peer(), local(), 0, false)
            .unwrap();
        let send = |bytes: &[u8], waits| NetworkRequest::RecordSend {
            id,
            bytes: bytes.to_vec(),
            at: None,
            waits,
        };
        engine.apply(at(5), send(b"a", 0)).unwrap();
        engine.apply(at(7), send(b"b", 2)).unwrap();
        let trace = engine.finish().unwrap();
        assert_eq!(trace.outputs[0].wait, None);
        assert_eq!(
            trace.outputs[1].wait,
            Some(NetworkSendWaitV4 {
                waits: 2,
                not_before_global_time: at(7),
            })
        );

        let mut engine = NetworkEngine::new_replay(trace);
        engine.replay_connect(id, &peer(), false).unwrap();
        let replay = |bytes: &[u8], waits| NetworkRequest::ReplaySend {
            id,
            bytes: bytes.to_vec(),
            nonblocking: false,
            waits,
        };
        assert_eq!(
            engine.apply(at(5), replay(b"ab", 0)),
            Ok(NetworkReply::Sent(1))
        );
        assert_eq!(
            engine.apply(at(6), replay(b"b", 2)),
            Ok(NetworkReply::NotYet)
        );
        assert_eq!(
            engine.apply(at(7), replay(b"b", 2)),
            Ok(NetworkReply::Sent(1))
        );

        // Both kinds of request reach the global state through a serializer
        // with their guest bytes intact.
        let every_byte: Vec<u8> = (0..=u8::MAX).collect();
        assert_survives_rpc_encodings(&send(&every_byte, 2), &every_byte);
        assert_survives_rpc_encodings(&replay(&every_byte, 2), &every_byte);
    }

    #[test]
    fn finished_replay_refuses_an_unmade_refused_send() {
        let mut engine = NetworkEngine::new_record(epoch());
        let id = sock(0);
        engine
            .record_connect(id, peer(), local(), 0, false)
            .unwrap();
        engine.record_send(id, b"req", None, at(0), 0).unwrap();
        engine.record_refused_send(id).unwrap();
        let mut engine = NetworkEngine::new_replay(engine.finish().unwrap());
        engine.replay_connect(id, &peer(), false).unwrap();
        assert_eq!(
            engine.replay_send(id, b"req", true, at(0), 0),
            Ok(ReplaySendOutcome::Sent(3))
        );
        assert_eq!(
            engine.finish_replay(),
            Err(NetworkEngineError::ReplayUnrefused {
                channel: id,
                offset: 3
            })
        );
    }

    #[test]
    fn record_refuses_a_send_interleaved_with_a_waiting_send() {
        let mut engine = NetworkEngine::new_record(epoch());
        let id = sock(0);
        engine
            .record_connect(id, peer(), local(), 0, false)
            .unwrap();
        // A blocking send accepted a prefix and waits; its continuation
        // lands where it left the stream.
        assert_eq!(
            engine.record_send(id, b"ab", None, at(0), 0),
            Ok(mark(2, 1))
        );
        assert_eq!(
            engine.record_send(id, b"cd", Some(mark(2, 1)), at(0), 0),
            Ok(mark(4, 2))
        );
        // Another send reached the stream while it waited.
        assert_eq!(
            engine.record_send(id, b"xy", None, at(0), 0),
            Ok(mark(6, 3))
        );
        let error = engine
            .record_send(id, b"ef", Some(mark(4, 2)), at(0), 0)
            .unwrap_err();
        assert_eq!(
            error,
            NetworkEngineError::InterleavedSend {
                channel: id,
                offset: 4
            }
        );
        assert!(error.remedy().contains("one thread at a time"));
        assert_eq!(
            engine.finish().unwrap().outputs.len(),
            3,
            "a refused continuation must not be recorded"
        );
    }

    #[test]
    fn record_refuses_a_send_that_overtook_a_zero_byte_wait() {
        let mut engine = NetworkEngine::new_record(epoch());
        let id = sock(0);
        engine
            .record_connect(id, peer(), local(), 0, false)
            .unwrap();
        assert_eq!(
            engine.record_send(id, b"ab", None, at(0), 0),
            Ok(mark(2, 1))
        );
        // A blocking send found the buffer full before accepting any byte
        // and took its mark before waiting; taking it records nothing.
        let waiting = engine.record_send(id, b"", None, at(0), 0).unwrap();
        assert_eq!(waiting, mark(2, 1));
        // Another thread's send was accepted while it waited. Replay never
        // waits there, so it would give the waiting send these offsets.
        assert_eq!(
            engine.record_send(id, b"xy", None, at(0), 0),
            Ok(mark(4, 2))
        );
        let error = engine
            .record_send(id, b"cd", Some(waiting), at(0), 0)
            .unwrap_err();
        assert_eq!(
            error,
            NetworkEngineError::InterleavedSend {
                channel: id,
                offset: 2
            }
        );
        assert_eq!(engine.finish().unwrap().outputs.len(), 2);
    }

    #[test]
    fn record_refuses_a_refusal_pushed_during_a_wait() {
        let mut engine = NetworkEngine::new_record(epoch());
        let id = sock(0);
        engine
            .record_connect(id, peer(), local(), 0, false)
            .unwrap();
        assert_eq!(
            engine.record_send(id, b"ab", None, at(0), 0),
            Ok(mark(2, 1))
        );
        // Another thread's nonblocking send was refused while this send
        // waited. The refusal does not move the offset, but replay would
        // hand it to the waiting blocking send.
        engine.record_refused_send(id).unwrap();
        let error = engine
            .record_send(id, b"cd", Some(mark(2, 1)), at(0), 0)
            .unwrap_err();
        assert_eq!(
            error,
            NetworkEngineError::InterleavedSend {
                channel: id,
                offset: 2
            }
        );
        assert_eq!(engine.finish().unwrap().outputs.len(), 2);
    }

    #[test]
    fn shutdown_follows_linux() {
        let mut engine = NetworkEngine::new_record(epoch());
        let id = sock(0);
        engine
            .record_connect(id, peer(), local(), 0, false)
            .unwrap();
        assert_eq!(engine.send_failure(id, at(1)), Ok(None));
        assert_eq!(engine.shutdown(id, libc::SHUT_WR), Ok(0));
        assert_eq!(engine.send_failure(id, at(1)), Ok(Some(libc::EPIPE)));
        assert_eq!(engine.shutdown(id, 7), Ok(libc::EINVAL));
        assert_eq!(engine.shutdown(id, libc::SHUT_RD), Ok(0));
        assert_eq!(
            engine.recv(id, at(1), 64, 1, false),
            Ok(NetworkRecvOutcome::Eof)
        );
        assert_eq!(
            engine.readiness(id, at(1), 1).unwrap() & libc::POLLHUP,
            libc::POLLHUP
        );
    }

    #[test]
    fn shut_rd_delivers_queued_bytes_before_end_of_file() {
        let mut engine = NetworkEngine::new_record(epoch());
        let id = sock(0);
        engine
            .record_connect(id, peer(), local(), 0, false)
            .unwrap();
        engine
            .record_arrival(id, at(1), NetworkArrival::Bytes(b"abc".to_vec()))
            .unwrap();
        assert_eq!(engine.shutdown(id, libc::SHUT_RD), Ok(0));
        // A blocking receive whose target exceeds the queue still returns it.
        assert_eq!(engine.recv(id, at(2), 64, 64, true), Ok(data(b"abc")));
        assert_eq!(engine.recv(id, at(2), 2, 64, false), Ok(data(b"ab")));
        assert_eq!(engine.recv(id, at(2), 64, 64, false), Ok(data(b"c")));
        assert_eq!(
            engine.recv(id, at(3), 64, 64, false),
            Ok(NetworkRecvOutcome::Eof)
        );
    }

    #[test]
    fn requests_record_pulls_at_the_time_they_answer() {
        let mut engine = NetworkEngine::new_record(epoch());
        let id = sock(0);
        let reply = engine.apply(
            at(1),
            NetworkRequest::RecordConnect {
                id,
                peer: peer(),
                local: local(),
                errno: 0,
                nonblocking: false,
            },
        );
        assert_eq!(reply, Ok(NetworkReply::Errno(0)));
        let reply = engine.apply(
            at(5),
            NetworkRequest::Readiness(vec![(id, 1, vec![NetworkArrival::Bytes(b"hi".to_vec())])]),
        );
        assert_eq!(
            reply,
            Ok(NetworkReply::Events(vec![
                libc::POLLIN | libc::POLLRDNORM | libc::POLLOUT | libc::POLLWRNORM
            ]))
        );
        // A drained host socket keeps reporting end of stream; only the
        // first report is recorded.
        let reply = engine.apply(
            at(9),
            NetworkRequest::Recv {
                id,
                arrivals: vec![
                    NetworkArrival::PeerWriteClosed,
                    NetworkArrival::PeerWriteClosed,
                ],
                max_len: 64,
                target: 1,
                peek: false,
            },
        );
        assert_eq!(reply, Ok(NetworkReply::Recv(data(b"hi"))));
        assert_eq!(
            engine.apply(at(9), NetworkRequest::Addresses(id)),
            Ok(NetworkReply::Addresses(Some((local(), peer()))))
        );
        let trace = engine.finish().unwrap();
        assert_eq!(trace.inputs.len(), 2);
        assert_eq!(trace.inputs[0].release.not_before_global_time, at(5));
        assert_eq!(trace.inputs[1].release.not_before_global_time, at(9));
    }

    #[test]
    fn modes_are_not_interchangeable() {
        let mut engine = NetworkEngine::new_replay(recorded_exchange());
        assert_eq!(
            engine.record_connect(sock(0), peer(), local(), 0, false),
            Err(NetworkEngineError::WrongMode)
        );
        assert!(matches!(
            engine.finish(),
            Err(NetworkEngineError::WrongMode)
        ));
        let mut engine = NetworkEngine::new_record(epoch());
        assert_eq!(
            engine.replay_connect(sock(0), &peer(), false),
            Err(NetworkEngineError::WrongMode)
        );
        assert_eq!(
            engine.recv(sock(9), at(1), 1, 1, false),
            Err(NetworkEngineError::UnknownChannel(sock(9)))
        );
    }

    /// `n` seconds after `at(0)`.
    fn after_secs(n: u64) -> LogicalTime {
        at(0) + std::time::Duration::from_secs(n)
    }

    #[test]
    fn replay_refuses_an_input_held_back_by_bytes_never_sent() {
        let mut engine = NetworkEngine::new_replay(recorded_exchange());
        let id = sock(0);
        engine.replay_connect(id, &peer(), false).unwrap();
        // Short of the limit past the recorded arrival at 20 us: still waiting.
        let waiting = at(19) + REPLAY_STALL_LIMIT;
        assert_eq!(
            engine.recv(id, waiting, 64, 1, false),
            Ok(NetworkRecvOutcome::WouldBlock)
        );
        let stalled = Err(NetworkEngineError::ReplayStalled {
            channel: id,
            needed: 3,
            sent: 0,
        });
        assert_eq!(engine.recv(id, after_secs(61), 64, 1, false), stalled);
        assert_eq!(engine.readiness(id, after_secs(61), 1), stalled.map(|_| 0));
    }

    #[test]
    fn replay_that_sends_what_gates_an_input_never_stalls() {
        let mut engine = NetworkEngine::new_replay(recorded_exchange());
        let id = sock(0);
        engine.replay_connect(id, &peer(), false).unwrap();
        assert_eq!(
            engine.replay_send(id, b"req", false, at(0), 0),
            Ok(ReplaySendOutcome::Sent(3))
        );
        assert_eq!(
            engine.recv(id, after_secs(3_600), 64, 1, false),
            Ok(data(b"abcdef"))
        );
    }

    #[test]
    fn finished_replay_accepts_a_complete_run_even_with_input_unread() {
        let mut engine = NetworkEngine::new_replay(recorded_exchange());
        let id = sock(0);
        engine.replay_connect(id, &peer(), false).unwrap();
        assert_eq!(
            engine.replay_send(id, b"req", false, at(0), 0),
            Ok(ReplaySendOutcome::Sent(3))
        );
        // The response was never read: release depends on the schedule.
        assert_eq!(engine.finish_replay(), Ok(()));
    }

    #[test]
    fn finished_replay_refuses_unsent_output_and_unmade_connections() {
        let mut engine = NetworkEngine::new_replay(recorded_exchange());
        let id = sock(0);
        engine.replay_connect(id, &peer(), false).unwrap();
        assert_eq!(
            engine.replay_send(id, b"r", false, at(0), 0),
            Ok(ReplaySendOutcome::Sent(1))
        );
        assert_eq!(
            engine.finish_replay(),
            Err(NetworkEngineError::ReplayUnsent {
                channel: id,
                sent: 1,
                recorded: 3,
            })
        );
        let engine = NetworkEngine::new_replay(recorded_exchange());
        assert_eq!(
            engine.finish_replay(),
            Err(NetworkEngineError::ReplayUnconnected(sock(0)))
        );
        let record = NetworkEngine::new_record(epoch());
        assert_eq!(record.finish_replay(), Err(NetworkEngineError::WrongMode));
    }

    #[test]
    fn record_refuses_a_peer_naming_no_single_host() {
        let mut engine = NetworkEngine::new_record(epoch());
        for peer in [
            NetworkAddressV1::Inet4 {
                address: [0, 0, 0, 0],
                port: 8080,
            },
            NetworkAddressV1::Inet4 {
                address: [127, 0, 0, 1],
                port: 0,
            },
        ] {
            assert_eq!(
                engine.record_connect(sock(0), peer.clone(), local(), 0, false),
                Err(NetworkEngineError::UntraceablePeer(peer))
            );
        }
        assert!(engine.finish().unwrap().channels.is_empty());
    }

    #[test]
    fn every_error_names_a_remedy() {
        let divergence = NetworkEngineError::ReplayUnsent {
            channel: sock(0),
            sent: 1,
            recorded: 3,
        };
        assert!(divergence.to_string().contains("sent 1 of the 3 bytes"));
        assert!(divergence.remedy().contains("--record-networking"));
        assert!(
            NetworkEngineError::UnknownChannel(sock(0))
                .remedy()
                .contains("hermit defect")
        );
        assert!(
            NetworkEngineError::UntraceablePeer(peer())
                .remedy()
                .contains("nonzero port")
        );
    }
}
