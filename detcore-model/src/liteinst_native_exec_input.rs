/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in LICENSE.
 */

//! Separate, inactive native Initial/Successor bootstrap input data.
//!
//! No decoder fallback to HLIINIT1 exists. Packet bytes, socket identities,
//! nonces, tickets and digests are not admission or kernel-exec authority. A
//! future trusted native consumer must compare independently held expectations,
//! verify distinct protected actual sockets, and redeem its one-use instance.
//! The runtime/Tool composite is delivered separately on the bounded state path.

use std::fmt;

/// Entire control packet bound, unchanged from the existing private input ABI.
pub const MAX_PACKET_LEN: usize = 4096;
/// A protocol distinct from the unchanged HLIINIT1 packet.
pub const MAGIC: [u8; 8] = *b"HLNEXEC1";
/// Only this wire version is admitted by the shape decoder.
pub const VERSION: u16 = 1;
/// Initial header, including all four variable-field lengths.
pub const INITIAL_HEADER_LEN: usize = 200;
/// Successor header additionally carries the composite state digest.
pub const SUCCESSOR_HEADER_LEN: usize = 232;
const TOKEN_START: usize = 160;
const TOKEN_END: usize = 192;
const LENGTH_BYTES: usize = 8;

/// The selected input shape; not a native lifecycle authorization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum Kind {
    /// Fresh one-use native launch instance.
    Initial = 1,
    /// One-use native exec successor transaction.
    Successor = 2,
}

/// Distinct fixed fields for the two native input kinds.
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum Image<'a> {
    /// Independently issued nonce, to be consumed by actual launch admission.
    Initial {
        /// Thirty-two opaque bytes, with no replay or authority check here.
        nonce: &'a [u8; 32],
    },
    /// Independently issued ticket and expected exact composite-state digest.
    Successor {
        /// Thirty-two opaque bytes, consumed before Config/state delivery.
        ticket: &'a [u8; 32],
        /// SHA256 of the entire exact Tool/runtime composite, not a prefix.
        composite_digest: &'a [u8; 32],
    },
}
impl fmt::Debug for Image<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Shape diagnostics must not print a protected nonce or exec ticket.
        match self {
            Self::Initial { .. } => f.write_str("Initial(<private nonce>)"),
            Self::Successor {
                composite_digest, ..
            } => f
                .debug_struct("Successor")
                .field("ticket", &"<private>")
                .field("composite_digest", composite_digest)
                .finish(),
        }
    }
}
impl Image<'_> {
    /// Exact wire kind corresponding to these fields.
    pub fn kind(&self) -> Kind {
        match self {
            Self::Initial { .. } => Kind::Initial,
            Self::Successor { .. } => Kind::Successor,
        }
    }
    fn header_len(&self) -> usize {
        match self {
            Self::Initial { .. } => INITIAL_HEADER_LEN,
            Self::Successor { .. } => SUCCESSOR_HEADER_LEN,
        }
    }
}

/// Borrowed native control fields. Decoding establishes byte shape only.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Packet<'a> {
    /// Canonical legacy wire Config SHA256 supplied independently by admission.
    pub config_digest: &'a [u8; 32],
    /// Existing exactly sixteen ASCII-hex Config schema characters.
    pub config_schema: &'a [u8; 16],
    /// Full declared Rust native source SHA256, independently computed.
    pub native_source_digest: &'a [u8; 32],
    /// Held immutable target identity; bytes here do not prove its file.
    pub target_digest: &'a [u8; 32],
    /// Held native bootstrap/interpreter identity; not exec attestation.
    pub bootstrap_digest: &'a [u8; 32],
    /// One of the distinct nonce or ticket/composite input shapes.
    pub image: Image<'a>,
    /// Nonempty NUL-free raw Unix coordinator path; UTF-8 is not required.
    pub coordinator: &'a [u8],
    /// Unchanged UTF-8 DETLOG fd/device/inode identity string contract.
    pub detlog_identity: &'a str,
    /// Unchanged UTF-8 genuine forwarding policy, compared by the consumer.
    pub forward_policy: &'a str,
    /// Separate UTF-8 inherited observer fd/device/inode identity data. The
    /// consumer must verify its actual socket is distinct and protected; string
    /// inequality is insufficient and this codec does not query descriptors.
    pub observer_identity: &'a str,
}

/// Independently held expected data, not values learned from packet decoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExpectedBindings<'a> {
    /// Canonical full wire Config SHA256.
    pub config_digest: &'a [u8; 32],
    /// Actual compiled Config schema, compared byte-for-byte, never by prefix.
    pub config_schema: &'a [u8; 16],
    /// Full actual compiled native source digest.
    pub native_source_digest: &'a [u8; 32],
    /// Independently held immutable target digest.
    pub target_digest: &'a [u8; 32],
    /// Independently held bootstrap/interpreter digest.
    pub bootstrap_digest: &'a [u8; 32],
}

/// Exact data disagreement. No variant is a kernel errno or authorization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BindingMismatch {
    ConfigDigest,
    ConfigSchema,
    NativeSourceDigest,
    TargetDigest,
    BootstrapDigest,
}
impl Packet<'_> {
    /// Compare complete fixed bindings against independent expected inputs.
    /// This neither verifies sockets nor redeems the nonce/ticket, checks the
    /// composite state bytes, or establishes a watched birth/exec transition.
    pub fn check_bindings(&self, expected: ExpectedBindings<'_>) -> Result<(), BindingMismatch> {
        for (actual, expected, error) in [
            (
                self.config_digest,
                expected.config_digest,
                BindingMismatch::ConfigDigest,
            ),
            (
                self.native_source_digest,
                expected.native_source_digest,
                BindingMismatch::NativeSourceDigest,
            ),
            (
                self.target_digest,
                expected.target_digest,
                BindingMismatch::TargetDigest,
            ),
            (
                self.bootstrap_digest,
                expected.bootstrap_digest,
                BindingMismatch::BootstrapDigest,
            ),
        ] {
            if actual != expected {
                return Err(error);
            }
        }
        if self.config_schema != expected.config_schema {
            return Err(BindingMismatch::ConfigSchema);
        }
        Ok(())
    }
    fn variable_fields(&self) -> [&[u8]; 4] {
        [
            self.coordinator,
            self.detlog_identity.as_bytes(),
            self.forward_policy.as_bytes(),
            self.observer_identity.as_bytes(),
        ]
    }
}

/// Bounded shape error with no allocated error message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketError {
    Size,
    Magic,
    Version,
    Kind,
    Reserved,
    Length,
    Empty,
    Nul,
    Utf8,
    Schema,
}
impl fmt::Display for PacketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Size => "native input complete size mismatch",
            Self::Magic => "native input magic mismatch",
            Self::Version => "native input version mismatch",
            Self::Kind => "native input kind mismatch",
            Self::Reserved => "native input reserved bits are nonzero",
            Self::Length => "native input field lengths do not consume exactly",
            Self::Empty => "native input mandatory field is empty",
            Self::Nul => "native input field contains NUL",
            Self::Utf8 => "native input identity or policy is not UTF-8",
            Self::Schema => "native input Config schema is not sixteen ASCII hex characters",
        })
    }
}
impl std::error::Error for PacketError {}

fn word(bytes: &[u8], start: usize) -> usize {
    usize::from(u16::from_be_bytes([bytes[start], bytes[start + 1]]))
}
fn check_field(bytes: &[u8]) -> Result<(), PacketError> {
    if bytes.is_empty() {
        return Err(PacketError::Empty);
    }
    if bytes.contains(&0) {
        return Err(PacketError::Nul);
    }
    Ok(())
}
fn check_schema(bytes: &[u8; 16]) -> Result<(), PacketError> {
    if bytes.iter().all(u8::is_ascii_hexdigit) {
        Ok(())
    } else {
        Err(PacketError::Schema)
    }
}

/// Validate the complete new packet shape without allocation or old fallback.
pub fn decode(bytes: &[u8]) -> Result<Packet<'_>, PacketError> {
    if !(INITIAL_HEADER_LEN..=MAX_PACKET_LEN).contains(&bytes.len()) {
        return Err(PacketError::Size);
    }
    if bytes[..8] != MAGIC {
        return Err(PacketError::Magic);
    }
    if word(bytes, 8) != usize::from(VERSION) {
        return Err(PacketError::Version);
    }
    if word(bytes, 10) != bytes.len() {
        return Err(PacketError::Size);
    }
    let kind = match word(bytes, 12) {
        1 => Kind::Initial,
        2 => Kind::Successor,
        _ => return Err(PacketError::Kind),
    };
    if word(bytes, 14) != 0 {
        return Err(PacketError::Reserved);
    }
    let header = match kind {
        Kind::Initial => INITIAL_HEADER_LEN,
        Kind::Successor => SUCCESSOR_HEADER_LEN,
    };
    if bytes.len() < header {
        return Err(PacketError::Size);
    }
    let lengths_start = header - LENGTH_BYTES;
    let mut ends = [header; 5];
    for index in 0..4 {
        ends[index + 1] = ends[index]
            .checked_add(word(bytes, lengths_start + index * 2))
            .filter(|end| *end <= bytes.len())
            .ok_or(PacketError::Length)?;
    }
    if ends[4] != bytes.len() {
        return Err(PacketError::Length);
    }
    let fields = std::array::from_fn::<_, 4, _>(|index| &bytes[ends[index]..ends[index + 1]]);
    for field in fields {
        check_field(field)?;
    }
    let token = bytes[TOKEN_START..TOKEN_END]
        .try_into()
        .map_err(|_| PacketError::Length)?;
    let image = match kind {
        Kind::Initial => Image::Initial { nonce: token },
        Kind::Successor => Image::Successor {
            ticket: token,
            composite_digest: bytes[TOKEN_END..TOKEN_END + 32]
                .try_into()
                .map_err(|_| PacketError::Length)?,
        },
    };
    let packet = Packet {
        config_digest: bytes[16..48].try_into().map_err(|_| PacketError::Length)?,
        config_schema: bytes[48..64].try_into().map_err(|_| PacketError::Schema)?,
        native_source_digest: bytes[64..96].try_into().map_err(|_| PacketError::Length)?,
        target_digest: bytes[96..128].try_into().map_err(|_| PacketError::Length)?,
        bootstrap_digest: bytes[128..160]
            .try_into()
            .map_err(|_| PacketError::Length)?,
        image,
        coordinator: fields[0],
        detlog_identity: std::str::from_utf8(fields[1]).map_err(|_| PacketError::Utf8)?,
        forward_policy: std::str::from_utf8(fields[2]).map_err(|_| PacketError::Utf8)?,
        observer_identity: std::str::from_utf8(fields[3]).map_err(|_| PacketError::Utf8)?,
    };
    check_schema(packet.config_schema)?;
    Ok(packet)
}

/// Host-side encoding of the same shape. State bytes do not enter this packet.
pub fn encode(packet: Packet<'_>) -> Result<Vec<u8>, PacketError> {
    let fields = packet.variable_fields();
    let mut total = packet.image.header_len();
    for field in fields {
        if field.len() > MAX_PACKET_LEN {
            return Err(PacketError::Size);
        }
        check_field(field)?;
        total = total
            .checked_add(field.len())
            .filter(|total| *total <= MAX_PACKET_LEN)
            .ok_or(PacketError::Size)?;
    }
    check_schema(packet.config_schema)?;
    let mut bytes = Vec::with_capacity(total);
    bytes.extend_from_slice(&MAGIC);
    bytes.extend_from_slice(&VERSION.to_be_bytes());
    bytes.extend_from_slice(&(total as u16).to_be_bytes());
    bytes.extend_from_slice(&(packet.image.kind() as u16).to_be_bytes());
    bytes.extend_from_slice(&0_u16.to_be_bytes());
    bytes.extend_from_slice(packet.config_digest);
    bytes.extend_from_slice(packet.config_schema);
    bytes.extend_from_slice(packet.native_source_digest);
    bytes.extend_from_slice(packet.target_digest);
    bytes.extend_from_slice(packet.bootstrap_digest);
    match packet.image {
        Image::Initial { nonce } => bytes.extend_from_slice(nonce),
        Image::Successor {
            ticket,
            composite_digest,
        } => {
            bytes.extend_from_slice(ticket);
            bytes.extend_from_slice(composite_digest);
        }
    }
    for field in fields {
        bytes.extend_from_slice(&(field.len() as u16).to_be_bytes());
    }
    for field in fields {
        bytes.extend_from_slice(field);
    }
    Ok(bytes)
}

/// Encode into caller-owned storage with exactly the same wire bytes as
/// `encode`. Bounds and mandatory fields are checked before the first write.
/// No allocation or packet-supplied authentication is performed.
pub fn encode_into(packet: Packet<'_>, output: &mut [u8]) -> Result<usize, PacketError> {
    let fields = packet.variable_fields();
    let mut total = packet.image.header_len();
    for field in fields {
        if field.len() > MAX_PACKET_LEN {
            return Err(PacketError::Size);
        }
        check_field(field)?;
        total = total
            .checked_add(field.len())
            .filter(|total| *total <= MAX_PACKET_LEN)
            .ok_or(PacketError::Size)?;
    }
    check_schema(packet.config_schema)?;
    if output.len() < total {
        return Err(PacketError::Size);
    }
    let mut offset = 0;
    let mut append = |bytes: &[u8]| {
        output[offset..offset + bytes.len()].copy_from_slice(bytes);
        offset += bytes.len();
    };
    append(&MAGIC);
    append(&VERSION.to_be_bytes());
    append(&(total as u16).to_be_bytes());
    append(&(packet.image.kind() as u16).to_be_bytes());
    append(&0_u16.to_be_bytes());
    append(packet.config_digest);
    append(packet.config_schema);
    append(packet.native_source_digest);
    append(packet.target_digest);
    append(packet.bootstrap_digest);
    match packet.image {
        Image::Initial { nonce } => append(nonce),
        Image::Successor {
            ticket,
            composite_digest,
        } => {
            append(ticket);
            append(composite_digest);
        }
    }
    for field in fields {
        append(&(field.len() as u16).to_be_bytes());
    }
    for field in fields {
        append(field);
    }
    if offset != total {
        return Err(PacketError::Length);
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(successor: bool) -> Packet<'static> {
        Packet {
            config_digest: &[2; 32],
            config_schema: b"0123456789abcdef",
            native_source_digest: &[3; 32],
            target_digest: &[4; 32],
            bootstrap_digest: &[5; 32],
            image: if successor {
                Image::Successor {
                    ticket: &[6; 32],
                    composite_digest: &[7; 32],
                }
            } else {
                Image::Initial { nonce: &[1; 32] }
            },
            coordinator: b"/tmp/native-\xff.sock",
            detlog_identity: "209:0000000000000007:0000000000001234",
            forward_policy: "INFO",
            observer_identity: "210:0000000000000007:0000000000005678",
        }
    }
    fn expected(packet: Packet<'_>) -> ExpectedBindings<'_> {
        ExpectedBindings {
            config_digest: packet.config_digest,
            config_schema: packet.config_schema,
            native_source_digest: packet.native_source_digest,
            target_digest: packet.target_digest,
            bootstrap_digest: packet.bootstrap_digest,
        }
    }
    fn advertised(bytes: &mut [u8]) {
        let total = bytes.len() as u16;
        bytes[10..12].copy_from_slice(&total.to_be_bytes());
    }
    fn field_start(packet: Packet<'_>, index: usize) -> usize {
        packet.image.header_len()
            + packet.variable_fields()[..index]
                .iter()
                .map(|field| field.len())
                .sum::<usize>()
    }

    #[test]
    fn exact_layout_both_kinds_roundtrip_and_borrow_all_fields() {
        assert_eq!(MAX_PACKET_LEN, 4096);
        assert_eq!(INITIAL_HEADER_LEN, 200);
        assert_eq!(SUCCESSOR_HEADER_LEN, 232);
        for successor in [false, true] {
            let original = packet(successor);
            let bytes = encode(original).unwrap();
            assert_eq!(&bytes[..8], b"HLNEXEC1");
            assert_eq!(&bytes[8..10], &[0, 1]);
            assert_eq!(word(&bytes, 10), bytes.len());
            assert_eq!(word(&bytes, 12), if successor { 2 } else { 1 });
            assert_eq!(&bytes[14..16], &[0, 0]);
            for (range, wanted) in [
                (16..48, original.config_digest.as_slice()),
                (48..64, original.config_schema.as_slice()),
                (64..96, original.native_source_digest.as_slice()),
                (96..128, original.target_digest.as_slice()),
                (128..160, original.bootstrap_digest.as_slice()),
                (160..192, if successor { &[6; 32] } else { &[1; 32] }),
            ] {
                assert_eq!(&bytes[range], wanted);
            }
            if successor {
                assert_eq!(&bytes[192..224], &[7; 32]);
            }
            let header = original.image.header_len();
            for (index, field) in original.variable_fields().into_iter().enumerate() {
                assert_eq!(word(&bytes, header - LENGTH_BYTES + 2 * index), field.len());
                let start = field_start(original, index);
                assert_eq!(&bytes[start..start + field.len()], field);
            }
            let decoded = decode(&bytes).unwrap();
            assert_eq!(decoded, original);
            assert_eq!(decoded.config_digest.as_ptr(), bytes[16..].as_ptr());
            assert_eq!(decoded.config_schema.as_ptr(), bytes[48..].as_ptr());
            assert_eq!(decoded.native_source_digest.as_ptr(), bytes[64..].as_ptr());
            assert_eq!(decoded.target_digest.as_ptr(), bytes[96..].as_ptr());
            assert_eq!(decoded.bootstrap_digest.as_ptr(), bytes[128..].as_ptr());
            assert_eq!(decoded.coordinator.as_ptr(), bytes[header..].as_ptr());
            assert_eq!(
                decoded.detlog_identity.as_ptr(),
                bytes[field_start(original, 1)..].as_ptr()
            );
            assert_eq!(
                decoded.forward_policy.as_ptr(),
                bytes[field_start(original, 2)..].as_ptr()
            );
            assert_eq!(
                decoded.observer_identity.as_ptr(),
                bytes[field_start(original, 3)..].as_ptr()
            );
            match decoded.image {
                Image::Initial { nonce } => assert_eq!(nonce.as_ptr(), bytes[160..].as_ptr()),
                Image::Successor {
                    ticket,
                    composite_digest,
                } => {
                    assert_eq!(ticket.as_ptr(), bytes[160..].as_ptr());
                    assert_eq!(composite_digest.as_ptr(), bytes[192..].as_ptr());
                }
            }
            assert_eq!(decoded.check_bindings(expected(original)), Ok(()));
        }
    }

    #[test]
    fn every_truncated_prefix_and_wrong_complete_size_refuse() {
        for successor in [false, true] {
            let valid = encode(packet(successor)).unwrap();
            for length in 0..valid.len() {
                assert!(
                    decode(&valid[..length]).is_err(),
                    "kind={successor} prefix={length}"
                );
                if length >= 12 {
                    let mut short = valid[..length].to_vec();
                    advertised(&mut short);
                    assert!(
                        decode(&short).is_err(),
                        "advertised kind={successor} prefix={length}"
                    );
                }
            }
            let mut wrong = valid.clone();
            wrong[10..12].copy_from_slice(&0_u16.to_be_bytes());
            assert_eq!(decode(&wrong), Err(PacketError::Size));
            wrong[10..12].copy_from_slice(&4096_u16.to_be_bytes());
            assert_eq!(decode(&wrong), Err(PacketError::Size));
            let mut extra = valid;
            extra.push(9);
            assert_eq!(decode(&extra), Err(PacketError::Size));
            advertised(&mut extra);
            assert_eq!(decode(&extra), Err(PacketError::Length));
        }
    }

    #[test]
    fn magic_version_kind_reserved_and_every_length_are_exact() {
        for successor in [false, true] {
            let original = packet(successor);
            let valid = encode(original).unwrap();
            for index in 0..8 {
                let mut changed = valid.clone();
                changed[index] ^= 1;
                assert_eq!(decode(&changed), Err(PacketError::Magic));
            }
            for version in [0_u16, 2, u16::MAX] {
                let mut changed = valid.clone();
                changed[8..10].copy_from_slice(&version.to_be_bytes());
                assert_eq!(decode(&changed), Err(PacketError::Version));
            }
            for kind in [0_u16, 3, u16::MAX] {
                let mut changed = valid.clone();
                changed[12..14].copy_from_slice(&kind.to_be_bytes());
                assert_eq!(decode(&changed), Err(PacketError::Kind));
            }
            for index in [14, 15] {
                let mut changed = valid.clone();
                changed[index] = 1;
                assert_eq!(decode(&changed), Err(PacketError::Reserved));
            }
            let lengths = original.image.header_len() - LENGTH_BYTES;
            for field in 0..4 {
                let index = lengths + 2 * field;
                for length in [
                    word(&valid, index) - 1,
                    word(&valid, index) + 1,
                    u16::MAX as usize,
                ] {
                    let mut changed = valid.clone();
                    changed[index..index + 2].copy_from_slice(&(length as u16).to_be_bytes());
                    assert_eq!(
                        decode(&changed),
                        Err(PacketError::Length),
                        "kind={successor} field={field} length={length}"
                    );
                }
            }
        }
    }

    #[test]
    fn every_empty_nul_and_nonutf8_text_field_refuses_raw_path_stays_raw() {
        for successor in [false, true] {
            let original = packet(successor);
            let valid = encode(original).unwrap();
            assert!(std::str::from_utf8(original.coordinator).is_err());
            assert_eq!(decode(&valid).unwrap().coordinator, original.coordinator);
            for index in 0..4 {
                let start = field_start(original, index);
                let length = original.variable_fields()[index].len();
                let mut empty = valid.clone();
                empty.drain(start..start + length);
                let lengths = original.image.header_len() - LENGTH_BYTES + 2 * index;
                empty[lengths..lengths + 2].copy_from_slice(&0_u16.to_be_bytes());
                advertised(&mut empty);
                assert_eq!(decode(&empty), Err(PacketError::Empty), "field {index}");
                let mut nul = valid.clone();
                nul[start] = 0;
                assert_eq!(decode(&nul), Err(PacketError::Nul), "field {index}");
                if index != 0 {
                    let mut nonutf8 = valid.clone();
                    nonutf8[start] = 0xff;
                    assert_eq!(decode(&nonutf8), Err(PacketError::Utf8), "field {index}");
                }
            }
        }
    }

    #[test]
    fn exact_bound_both_kinds_and_each_host_validation_refuse_without_widening() {
        for successor in [false, true] {
            let mut original = packet(successor);
            let used = original.image.header_len()
                + original.variable_fields()[1..]
                    .iter()
                    .map(|field| field.len())
                    .sum::<usize>();
            let path = vec![b'x'; MAX_PACKET_LEN - used];
            original.coordinator = &path;
            let full = encode(original).unwrap();
            assert_eq!(full.len(), 4096);
            assert_eq!(decode(&full).unwrap(), original);
            let oversized = vec![b'x'; path.len() + 1];
            original.coordinator = &oversized;
            assert_eq!(encode(original), Err(PacketError::Size));
            assert_eq!(decode(&vec![1; 4097]), Err(PacketError::Size));
            for field in 0..4 {
                for empty in [false, true] {
                    let value = if empty { "" } else { "x\0" };
                    let mut invalid = packet(successor);
                    match field {
                        0 => invalid.coordinator = value.as_bytes(),
                        1 => invalid.detlog_identity = value,
                        2 => invalid.forward_policy = value,
                        3 => invalid.observer_identity = value,
                        _ => unreachable!(),
                    }
                    assert_eq!(
                        encode(invalid),
                        Err(if empty {
                            PacketError::Empty
                        } else {
                            PacketError::Nul
                        })
                    );
                }
            }
        }
    }

    #[test]
    fn schema_all_positions_are_ascii_hex_and_expected_match_never_uses_a_prefix() {
        for successor in [false, true] {
            let original = packet(successor);
            let valid = encode(original).unwrap();
            for index in 0..16 {
                for byte in [b'g', 0, 0xff, b'/', b':'] {
                    let mut changed = valid.clone();
                    changed[48 + index] = byte;
                    assert_eq!(decode(&changed), Err(PacketError::Schema));
                    let schema: &[u8; 16] = changed[48..64].try_into().unwrap();
                    let mut packet = original;
                    packet.config_schema = schema;
                    assert_eq!(encode(packet), Err(PacketError::Schema));
                }
            }
            let uppercase = *b"0123456789ABCDEF";
            let mut packet = original;
            packet.config_schema = &uppercase;
            let bytes = encode(packet).unwrap();
            assert_eq!(
                decode(&bytes).unwrap().check_bindings(expected(original)),
                Err(BindingMismatch::ConfigSchema)
            );
            let mut last = *original.config_schema;
            last[15] = b'0';
            assert_eq!(&last[..15], &original.config_schema[..15]);
            let mut independent = expected(original);
            independent.config_schema = &last;
            assert_eq!(
                decode(&valid).unwrap().check_bindings(independent),
                Err(BindingMismatch::ConfigSchema)
            );
        }
    }

    #[test]
    fn all_digest_bytes_compare_exactly_with_independent_expectations() {
        for successor in [false, true] {
            let original = packet(successor);
            let bytes = encode(original).unwrap();
            let decoded = decode(&bytes).unwrap();
            for field in 0..4 {
                for index in 0..32 {
                    let mut changed = *[
                        original.config_digest,
                        original.native_source_digest,
                        original.target_digest,
                        original.bootstrap_digest,
                    ][field];
                    changed[index] ^= 1;
                    let mut bindings = expected(original);
                    let error = match field {
                        0 => {
                            bindings.config_digest = &changed;
                            BindingMismatch::ConfigDigest
                        }
                        1 => {
                            bindings.native_source_digest = &changed;
                            BindingMismatch::NativeSourceDigest
                        }
                        2 => {
                            bindings.target_digest = &changed;
                            BindingMismatch::TargetDigest
                        }
                        3 => {
                            bindings.bootstrap_digest = &changed;
                            BindingMismatch::BootstrapDigest
                        }
                        _ => unreachable!(),
                    };
                    assert_eq!(
                        decoded.check_bindings(bindings),
                        Err(error),
                        "field={field} byte={index}"
                    );
                }
            }
        }
    }

    #[test]
    fn kinds_have_distinct_token_shapes_and_no_hliinit1_reader_fallback() {
        for successor in [false, true] {
            let valid = encode(packet(successor)).unwrap();
            let mut opposite = valid.clone();
            opposite[12..14]
                .copy_from_slice(&(if successor { 1_u16 } else { 2_u16 }).to_be_bytes());
            assert!(decode(&opposite).is_err());
            assert_eq!(
                crate::liteinst_initial_image::decode(&valid),
                Err(crate::liteinst_initial_image::PacketError::Magic)
            );
        }
        let mut missing_composite = encode(packet(true)).unwrap();
        missing_composite.drain(192..224);
        advertised(&mut missing_composite);
        assert!(decode(&missing_composite).is_err());
        let mut spurious_composite = encode(packet(false)).unwrap();
        spurious_composite.splice(192..192, [7; 32]);
        advertised(&mut spurious_composite);
        assert!(decode(&spurious_composite).is_err());
        let path = vec![b'x'; 200];
        let legacy = crate::liteinst_initial_image::Packet {
            nonce: &[1; 32],
            config_digest: &[2; 32],
            schema_fingerprint: b"0123456789abcdef",
            coordinator: &path,
            detlog_identity: "19:7:1234",
            forward_policy: "INFO",
        };
        let legacy_bytes = crate::liteinst_initial_image::encode(legacy).unwrap();
        assert_eq!(
            crate::liteinst_initial_image::decode(&legacy_bytes),
            Ok(legacy)
        );
        assert_eq!(decode(&legacy_bytes), Err(PacketError::Magic));
    }

    #[test]
    fn packet_tokens_and_identity_text_are_data_not_socket_or_redemption_proof() {
        let mut original = packet(false);
        original.image = Image::Initial { nonce: &[0; 32] };
        // Shape does not invent real descriptor separation or one-use proof.
        original.observer_identity = original.detlog_identity;
        let bytes = encode(original).unwrap();
        assert_eq!(decode(&bytes), Ok(original));
        let mut successor = packet(true);
        successor.image = Image::Successor {
            ticket: &[0; 32],
            composite_digest: &[0; 32],
        };
        let bytes = encode(successor).unwrap();
        assert_eq!(decode(&bytes), Ok(successor));
        assert!(!format!("{:?}", original.image).contains("[0,"));
        assert!(!format!("{:?}", successor.image).contains("ticket: ["));
    }
    #[test]
    fn preallocated_encode_preserves_every_existing_wire_byte_and_unused_tail() {
        for successor in [false, true] {
            let value = packet(successor);
            let expected = encode(value).unwrap();
            let mut output = [0xa5; MAX_PACKET_LEN];
            let length = encode_into(value, &mut output).unwrap();
            assert_eq!(&output[..length], expected);
            assert!(output[length..].iter().all(|byte| *byte == 0xa5));
            assert_eq!(decode(&output[..length]).unwrap(), value);
            for short in 0..length {
                let mut output = vec![0xa5; short];
                assert_eq!(encode_into(value, &mut output), Err(PacketError::Size));
                assert!(output.iter().all(|byte| *byte == 0xa5));
            }
        }
    }

    #[test]
    fn preallocated_invalid_fields_fail_before_writing_any_output() {
        for successor in [false, true] {
            for mode in 0..5 {
                let mut value = packet(successor);
                match mode {
                    0 => value.coordinator = b"",
                    1 => value.coordinator = b"bad\0path",
                    2 => value.config_schema = b"not-hexadecimal!",
                    3 => value.forward_policy = "",
                    4 => value.observer_identity = "bad\0identity",
                    _ => unreachable!(),
                }
                let expected = encode(value).unwrap_err();
                let mut output = [0xa5; MAX_PACKET_LEN];
                assert_eq!(encode_into(value, &mut output), Err(expected));
                assert!(output.iter().all(|byte| *byte == 0xa5));
            }
        }
    }
}
