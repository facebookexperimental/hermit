/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fmt;
use std::fs;
use std::io;
use std::ops;
use std::path::Path;
use std::str::FromStr;

use hex::FromHex;
use hex::FromHexError;
use serde::de;
use serde::de::Deserialize;
use serde::de::Deserializer;
use serde::de::Visitor;
use serde::ser::Serialize;
use serde::ser::Serializer;
use sha2::Digest as _;
use sha2::Sha256;

const DIGEST_LENGTH: usize = 32;

/// A SHA-256 content digest.
#[derive(Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Hash, Default)]
pub struct Digest([u8; DIGEST_LENGTH]);

impl Digest {
    /// Computes the digest from a byte slice.
    pub fn new(data: &[u8]) -> Self {
        Digest(Sha256::digest(data).into())
    }

    /// Computes the digest from a reader.
    pub fn digest_reader<R: io::Read>(mut reader: R) -> io::Result<Self> {
        let mut hasher = Sha256::default();
        let mut buf = [0u8; 16384];

        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }

        Ok(Digest(hasher.finalize().into()))
    }

    /// Computes the digest by reading the file at the given path.
    pub fn digest_path<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        Self::digest_reader(fs::File::open(path)?)
    }
}

impl fmt::LowerHex for Digest {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::UpperHex for Digest {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02X}")?;
        }
        Ok(())
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        <Self as fmt::LowerHex>::fmt(self, f)
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        <Self as fmt::Display>::fmt(self, f)
    }
}

impl ops::Deref for Digest {
    type Target = [u8; DIGEST_LENGTH];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Serialize for Digest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if serializer.is_human_readable() {
            serializer.serialize_str(&self.to_string())
        } else {
            serializer.serialize_bytes(self.0.as_ref())
        }
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct DigestVisitor;

        impl<'de> Visitor<'de> for DigestVisitor {
            type Value = Digest;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(f, "hex string or 32 bytes")
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                let v = <[u8; DIGEST_LENGTH]>::from_hex(v).map_err(|e| match e {
                    FromHexError::InvalidHexCharacter { c, .. } => E::invalid_value(
                        de::Unexpected::Char(c),
                        &"string with only hexadecimal characters",
                    ),
                    FromHexError::InvalidStringLength => {
                        E::invalid_length(v.len(), &"hex string with a valid length")
                    }
                    FromHexError::OddLength => {
                        E::invalid_length(v.len(), &"hex string with an even length")
                    }
                })?;
                Ok(Digest(v))
            }

            fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if v.len() != DIGEST_LENGTH {
                    return Err(E::invalid_length(v.len(), &"32 bytes"));
                }
                let mut inner = [0; DIGEST_LENGTH];
                inner.copy_from_slice(v);
                Ok(Digest(inner))
            }
        }

        if deserializer.is_human_readable() {
            deserializer.deserialize_str(DigestVisitor)
        } else {
            deserializer.deserialize_bytes(DigestVisitor)
        }
    }
}

impl FromHex for Digest {
    type Error = FromHexError;

    fn from_hex<T: AsRef<[u8]>>(hex: T) -> Result<Self, Self::Error> {
        <[u8; DIGEST_LENGTH]>::from_hex(hex).map(Digest)
    }
}

impl AsRef<[u8]> for Digest {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl FromStr for Digest {
    type Err = FromHexError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Digest::from_hex(s)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_from_hex() {
        let s = "b1fbeefc23e6a149a6f7d0c2fb635bfc78f7ddc2da963ea9c6a63eb324260e6d";
        assert_eq!(Digest::from_str(s).unwrap().to_string(), s);
    }

    /// Known-answer test pinning what `Digest::new` actually computes.
    ///
    /// The root `Cargo.toml` builds `sha2` at `opt-level = 3` even in a debug
    /// build, because an unoptimised SHA-256 is 34.6x slower and dominates
    /// `--detlog-io-buffers`. That is a build-configuration change, and the
    /// hazard with build-configuration changes is that they are assumed not to
    /// affect behaviour rather than shown not to. This pins the output against
    /// the published SHA-256 vectors, so it fails if the digest of a fixed
    /// input ever moves -- whatever the reason, optimisation or otherwise.
    ///
    /// Values are the standard NIST/FIPS-180 vectors for "" and "abc", not
    /// numbers copied back out of a previous run of this code.
    #[test]
    fn digest_is_independent_of_optimisation_level() {
        assert_eq!(
            Digest::new(b"").to_string(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            Digest::new(b"abc").to_string(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // A multi-block input, so the compress loop runs more than once: the
        // optimised and unoptimised builds take different code paths through
        // it, and a single-block vector would not exercise that.
        let long = vec![0x61u8; 1_000_000];
        assert_eq!(
            Digest::new(&long).to_string(),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }
}
