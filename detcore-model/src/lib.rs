/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Widely-shared type definitions.

use std::fmt;
use std::str::FromStr;

use nix::errno;
use nix::sys::signal::Signal;
use serde::de;
use serde::Serialize;
use serde::Serializer;

pub mod config;
pub mod fd;
pub mod futex;
pub mod pid;
pub mod schedule;
pub mod time;

/// Simply a type to hang Serialize/Deserialize instances off of.
#[derive(PartialEq, Debug, Eq, Clone, Hash)]
pub struct SigWrapper(pub Signal);

impl Serialize for SigWrapper {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.0.as_str())
    }
}

impl<'de> de::Deserialize<'de> for SigWrapper {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        struct SignalVisitor;
        impl<'de> de::Visitor<'de> for SignalVisitor {
            type Value = Signal;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(f, "string representing a Signal")
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                let sig: Result<Signal, String> =
                    FromStr::from_str(v).map_err(|e: errno::Errno| e.to_string());
                sig.map_err(de::Error::custom)
            }
        }

        let sig = deserializer.deserialize_str(SignalVisitor)?;
        Ok(SigWrapper(sig))
    }
}
