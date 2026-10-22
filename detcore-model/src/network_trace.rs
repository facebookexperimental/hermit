/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Versioned data model for schedule-independent external network input.
//!
//! This module deliberately contains no recorder, replayer, or scheduler hook.
//! It defines the fail-closed trace envelopes and their framing so the runtime
//! integration cannot accidentally reuse the schedule-coupled syscall event
//! stream. [`NetworkTraceV1`] is the original single-channel envelope;
//! [`NetworkTraceV2`] is the multi-channel form the recorder writes, and its
//! reader upgrades v1 traces. The v3 and v4 framings extend the v2 payload
//! with refused sends and with send waits respectively. Merely constructing a [`NetworkTraceConfig`]
//! does not enable any behavior.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::io;
use std::io::Read;
use std::io::Write;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::path::PathBuf;

use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;

use crate::fd::OpenFileId;
use crate::time::LogicalTime;

/// The on-disk format magic. The version is stored in the following four bytes.
pub const NETWORK_TRACE_MAGIC: [u8; 16] = *b"HERMIT-NET-TRACE";
/// The first network trace format; still accepted and upgraded on read.
pub const NETWORK_TRACE_VERSION_V1: u32 = 1;

const FRAME_HEADER_LEN: usize = NETWORK_TRACE_MAGIC.len() + 4 + 8;

/// Whether the future runtime integration records or replays external input.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkTraceMode {
    /// No network trace behavior. This is the default and has no runtime effect.
    #[default]
    Off,
    /// Capture supported external TCP input in a new trace.
    Record,
    /// Replay a trace without consulting the host network.
    Replay,
}

/// Configuration reserved for the future network recorder/replayer seam.
///
/// `network_perturb_seed` is intentionally optional and has no fallback to the
/// scheduler or global seed. A future perturbation layer must consume only this
/// seed so varying `sched_seed` cannot change the external input stream.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkTraceConfig {
    pub mode: NetworkTraceMode,
    pub path: Option<PathBuf>,
    pub network_perturb_seed: Option<u64>,
}

impl NetworkTraceConfig {
    /// Validate configuration independently of any scheduler configuration.
    pub fn validate(&self) -> Result<(), NetworkTraceConfigError> {
        match self.mode {
            NetworkTraceMode::Off => {
                if self.path.is_some() || self.network_perturb_seed.is_some() {
                    return Err(NetworkTraceConfigError::OptionsWhileOff);
                }
            }
            NetworkTraceMode::Record => {
                validate_trace_path(self.path.as_ref())?;
                if self.network_perturb_seed.is_some() {
                    return Err(NetworkTraceConfigError::PerturbationDuringRecord);
                }
            }
            NetworkTraceMode::Replay => validate_trace_path(self.path.as_ref())?,
        }
        Ok(())
    }
}

fn validate_trace_path(path: Option<&PathBuf>) -> Result<(), NetworkTraceConfigError> {
    let Some(path) = path else {
        return Err(NetworkTraceConfigError::MissingPath);
    };
    if path.as_os_str().is_empty() {
        return Err(NetworkTraceConfigError::EmptyPath);
    }
    Ok(())
}

/// Invalid combinations in [`NetworkTraceConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkTraceConfigError {
    OptionsWhileOff,
    MissingPath,
    EmptyPath,
    PerturbationDuringRecord,
}

impl fmt::Display for NetworkTraceConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OptionsWhileOff => {
                write!(f, "network trace options require record or replay mode")
            }
            Self::MissingPath => write!(f, "network trace record/replay requires a path"),
            Self::EmptyPath => write!(f, "network trace path must not be empty"),
            Self::PerturbationDuringRecord => {
                write!(f, "network perturbation is replay-only")
            }
        }
    }
}

impl Error for NetworkTraceConfigError {}

/// Internet address recorded without host-layout `sockaddr` padding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum NetworkAddressV1 {
    Inet4 {
        address: [u8; 4],
        port: u16,
    },
    Inet6 {
        address: [u8; 16],
        port: u16,
        flowinfo: u32,
        scope_id: u32,
    },
}

impl NetworkAddressV1 {
    fn is_supported_external_peer(&self) -> bool {
        match self {
            Self::Inet4 { address, port } => {
                *port != 0 && is_supported_external_ipv4(Ipv4Addr::from(*address))
            }
            Self::Inet6 { address, port, .. } => {
                let address = Ipv6Addr::from(*address);
                *port != 0
                    && match address.to_ipv4_mapped() {
                        Some(mapped) => is_supported_external_ipv4(mapped),
                        None => {
                            !address.is_loopback()
                                && !address.is_unspecified()
                                && !address.is_multicast()
                        }
                    }
            }
        }
    }

    /// The port in host byte order.
    pub fn port(&self) -> u16 {
        match self {
            Self::Inet4 { port, .. } | Self::Inet6 { port, .. } => *port,
        }
    }

    /// An address no connection can have as its peer.
    /// Whether a v2 trace can hold a connection to this peer: a nonzero port
    /// and an address that names one host. The unspecified address, which
    /// Linux connects to loopback, is excluded.
    pub fn is_traceable_peer(&self) -> bool {
        self.port() != 0 && !self.is_unroutable()
    }

    fn is_unroutable(&self) -> bool {
        match self {
            Self::Inet4 { address, .. } => {
                let address = Ipv4Addr::from(*address);
                address.is_unspecified() || address.is_multicast() || address.is_broadcast()
            }
            Self::Inet6 { address, .. } => {
                let address = Ipv6Addr::from(*address);
                // Linux connects an IPv4-mapped address over IPv4.
                if let Some(mapped) = address.to_ipv4_mapped() {
                    return mapped.is_unspecified()
                        || mapped.is_multicast()
                        || mapped.is_broadcast();
                }
                address.is_unspecified() || address.is_multicast()
            }
        }
    }

    fn same_family(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (Self::Inet4 { .. }, Self::Inet4 { .. }) | (Self::Inet6 { .. }, Self::Inet6 { .. })
        )
    }
}

fn is_supported_external_ipv4(address: Ipv4Addr) -> bool {
    !address.is_loopback()
        && !address.is_unspecified()
        && !address.is_multicast()
        && !address.is_broadcast()
}

/// Transport admitted by the v1 trace envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkTransportV1 {
    Tcp,
}

/// Connection role admitted by v1. Server-side `accept` needs a different
/// identity and arrival model and is intentionally not representable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkEndpointRoleV1 {
    OutboundClient,
}

/// The only channel supported by v1: one outbound TCP client connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkChannelV1 {
    /// Stable Detcore open-file-description identity, never a raw fd.
    pub id: OpenFileId,
    pub transport: NetworkTransportV1,
    pub role: NetworkEndpointRoleV1,
    pub local_address: NetworkAddressV1,
    pub peer_address: NetworkAddressV1,
    /// The recorder must prove socket allocation occurred before competing
    /// guest threads could make this identity schedule-dependent.
    pub created_before_competing_threads: bool,
}

/// Conditions that must both hold before an input becomes observable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkReleaseV1 {
    /// Absolute time in exactly the domain returned by `GlobalTime::as_nanos`.
    /// It includes the trace epoch; it is not a duration since that epoch.
    pub not_before_global_time: LogicalTime,
    /// Number of validated outbound bytes required before release.
    pub after_transmitted_offset: u64,
}

impl NetworkReleaseV1 {
    /// Whether both scheduler-owned eligibility conditions have been met.
    pub fn is_eligible(
        &self,
        committed_global_time: LogicalTime,
        validated_transmitted_offset: u64,
    ) -> bool {
        committed_global_time >= self.not_before_global_time
            && validated_transmitted_offset >= self.after_transmitted_offset
    }
}

/// Supported external observations for the initial TCP byte-stream model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum NetworkInputKindV1 {
    InboundBytes {
        stream_offset: u64,
        bytes: Vec<u8>,
    },
    PeerWriteClosed {
        stream_offset: u64,
    },
    /// A terminal stream error, not a retryable or interrupted receive attempt.
    SocketError {
        stream_offset: u64,
        errno: i32,
    },
}

/// One globally ordered external observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkInputEventV1 {
    pub ordinal: u64,
    pub channel: OpenFileId,
    pub release: NetworkReleaseV1,
    pub event: NetworkInputKindV1,
}

/// One contiguous recorded fragment of the expected outbound TCP byte stream.
///
/// From v3 on, a fragment with no bytes records a nonblocking send that
/// Linux refused with `EAGAIN` when the stream had reached `stream_offset`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkOutputV1 {
    pub channel: OpenFileId,
    pub stream_offset: u64,
    pub bytes: Vec<u8>,
}

/// How long a blocking send waited for buffer space before Linux accepted
/// one fragment of it.
///
/// Replay hands the fragment to the guest only after the same send has
/// waited `waits` times and global time has reached `not_before_global_time`,
/// so the send returns at the same point of the guest's execution as it did
/// in the recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkSendWaitV4 {
    /// Waits for buffer space since the send started or since its previous
    /// accepted fragment, whichever came later; never zero.
    pub waits: u64,
    /// The global time at which the recording accepted the fragment.
    pub not_before_global_time: LogicalTime,
}

/// One recorded outbound fragment, as a v4 trace holds it: a v1 fragment
/// and, when a blocking send waited before Linux accepted it, that wait.
/// Fragments of earlier versions have no wait.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkOutputV4 {
    pub channel: OpenFileId,
    pub stream_offset: u64,
    pub bytes: Vec<u8>,
    pub wait: Option<NetworkSendWaitV4>,
}

impl From<NetworkOutputV1> for NetworkOutputV4 {
    fn from(output: NetworkOutputV1) -> Self {
        Self {
            channel: output.channel,
            stream_offset: output.stream_offset,
            bytes: output.bytes,
            wait: None,
        }
    }
}

/// Version-one schedule-independent network input trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkTraceV1 {
    pub epoch: DateTime<Utc>,
    pub channels: Vec<NetworkChannelV1>,
    pub inputs: Vec<NetworkInputEventV1>,
    pub outputs: Vec<NetworkOutputV1>,
}

impl NetworkTraceV1 {
    /// Convert the recorded epoch to the absolute starting time used by
    /// `GlobalTime`, rejecting Chrono values outside its `u64` nanosecond
    /// domain. `GlobalTime` truncates the epoch to microseconds, so this does
    /// the same conversion without an unchecked signed cast or multiplication.
    pub fn epoch_global_time(&self) -> Result<LogicalTime, NetworkTraceValidationError> {
        epoch_global_time(&self.epoch)
    }

    /// Enforce the narrow v1 envelope before either writing or consuming a trace.
    pub fn validate(&self) -> Result<(), NetworkTraceValidationError> {
        if self.channels.len() != 1 {
            return Err(NetworkTraceValidationError::ChannelCount(
                self.channels.len(),
            ));
        }
        let channel = &self.channels[0];
        if !channel.id.is_socket() {
            return Err(NetworkTraceValidationError::NonSocketChannelIdentity);
        }
        if !channel.created_before_competing_threads {
            return Err(NetworkTraceValidationError::ScheduleDependentChannelIdentity);
        }
        if !channel.local_address.same_family(&channel.peer_address) {
            return Err(NetworkTraceValidationError::AddressFamilyMismatch);
        }
        if !channel.peer_address.is_supported_external_peer() {
            return Err(NetworkTraceValidationError::UnsupportedPeerAddress);
        }

        let mut inbound_offset = 0u64;
        let epoch_start = self.epoch_global_time()?;
        let mut previous_time = epoch_start;
        let mut previous_tx_watermark = 0u64;
        let mut terminal = false;

        let total_output = validate_outputs(&self.outputs, channel.id)?;
        for (index, input) in self.inputs.iter().enumerate() {
            if input.ordinal != index as u64 {
                return Err(NetworkTraceValidationError::NonCanonicalOrdinal);
            }
            if input.channel != channel.id {
                return Err(NetworkTraceValidationError::UnknownChannel);
            }
            if input.release.not_before_global_time < epoch_start {
                return Err(NetworkTraceValidationError::ReleaseBeforeEpoch);
            }
            if input.release.not_before_global_time < previous_time
                || input.release.after_transmitted_offset < previous_tx_watermark
            {
                return Err(NetworkTraceValidationError::NonMonotonicRelease);
            }
            if input.release.after_transmitted_offset > total_output {
                return Err(NetworkTraceValidationError::UnreachableTransmitWatermark);
            }
            if terminal {
                return Err(NetworkTraceValidationError::EventAfterTerminal);
            }

            match &input.event {
                NetworkInputKindV1::InboundBytes {
                    stream_offset,
                    bytes,
                } => {
                    if bytes.is_empty() {
                        return Err(NetworkTraceValidationError::EmptyByteChunk);
                    }
                    if *stream_offset != inbound_offset {
                        return Err(NetworkTraceValidationError::NonContiguousInput);
                    }
                    inbound_offset = inbound_offset
                        .checked_add(bytes.len() as u64)
                        .ok_or(NetworkTraceValidationError::StreamOffsetOverflow)?;
                }
                NetworkInputKindV1::PeerWriteClosed { stream_offset } => {
                    if *stream_offset != inbound_offset {
                        return Err(NetworkTraceValidationError::NonContiguousInput);
                    }
                    terminal = true;
                }
                NetworkInputKindV1::SocketError {
                    stream_offset,
                    errno,
                } => {
                    if *stream_offset != inbound_offset {
                        return Err(NetworkTraceValidationError::NonContiguousInput);
                    }
                    if !(1..=4095).contains(errno) {
                        return Err(NetworkTraceValidationError::InvalidErrno);
                    }
                    // EWOULDBLOCK equals EAGAIN on the supported Linux hosts.
                    // Neither readiness nor signal interruption terminates the
                    // peer's byte stream, so neither belongs in this variant.
                    if matches!(*errno, libc::EAGAIN | libc::EINTR) {
                        return Err(NetworkTraceValidationError::NonTerminalSocketError);
                    }
                    terminal = true;
                }
            }
            previous_time = input.release.not_before_global_time;
            previous_tx_watermark = input.release.after_transmitted_offset;
        }
        Ok(())
    }

    /// Write one complete, length-delimited v1 trace.
    pub fn write_framed<W: Write>(&self, writer: W) -> Result<(), NetworkTraceCodecError> {
        self.validate()?;
        write_frame(writer, NETWORK_TRACE_VERSION_V1, self)
    }

    /// Read exactly one complete trace, rejecting truncation, trailing bytes,
    /// unknown versions, malformed payloads, and invalid v1 semantics.
    pub fn read_framed<R: Read>(reader: R) -> Result<Self, NetworkTraceCodecError> {
        let (version, payload) = read_frame(reader)?;
        if version != NETWORK_TRACE_VERSION_V1 {
            return Err(NetworkTraceCodecError::UnsupportedVersion(version));
        }
        let trace: Self = decode_payload(&payload)?;
        trace.validate()?;
        Ok(trace)
    }
}

/// Read one frame header and its payload, rejecting truncation and trailing bytes.
fn read_frame<R: Read>(mut reader: R) -> Result<(u32, Vec<u8>), NetworkTraceCodecError> {
    let mut header = [0u8; FRAME_HEADER_LEN];
    read_exact_or_truncated(&mut reader, &mut header)?;
    if header[..NETWORK_TRACE_MAGIC.len()] != NETWORK_TRACE_MAGIC {
        return Err(NetworkTraceCodecError::BadMagic);
    }
    let version_start = NETWORK_TRACE_MAGIC.len();
    let version = u32::from_le_bytes(
        header[version_start..version_start + 4]
            .try_into()
            .expect("fixed-size version field"),
    );
    if ![
        NETWORK_TRACE_VERSION_V1,
        NETWORK_TRACE_VERSION_V2,
        NETWORK_TRACE_VERSION_V3,
        NETWORK_TRACE_VERSION_V4,
    ]
    .contains(&version)
    {
        return Err(NetworkTraceCodecError::UnsupportedVersion(version));
    }
    let len_start = version_start + 4;
    let payload_len = u64::from_le_bytes(
        header[len_start..len_start + 8]
            .try_into()
            .expect("fixed-size length field"),
    );
    // Grow the payload only as its bytes arrive, so a corrupt or hostile
    // length header cannot make the reader allocate more than the input holds.
    let mut payload = Vec::new();
    let read = reader
        .by_ref()
        .take(payload_len)
        .read_to_end(&mut payload)?;
    if read as u64 != payload_len {
        return Err(NetworkTraceCodecError::Truncated);
    }
    let mut trailing = [0u8; 1];
    if reader.read(&mut trailing)? != 0 {
        return Err(NetworkTraceCodecError::TrailingData);
    }
    Ok((version, payload))
}

fn decode_payload<T: serde::de::DeserializeOwned>(
    payload: &[u8],
) -> Result<T, NetworkTraceCodecError> {
    let (value, consumed): (T, usize) =
        bincode::serde::decode_from_slice(payload, bincode::config::standard())
            .map_err(NetworkTraceCodecError::Decode)?;
    if consumed != payload.len() {
        return Err(NetworkTraceCodecError::TrailingPayloadData);
    }
    Ok(value)
}

/// Encode one complete frame: the header, then the payload, in one buffer.
fn encode_frame<T: Serialize>(version: u32, value: &T) -> Result<Vec<u8>, NetworkTraceCodecError> {
    let mut frame = Vec::with_capacity(FRAME_HEADER_LEN);
    frame.extend_from_slice(&NETWORK_TRACE_MAGIC);
    frame.extend_from_slice(&version.to_le_bytes());
    frame.extend_from_slice(&[0; 8]);
    bincode::serde::encode_into_std_write(value, &mut frame, bincode::config::standard())
        .map_err(NetworkTraceCodecError::Encode)?;
    let payload_len = (frame.len() - FRAME_HEADER_LEN) as u64;
    frame[FRAME_HEADER_LEN - 8..FRAME_HEADER_LEN].copy_from_slice(&payload_len.to_le_bytes());
    Ok(frame)
}

fn write_frame<W: Write, T: Serialize>(
    mut writer: W,
    version: u32,
    value: &T,
) -> Result<(), NetworkTraceCodecError> {
    writer.write_all(&encode_frame(version, value)?)?;
    Ok(())
}

/// The trace format written by the runtime recorder when no nonblocking send
/// was refused.
pub const NETWORK_TRACE_VERSION_V2: u32 = 2;
/// The v2 format, extended with empty outbound fragments that record a
/// nonblocking send refused with `EAGAIN`. The recorder writes it only for a
/// trace that holds one, so a reader that predates it rejects only those.
pub const NETWORK_TRACE_VERSION_V3: u32 = 3;
/// The v3 format, extended with the wait of each outbound fragment that a
/// blocking send waited for. The recorder writes it only for a trace that
/// holds one.
pub const NETWORK_TRACE_VERSION_V4: u32 = 4;

/// One outbound TCP client connection in a v2 trace.
///
/// Unlike v1, a v2 trace may hold several channels, may name a loopback peer
/// (the recording ran with explicit host networking, so a host-loopback
/// server is as external as any other), and records the connect outcome so a
/// replay can reproduce a refused connection without touching the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkChannelV2 {
    /// Stable Detcore open-file-description identity, never a raw fd.
    pub id: OpenFileId,
    pub transport: NetworkTransportV1,
    pub role: NetworkEndpointRoleV1,
    /// The local address the host assigned, or `None` when connect failed.
    pub local_address: Option<NetworkAddressV1>,
    pub peer_address: NetworkAddressV1,
    /// `0` when the connection was established, otherwise the positive errno
    /// the guest observed for it.
    pub connect_errno: i32,
}

/// Version-two schedule-independent network trace: v1's release model over
/// several channels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkTraceV2 {
    pub epoch: DateTime<Utc>,
    pub channels: Vec<NetworkChannelV2>,
    /// Inputs of every channel, in the global order they were observed.
    pub inputs: Vec<NetworkInputEventV1>,
    /// Outbound fragments of every channel, in the order they were sent.
    pub outputs: Vec<NetworkOutputV4>,
}

/// The v2 and v3 payload: [`NetworkTraceV2`] before fragments could carry a
/// wait. Decoding through it is what keeps a wait out of a v2 or v3 trace.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NetworkTraceV2Wire {
    epoch: DateTime<Utc>,
    channels: Vec<NetworkChannelV2>,
    inputs: Vec<NetworkInputEventV1>,
    outputs: Vec<NetworkOutputV1>,
}

impl From<NetworkTraceV2Wire> for NetworkTraceV2 {
    fn from(trace: NetworkTraceV2Wire) -> Self {
        Self {
            epoch: trace.epoch,
            channels: trace.channels,
            inputs: trace.inputs,
            outputs: trace.outputs.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<NetworkTraceV1> for NetworkTraceV2 {
    fn from(trace: NetworkTraceV1) -> Self {
        Self {
            epoch: trace.epoch,
            channels: trace
                .channels
                .into_iter()
                .map(|channel| NetworkChannelV2 {
                    id: channel.id,
                    transport: channel.transport,
                    role: channel.role,
                    local_address: Some(channel.local_address),
                    peer_address: channel.peer_address,
                    connect_errno: 0,
                })
                .collect(),
            inputs: trace.inputs,
            outputs: trace.outputs.into_iter().map(Into::into).collect(),
        }
    }
}

impl NetworkTraceV2 {
    /// Enforce the v2 envelope before either writing or consuming a trace.
    pub fn validate(&self) -> Result<(), NetworkTraceValidationError> {
        let epoch_start = epoch_global_time(&self.epoch)?;
        let mut channels = BTreeMap::new();
        for channel in &self.channels {
            if !channel.id.is_socket() {
                return Err(NetworkTraceValidationError::NonSocketChannelIdentity);
            }
            if !channel.peer_address.is_traceable_peer() {
                return Err(NetworkTraceValidationError::UnsupportedPeerAddress);
            }
            if let Some(local) = &channel.local_address
                && !local.same_family(&channel.peer_address)
            {
                return Err(NetworkTraceValidationError::AddressFamilyMismatch);
            }
            if !(0..=4095).contains(&channel.connect_errno)
                || (channel.connect_errno == 0) != channel.local_address.is_some()
            {
                return Err(NetworkTraceValidationError::InvalidErrno);
            }
            if channels.insert(channel.id, channel).is_some() {
                return Err(NetworkTraceValidationError::DuplicateChannel);
            }
        }

        let mut total_output: BTreeMap<OpenFileId, u64> = BTreeMap::new();
        let mut last_wait: BTreeMap<OpenFileId, LogicalTime> = BTreeMap::new();
        for output in &self.outputs {
            let channel = channels
                .get(&output.channel)
                .ok_or(NetworkTraceValidationError::UnknownChannel)?;
            if channel.connect_errno != 0 {
                return Err(NetworkTraceValidationError::TrafficOnFailedChannel);
            }
            let offset = total_output.entry(output.channel).or_default();
            if output.stream_offset != *offset {
                return Err(NetworkTraceValidationError::NonContiguousOutput);
            }
            *offset = offset
                .checked_add(output.bytes.len() as u64)
                .ok_or(NetworkTraceValidationError::StreamOffsetOverflow)?;
            if let Some(wait) = &output.wait {
                // A refused send returned without waiting, so it has no wait.
                if output.bytes.is_empty() {
                    return Err(NetworkTraceValidationError::WaitOnRefusedSend);
                }
                if wait.waits == 0 {
                    return Err(NetworkTraceValidationError::ZeroSendWaits);
                }
                if wait.not_before_global_time < epoch_start {
                    return Err(NetworkTraceValidationError::SendWaitBeforeEpoch);
                }
                let previous = last_wait.entry(output.channel).or_insert(epoch_start);
                if wait.not_before_global_time < *previous {
                    return Err(NetworkTraceValidationError::NonMonotonicSendWait);
                }
                *previous = wait.not_before_global_time;
            }
        }

        #[derive(Default)]
        struct InboundCursor {
            offset: u64,
            tx_watermark: u64,
            terminal: bool,
        }
        let mut cursors: BTreeMap<OpenFileId, InboundCursor> = BTreeMap::new();
        let mut previous_time = epoch_start;
        for (index, input) in self.inputs.iter().enumerate() {
            if input.ordinal != index as u64 {
                return Err(NetworkTraceValidationError::NonCanonicalOrdinal);
            }
            let channel = channels
                .get(&input.channel)
                .ok_or(NetworkTraceValidationError::UnknownChannel)?;
            if channel.connect_errno != 0 {
                return Err(NetworkTraceValidationError::TrafficOnFailedChannel);
            }
            if input.release.not_before_global_time < epoch_start {
                return Err(NetworkTraceValidationError::ReleaseBeforeEpoch);
            }
            let cursor = cursors.entry(input.channel).or_default();
            if input.release.not_before_global_time < previous_time
                || input.release.after_transmitted_offset < cursor.tx_watermark
            {
                return Err(NetworkTraceValidationError::NonMonotonicRelease);
            }
            if input.release.after_transmitted_offset
                > total_output.get(&input.channel).copied().unwrap_or(0)
            {
                return Err(NetworkTraceValidationError::UnreachableTransmitWatermark);
            }
            if cursor.terminal {
                return Err(NetworkTraceValidationError::EventAfterTerminal);
            }
            cursor.offset = validate_input_kind(&input.event, cursor.offset)?;
            cursor.terminal = !matches!(input.event, NetworkInputKindV1::InboundBytes { .. });
            cursor.tx_watermark = input.release.after_transmitted_offset;
            previous_time = input.release.not_before_global_time;
        }
        Ok(())
    }

    /// Whether the trace records a nonblocking send refused with `EAGAIN`,
    /// which only v3 and v4 can hold.
    pub fn records_refused_sends(&self) -> bool {
        self.outputs.iter().any(|output| output.bytes.is_empty())
    }

    /// Whether any outbound fragment records a wait, which only v4 can hold.
    pub fn records_send_waits(&self) -> bool {
        self.outputs.iter().any(|output| output.wait.is_some())
    }

    /// Write one complete, length-delimited trace: v4 when it records a send
    /// wait, otherwise v3 when it records a refused send, otherwise v2.
    pub fn write_framed<W: Write>(&self, mut writer: W) -> Result<(), NetworkTraceCodecError> {
        writer.write_all(&self.encode_framed()?)?;
        Ok(())
    }

    /// The bytes [`Self::write_framed`] writes, encoded without touching any
    /// output, so a caller can refuse an invalid trace before it truncates a file.
    pub fn encode_framed(&self) -> Result<Vec<u8>, NetworkTraceCodecError> {
        self.validate()?;
        if self.records_send_waits() {
            return encode_frame(NETWORK_TRACE_VERSION_V4, self);
        }
        let version = if self.records_refused_sends() {
            NETWORK_TRACE_VERSION_V3
        } else {
            NETWORK_TRACE_VERSION_V2
        };
        let wire = NetworkTraceV2Wire {
            epoch: self.epoch,
            channels: self.channels.clone(),
            inputs: self.inputs.clone(),
            outputs: self
                .outputs
                .iter()
                .map(|output| NetworkOutputV1 {
                    channel: output.channel,
                    stream_offset: output.stream_offset,
                    bytes: output.bytes.clone(),
                })
                .collect(),
        };
        encode_frame(version, &wire)
    }

    /// Read exactly one complete v1, v2, v3 or v4 trace. A v1 trace is
    /// upgraded to the equivalent v2 trace after passing v1 validation; a v2
    /// trace may not hold the refused sends that v3 added, and only a v4
    /// trace can hold a send wait.
    pub fn read_framed<R: Read>(reader: R) -> Result<Self, NetworkTraceCodecError> {
        let (version, payload) = read_frame(reader)?;
        let trace = match version {
            NETWORK_TRACE_VERSION_V1 => {
                let trace: NetworkTraceV1 = decode_payload(&payload)?;
                trace.validate()?;
                Self::from(trace)
            }
            NETWORK_TRACE_VERSION_V4 => decode_payload(&payload)?,
            _ => Self::from(decode_payload::<NetworkTraceV2Wire>(&payload)?),
        };
        if version < NETWORK_TRACE_VERSION_V3 && trace.records_refused_sends() {
            return Err(NetworkTraceValidationError::EmptyByteChunk.into());
        }
        trace.validate()?;
        Ok(trace)
    }
}

/// Check one input against its channel's inbound cursor and return the next offset.
fn validate_input_kind(
    event: &NetworkInputKindV1,
    offset: u64,
) -> Result<u64, NetworkTraceValidationError> {
    match event {
        NetworkInputKindV1::InboundBytes {
            stream_offset,
            bytes,
        } => {
            if bytes.is_empty() {
                return Err(NetworkTraceValidationError::EmptyByteChunk);
            }
            if *stream_offset != offset {
                return Err(NetworkTraceValidationError::NonContiguousInput);
            }
            offset
                .checked_add(bytes.len() as u64)
                .ok_or(NetworkTraceValidationError::StreamOffsetOverflow)
        }
        NetworkInputKindV1::PeerWriteClosed { stream_offset } => {
            if *stream_offset != offset {
                return Err(NetworkTraceValidationError::NonContiguousInput);
            }
            Ok(offset)
        }
        NetworkInputKindV1::SocketError {
            stream_offset,
            errno,
        } => {
            if *stream_offset != offset {
                return Err(NetworkTraceValidationError::NonContiguousInput);
            }
            if !(1..=4095).contains(errno) {
                return Err(NetworkTraceValidationError::InvalidErrno);
            }
            if matches!(*errno, libc::EAGAIN | libc::EINTR) {
                return Err(NetworkTraceValidationError::NonTerminalSocketError);
            }
            Ok(offset)
        }
    }
}

/// The absolute `GlobalTime` starting point for `epoch`; see
/// [`NetworkTraceV1::epoch_global_time`].
pub fn epoch_global_time(
    epoch: &DateTime<Utc>,
) -> Result<LogicalTime, NetworkTraceValidationError> {
    let seconds = u64::try_from(epoch.timestamp())
        .map_err(|_| NetworkTraceValidationError::EpochOutOfRange)?;
    let whole_seconds = seconds
        .checked_mul(1_000_000_000)
        .ok_or(NetworkTraceValidationError::EpochOutOfRange)?;
    let fractional_micros = u64::from(epoch.timestamp_subsec_micros())
        .checked_mul(1_000)
        .ok_or(NetworkTraceValidationError::EpochOutOfRange)?;
    whole_seconds
        .checked_add(fractional_micros)
        .map(LogicalTime::from_nanos)
        .ok_or(NetworkTraceValidationError::EpochOutOfRange)
}

fn validate_outputs(
    outputs: &[NetworkOutputV1],
    channel: OpenFileId,
) -> Result<u64, NetworkTraceValidationError> {
    let mut expected_offset = 0u64;
    for output in outputs {
        if output.channel != channel {
            return Err(NetworkTraceValidationError::UnknownChannel);
        }
        if output.bytes.is_empty() {
            return Err(NetworkTraceValidationError::EmptyByteChunk);
        }
        if output.stream_offset != expected_offset {
            return Err(NetworkTraceValidationError::NonContiguousOutput);
        }
        expected_offset = expected_offset
            .checked_add(output.bytes.len() as u64)
            .ok_or(NetworkTraceValidationError::StreamOffsetOverflow)?;
    }
    Ok(expected_offset)
}

fn read_exact_or_truncated<R: Read>(
    reader: &mut R,
    bytes: &mut [u8],
) -> Result<(), NetworkTraceCodecError> {
    reader.read_exact(bytes).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            NetworkTraceCodecError::Truncated
        } else {
            NetworkTraceCodecError::Io(error)
        }
    })
}

/// Semantically invalid trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkTraceValidationError {
    ChannelCount(usize),
    EpochOutOfRange,
    NonSocketChannelIdentity,
    ScheduleDependentChannelIdentity,
    AddressFamilyMismatch,
    UnsupportedPeerAddress,
    UnknownChannel,
    NonCanonicalOrdinal,
    ReleaseBeforeEpoch,
    NonMonotonicRelease,
    UnreachableTransmitWatermark,
    EmptyByteChunk,
    NonContiguousInput,
    NonContiguousOutput,
    StreamOffsetOverflow,
    InvalidErrno,
    NonTerminalSocketError,
    EventAfterTerminal,
    DuplicateChannel,
    TrafficOnFailedChannel,
    WaitOnRefusedSend,
    ZeroSendWaits,
    SendWaitBeforeEpoch,
    NonMonotonicSendWait,
}

impl fmt::Display for NetworkTraceValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid network trace: {self:?}")
    }
}

impl Error for NetworkTraceValidationError {}

/// Failure to decode or encode the framed v1 representation.
#[derive(Debug)]
pub enum NetworkTraceCodecError {
    Io(io::Error),
    Truncated,
    BadMagic,
    UnsupportedVersion(u32),
    TrailingData,
    TrailingPayloadData,
    Encode(bincode::error::EncodeError),
    Decode(bincode::error::DecodeError),
    Validation(NetworkTraceValidationError),
}

impl fmt::Display for NetworkTraceCodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "network trace codec error: {self:?}")
    }
}

impl Error for NetworkTraceCodecError {}

impl From<io::Error> for NetworkTraceCodecError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<NetworkTraceValidationError> for NetworkTraceCodecError {
    fn from(error: NetworkTraceValidationError) -> Self {
        Self::Validation(error)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::net::Ipv4Addr;
    use std::time::Duration;

    use chrono::TimeZone;

    use super::*;
    use crate::config::Config;
    use crate::pid::DetTid;
    use crate::time::GlobalTime;

    fn channel_id() -> OpenFileId {
        OpenFileId::new_socket(DetTid::from_raw(1), 0)
    }

    fn trace_epoch() -> DateTime<Utc> {
        Utc.timestamp_opt(1_790_000_000, 0).unwrap()
    }

    fn global_time_after_epoch(nanos: u64) -> LogicalTime {
        let config = Config {
            epoch: trace_epoch(),
            ..Config::default()
        };
        GlobalTime::new(&config).as_nanos() + LogicalTime::from_nanos(nanos)
    }

    fn valid_trace() -> NetworkTraceV1 {
        let channel = channel_id();
        NetworkTraceV1 {
            epoch: trace_epoch(),
            channels: vec![NetworkChannelV1 {
                id: channel,
                transport: NetworkTransportV1::Tcp,
                role: NetworkEndpointRoleV1::OutboundClient,
                local_address: NetworkAddressV1::Inet4 {
                    address: [10, 0, 0, 2],
                    port: 40_000,
                },
                peer_address: NetworkAddressV1::Inet4 {
                    address: [192, 0, 2, 10],
                    port: 443,
                },
                created_before_competing_threads: true,
            }],
            inputs: vec![
                NetworkInputEventV1 {
                    ordinal: 0,
                    channel,
                    release: NetworkReleaseV1 {
                        not_before_global_time: global_time_after_epoch(10),
                        after_transmitted_offset: 3,
                    },
                    event: NetworkInputKindV1::InboundBytes {
                        stream_offset: 0,
                        bytes: b"response".to_vec(),
                    },
                },
                NetworkInputEventV1 {
                    ordinal: 1,
                    channel,
                    release: NetworkReleaseV1 {
                        not_before_global_time: global_time_after_epoch(20),
                        after_transmitted_offset: 3,
                    },
                    event: NetworkInputKindV1::PeerWriteClosed { stream_offset: 8 },
                },
            ],
            outputs: vec![NetworkOutputV1 {
                channel,
                stream_offset: 0,
                bytes: b"req".to_vec(),
            }],
        }
    }

    fn framed(trace: &NetworkTraceV1) -> Vec<u8> {
        let mut bytes = Vec::new();
        trace.write_framed(&mut bytes).unwrap();
        bytes
    }

    fn framed_without_validation(trace: &NetworkTraceV1) -> Vec<u8> {
        let payload = bincode::serde::encode_to_vec(trace, bincode::config::standard()).unwrap();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&NETWORK_TRACE_MAGIC);
        bytes.extend_from_slice(&NETWORK_TRACE_VERSION_V1.to_le_bytes());
        bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&payload);
        bytes
    }

    #[test]
    fn traceable_peer_names_one_host_including_ipv4_mapped_addresses() {
        let inet6 = |address: Ipv6Addr| NetworkAddressV1::Inet6 {
            port: 80,
            flowinfo: 0,
            address: address.octets(),
            scope_id: 0,
        };
        let mapped = |v4: Ipv4Addr| inet6(v4.to_ipv6_mapped());
        assert!(mapped(Ipv4Addr::LOCALHOST).is_traceable_peer());
        assert!(inet6(Ipv6Addr::LOCALHOST).is_traceable_peer());
        for v4 in [
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::BROADCAST,
            Ipv4Addr::new(224, 0, 0, 1),
        ] {
            assert!(!mapped(v4).is_traceable_peer(), "{v4} names no single host");
        }
        assert!(!inet6(Ipv6Addr::UNSPECIFIED).is_traceable_peer());
    }

    #[test]
    fn network_trace_v1_round_trips_exactly() {
        let trace = valid_trace();
        let decoded = NetworkTraceV1::read_framed(Cursor::new(framed(&trace))).unwrap();
        assert_eq!(decoded, trace);
    }

    #[test]
    fn transient_socket_errors_are_rejected_by_validation_writer_and_reader() {
        assert_eq!(libc::EWOULDBLOCK, libc::EAGAIN);
        for errno in [libc::EAGAIN, libc::EINTR] {
            let mut trace = valid_trace();
            trace.inputs[1].event = NetworkInputKindV1::SocketError {
                stream_offset: 8,
                errno,
            };
            assert_eq!(
                trace.validate(),
                Err(NetworkTraceValidationError::NonTerminalSocketError),
                "transient errno {errno} cannot terminate the stream"
            );

            let mut output = b"existing output".to_vec();
            assert!(matches!(
                trace.write_framed(&mut output),
                Err(NetworkTraceCodecError::Validation(
                    NetworkTraceValidationError::NonTerminalSocketError
                ))
            ));
            assert_eq!(output, b"existing output");
            // Bypass the writer to prove untrusted serialized input is refused
            // independently, rather than relying on the writer's validation.
            assert!(matches!(
                NetworkTraceV1::read_framed(Cursor::new(framed_without_validation(&trace))),
                Err(NetworkTraceCodecError::Validation(
                    NetworkTraceValidationError::NonTerminalSocketError
                ))
            ));
        }
    }

    #[test]
    fn terminal_socket_errors_preserve_framing_and_the_stream_boundary() {
        for errno in [libc::ECONNRESET, libc::ETIMEDOUT] {
            let mut trace = valid_trace();
            trace.inputs[1].event = NetworkInputKindV1::SocketError {
                stream_offset: 8,
                errno,
            };
            assert_eq!(trace.validate(), Ok(()));
            let bytes = framed(&trace);
            assert_eq!(bytes, framed_without_validation(&trace));
            assert_eq!(
                NetworkTraceV1::read_framed(Cursor::new(bytes)).unwrap(),
                trace
            );

            let mut late = trace.inputs[0].clone();
            late.ordinal = 2;
            late.release = trace.inputs[1].release;
            late.event = NetworkInputKindV1::InboundBytes {
                stream_offset: 8,
                bytes: b"late".to_vec(),
            };
            trace.inputs.push(late);
            assert_eq!(
                trace.validate(),
                Err(NetworkTraceValidationError::EventAfterTerminal)
            );
        }
    }

    #[test]
    fn socket_errno_range_is_still_checked_before_framing() {
        for errno in [-1, 0, 4096] {
            let mut trace = valid_trace();
            trace.inputs[1].event = NetworkInputKindV1::SocketError {
                stream_offset: 8,
                errno,
            };
            assert_eq!(
                trace.validate(),
                Err(NetworkTraceValidationError::InvalidErrno)
            );
            assert!(matches!(
                trace.write_framed(Vec::new()),
                Err(NetworkTraceCodecError::Validation(
                    NetworkTraceValidationError::InvalidErrno
                ))
            ));
            assert!(matches!(
                NetworkTraceV1::read_framed(Cursor::new(framed_without_validation(&trace))),
                Err(NetworkTraceCodecError::Validation(
                    NetworkTraceValidationError::InvalidErrno
                ))
            ));
        }
    }

    #[test]
    fn every_strict_prefix_is_reported_as_truncated() {
        let bytes = framed(&valid_trace());
        for end in 0..bytes.len() {
            assert!(
                matches!(
                    NetworkTraceV1::read_framed(Cursor::new(&bytes[..end])),
                    Err(NetworkTraceCodecError::Truncated)
                ),
                "prefix of length {end} was not rejected as truncation"
            );
        }
    }

    #[test]
    fn unknown_versions_are_rejected_before_payload_decode() {
        let mut bytes = framed(&valid_trace());
        let start = NETWORK_TRACE_MAGIC.len();
        bytes[start..start + 4].copy_from_slice(&2u32.to_le_bytes());
        assert!(matches!(
            NetworkTraceV1::read_framed(Cursor::new(bytes)),
            Err(NetworkTraceCodecError::UnsupportedVersion(2))
        ));
    }

    #[test]
    fn hostile_length_is_truncation_without_allocating_it() {
        // Allocating the claimed length before reading it panics here.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&NETWORK_TRACE_MAGIC);
        bytes.extend_from_slice(&NETWORK_TRACE_VERSION_V1.to_le_bytes());
        bytes.extend_from_slice(&u64::MAX.to_le_bytes());
        bytes.extend_from_slice(b"short");
        assert!(matches!(
            NetworkTraceV1::read_framed(Cursor::new(bytes)),
            Err(NetworkTraceCodecError::Truncated)
        ));
    }

    #[test]
    fn trailing_file_data_is_rejected() {
        let mut bytes = framed(&valid_trace());
        bytes.push(0);
        assert!(matches!(
            NetworkTraceV1::read_framed(Cursor::new(bytes)),
            Err(NetworkTraceCodecError::TrailingData)
        ));
    }

    #[test]
    fn reader_and_writer_both_refuse_semantically_invalid_traces() {
        let mut trace = valid_trace();
        trace.inputs[0].ordinal = 7;

        assert!(matches!(
            trace.write_framed(Vec::new()),
            Err(NetworkTraceCodecError::Validation(
                NetworkTraceValidationError::NonCanonicalOrdinal
            ))
        ));
        assert!(matches!(
            NetworkTraceV1::read_framed(Cursor::new(framed_without_validation(&trace))),
            Err(NetworkTraceCodecError::Validation(
                NetworkTraceValidationError::NonCanonicalOrdinal
            ))
        ));
    }

    #[test]
    fn deserialized_pre_unix_epoch_is_a_typed_validation_error() {
        let mut trace = valid_trace();
        trace.epoch = Utc.timestamp_opt(-1, 0).unwrap();
        assert!(matches!(
            NetworkTraceV1::read_framed(Cursor::new(framed_without_validation(&trace))),
            Err(NetworkTraceCodecError::Validation(
                NetworkTraceValidationError::EpochOutOfRange
            ))
        ));
    }

    #[test]
    fn deserialized_epoch_beyond_global_time_is_a_typed_validation_error() {
        let mut trace = valid_trace();
        trace.epoch = Utc.with_ymd_and_hms(9999, 1, 1, 0, 0, 0).unwrap();
        assert!(matches!(
            NetworkTraceV1::read_framed(Cursor::new(framed_without_validation(&trace))),
            Err(NetworkTraceCodecError::Validation(
                NetworkTraceValidationError::EpochOutOfRange
            ))
        ));
    }

    #[test]
    fn config_is_off_by_default_and_perturbation_has_no_seed_fallback() {
        let config = NetworkTraceConfig::default();
        assert_eq!(config.mode, NetworkTraceMode::Off);
        assert_eq!(config.network_perturb_seed, None);
        assert_eq!(config.validate(), Ok(()));

        let replay = NetworkTraceConfig {
            mode: NetworkTraceMode::Replay,
            path: Some("trace.net".into()),
            network_perturb_seed: Some(17),
        };
        assert_eq!(replay.network_perturb_seed, Some(17));
        assert_eq!(replay.validate(), Ok(()));

        let encoded = serde_json::to_string(&replay).unwrap();
        assert_eq!(
            serde_json::from_str::<NetworkTraceConfig>(&encoded).unwrap(),
            replay
        );
        assert!(
            serde_json::from_str::<NetworkTraceConfig>(
                r#"{"mode":"off","path":null,"network_perturb_seed":null,"unexpected":true}"#
            )
            .is_err()
        );
    }

    #[test]
    fn config_rejects_ambiguous_or_record_perturbation_combinations() {
        assert_eq!(
            NetworkTraceConfig {
                path: Some("trace.net".into()),
                ..NetworkTraceConfig::default()
            }
            .validate(),
            Err(NetworkTraceConfigError::OptionsWhileOff)
        );
        assert_eq!(
            NetworkTraceConfig {
                mode: NetworkTraceMode::Replay,
                ..NetworkTraceConfig::default()
            }
            .validate(),
            Err(NetworkTraceConfigError::MissingPath)
        );
        assert_eq!(
            NetworkTraceConfig {
                mode: NetworkTraceMode::Record,
                path: Some("trace.net".into()),
                network_perturb_seed: Some(1),
            }
            .validate(),
            Err(NetworkTraceConfigError::PerturbationDuringRecord)
        );
    }

    #[test]
    fn release_eligibility_uses_the_absolute_global_time_domain() {
        let mut config = Config {
            epoch: trace_epoch(),
            ..Config::default()
        };
        // A different scheduler seed must not affect the clock-domain conversion.
        config.sched_seed = Some(999);
        let mut global_time = GlobalTime::new(&config);
        let start = global_time.as_nanos();
        assert_eq!(valid_trace().epoch_global_time(), Ok(start));
        let release = NetworkReleaseV1 {
            not_before_global_time: start + LogicalTime::from_nanos(10),
            after_transmitted_offset: 3,
        };

        assert!(!release.is_eligible(global_time.as_nanos(), 3));
        global_time.add_extra_time(Duration::from_nanos(10));
        assert_eq!(global_time.as_nanos(), release.not_before_global_time);
        assert!(!release.is_eligible(global_time.as_nanos(), 2));
        assert!(release.is_eligible(global_time.as_nanos(), 3));
    }

    #[test]
    fn v1_validation_rejects_channels_outside_the_supported_envelope() {
        let mut trace = valid_trace();
        trace.channels.push(trace.channels[0].clone());
        assert_eq!(
            trace.validate(),
            Err(NetworkTraceValidationError::ChannelCount(2))
        );

        let mut trace = valid_trace();
        let non_socket = OpenFileId::new(DetTid::from_raw(1), 0);
        trace.channels[0].id = non_socket;
        for input in &mut trace.inputs {
            input.channel = non_socket;
        }
        for output in &mut trace.outputs {
            output.channel = non_socket;
        }
        assert_eq!(
            trace.validate(),
            Err(NetworkTraceValidationError::NonSocketChannelIdentity)
        );

        let mut trace = valid_trace();
        trace.channels[0].peer_address = NetworkAddressV1::Inet4 {
            address: [127, 0, 0, 1],
            port: 443,
        };
        assert_eq!(
            trace.validate(),
            Err(NetworkTraceValidationError::UnsupportedPeerAddress)
        );

        for mapped in [
            Ipv4Addr::LOCALHOST,
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::new(224, 0, 0, 1),
        ] {
            let mut trace = valid_trace();
            trace.channels[0].local_address = NetworkAddressV1::Inet6 {
                address: Ipv6Addr::LOCALHOST.octets(),
                port: 40_000,
                flowinfo: 0,
                scope_id: 0,
            };
            trace.channels[0].peer_address = NetworkAddressV1::Inet6 {
                address: mapped.to_ipv6_mapped().octets(),
                port: 443,
                flowinfo: 0,
                scope_id: 0,
            };
            assert_eq!(
                trace.validate(),
                Err(NetworkTraceValidationError::UnsupportedPeerAddress),
                "IPv4-mapped {mapped} escaped the IPv4 policy"
            );
        }

        let mut trace = valid_trace();
        trace.channels[0].created_before_competing_threads = false;
        assert_eq!(
            trace.validate(),
            Err(NetworkTraceValidationError::ScheduleDependentChannelIdentity)
        );
    }

    #[test]
    fn v1_validation_rejects_noncanonical_streams_and_release_conditions() {
        let mut trace = valid_trace();
        trace.inputs[0].release.not_before_global_time = LogicalTime::ZERO;
        assert_eq!(
            trace.validate(),
            Err(NetworkTraceValidationError::ReleaseBeforeEpoch)
        );

        let mut trace = valid_trace();
        trace.inputs[0].ordinal = 1;
        assert_eq!(
            trace.validate(),
            Err(NetworkTraceValidationError::NonCanonicalOrdinal)
        );

        let mut trace = valid_trace();
        trace.inputs[0].release.after_transmitted_offset = 4;
        assert_eq!(
            trace.validate(),
            Err(NetworkTraceValidationError::UnreachableTransmitWatermark)
        );

        let mut trace = valid_trace();
        trace.outputs[0].stream_offset = 1;
        assert_eq!(
            trace.validate(),
            Err(NetworkTraceValidationError::NonContiguousOutput)
        );

        let mut trace = valid_trace();
        trace.inputs.push(NetworkInputEventV1 {
            ordinal: 2,
            channel: channel_id(),
            release: NetworkReleaseV1 {
                not_before_global_time: global_time_after_epoch(21),
                after_transmitted_offset: 3,
            },
            event: NetworkInputKindV1::InboundBytes {
                stream_offset: 8,
                bytes: b"late".to_vec(),
            },
        });
        assert_eq!(
            trace.validate(),
            Err(NetworkTraceValidationError::EventAfterTerminal)
        );
    }

    fn second_channel_id() -> OpenFileId {
        OpenFileId::new_socket(DetTid::from_raw(1), 1)
    }

    fn loopback(port: u16) -> NetworkAddressV1 {
        NetworkAddressV1::Inet4 {
            address: [127, 0, 0, 1],
            port,
        }
    }

    /// Two loopback channels whose events interleave, plus one refused connect.
    fn valid_trace_v2() -> NetworkTraceV2 {
        let first = channel_id();
        let second = second_channel_id();
        let refused = OpenFileId::new_socket(DetTid::from_raw(2), 0);
        let channel = |id, local: Option<u16>, errno| NetworkChannelV2 {
            id,
            transport: NetworkTransportV1::Tcp,
            role: NetworkEndpointRoleV1::OutboundClient,
            local_address: local.map(loopback),
            peer_address: loopback(8080),
            connect_errno: errno,
        };
        let input = |ordinal, channel, time, tx, event| NetworkInputEventV1 {
            ordinal,
            channel,
            release: NetworkReleaseV1 {
                not_before_global_time: global_time_after_epoch(time),
                after_transmitted_offset: tx,
            },
            event,
        };
        NetworkTraceV2 {
            epoch: trace_epoch(),
            channels: vec![
                channel(first, Some(40_000), 0),
                channel(second, Some(40_001), 0),
                channel(refused, None, libc::ECONNREFUSED),
            ],
            inputs: vec![
                input(
                    0,
                    second,
                    10,
                    2,
                    NetworkInputKindV1::InboundBytes {
                        stream_offset: 0,
                        bytes: b"xy".to_vec(),
                    },
                ),
                input(
                    1,
                    first,
                    10,
                    3,
                    NetworkInputKindV1::InboundBytes {
                        stream_offset: 0,
                        bytes: b"abc".to_vec(),
                    },
                ),
                input(
                    2,
                    first,
                    30,
                    3,
                    NetworkInputKindV1::PeerWriteClosed { stream_offset: 3 },
                ),
            ],
            outputs: vec![
                NetworkOutputV4 {
                    channel: first,
                    stream_offset: 0,
                    bytes: b"req".to_vec(),
                    wait: None,
                },
                NetworkOutputV4 {
                    channel: second,
                    stream_offset: 0,
                    bytes: b"hi".to_vec(),
                    wait: None,
                },
            ],
        }
    }

    #[test]
    fn network_trace_v2_round_trips_exactly() {
        let trace = valid_trace_v2();
        let mut bytes = Vec::new();
        trace.write_framed(&mut bytes).unwrap();
        let start = NETWORK_TRACE_MAGIC.len();
        assert_eq!(
            bytes[start..start + 4],
            NETWORK_TRACE_VERSION_V2.to_le_bytes()
        );
        assert_eq!(
            NetworkTraceV2::read_framed(Cursor::new(bytes)).unwrap(),
            trace
        );
    }

    #[test]
    fn frames_are_the_header_then_the_bincode_payload() {
        // The layout every earlier writer produced: magic, version, payload
        // length, then the payload encoded on its own.
        let v1 = valid_trace();
        assert_eq!(framed(&v1), framed_without_validation(&v1));
        let trace = valid_trace_v2();
        let wire = NetworkTraceV2Wire {
            epoch: trace.epoch,
            channels: trace.channels.clone(),
            inputs: trace.inputs.clone(),
            outputs: trace
                .outputs
                .iter()
                .map(|output| NetworkOutputV1 {
                    channel: output.channel,
                    stream_offset: output.stream_offset,
                    bytes: output.bytes.clone(),
                })
                .collect(),
        };
        let payload = bincode::serde::encode_to_vec(&wire, bincode::config::standard()).unwrap();
        // Literal bytes, so a changed magic or version number fails here too.
        let mut expected = Vec::new();
        expected.extend_from_slice(b"HERMIT-NET-TRACE");
        expected.extend_from_slice(&[2, 0, 0, 0]);
        expected.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        expected.extend_from_slice(&payload);
        assert_eq!(trace.encode_framed().unwrap(), expected);
    }

    #[test]
    fn network_trace_above_64_mib_round_trips() {
        // 64 MiB was the old limit on a whole trace; a download of that size
        // recorded completely and then lost its trace at exit.
        let mut trace = valid_trace_v2();
        let len = 64 * 1024 * 1024 + 1;
        trace.inputs[1].event = NetworkInputKindV1::InboundBytes {
            stream_offset: 0,
            bytes: vec![0xa5; len],
        };
        trace.inputs[2].event = NetworkInputKindV1::PeerWriteClosed {
            stream_offset: len as u64,
        };
        let mut framed = Vec::new();
        trace.write_framed(&mut framed).unwrap();
        assert!(framed.len() > len);
        assert!(NetworkTraceV2::read_framed(Cursor::new(framed)).unwrap() == trace);
    }

    #[test]
    fn network_trace_v2_reader_upgrades_v1() {
        let v1 = valid_trace();
        let upgraded = NetworkTraceV2::read_framed(Cursor::new(framed(&v1))).unwrap();
        assert_eq!(upgraded, NetworkTraceV2::from(v1.clone()));
        assert_eq!(upgraded.channels.len(), 1);
        assert_eq!(upgraded.channels[0].connect_errno, 0);
        assert_eq!(
            upgraded.channels[0].local_address,
            Some(v1.channels[0].local_address.clone())
        );
        assert_eq!(upgraded.inputs, v1.inputs);
        let v1_outputs: Vec<NetworkOutputV4> = v1.outputs.into_iter().map(Into::into).collect();
        assert_eq!(upgraded.outputs, v1_outputs);
    }

    #[test]
    fn network_trace_v2_reader_keeps_v1_validation() {
        let mut v1 = valid_trace();
        v1.channels[0].created_before_competing_threads = false;
        assert!(matches!(
            NetworkTraceV2::read_framed(Cursor::new(framed_without_validation(&v1))),
            Err(NetworkTraceCodecError::Validation(_))
        ));
    }

    #[test]
    fn network_trace_v2_reader_rejects_unknown_versions() {
        let mut bytes = Vec::new();
        valid_trace_v2().write_framed(&mut bytes).unwrap();
        let start = NETWORK_TRACE_MAGIC.len();
        bytes[start..start + 4].copy_from_slice(&5u32.to_le_bytes());
        assert!(matches!(
            NetworkTraceV2::read_framed(Cursor::new(bytes)),
            Err(NetworkTraceCodecError::UnsupportedVersion(5))
        ));
    }

    /// `valid_trace_v2` with one more fragment on the first channel, accepted
    /// after its send waited twice.
    fn waited_trace_v4() -> NetworkTraceV2 {
        let mut trace = valid_trace_v2();
        trace.outputs.push(NetworkOutputV4 {
            channel: trace.channels[0].id,
            stream_offset: 3,
            bytes: b"more".to_vec(),
            wait: Some(NetworkSendWaitV4 {
                waits: 2,
                not_before_global_time: global_time_after_epoch(20),
            }),
        });
        trace
    }

    #[test]
    fn network_trace_v4_holds_send_waits_that_v2_and_v3_cannot() {
        let trace = waited_trace_v4();
        let mut bytes = Vec::new();
        trace.write_framed(&mut bytes).unwrap();
        let start = NETWORK_TRACE_MAGIC.len();
        assert_eq!(
            bytes[start..start + 4],
            NETWORK_TRACE_VERSION_V4.to_le_bytes()
        );
        assert_eq!(
            NetworkTraceV2::read_framed(Cursor::new(bytes.clone())).unwrap(),
            trace
        );
        // The older payloads have no field for a wait, so the same payload
        // labelled v2 or v3 does not decode.
        for older in [NETWORK_TRACE_VERSION_V2, NETWORK_TRACE_VERSION_V3] {
            let mut relabelled = bytes.clone();
            relabelled[start..start + 4].copy_from_slice(&older.to_le_bytes());
            assert!(
                NetworkTraceV2::read_framed(Cursor::new(relabelled)).is_err(),
                "a v4 payload labelled v{older} decoded"
            );
        }
        // A trace without a wait keeps its older version byte for byte.
        let mut unwaited = Vec::new();
        valid_trace_v2().write_framed(&mut unwaited).unwrap();
        assert_eq!(
            unwaited[start..start + 4],
            NETWORK_TRACE_VERSION_V2.to_le_bytes()
        );
    }

    #[test]
    fn network_trace_v4_validation_rejects_each_malformed_wait() {
        let check = |mutate: &dyn Fn(&mut NetworkTraceV2), expected| {
            let mut trace = waited_trace_v4();
            mutate(&mut trace);
            assert_eq!(trace.validate(), Err(expected));
        };
        use NetworkTraceValidationError as E;
        let wait_at = |nanos| {
            Some(NetworkSendWaitV4 {
                waits: 1,
                not_before_global_time: global_time_after_epoch(nanos),
            })
        };
        check(
            &|t| {
                t.outputs.push(NetworkOutputV4 {
                    channel: t.channels[0].id,
                    stream_offset: 7,
                    bytes: Vec::new(),
                    wait: wait_at(30),
                })
            },
            E::WaitOnRefusedSend,
        );
        check(
            &|t| t.outputs[2].wait.as_mut().unwrap().waits = 0,
            E::ZeroSendWaits,
        );
        check(
            &|t| {
                let epoch = epoch_global_time(&t.epoch).unwrap().as_nanos();
                t.outputs[2].wait.as_mut().unwrap().not_before_global_time =
                    LogicalTime::from_nanos(epoch - 1);
            },
            E::SendWaitBeforeEpoch,
        );
        check(
            &|t| {
                t.outputs.push(NetworkOutputV4 {
                    channel: t.channels[0].id,
                    stream_offset: 7,
                    bytes: b"x".to_vec(),
                    wait: wait_at(19),
                })
            },
            E::NonMonotonicSendWait,
        );
        // Monotonicity is per channel: an earlier wait on another channel
        // after a later one on the first is valid.
        let mut trace = waited_trace_v4();
        trace.outputs.push(NetworkOutputV4 {
            channel: trace.channels[1].id,
            stream_offset: 2,
            bytes: b"x".to_vec(),
            wait: wait_at(5),
        });
        assert_eq!(trace.validate(), Ok(()));
    }

    #[test]
    fn network_trace_v3_holds_refused_sends_that_v2_rejects() {
        let mut trace = valid_trace_v2();
        let first = trace.channels[0].id;
        trace.outputs.insert(
            0,
            NetworkOutputV4 {
                channel: first,
                stream_offset: 0,
                bytes: Vec::new(),
                wait: None,
            },
        );
        let mut bytes = Vec::new();
        trace.write_framed(&mut bytes).unwrap();
        let start = NETWORK_TRACE_MAGIC.len();
        assert_eq!(
            bytes[start..start + 4],
            NETWORK_TRACE_VERSION_V3.to_le_bytes()
        );
        assert_eq!(
            NetworkTraceV2::read_framed(Cursor::new(bytes.clone())).unwrap(),
            trace
        );
        bytes[start..start + 4].copy_from_slice(&NETWORK_TRACE_VERSION_V2.to_le_bytes());
        assert!(matches!(
            NetworkTraceV2::read_framed(Cursor::new(bytes)),
            Err(NetworkTraceCodecError::Validation(
                NetworkTraceValidationError::EmptyByteChunk
            ))
        ));
    }

    #[test]
    fn network_trace_v2_validation_rejects_each_malformation() {
        let check = |mutate: &dyn Fn(&mut NetworkTraceV2), expected| {
            let mut trace = valid_trace_v2();
            mutate(&mut trace);
            assert_eq!(trace.validate(), Err(expected));
        };
        use NetworkTraceValidationError as E;
        check(&|t| t.channels[1].id = channel_id(), E::DuplicateChannel);
        check(
            &|t| t.channels[0].id = OpenFileId::new(DetTid::from_raw(1), 9),
            E::NonSocketChannelIdentity,
        );
        check(
            &|t| t.channels[0].peer_address = loopback(0),
            E::UnsupportedPeerAddress,
        );
        check(
            &|t| {
                t.channels[0].peer_address = NetworkAddressV1::Inet4 {
                    address: [0, 0, 0, 0],
                    port: 80,
                }
            },
            E::UnsupportedPeerAddress,
        );
        check(
            &|t| {
                t.channels[0].peer_address = NetworkAddressV1::Inet6 {
                    address: [0; 16],
                    port: 80,
                    flowinfo: 0,
                    scope_id: 0,
                }
            },
            E::UnsupportedPeerAddress,
        );
        check(
            &|t| {
                t.channels[0].peer_address = NetworkAddressV1::Inet6 {
                    address: Ipv6Addr::LOCALHOST.octets(),
                    port: 80,
                    flowinfo: 0,
                    scope_id: 0,
                }
            },
            E::AddressFamilyMismatch,
        );
        check(&|t| t.channels[0].connect_errno = -1, E::InvalidErrno);
        check(&|t| t.channels[0].local_address = None, E::InvalidErrno);
        check(
            &|t| t.channels[2].local_address = Some(loopback(1)),
            E::InvalidErrno,
        );
        check(
            &|t| t.outputs[1].channel = t.channels[2].id,
            E::TrafficOnFailedChannel,
        );
        check(
            &|t| t.inputs[0].channel = t.channels[2].id,
            E::TrafficOnFailedChannel,
        );
        check(
            &|t| t.inputs[0].channel = OpenFileId::new_socket(DetTid::from_raw(9), 0),
            E::UnknownChannel,
        );
        check(&|t| t.outputs[1].stream_offset = 1, E::NonContiguousOutput);
        check(&|t| t.inputs[2].ordinal = 7, E::NonCanonicalOrdinal);
        check(
            &|t| t.inputs[1].release.not_before_global_time = global_time_after_epoch(5),
            E::NonMonotonicRelease,
        );
        check(
            &|t| t.inputs[2].release.after_transmitted_offset = 2,
            E::NonMonotonicRelease,
        );
        check(
            &|t| t.inputs[0].release.after_transmitted_offset = 3,
            E::UnreachableTransmitWatermark,
        );
        check(
            &|t| {
                t.inputs[2].event = NetworkInputKindV1::PeerWriteClosed { stream_offset: 2 };
            },
            E::NonContiguousInput,
        );
        check(
            &|t| {
                let mut late = t.inputs[2].clone();
                late.ordinal = 3;
                late.release.not_before_global_time = global_time_after_epoch(40);
                late.event = NetworkInputKindV1::InboundBytes {
                    stream_offset: 3,
                    bytes: b"z".to_vec(),
                };
                t.inputs.push(late);
            },
            E::EventAfterTerminal,
        );
    }

    #[test]
    fn network_trace_v2_offsets_are_per_channel() {
        // The second channel's bytes start at offset 0 even though the first
        // channel already delivered bytes: a shared cursor would reject this.
        let mut trace = valid_trace_v2();
        trace.inputs.push(NetworkInputEventV1 {
            ordinal: 3,
            channel: second_channel_id(),
            release: NetworkReleaseV1 {
                not_before_global_time: global_time_after_epoch(30),
                after_transmitted_offset: 2,
            },
            event: NetworkInputKindV1::InboundBytes {
                stream_offset: 2,
                bytes: b"z".to_vec(),
            },
        });
        assert_eq!(trace.validate(), Ok(()));
    }
}
