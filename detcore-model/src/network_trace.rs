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
//! It defines the fail-closed v1 envelope and its framing so a future runtime
//! integration cannot accidentally reuse the schedule-coupled syscall event
//! stream. Merely constructing a [`NetworkTraceConfig`] does not enable any
//! behavior today.

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
/// The only network trace format this build accepts.
pub const NETWORK_TRACE_VERSION_V1: u32 = 1;
/// Refuse hostile or corrupt length headers before allocating memory.
pub const MAX_NETWORK_TRACE_PAYLOAD_BYTES: u64 = 64 * 1024 * 1024;

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkOutputV1 {
    pub channel: OpenFileId,
    pub stream_offset: u64,
    pub bytes: Vec<u8>,
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
        let seconds = u64::try_from(self.epoch.timestamp())
            .map_err(|_| NetworkTraceValidationError::EpochOutOfRange)?;
        let whole_seconds = seconds
            .checked_mul(1_000_000_000)
            .ok_or(NetworkTraceValidationError::EpochOutOfRange)?;
        let fractional_micros = u64::from(self.epoch.timestamp_subsec_micros())
            .checked_mul(1_000)
            .ok_or(NetworkTraceValidationError::EpochOutOfRange)?;
        whole_seconds
            .checked_add(fractional_micros)
            .map(LogicalTime::from_nanos)
            .ok_or(NetworkTraceValidationError::EpochOutOfRange)
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
    pub fn write_framed<W: Write>(&self, mut writer: W) -> Result<(), NetworkTraceCodecError> {
        self.validate()?;
        let payload = bincode::serde::encode_to_vec(self, bincode::config::standard())
            .map_err(NetworkTraceCodecError::Encode)?;
        let payload_len =
            u64::try_from(payload.len()).map_err(|_| NetworkTraceCodecError::TooLarge)?;
        if payload_len > MAX_NETWORK_TRACE_PAYLOAD_BYTES {
            return Err(NetworkTraceCodecError::TooLarge);
        }
        writer.write_all(&NETWORK_TRACE_MAGIC)?;
        writer.write_all(&NETWORK_TRACE_VERSION_V1.to_le_bytes())?;
        writer.write_all(&payload_len.to_le_bytes())?;
        writer.write_all(&payload)?;
        Ok(())
    }

    /// Read exactly one complete trace, rejecting truncation, trailing bytes,
    /// unknown versions, malformed payloads, and invalid v1 semantics.
    pub fn read_framed<R: Read>(mut reader: R) -> Result<Self, NetworkTraceCodecError> {
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
        if version != NETWORK_TRACE_VERSION_V1 {
            return Err(NetworkTraceCodecError::UnsupportedVersion(version));
        }
        let len_start = version_start + 4;
        let payload_len = u64::from_le_bytes(
            header[len_start..len_start + 8]
                .try_into()
                .expect("fixed-size length field"),
        );
        if payload_len > MAX_NETWORK_TRACE_PAYLOAD_BYTES {
            return Err(NetworkTraceCodecError::TooLarge);
        }
        let payload_len =
            usize::try_from(payload_len).map_err(|_| NetworkTraceCodecError::TooLarge)?;
        let mut payload = vec![0; payload_len];
        read_exact_or_truncated(&mut reader, &mut payload)?;
        let mut trailing = [0u8; 1];
        if reader.read(&mut trailing)? != 0 {
            return Err(NetworkTraceCodecError::TrailingData);
        }
        let (trace, consumed): (Self, usize) =
            bincode::serde::decode_from_slice(&payload, bincode::config::standard())
                .map_err(NetworkTraceCodecError::Decode)?;
        if consumed != payload.len() {
            return Err(NetworkTraceCodecError::TrailingPayloadData);
        }
        trace.validate()?;
        Ok(trace)
    }
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

/// Semantically invalid v1 trace.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    TooLarge,
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
    fn oversized_length_is_rejected_before_allocation() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&NETWORK_TRACE_MAGIC);
        bytes.extend_from_slice(&NETWORK_TRACE_VERSION_V1.to_le_bytes());
        bytes.extend_from_slice(&(MAX_NETWORK_TRACE_PAYLOAD_BYTES + 1).to_le_bytes());
        assert!(matches!(
            NetworkTraceV1::read_framed(Cursor::new(bytes)),
            Err(NetworkTraceCodecError::TooLarge)
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
}
