/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in LICENSE.
 */

//! Bounded InitialImage launch packet shared by the host and private runtime.
//!
//! Packet bytes, nonce and file seals are not authorization. The coordinator
//! separately redeems a one-use launch instance against its actual process watch.
//! Decoding validates the complete byte shape without allocating. Only the host
//! encodes packets; the private initializer consumes borrowed fields.

use std::fmt;

use crate::config::Config;

/// Maximum complete packet size, including its fixed header.
pub const MAX_PACKET_LEN: usize = 4096;
/// Magic naming this explicit InitialImage protocol.
pub const MAGIC: [u8; 8] = *b"HLIINIT1";
/// Only this wire version is accepted.
pub const VERSION: u16 = 1;
/// Bytes before the three variable length fields.
pub const HEADER_LEN: usize = 98;

/// Borrowed fields of one complete, bounded launch packet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Packet<'a> {
    /// Coordinator-held one-use challenge to redeem; not authority by itself.
    pub nonce: &'a [u8; 32],
    /// Independently supplied SHA-256 of the exact wire Config bytes.
    pub config_digest: &'a [u8; 32],
    /// Sixteen ASCII hex characters naming the actual Config/RPC schema.
    pub schema_fingerprint: &'a [u8; 16],
    /// Nonempty raw Unix coordinator path bytes (UTF-8 is not required).
    pub coordinator: &'a [u8],
    /// Nonempty UTF-8 inherited DETLOG fd and physical socket identity.
    pub detlog_identity: &'a str,
    /// Nonempty UTF-8 encoded genuine DETLOG forwarding policy.
    pub forward_policy: &'a str,
}

/// A packet shape refusal, with no heap-backed error payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketError {
    /// Complete size is outside the bound or differs from the advertised size.
    Size,
    /// Magic does not name this protocol.
    Magic,
    /// Unsupported version.
    Version,
    /// Field lengths do not consume the complete packet exactly.
    Length,
    /// A mandatory text/path field is empty.
    Empty,
    /// A text/path field contains a NUL byte.
    Nul,
    /// DETLOG identity or policy is not UTF-8.
    Utf8,
    /// Schema fingerprint is not exactly sixteen ASCII hex characters.
    Fingerprint,
}
impl fmt::Display for PacketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Size => "InitialImage packet size mismatch",
            Self::Magic => "InitialImage packet magic mismatch",
            Self::Version => "InitialImage packet version mismatch",
            Self::Length => "InitialImage packet field length mismatch",
            Self::Empty => "InitialImage packet mandatory field is empty",
            Self::Nul => "InitialImage packet field contains NUL",
            Self::Utf8 => "InitialImage packet DETLOG field is not UTF-8",
            Self::Fingerprint => "InitialImage packet schema fingerprint is not ASCII hex",
        })
    }
}
impl std::error::Error for PacketError {}

/// Decode and validate all fields and exact full consumption before allocation.
pub fn decode(bytes: &[u8]) -> Result<Packet<'_>, PacketError> {
    if !(HEADER_LEN..=MAX_PACKET_LEN).contains(&bytes.len()) {
        return Err(PacketError::Size);
    }
    if bytes[..8] != MAGIC {
        return Err(PacketError::Magic);
    }
    if u16::from_be_bytes([bytes[8], bytes[9]]) != VERSION {
        return Err(PacketError::Version);
    }
    if usize::from(u16::from_be_bytes([bytes[10], bytes[11]])) != bytes.len() {
        return Err(PacketError::Size);
    }
    let lengths = [92, 94, 96].map(|i| usize::from(u16::from_be_bytes([bytes[i], bytes[i + 1]])));
    let path_end = HEADER_LEN + lengths[0];
    let identity_end = path_end + lengths[1];
    let policy_end = identity_end + lengths[2];
    if policy_end != bytes.len() {
        return Err(PacketError::Length);
    }
    // Positive lengths and the exact end prove all three bounds before slicing.
    let coordinator = &bytes[HEADER_LEN..path_end];
    let identity = &bytes[path_end..identity_end];
    let policy = &bytes[identity_end..policy_end];
    check_text(coordinator)?;
    check_text(identity)?;
    check_text(policy)?;
    let packet = Packet {
        nonce: bytes[12..44].try_into().map_err(|_| PacketError::Length)?,
        config_digest: bytes[44..76].try_into().map_err(|_| PacketError::Length)?,
        schema_fingerprint: bytes[76..92]
            .try_into()
            .map_err(|_| PacketError::Fingerprint)?,
        coordinator,
        detlog_identity: std::str::from_utf8(identity).map_err(|_| PacketError::Utf8)?,
        forward_policy: std::str::from_utf8(policy).map_err(|_| PacketError::Utf8)?,
    };
    check_fingerprint(packet.schema_fingerprint)?;
    Ok(packet)
}

/// Host-side encoding with the same shape checks as the borrowed decoder.
pub fn encode(packet: Packet<'_>) -> Result<Vec<u8>, PacketError> {
    let fields = [
        packet.coordinator,
        packet.detlog_identity.as_bytes(),
        packet.forward_policy.as_bytes(),
    ];
    for field in fields {
        if field.len() > MAX_PACKET_LEN {
            return Err(PacketError::Size);
        }
        check_text(field)?;
    }
    check_fingerprint(packet.schema_fingerprint)?;
    let total = HEADER_LEN
        .checked_add(fields.iter().map(|field| field.len()).sum::<usize>())
        .filter(|total| *total <= MAX_PACKET_LEN)
        .ok_or(PacketError::Size)?;
    let mut bytes = Vec::with_capacity(total);
    bytes.extend_from_slice(&MAGIC);
    bytes.extend_from_slice(&VERSION.to_be_bytes());
    bytes.extend_from_slice(&(total as u16).to_be_bytes());
    bytes.extend_from_slice(packet.nonce);
    bytes.extend_from_slice(packet.config_digest);
    bytes.extend_from_slice(packet.schema_fingerprint);
    for field in fields {
        bytes.extend_from_slice(&(field.len() as u16).to_be_bytes());
    }
    for field in fields {
        bytes.extend_from_slice(field);
    }
    Ok(bytes)
}

fn check_text(bytes: &[u8]) -> Result<(), PacketError> {
    if bytes.is_empty() {
        return Err(PacketError::Empty);
    }
    if bytes.contains(&0) {
        return Err(PacketError::Nul);
    }
    Ok(())
}
fn check_fingerprint(bytes: &[u8; 16]) -> Result<(), PacketError> {
    if bytes.iter().all(u8::is_ascii_hexdigit) {
        Ok(())
    } else {
        Err(PacketError::Fingerprint)
    }
}

/// Refuse unsupported InitialImage configuration without changing it.
///
/// This first slice excludes PMU/RCB time, maximum/target timeslices, imprecise
/// timers, chaos and slowdown, preemption recording/replay and stack traces,
/// schedule replay, and record/replay execution modes. `record_host_inputs`
/// remains supported for genuine strict verification. No startup-accounting,
/// successor exec, fork or thread capability is implied by this predicate.
pub fn config_violation(config: &Config) -> Option<&'static str> {
    if config.use_rcb_time() || config.max_timeslice.is_some() || config.target_timeslice.is_some()
    {
        return Some("InitialImage does not support RCB time or preemption timeslices");
    }
    if config.imprecise_timers
        || config.chaos
        || config.chaos_target_races
        || config.chaos_per_thread_slowdown
    {
        return Some("InitialImage does not support chaos or imprecise timers");
    }
    if config.record_preemptions
        || config.record_preemptions_to.is_some()
        || config.replay_preemptions_from.is_some()
        || config.replay_schedule_from.is_some()
        || config.preemption_stacktrace
        || config.preemption_stacktrace_log_file.is_some()
        || !config.stacktrace_event.is_empty()
        || config.stacktrace_signal.is_some()
    {
        return Some(
            "InitialImage does not support preemption/schedule recording, replay or stack traces",
        );
    }
    if config.recordreplay_modes || config.replaying || config.replay_data.is_some() {
        return Some("InitialImage does not support record/replay execution");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet<'a>() -> Packet<'a> {
        Packet {
            nonce: &[1; 32],
            config_digest: &[2; 32],
            schema_fingerprint: b"0123456789abcdef",
            coordinator: b"/tmp/coordinator-\xff.sock",
            detlog_identity: "19:7:1234",
            forward_policy: "INFO",
        }
    }

    #[test]
    fn exact_wire_shape_and_borrowed_roundtrip_accept_raw_path() {
        let packet = packet();
        let encoded = encode(packet).unwrap();
        assert_eq!(
            &encoded[..12],
            &[b'H', b'L', b'I', b'I', b'N', b'I', b'T', b'1', 0, 1, 0, 134]
        );
        assert_eq!(HEADER_LEN, 98);
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded, packet);
        assert_eq!(decoded.coordinator.as_ptr(), encoded[HEADER_LEN..].as_ptr());
        assert_eq!(decoded.nonce.as_ptr(), encoded[12..].as_ptr());
    }

    #[test]
    fn malformed_shapes_and_every_truncated_prefix_refuse() {
        let valid = encode(packet()).unwrap();
        for length in 0..valid.len() {
            assert!(decode(&valid[..length]).is_err(), "prefix {length}");
        }
        for (index, byte, expected) in [
            (0, 0, PacketError::Magic),
            (9, 2, PacketError::Version),
            (11, 0, PacketError::Size),
            (76, b'g', PacketError::Fingerprint),
            (93, 0, PacketError::Length),
            (98, 0, PacketError::Nul),
        ] {
            let mut bytes = valid.clone();
            bytes[index] = byte;
            assert_eq!(decode(&bytes), Err(expected), "offset {index}");
        }
        for field in 0..3 {
            let mut bytes = valid.clone();
            let index = 92 + field * 2;
            let length = u16::from_be_bytes([bytes[index], bytes[index + 1]]) as usize;
            let start = 98
                + [
                    packet().coordinator.len(),
                    packet().detlog_identity.len(),
                    packet().forward_policy.len(),
                ][..field]
                    .iter()
                    .sum::<usize>();
            bytes.drain(start..start + length);
            bytes[index..index + 2].copy_from_slice(&0_u16.to_be_bytes());
            let total = bytes.len() as u16;
            bytes[10..12].copy_from_slice(&total.to_be_bytes());
            assert_eq!(
                decode(&bytes),
                Err(PacketError::Empty),
                "empty field {field}"
            );
        }
        let mut trailing = valid.clone();
        trailing.push(1);
        assert_eq!(decode(&trailing), Err(PacketError::Size));
        let total = trailing.len() as u16;
        trailing[10..12].copy_from_slice(&total.to_be_bytes());
        assert_eq!(decode(&trailing), Err(PacketError::Length));
        let identity_start = 98 + packet().coordinator.len();
        for (byte, error) in [(0, PacketError::Nul), (0xff, PacketError::Utf8)] {
            let mut bytes = valid.clone();
            bytes[identity_start] = byte;
            assert_eq!(decode(&bytes), Err(error));
        }
        let policy_start = identity_start + packet().detlog_identity.len();
        let mut bytes = valid;
        bytes[policy_start] = 0xff;
        assert_eq!(decode(&bytes), Err(PacketError::Utf8));
    }

    #[test]
    fn packet_bound_and_host_validation_have_no_fallback() {
        let mut packet = packet();
        let path = vec![
            b'x';
            MAX_PACKET_LEN
                - HEADER_LEN
                - packet.detlog_identity.len()
                - packet.forward_policy.len()
        ];
        packet.coordinator = &path;
        let bytes = encode(packet).unwrap();
        assert_eq!(bytes.len(), 4096);
        assert!(decode(&bytes).is_ok());
        let oversized = vec![b'x'; path.len() + 1];
        packet.coordinator = &oversized;
        assert_eq!(encode(packet), Err(PacketError::Size));
        packet.coordinator = b"";
        assert_eq!(encode(packet), Err(PacketError::Empty));
        packet.coordinator = b"x\0";
        assert_eq!(encode(packet), Err(PacketError::Nul));
        packet.coordinator = b"x";
        packet.detlog_identity = "";
        assert_eq!(encode(packet), Err(PacketError::Empty));
        packet.detlog_identity = "1";
        packet.forward_policy = "\0";
        assert_eq!(encode(packet), Err(PacketError::Nul));
        assert_eq!(decode(&vec![1; 4097]), Err(PacketError::Size));
    }

    fn bounded_config() -> Config {
        Config {
            max_timeslice: None,
            target_timeslice: None,
            no_rcb_time: true,
            ..Config::default()
        }
    }
    #[test]
    fn actual_config_options_refuse_without_mutation_and_verify_capture_remains() {
        let mut allowed = bounded_config();
        allowed.record_host_inputs = true;
        assert_eq!(config_violation(&allowed), None);
        let mut rcb = bounded_config();
        rcb.no_rcb_time = false;
        rcb.max_timeslice = Some(std::num::NonZeroU64::new(1).unwrap());
        assert!(rcb.use_rcb_time());
        assert!(config_violation(&rcb).is_some());
        let mutations: &[fn(&mut Config)] = &[
            |c| c.max_timeslice = Some(std::num::NonZeroU64::new(1).unwrap()),
            |c| c.target_timeslice = Some(std::num::NonZeroU64::new(1).unwrap()),
            |c| c.imprecise_timers = true,
            |c| c.chaos = true,
            |c| c.chaos_target_races = true,
            |c| c.chaos_per_thread_slowdown = true,
            |c| c.record_preemptions = true,
            |c| c.record_preemptions_to = Some("trace".into()),
            |c| c.replay_preemptions_from = Some("trace".into()),
            |c| c.replay_schedule_from = Some("trace".into()),
            |c| c.preemption_stacktrace = true,
            |c| c.preemption_stacktrace_log_file = Some("trace".into()),
            |c| c.stacktrace_event.push((1, None)),
            |c| c.stacktrace_signal = Some(crate::schedule::SigWrapper(libc::SIGINT)),
            |c| c.recordreplay_modes = true,
            |c| c.replaying = true,
            |c| c.replay_data = Some("record".into()),
        ];
        for (index, mutate) in mutations.iter().enumerate() {
            let mut config = bounded_config();
            mutate(&mut config);
            let before = bincode::serde::encode_to_vec(&config, bincode::config::legacy()).unwrap();
            assert!(config_violation(&config).is_some(), "case {index}");
            assert_eq!(
                bincode::serde::encode_to_vec(&config, bincode::config::legacy()).unwrap(),
                before
            );
        }
    }
}
