//! The ordinary shared engine's existing DETLOG policy decoder.
//!
//! This parses configuration only. Descriptor identity, reserved transport,
//! publication and Tool installation stay in the real loader owner.

use std::ffi::OsStr;

use detcore::detlog::ForwardPolicy;

pub(super) fn forwarding_policy(
    requested: Option<&OsStr>,
) -> Result<Option<ForwardPolicy>, String> {
    requested
        .map(|value| {
            value
                .to_str()
                .ok_or_else(|| "it is not UTF-8".to_owned())
                .and_then(ForwardPolicy::decode)
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStrExt;

    use super::*;

    #[test]
    fn detlog_forwarding_follows_the_coordinators_per_target_policy() {
        assert_eq!(forwarding_policy(None), Ok(None));
        // The historical value still forwards everything.
        assert_eq!(
            forwarding_policy(Some(OsStr::new("1"))),
            Ok(Some(ForwardPolicy::all()))
        );
        let scoped = forwarding_policy(Some(OsStr::new("0,detcore::random=1")))
            .unwrap()
            .unwrap();
        assert!(scoped.forwards("detcore::random"));
        assert!(!scoped.forwards("detcore::tool_local"));
        assert!(forwarding_policy(Some(OsStr::new("yes"))).is_err());
        assert!(forwarding_policy(Some(OsStr::from_bytes(b"\xff"))).is_err());
    }
}
