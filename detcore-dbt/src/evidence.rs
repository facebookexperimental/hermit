/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Hermit's tracing-record policy for protected DBT evidence.

use std::io;

use crate::DBT_LOG_RECORD_PREFIX;

const MAX_RECORD_BYTES: usize = 1024 * 1024;

/// Validates one canonical Hermit tracing record before it enters the evidence.
///
/// Exactly one terminal newline prevents a guest-controlled field from
/// synthesizing another parser-visible INFO record. Reverie separately checks
/// authenticated transport, framing, lifecycle, counts, and digests.
pub fn validate_evidence_record(record: &[u8]) -> io::Result<()> {
    if record.is_empty()
        || record.len() > MAX_RECORD_BYTES
        || record.last() != Some(&b'\n')
        || record[..record.len() - 1]
            .iter()
            .any(|byte| matches!(*byte, b'\r' | b'\n'))
    {
        return Err(invalid_data(
            "DBT evidence record is not exactly one terminal-newline tracing record",
        ));
    }
    let tagged = record
        .strip_prefix(DBT_LOG_RECORD_PREFIX.as_bytes())
        .ok_or_else(|| invalid_data("DBT evidence record has no canonical timestamp prefix"))?;
    if ![
        b"ERROR ".as_slice(),
        b"WARN ",
        b"INFO ",
        b"DEBUG ",
        b"TRACE ",
    ]
    .iter()
    .any(|prefix| tagged.starts_with(prefix))
    {
        return Err(invalid_data("DBT evidence record has no tracing level tag"));
    }
    Ok(())
}

/// Decodes protected DBT evidence using Hermit's collection-time record policy.
pub fn decode_evidence(bytes: &[u8]) -> io::Result<reverie_dbt::DbtEvidence> {
    reverie_dbt::decode_evidence_with_validator(bytes, validate_evidence_record)
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    const INITIALIZATION: &[u8] =
        b"1970-01-01T00:00:00.000000Z INFO reverie_dbt::evidence: protected evidence initialized\n";

    // Construct the public artifact format independently of the collector.
    // The generic decoder below must accept it before the Hermit policy is
    // tested, so framing failures cannot masquerade as policy rejection.
    fn artifact(records: &[&[u8]]) -> Vec<u8> {
        let mut payload = Vec::new();
        let mut bytes = 0_u64;
        for record in records {
            payload.extend_from_slice(&(record.len() as u32).to_le_bytes());
            payload.extend_from_slice(record);
            bytes += record.len() as u64;
        }
        let hash = payload
            .iter()
            .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
                (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
            });
        let mut output = b"RVDBTEF1".to_vec();
        output.extend_from_slice(&1_u16.to_le_bytes());
        output.extend_from_slice(&40_u16.to_le_bytes());
        output.extend_from_slice(&0_u32.to_le_bytes());
        output.extend_from_slice(&(records.len() as u64).to_le_bytes());
        output.extend_from_slice(&bytes.to_le_bytes());
        output.extend_from_slice(&hash.to_le_bytes());
        output.extend_from_slice(&payload);
        output
    }

    #[test]
    fn canonical_evidence_accepts_the_existing_tracing_language() {
        for level in ["ERROR", "WARN", "INFO", "DEBUG", "TRACE"] {
            let record = format!("{DBT_LOG_RECORD_PREFIX}{level} detcore: value=1\n");
            validate_evidence_record(record.as_bytes()).unwrap();
            let decoded = decode_evidence(&artifact(&[INITIALIZATION, record.as_bytes()])).unwrap();
            assert_eq!(decoded.records(), [record.as_bytes()]);
        }
        validate_evidence_record(INITIALIZATION).unwrap();
        // The old grammar constrained framing and tags, not UTF-8 fields.
        let binary_field = b"1970-01-01T00:00:00.000000Z INFO detcore: \0\xff\n";
        validate_evidence_record(binary_field).unwrap();
        assert_eq!(
            decode_evidence(&artifact(&[INITIALIZATION, binary_field]))
                .unwrap()
                .records(),
            [binary_field.as_slice()]
        );
        let mut largest = format!("{DBT_LOG_RECORD_PREFIX}INFO ").into_bytes();
        largest.resize(MAX_RECORD_BYTES - 1, b'x');
        largest.push(b'\n');
        validate_evidence_record(&largest).unwrap();
        largest.insert(largest.len() - 1, b'x');
        assert!(validate_evidence_record(&largest).is_err());
    }

    #[test]
    fn canonical_evidence_keeps_all_tracing_rejections() {
        assert!(validate_evidence_record(b"").is_err());
        for record in [
            b"INFO detcore: no canonical prefix\n".as_slice(),
            b"1970-01-01T00:00:00.000000Z INFO detcore: no newline",
            b"1970-01-01T00:00:00.000000Z INFO detcore: two\nlines\n",
            b"1970-01-01T00:00:00.000000Z NOTICE detcore: unknown level\n",
            b"1970-01-01T00:00:00.000000Z INFO\n",
            b"1970-01-01T00:00:00.000000Z INFOX detcore: wrong tag\n",
            b"1970-01-01T00:00:00.000000Z info detcore: lowercase\n",
            b"1970-01-01T00:00:00.000001Z INFO detcore: different prefix\n",
            b"1970-01-01T00:00:00.000000Z INFO detcore: carriage\rreturn\n",
            b"1970-01-01T00:00:00.000000Z INFO detcore: CRLF\r\n",
            b"1970-01-01T00:00:00.000000Z INFO detcore: guest=ok\n1970-01-01T00:00:00.000000Z INFO detcore: forged\n",
            b"\0\xffrecord\npayload\r\0",
        ] {
            assert!(validate_evidence_record(record).is_err(), "{record:?}");
            let bytes = artifact(&[INITIALIZATION, record]);
            assert_eq!(reverie_dbt::decode_evidence(&bytes).unwrap().records(), [record]);
            assert!(decode_evidence(&bytes).is_err(), "{record:?}");
        }
    }
}
