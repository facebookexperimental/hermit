/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! What Hermit tells the in-guest LiteInst runtime at launch, carried in the
//! sealed preload bootstrap's tool data (`reverie_inguest::bootstrap`) rather
//! than in the guest's environment.
//!
//! The kernel copies a launch environment onto the guest's initial stack, so a
//! value there shifts every stack address and stays readable in
//! `/proc/self/environ` even after the runtime removes it. The bootstrap is a
//! sealed memfd the runtime reads and closes before the guest's code runs.

/// Identifies this record, and its version, at the start of the tool data.
const MAGIC: &[u8] = b"HERMIT-LITEINST-LAUNCH-V1\0";

const FORWARD: &str = "forward";
const POLICY: &str = "policy";
const FINGERPRINT: &str = "fingerprint";

/// Hermit's launch settings for one in-guest LiteInst run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InGuestLaunch {
    /// The DETLOG forwarding socket, as
    /// [`crate::detlog::tool_output_env_value`] writes it.
    pub forward_request: Option<String>,
    /// The forwarding policy, [`crate::detlog::ForwardPolicy::encode`].
    pub forward_policy: Option<String>,
    /// This build's [`crate::config_wire_fingerprint`].
    pub coordinator_fingerprint: Option<String>,
}

impl InGuestLaunch {
    /// The tool-data bytes: the magic, then `name=value` entries, each ended
    /// by a NUL byte. Values never contain NUL.
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = MAGIC.to_vec();
        for (name, value) in [
            (FORWARD, &self.forward_request),
            (POLICY, &self.forward_policy),
            (FINGERPRINT, &self.coordinator_fingerprint),
        ] {
            if let Some(value) = value {
                assert!(!value.contains('\0'), "{name} contains a NUL byte");
                bytes.extend_from_slice(name.as_bytes());
                bytes.push(b'=');
                bytes.extend_from_slice(value.as_bytes());
                bytes.push(0);
            }
        }
        bytes
    }

    /// Reads [`Self::encode`]'s bytes. Refuses another record, an unknown or
    /// repeated name, and an unterminated entry: a runtime that cannot read
    /// what Hermit sent must say so rather than run with less.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let entries = bytes
            .strip_prefix(MAGIC)
            .ok_or("the tool data is not a Hermit LiteInst launch record")?;
        let mut launch = Self::default();
        let mut rest = entries;
        while !rest.is_empty() {
            let end = rest
                .iter()
                .position(|byte| *byte == 0)
                .ok_or("a launch setting is not NUL-terminated")?;
            let entry = std::str::from_utf8(&rest[..end])
                .map_err(|_| "a launch setting is not UTF-8".to_owned())?;
            rest = &rest[end + 1..];
            let (name, value) = entry
                .split_once('=')
                .ok_or_else(|| format!("launch setting {entry:?} is not name=value"))?;
            let slot = match name {
                FORWARD => &mut launch.forward_request,
                POLICY => &mut launch.forward_policy,
                FINGERPRINT => &mut launch.coordinator_fingerprint,
                _ => return Err(format!("unknown launch setting {name:?}")),
            };
            if slot.replace(value.to_owned()).is_some() {
                return Err(format!("launch setting {name:?} is repeated"));
            }
        }
        Ok(launch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_launch_record_round_trips() {
        for launch in [
            InGuestLaunch::default(),
            InGuestLaunch {
                forward_request: Some("7:12345:67890".to_owned()),
                forward_policy: Some("info,detcore::random=warn".to_owned()),
                coordinator_fingerprint: Some("9349c85b2c48fc92".to_owned()),
            },
            InGuestLaunch {
                coordinator_fingerprint: Some("0123456789abcdef".to_owned()),
                ..Default::default()
            },
        ] {
            assert_eq!(InGuestLaunch::decode(&launch.encode()), Ok(launch));
        }
    }

    #[test]
    fn a_launch_record_refuses_what_it_cannot_read() {
        let mut repeated = MAGIC.to_vec();
        repeated.extend_from_slice(b"policy=a\0policy=b\0");
        let mut unknown = MAGIC.to_vec();
        unknown.extend_from_slice(b"colour=blue\0");
        let mut unterminated = MAGIC.to_vec();
        unterminated.extend_from_slice(b"policy=a");
        for bytes in [&b"REVERIE-LI-V1"[..], &repeated, &unknown, &unterminated] {
            assert!(InGuestLaunch::decode(bytes).is_err(), "{bytes:?}");
        }
    }
}
