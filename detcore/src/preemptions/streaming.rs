/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Buffered JSON transport without a complete raw payload or output String.
//! The typed history itself still owns its rows.

use std::fmt;
use std::fs::File;
use std::io::BufReader;
use std::io::BufWriter;
use std::io::Read;
use std::io::Write;
use std::io::{self};
use std::path::Path;

use anyhow::Context;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::Digest;
use sha2::Sha256;

struct Fingerprinted<R> {
    inner: R,
    digest: Sha256,
}

impl<R: Read> Read for Fingerprinted<R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(bytes)?;
        self.digest.update(&bytes[..count]);
        Ok(count)
    }
}

/// `end` is required even after typed deserialization: trailing data, I/O
/// failures and all trailing whitespace belong to the complete input pass.
pub(super) fn parse_pass<T: DeserializeOwned, R: Read>(input: R) -> anyhow::Result<(T, [u8; 32])> {
    let mut input = BufReader::new(Fingerprinted {
        inner: input,
        digest: Sha256::new(),
    });
    let mut deserializer = serde_json::Deserializer::from_reader(&mut input);
    let value = T::deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok((value, input.into_inner().digest.finalize().into()))
}

/// Preserve callers that historically read a String: IgnoredAny can skip
/// invalid UTF-8 in an unknown field, whereas read_to_string rejected it.
pub(super) fn parse_utf8_pass<T: DeserializeOwned, R: Read>(
    input: R,
) -> anyhow::Result<(T, [u8; 32])> {
    struct Utf8<R> {
        input: R,
        pending: [u8; 4],
        len: usize,
    }
    impl<R: Read> Read for Utf8<R> {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            if bytes.is_empty() {
                return Ok(0);
            }
            let count = self.input.read(bytes)?;
            let invalid =
                || io::Error::new(io::ErrorKind::InvalidData, "invalid UTF-8 history input");
            if count == 0 && self.len != 0 {
                return Err(invalid());
            }
            let mut remaining = &bytes[..count];
            while self.len > 0 && !remaining.is_empty() {
                self.pending[self.len] = remaining[0];
                self.len += 1;
                remaining = &remaining[1..];
                match std::str::from_utf8(&self.pending[..self.len]) {
                    Ok(_) => self.len = 0,
                    Err(error) if error.error_len().is_none() && self.len < 4 => {}
                    Err(_) => return Err(invalid()),
                }
            }
            if let Err(error) = std::str::from_utf8(remaining) {
                if error.error_len().is_some() {
                    return Err(invalid());
                }
                let suffix = &remaining[error.valid_up_to()..];
                self.len = suffix.len();
                self.pending[..self.len].copy_from_slice(suffix);
            }
            Ok(count)
        }
    }
    parse_pass(Utf8 {
        input,
        pending: [0; 4],
        len: 0,
    })
}

pub(super) fn write_json<T: Serialize + ?Sized, W: Write>(
    output: &mut W,
    value: &T,
) -> anyhow::Result<()> {
    serde_json::to_writer(&mut *output, value)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

/// Serialization errors must not truncate an existing destination. A sink
/// preflight costs another pass, but needs no complete output allocation and
/// retains descriptor-path support (no rename, seek or temporary destination).
pub(super) fn write_file<T: Serialize + ?Sized>(path: &Path, value: &T) -> anyhow::Result<()> {
    serde_json::to_writer(io::sink(), value)?;
    let mut file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
    write_buffered(&mut file, value).with_context(|| format!("writing {}", path.display()))
}

fn write_buffered<T: Serialize + ?Sized, W: Write>(
    output: &mut W,
    value: &T,
) -> anyhow::Result<()> {
    let mut buffered = BufWriter::new(output);
    let result = write_json(&mut buffered, value);
    // Never let BufWriter's Drop retry a failed flush or perform unobserved
    // writes after the consuming result. A partial artifact remains failed.
    let _ = buffered.into_parts();
    result
}

/// Preserve full-JSON Display compatibility without an intermediate String.
/// A UTF-8 code point may straddle serializer writes, so retain at most its
/// four bytes rather than assuming every Write call is a complete &str.
pub(super) fn fmt_json<T: Serialize + ?Sized>(
    value: &T,
    output: &mut fmt::Formatter<'_>,
    newline: bool,
) -> fmt::Result {
    struct Output<'a, 'b> {
        formatter: &'a mut fmt::Formatter<'b>,
        pending: [u8; 4],
        len: usize,
    }
    impl Write for Output<'_, '_> {
        fn write(&mut self, mut bytes: &[u8]) -> io::Result<usize> {
            let count = bytes.len();
            let invalid =
                || io::Error::new(io::ErrorKind::InvalidData, "invalid UTF-8 JSON output");
            let failed = || io::Error::other("JSON formatter failed");
            while self.len > 0 && !bytes.is_empty() {
                self.pending[self.len] = bytes[0];
                self.len += 1;
                bytes = &bytes[1..];
                match std::str::from_utf8(&self.pending[..self.len]) {
                    Ok(text) => {
                        self.formatter.write_str(text).map_err(|_| failed())?;
                        self.len = 0;
                    }
                    Err(error) if error.error_len().is_none() && self.len < 4 => {}
                    Err(_) => return Err(invalid()),
                }
            }
            if !bytes.is_empty() {
                match std::str::from_utf8(bytes) {
                    Ok(text) => self.formatter.write_str(text).map_err(|_| failed())?,
                    Err(error) => {
                        let valid = error.valid_up_to();
                        self.formatter
                            .write_str(std::str::from_utf8(&bytes[..valid]).map_err(|_| invalid())?)
                            .map_err(|_| failed())?;
                        if error.error_len().is_some() || bytes.len() - valid >= 4 {
                            return Err(invalid());
                        }
                        self.len = bytes.len() - valid;
                        self.pending[..self.len].copy_from_slice(&bytes[valid..]);
                    }
                }
            }
            Ok(count)
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.len != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "incomplete UTF-8 JSON output",
                ));
            }
            Ok(())
        }
    }
    let mut output = Output {
        formatter: output,
        pending: [0; 4],
        len: 0,
    };
    serde_json::to_writer(&mut output, value).map_err(|_| fmt::Error)?;
    if newline {
        output.write_all(b"\n").map_err(|_| fmt::Error)?;
    }
    output.flush().map_err(|_| fmt::Error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct ShortOutput {
        bytes: Vec<u8>,
        fail_at: Option<usize>,
        fail_flush: bool,
        flushes: usize,
        writes: usize,
    }
    impl Write for ShortOutput {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            if self.fail_at == Some(self.bytes.len()) {
                return Err(io::Error::other("injected write refusal"));
            }
            let count = bytes.len().min(3).min(
                self.fail_at
                    .map_or(usize::MAX, |end| end - self.bytes.len()),
            );
            self.bytes.extend_from_slice(&bytes[..count]);
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            if self.fail_flush {
                Err(io::Error::other("injected flush refusal"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn actual_stream_writer_preserves_bytes_and_propagates_write_and_flush_refusals() {
        let value = ["UTF-8: hé🦀", "newline\nquote\"slash\\"];
        let mut expected = serde_json::to_vec(&value).unwrap();
        expected.push(b'\n');
        let mut output = ShortOutput::default();
        write_json(&mut output, &value).unwrap();
        assert_eq!(output.bytes, expected);
        assert_eq!(output.flushes, 1);
        let mut output = ShortOutput {
            fail_at: Some(0),
            ..Default::default()
        };
        assert!(write_buffered(&mut output, &value).is_err());
        assert_eq!(
            output.writes, 1,
            "a failed buffered write must not retry on Drop"
        );
        for fail_at in 0..expected.len() {
            let mut output = ShortOutput {
                fail_at: Some(fail_at),
                ..Default::default()
            };
            assert!(write_json(&mut output, &value).is_err());
            assert_eq!(output.bytes, expected[..fail_at]);
            assert_eq!(output.flushes, 0);
        }
        let mut output = ShortOutput {
            fail_flush: true,
            ..Default::default()
        };
        assert!(write_json(&mut output, &value).is_err());
        assert_eq!(output.bytes, expected);
        assert_eq!(output.flushes, 1);
        let mut output = ShortOutput {
            fail_flush: true,
            ..Default::default()
        };
        assert!(write_buffered(&mut output, &value).is_err());
        assert_eq!(output.bytes, expected);
        assert_eq!(output.flushes, 1);
        struct Display<'a>(&'a [&'a str]);
        impl fmt::Display for Display<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt_json(self.0, f, true)
            }
        }
        assert_eq!(Display(&value).to_string().as_bytes(), expected);
        struct RefusedFormatter;
        impl fmt::Write for RefusedFormatter {
            fn write_str(&mut self, _: &str) -> fmt::Result {
                Err(fmt::Error)
            }
        }
        use fmt::Write as _;
        assert!(write!(&mut RefusedFormatter, "{}", Display(&value)).is_err());
    }

    #[test]
    fn serialization_refusal_does_not_overwrite_an_existing_destination() {
        struct Refused;
        impl Serialize for Refused {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("injected serialization refusal"))
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("retained");
        std::fs::write(&path, b"retained original").unwrap();
        assert!(write_file(&path, &Refused).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"retained original");
    }

    #[test]
    fn a_read_error_after_the_json_value_cannot_be_mistaken_for_eof() {
        struct RefusedEof {
            bytes: std::io::Cursor<&'static [u8]>,
        }
        impl Read for RefusedEof {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                if self.bytes.position() == self.bytes.get_ref().len() as u64 {
                    return Err(io::Error::other("injected EOF read refusal"));
                }
                self.bytes.read(bytes)
            }
        }
        let input = RefusedEof {
            bytes: std::io::Cursor::new(b"{}".as_slice()),
        };
        assert!(parse_pass::<std::collections::BTreeMap<String, bool>, _>(input).is_err());
    }

    #[test]
    fn string_reader_utf8_validation_spans_reads_and_rejects_ignored_invalid_bytes() {
        struct OneByte<'a>(std::io::Cursor<&'a [u8]>);
        impl Read for OneByte<'_> {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                let count = bytes.len().min(1);
                self.0.read(&mut bytes[..count])
            }
        }
        let bytes = "{\"text\":\"hé🦀\"}".as_bytes();
        let (actual, _) = parse_utf8_pass::<std::collections::BTreeMap<String, String>, _>(
            OneByte(std::io::Cursor::new(bytes)),
        )
        .unwrap();
        assert_eq!(actual["text"], "hé🦀");
        #[derive(serde::Deserialize)]
        struct IgnoreUnknown {}
        for bytes in [
            b"{\"extra\":\"\xff\"}".as_slice(),
            b"{\"extra\":\"\xf0\x9f\"}".as_slice(),
        ] {
            assert!(
                parse_utf8_pass::<IgnoreUnknown, _>(OneByte(std::io::Cursor::new(bytes)),).is_err()
            );
        }
    }
}
