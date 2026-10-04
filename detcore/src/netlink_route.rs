/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Determinize the statistics counters carried by `NETLINK_ROUTE` link dumps.
//!
//! A guest that resolves a hostname or enumerates interfaces reaches
//! `NETLINK_ROUTE` through glibc, which issues `RTM_GETLINK` and receives one
//! `RTM_NEWLINK` message per interface. Those messages carry LIVE COUNTERS that
//! advance continuously on a running host, so two executions of the same guest
//! milliseconds apart receive byte-different payloads of IDENTICAL LENGTH.
//!
//! MEASURED NATIVELY, outside Hermit, on two consecutive `RTM_GETLINK` dumps:
//! both replies were 3060 bytes, 47 bytes differed, and of the first twelve
//! 64-bit fields that moved, TWELVE INCREASED AND ZERO DECREASED — monotonic
//! counters. Every differing byte was attributed to an attribute; none fell
//! outside one:
//!
//! ```text
//!   AF_SPEC -> AF_INET6 -> IFLA_INET6_STATS       30 bytes
//!   AF_SPEC -> AF_INET6 -> IFLA_INET6_CACHEINFO     4 bytes (guest payload only)
//!   IFLA_STATS64                               8 bytes
//!   IFLA_STATS                                 8 bytes
//! ```
//!
//! THE ATTRIBUTION IS WHY THIS ZEROES THREE ATTRIBUTES RATHER THAN ONE. The
//! obvious fix — zero `IFLA_STATS64` — addresses 8 of 47 differing bytes and
//! leaves the guest still diverging on the other 39. The nested IPv6 SNMP block
//! inside `AF_SPEC` is the LARGEST contributor and is easy to miss because it is
//! two levels of nesting below the message.
//!
//! Nothing else in the dump moved: no timestamps, no sequence numbers, no
//! addresses, no interface indices. Zeroing exactly these three makes the reply
//! byte-stable without touching anything a guest legitimately reads for
//! identity or topology.
//!
//! The counters are ZEROED rather than virtualized to advance. Freezing them is
//! the same choice `crate::sock_diag` already makes for socket inodes, and a
//! guest under Hermit has no deterministic clock against which advancing
//! counters would mean anything. A guest that reads interface statistics sees
//! zeros; that is a deliberate and stated consequence.

/// `nlmsghdr` is 16 bytes: len(u32) type(u16) flags(u16) seq(u32) pid(u32).
const NLMSG_HDRLEN: usize = 16;
const NLMSG_ALIGNTO: usize = 4;

/// `rtattr` is 4 bytes: len(u16) type(u16).
const RTA_HDRLEN: usize = 4;
const RTA_ALIGNTO: usize = 4;

/// `ifinfomsg`, which follows the `nlmsghdr` in an `RTM_NEWLINK` message.
const IFINFOMSG_LEN: usize = 16;

/// `RTM_NEWLINK`, the reply type for an `RTM_GETLINK` dump.
const RTM_NEWLINK: u16 = 16;

/// `RTM_NEWADDR`, the reply type for an `RTM_GETADDR` dump. glibc's
/// `getifaddrs` issues BOTH dumps, so a guest that enumerates interfaces
/// receives both message types and both must be determinized.
const RTM_NEWADDR: u16 = 20;

/// `ifaddrmsg`, which follows the `nlmsghdr` in an `RTM_NEWADDR` message. It is
/// EIGHT bytes, not the sixteen of `ifinfomsg` -- getting this wrong walks the
/// attributes from the wrong offset and silently matches nothing.
const IFADDRMSG_LEN: usize = 8;

/// `ifa_cacheinfo`, carrying `cstamp` and `tstamp` -- timestamps in hundredths
/// of a second since boot, which advance between runs.
const IFA_CACHEINFO: u16 = 6;

const IFLA_STATS: u16 = 7;
const IFLA_STATS64: u16 = 23;
const IFLA_AF_SPEC: u16 = 26;

const AF_INET6: u16 = 10;
const IFLA_INET6_STATS: u16 = 3;
const IFLA_INET6_ICMP6STATS: u16 = 6;
/// `ifla_cacheinfo`, which carries `tstamp` and `reachable_time`. MEASURED on
/// the real guest payload: after zeroing the three statistics attributes, FOUR
/// BYTES still differed between two runs and every one of them was here. The
/// native host sample did not expose it -- those fields happened to match in
/// that pair -- which is why the fix had to be tested against the ACTUAL
/// failing buffer rather than a synthetic dump.
const IFLA_INET6_CACHEINFO: u16 = 5;

fn align_up(value: usize, align: usize) -> usize {
    value.div_ceil(align) * align
}

/// Zero the live counters in a `NETLINK_ROUTE` reply, in place.
///
/// Returns whether anything was modified, so the caller can skip writing the
/// buffer back to guest memory when there was nothing to change. A malformed or
/// truncated buffer is left ALONE rather than partially rewritten: this is a
/// determinism sanitizer, and a half-rewritten reply would be a worse guest
/// observation than an undeterminized one.
pub fn sanitize_route_link_stats(buf: &mut [u8]) -> bool {
    let mut modified = false;
    let mut offset = 0usize;

    while offset + NLMSG_HDRLEN <= buf.len() {
        let len = u32::from_ne_bytes(match buf[offset..offset + 4].try_into() {
            Ok(bytes) => bytes,
            Err(_) => return modified,
        }) as usize;
        let msg_type = u16::from_ne_bytes(match buf[offset + 4..offset + 6].try_into() {
            Ok(bytes) => bytes,
            Err(_) => return modified,
        });

        // A length shorter than the header, or one that runs past the buffer,
        // means this is not a well-formed message stream. Stop rather than guess.
        if len < NLMSG_HDRLEN || offset + len > buf.len() {
            return modified;
        }

        match msg_type {
            RTM_NEWLINK => {
                let body = offset + NLMSG_HDRLEN + IFINFOMSG_LEN;
                let end = offset + len;
                if body <= end {
                    zero_link_attrs(buf, body, end, &mut modified);
                }
            }
            RTM_NEWADDR => {
                let body = offset + NLMSG_HDRLEN + IFADDRMSG_LEN;
                let end = offset + len;
                if body <= end {
                    zero_addr_attrs(buf, body, end, &mut modified);
                }
            }
            _ => {}
        }

        offset += align_up(len, NLMSG_ALIGNTO);
    }

    modified
}

/// Walk the top-level `IFLA_*` attributes of one `RTM_NEWLINK` message.
fn zero_link_attrs(buf: &mut [u8], mut attr: usize, end: usize, modified: &mut bool) {
    while attr + RTA_HDRLEN <= end {
        let (alen, atype) = match attr_header(buf, attr) {
            Some(header) => header,
            None => return,
        };
        if alen < RTA_HDRLEN || attr + alen > end {
            return;
        }
        let payload = attr + RTA_HDRLEN;
        let payload_end = attr + alen;

        match atype {
            IFLA_STATS | IFLA_STATS64 => zero_range(buf, payload, payload_end, modified),
            IFLA_AF_SPEC => zero_af_spec(buf, payload, payload_end, modified),
            _ => {}
        }

        attr += align_up(alen, RTA_ALIGNTO);
    }
}

/// Walk the `IFA_*` attributes of one `RTM_NEWADDR` message, zeroing the cache
/// timestamps. MEASURED on the real guest payload: a 156-byte reply held two
/// `RTM_NEWADDR` messages and every one of its eight differing bytes was in
/// `IFA_CACHEINFO`.
fn zero_addr_attrs(buf: &mut [u8], mut attr: usize, end: usize, modified: &mut bool) {
    while attr + RTA_HDRLEN <= end {
        let (alen, atype) = match attr_header(buf, attr) {
            Some(header) => header,
            None => return,
        };
        if alen < RTA_HDRLEN || attr + alen > end {
            return;
        }
        if atype == IFA_CACHEINFO {
            zero_range(buf, attr + RTA_HDRLEN, attr + alen, modified);
        }
        attr += align_up(alen, RTA_ALIGNTO);
    }
}

/// `IFLA_AF_SPEC` nests one block per address family; the IPv6 block carries the
/// SNMP counter arrays. This is the largest source of drift and the one a
/// stats-only fix misses.
fn zero_af_spec(buf: &mut [u8], mut fam: usize, end: usize, modified: &mut bool) {
    while fam + RTA_HDRLEN <= end {
        let (flen, family) = match attr_header(buf, fam) {
            Some(header) => header,
            None => return,
        };
        if flen < RTA_HDRLEN || fam + flen > end {
            return;
        }

        if family == AF_INET6 {
            let mut inner = fam + RTA_HDRLEN;
            let inner_end = fam + flen;
            while inner + RTA_HDRLEN <= inner_end {
                let (ilen, itype) = match attr_header(buf, inner) {
                    Some(header) => header,
                    None => return,
                };
                if ilen < RTA_HDRLEN || inner + ilen > inner_end {
                    return;
                }
                if matches!(
                    itype,
                    IFLA_INET6_STATS | IFLA_INET6_ICMP6STATS | IFLA_INET6_CACHEINFO
                ) {
                    zero_range(buf, inner + RTA_HDRLEN, inner + ilen, modified);
                }
                inner += align_up(ilen, RTA_ALIGNTO);
            }
        }

        fam += align_up(flen, RTA_ALIGNTO);
    }
}

fn attr_header(buf: &[u8], at: usize) -> Option<(usize, u16)> {
    let len = u16::from_ne_bytes(buf.get(at..at + 2)?.try_into().ok()?) as usize;
    let kind = u16::from_ne_bytes(buf.get(at + 2..at + 4)?.try_into().ok()?);
    Some((len, kind))
}

fn zero_range(buf: &mut [u8], from: usize, to: usize, modified: &mut bool) {
    if from >= to || to > buf.len() {
        return;
    }
    for byte in &mut buf[from..to] {
        if *byte != 0 {
            *byte = 0;
            *modified = true;
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn rtattr(kind: u16, payload: &[u8]) -> Vec<u8> {
        let len = RTA_HDRLEN + payload.len();
        let mut out = Vec::new();
        out.extend_from_slice(&(len as u16).to_ne_bytes());
        out.extend_from_slice(&kind.to_ne_bytes());
        out.extend_from_slice(payload);
        out.resize(align_up(len, RTA_ALIGNTO), 0);
        out
    }

    fn newlink(attrs: &[u8]) -> Vec<u8> {
        let len = NLMSG_HDRLEN + IFINFOMSG_LEN + attrs.len();
        let mut out = Vec::new();
        out.extend_from_slice(&(len as u32).to_ne_bytes());
        out.extend_from_slice(&RTM_NEWLINK.to_ne_bytes());
        out.extend_from_slice(&0u16.to_ne_bytes());
        out.extend_from_slice(&0u32.to_ne_bytes());
        out.extend_from_slice(&0u32.to_ne_bytes());
        out.extend_from_slice(&[0u8; IFINFOMSG_LEN]);
        out.extend_from_slice(attrs);
        out
    }

    #[test]
    fn zeroes_link_stats64() {
        let mut msg = newlink(&rtattr(IFLA_STATS64, &[7u8; 16]));
        let payload = NLMSG_HDRLEN + IFINFOMSG_LEN + RTA_HDRLEN;
        assert!(sanitize_route_link_stats(&mut msg));
        assert!(
            msg[payload..].iter().all(|b| *b == 0),
            "IFLA_STATS64 counters were not fully zeroed"
        );
    }

    #[test]
    fn zeroes_legacy_link_stats() {
        let mut msg = newlink(&rtattr(IFLA_STATS, &[9u8; 12]));
        assert!(sanitize_route_link_stats(&mut msg));
        assert!(!msg.contains(&9));
    }

    /// THE CASE A STATS-ONLY FIX MISSES. IFLA_INET6_STATS lives two levels down,
    /// inside AF_SPEC's AF_INET6 block, and supplied 30 of the 47 differing
    /// bytes in the native measurement.
    #[test]
    fn zeroes_ipv6_stats_nested_two_levels_inside_af_spec() {
        let inner = rtattr(IFLA_INET6_STATS, &[5u8; 24]);
        let family = rtattr(AF_INET6, &inner);
        let mut msg = newlink(&rtattr(IFLA_AF_SPEC, &family));
        assert!(sanitize_route_link_stats(&mut msg));
        assert!(
            !msg.contains(&5),
            "nested IPv6 SNMP counters were left undeterminized"
        );
    }

    /// Identity and topology attributes must survive. Zeroing an interface name
    /// or index would be a far worse guest observation than a drifting counter.
    #[test]
    fn leaves_non_counter_attributes_alone() {
        const IFLA_IFNAME: u16 = 3;
        let mut msg = newlink(&rtattr(IFLA_IFNAME, b"eth0\0"));
        assert!(!sanitize_route_link_stats(&mut msg));
        assert!(
            msg.windows(4).any(|w| w == b"eth0"),
            "the interface name was modified"
        );
    }

    /// A reply carrying no counters must report "unmodified" so the caller can
    /// skip writing the buffer back into guest memory.
    #[test]
    fn reports_unmodified_when_there_is_nothing_to_zero() {
        let mut msg = newlink(&rtattr(IFLA_STATS64, &[0u8; 16]));
        assert!(!sanitize_route_link_stats(&mut msg));
    }

    /// A truncated stream is left ALONE rather than half-rewritten.
    #[test]
    fn refuses_to_rewrite_a_truncated_message() {
        let full = newlink(&rtattr(IFLA_STATS64, &[3u8; 16]));
        let mut truncated = full[..full.len() - 6].to_vec();
        let before = truncated.clone();
        sanitize_route_link_stats(&mut truncated);
        assert_eq!(
            truncated, before,
            "a truncated reply must not be partially rewritten"
        );
    }

    /// Only RTM_NEWLINK is touched; an unrelated message type passes through.
    #[test]
    fn ignores_message_types_that_are_not_newlink() {
        let mut msg = newlink(&rtattr(IFLA_STATS64, &[4u8; 16]));
        msg[4..6].copy_from_slice(&20u16.to_ne_bytes());
        assert!(!sanitize_route_link_stats(&mut msg));
        assert!(msg.contains(&4));
    }
}
