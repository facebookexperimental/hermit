//! Exact fixed runtime envelope shared by native consumer and coordinator.
//! Decoding shape is not authority to restore counters or publish an image.

pub const STATS_BYTES: usize = 8338;
pub const PACKET_BYTES: usize = 24 + STATS_BYTES;
const HEADER: [u8; 16] = *b"HLXRT001\x01\x00\x00\x00\x00\x00\x00\x00";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimePacket<'a> {
    pub sequence: u64,
    pub stats: &'a [u8; STATS_BYTES],
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PacketError;

impl RuntimePacket<'_> {
    pub fn encode(&self, output: &mut [u8; PACKET_BYTES]) -> Result<(), PacketError> {
        if self.sequence == 0 || self.sequence == u64::MAX {
            return Err(PacketError);
        }
        output[..16].copy_from_slice(&HEADER);
        output[16..24].copy_from_slice(&self.sequence.to_le_bytes());
        output[24..].copy_from_slice(self.stats);
        Ok(())
    }
}
pub fn decode(bytes: &[u8]) -> Result<RuntimePacket<'_>, PacketError> {
    if bytes.len() != PACKET_BYTES || bytes[..16] != HEADER {
        return Err(PacketError);
    }
    let sequence = u64::from_le_bytes(bytes[16..24].try_into().map_err(|_| PacketError)?);
    if sequence == 0 || sequence == u64::MAX {
        return Err(PacketError);
    }
    Ok(RuntimePacket {
        sequence,
        stats: bytes[24..].try_into().map_err(|_| PacketError)?,
    })
}
