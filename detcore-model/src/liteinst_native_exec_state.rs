//! Exact neutral identity envelope around opaque Tool and backend state.
//!
//! These are authenticated transport data, never native-exec authority. The
//! registry must independently join the cut to its real scheduler/watch owner.
//! Old descriptor-table identity originates in the trusted live guest receipt;
//! the host does not reconstruct it from kernel descriptors or decode a Tool.

use serde::Deserialize;
use serde::Serialize;

use crate::fd::FilesId;
use crate::futex::MmId;
use crate::pid::DetPid;
use crate::pid::DetTid;

/// Maximum complete Tool/runtime frame; the bootstrap control stays 4 KiB.
pub const MAX_BYTES: usize = 1024 * 1024;
/// Fixed identity bytes, using the existing legacy positional model encoding.
pub const BINDING_BYTES: usize = 80;
/// Magic, version, two exact lengths and the neutral cut identity.
pub const HEADER_BYTES: usize = 18 + BINDING_BYTES;

/// Actual live producer identities retained by the authenticated coordinator.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CutBinding {
    /// Actual deterministic main-thread identity.
    pub dettid: DetTid,
    /// Continuing deterministic process identity.
    pub detpid: DetPid,
    /// Actual watched physical leader/TID.
    pub physical_tid: i32,
    /// Old memory incarnation from the live pre-preparation receipt.
    pub old_mm: MmId,
    /// Old table incarnation from that same actual live table receipt.
    pub old_files: FilesId,
    /// Replacement memory incarnation proved by ordinary exec preparation.
    pub prepared_mm: MmId,
    /// Independently admitted exact wire Config digest.
    pub config_digest: [u8; 32],
}

impl CutBinding {
    /// Refuse unsupported identities and overflowing model transitions.
    pub fn validate(self) -> Result<(), Error> {
        if self.dettid != self.detpid
            || self.dettid.as_raw() <= 0
            || self.physical_tid <= 0
            || self.old_mm.creator() != self.dettid
            || self.old_files.creator() != self.dettid
            || self.old_mm.generation().checked_add(1) != Some(self.prepared_mm.generation())
            || self.prepared_mm.creator() != self.dettid
            || self.prepared_mm.generation() == 0
            || self.old_files.generation() == u64::MAX
            || self.config_digest == [0; 32]
        {
            return Err(Error::Identity);
        }
        Ok(())
    }
}

/// Structural/data error, distinct from physical launch proof or Linux errno.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// Unsupported magic/version, lengths, trailing bytes or finite bound.
    Bounds,
    /// Unsupported or inconsistent neutral identities.
    Identity,
}

/// Encode the exact version-2 header into preallocated storage, with no heap.
pub fn encode_header(
    binding: CutBinding,
    tool_len: usize,
    runtime_len: usize,
    out: &mut [u8; HEADER_BYTES],
) -> Result<(), Error> {
    binding.validate()?;
    let length = HEADER_BYTES
        .checked_add(tool_len)
        .and_then(|n| n.checked_add(runtime_len));
    if tool_len == 0 || runtime_len == 0 || length.is_none_or(|n| n > MAX_BYTES) {
        return Err(Error::Bounds);
    }
    out[..8].copy_from_slice(b"HLXSTATE");
    out[8..10].copy_from_slice(&2u16.to_le_bytes());
    out[10..14].copy_from_slice(&(tool_len as u32).to_le_bytes());
    out[14..18].copy_from_slice(&(runtime_len as u32).to_le_bytes());
    let n = bincode::serde::encode_into_slice(binding, &mut out[18..], bincode::config::legacy())
        .map_err(|_| Error::Bounds)?;
    if n != BINDING_BYTES {
        return Err(Error::Bounds);
    }
    Ok(())
}

/// Split only a bounded, exact frame. Its entire digest must first be checked
/// against the independently admitted successor ticket's expected state.
pub fn decode(bytes: &[u8]) -> Result<(CutBinding, &[u8], &[u8]), Error> {
    if bytes.len() < HEADER_BYTES
        || bytes.len() > MAX_BYTES
        || &bytes[..8] != b"HLXSTATE"
        || bytes[8..10] != 2u16.to_le_bytes()
    {
        return Err(Error::Bounds);
    }
    let tool_len = u32::from_le_bytes(bytes[10..14].try_into().unwrap()) as usize;
    let runtime_len = u32::from_le_bytes(bytes[14..18].try_into().unwrap()) as usize;
    let end = HEADER_BYTES.checked_add(tool_len).ok_or(Error::Bounds)?;
    if tool_len == 0 || runtime_len == 0 || end.checked_add(runtime_len) != Some(bytes.len()) {
        return Err(Error::Bounds);
    }
    let (binding, used): (CutBinding, usize) =
        bincode::serde::decode_from_slice(&bytes[18..HEADER_BYTES], bincode::config::legacy())
            .map_err(|_| Error::Identity)?;
    if used != BINDING_BYTES {
        return Err(Error::Bounds);
    }
    binding.validate()?;
    Ok((binding, &bytes[HEADER_BYTES..end], &bytes[end..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding() -> CutBinding {
        let owner = DetTid::from_raw(41);
        let old_mm = MmId::initial(owner).for_exec(owner);
        CutBinding {
            dettid: owner,
            detpid: owner,
            physical_tid: 73,
            old_mm,
            old_files: FilesId::initial(owner).for_exec(owner),
            prepared_mm: old_mm.for_exec(owner),
            config_digest: [1; 32],
        }
    }

    #[test]
    fn neutral_state_exact_identities_lengths_and_version() {
        let original = binding();
        let mut header = [0; HEADER_BYTES];
        encode_header(original, 4, 7, &mut header).unwrap();
        assert_eq!(&header[..18], b"HLXSTATE\x02\0\x04\0\0\0\x07\0\0\0");
        let mut frame = header.to_vec();
        frame.extend_from_slice(b"toolruntime");
        assert_eq!(
            decode(&frame),
            Ok((original, b"tool".as_slice(), b"runtime".as_slice()))
        );
        for n in 0..frame.len() {
            assert!(decode(&frame[..n]).is_err(), "truncation {n}");
        }
        for offset in [0, 8, 10, 14] {
            let mut bad = frame.clone();
            bad[offset] ^= 1;
            assert!(decode(&bad).is_err(), "header {offset}");
        }
        frame.push(0);
        assert!(decode(&frame).is_err());
        assert!(encode_header(original, MAX_BYTES, 1, &mut header).is_err());
        assert!(encode_header(original, 0, 1, &mut header).is_err());
        assert!(encode_header(original, 1, 0, &mut header).is_err());
    }

    #[test]
    fn neutral_state_rejects_invalid_owner_mm_files_and_generation() {
        let good = binding();
        let other = DetTid::from_raw(42);
        let cases = [
            CutBinding {
                dettid: other,
                ..good
            },
            CutBinding {
                detpid: other,
                ..good
            },
            CutBinding {
                physical_tid: 0,
                ..good
            },
            CutBinding {
                old_mm: MmId::initial(other),
                ..good
            },
            CutBinding {
                prepared_mm: good.old_mm,
                ..good
            },
            CutBinding {
                prepared_mm: MmId::initial(other).for_exec(other),
                ..good
            },
            CutBinding {
                old_files: FilesId::initial(other),
                ..good
            },
            CutBinding {
                config_digest: [0; 32],
                ..good
            },
        ];
        for bad in cases {
            assert_eq!(bad.validate(), Err(Error::Identity));
        }
        // Exact legacy data decode supplies an overflow value, not authority.
        let mut wire = bincode::serde::encode_to_vec(good, bincode::config::legacy()).unwrap();
        wire[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
        let (bad, used): (CutBinding, usize) =
            bincode::serde::decode_from_slice(&wire, bincode::config::legacy()).unwrap();
        assert_eq!(used, BINDING_BYTES);
        assert_eq!(bad.validate(), Err(Error::Identity));
        let mut wire = bincode::serde::encode_to_vec(good, bincode::config::legacy()).unwrap();
        wire[28..36].copy_from_slice(&u64::MAX.to_le_bytes());
        let (bad, used): (CutBinding, usize) =
            bincode::serde::decode_from_slice(&wire, bincode::config::legacy()).unwrap();
        assert_eq!(used, BINDING_BYTES);
        assert_eq!(bad.validate(), Err(Error::Identity));
    }
}
