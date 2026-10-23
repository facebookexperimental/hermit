/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Determinize host-assigned identities in `NETLINK_SOCK_DIAG` dump replies.
//!
//! Tools such as `ss` enumerate sockets over an `AF_NETLINK`/`NETLINK_SOCK_DIAG`
//! socket and receive a binary, multipart `nlmsghdr`-framed reply. Each
//! `SOCK_DIAG_BY_FAMILY` message carries a family-specific body whose socket
//! inode number (`udiag_ino` for `AF_UNIX`, `ndiag_ino` for `AF_NETLINK`) is
//! assigned by a host-global kernel counter and therefore differs on every run.
//! `AF_UNIX` bodies additionally carry a `UNIX_DIAG_PEER` attribute holding the
//! peer socket's inode and a host-assigned socket cookie. Those identities leak
//! into guest-visible output (for example `ss -a`), breaking `--strict
//! --verify`.
//!
//! detcore already zeroes the same inodes in the procfs *text* interfaces
//! (`/proc/net/unix`, `/proc/net/netlink`; see `crate::procfs`). This module
//! applies the identical inode zeroing to the binary `SOCK_DIAG` path. For
//! `AF_UNIX` only, it also replaces the cookie with Linux's explicit
//! `[INET_DIAG_NOCOOKIE; 2]` sentinel. Linux exact UNIX lookups require the
//! already-zeroed `udiag_ino` before checking the cookie, so this does not
//! remove a round trip that the existing inode policy supported. `AF_INET` and
//! `AF_INET6` cookies remain untouched because they select exact queries and
//! `SOCK_DESTROY`; `AF_NETLINK` cookies remain untouched because its handler is
//! dump-only and no observed failure requires changing its reply contract.
//!
//! The parser is deliberately pure and fail-open: on any framing inconsistency
//! it leaves the buffer untouched (mirroring the procfs sanitizers' fail-open
//! behavior on unknown schemas). It only rewrites fixed-width identity fields
//! in place; it never grows, shrinks, or shifts the buffer, so a partial parse
//! can never corrupt message boundaries.

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1064)

/// `sizeof(struct nlmsghdr)`.
const NLMSG_HDRLEN: usize = 16;
/// Netlink message alignment (`NLMSG_ALIGNTO`).
const NLMSG_ALIGNTO: usize = 4;
/// Routing-attribute header length (`sizeof(struct rtattr)`).
const RTA_HDRLEN: usize = 4;
/// Routing-attribute alignment (`RTA_ALIGNTO`).
const RTA_ALIGNTO: usize = 4;
/// `nlmsg_type` for a socket-diag payload message.
const SOCK_DIAG_BY_FAMILY: u16 = 20;

/// Offset of `udiag_ino` within `struct unix_diag_msg`.
const UNIX_DIAG_INO_OFFSET: usize = 4;
/// Offset of `udiag_cookie` within `struct unix_diag_msg`.
const UNIX_DIAG_COOKIE_OFFSET: usize = 8;
/// Linux's cookie-check bypass sentinel, shared by socket-diag families.
const SOCK_DIAG_NOCOOKIE: [u32; 2] = [u32::MAX; 2];
/// `sizeof(struct unix_diag_msg)` — attributes begin immediately after.
const UNIX_DIAG_MSG_LEN: usize = 16;
/// `UNIX_DIAG_PEER` attribute type (payload is the peer's `__u32` inode).
const UNIX_DIAG_PEER: u16 = 2;

/// Offset of `ndiag_ino` within `struct netlink_diag_msg`.
const NETLINK_DIAG_INO_OFFSET: usize = 16;

/// Offset of `idiag_inode` within `struct inet_diag_msg`, shared by `AF_INET`
/// and `AF_INET6`: four leading bytes, then a 48-byte `inet_diag_sockid`, then
/// `idiag_expires`, `idiag_rqueue`, `idiag_wqueue` and `idiag_uid`.
///
/// Confirmed against the installed headers rather than counted by hand --
/// `offsetof(struct inet_diag_msg, idiag_inode)` is 68 and `sizeof` is 72.
const INET_DIAG_INO_OFFSET: usize = 68;

/// Canonicalize supported host-assigned identities in a socket-diag reply.
///
/// Inodes and UNIX peer inodes are zeroed. AF_UNIX cookies are replaced with
/// Linux's explicit no-cookie sentinel; other families' cookies are preserved.
///
/// Returns `true` when `buf` was rewritten and the stream parsed consistently.
/// Returns `false` — leaving `buf` unchanged — when the bytes do not match the
/// expected netlink framing
/// (fail-open) or when every supported identity was already canonical.
pub fn sanitize_sock_diag_identities(buf: &mut [u8]) -> bool {
    match rewritten(buf) {
        Some(next) => {
            buf.copy_from_slice(&next);
            true
        }
        None => false,
    }
}

/// Round `value` up to the next multiple of `align` (a power of two).
fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

/// One exact, contiguous netlink message span (including its alignment
/// padding), so reordering whole spans preserves the buffer's total length.
struct Span {
    nlmsg_type: u16,
    nlmsg_len: usize,
    bytes: Vec<u8>,
}

/// Parse the multipart stream, rewriting identity fields and canonicalizing the
/// order of the diag messages. Returns `Some(copy)` when it parsed consistently
/// and changed something, otherwise `None` (fail-open / nothing to do).
fn rewritten(buf: &[u8]) -> Option<Vec<u8>> {
    // Split the stream into exact contiguous spans that tile `buf`; reordering
    // whole spans therefore preserves the total length exactly.
    let mut spans: Vec<Span> = Vec::new();
    let mut offset = 0usize;
    while offset < buf.len() {
        // A well-formed stream ends on a message boundary; a trailing remnant
        // too small for a header is a framing error -> fail open.
        if offset + NLMSG_HDRLEN > buf.len() {
            return None;
        }
        let nlmsg_len = u32::from_ne_bytes(buf[offset..offset + 4].try_into().ok()?) as usize;
        let nlmsg_type = u16::from_ne_bytes(buf[offset + 4..offset + 6].try_into().ok()?);
        if nlmsg_len < NLMSG_HDRLEN || offset + nlmsg_len > buf.len() {
            return None;
        }
        let advance = align_up(nlmsg_len, NLMSG_ALIGNTO);
        if advance == 0 {
            return None;
        }
        // The final message may lack trailing alignment padding.
        let span_end = (offset + advance).min(buf.len());
        spans.push(Span {
            nlmsg_type,
            nlmsg_len,
            bytes: buf[offset..span_end].to_vec(),
        });
        offset += advance;
    }

    let mut modified = false;

    // Rewrite the supported host-assigned identities in every diag message.
    for span in &mut spans {
        if span.nlmsg_type == SOCK_DIAG_BY_FAMILY {
            let end = span.nlmsg_len.min(span.bytes.len());
            if NLMSG_HDRLEN < end {
                rewrite_family_identities(&mut span.bytes, NLMSG_HDRLEN, end, &mut modified);
            }
        }
    }

    // Canonicalize the order of the diag messages, mirroring the row-sort the
    // procfs text sanitizers apply. Only when the diag messages form a
    // contiguous prefix terminated by a non-diag trailer (the normal
    // `[diag*, NLMSG_DONE]` dump shape) — this guarantees every diag message
    // keeps its full alignment padding, so reordering cannot misalign the
    // stream. Any other shape is left in original order (fail-open).
    let diag_count = spans
        .iter()
        .filter(|s| s.nlmsg_type == SOCK_DIAG_BY_FAMILY)
        .count();
    let sortable = diag_count > 1
        && diag_count < spans.len()
        && spans[..diag_count]
            .iter()
            .all(|s| s.nlmsg_type == SOCK_DIAG_BY_FAMILY);
    if sortable {
        let mut order: Vec<usize> = (0..diag_count).collect();
        order.sort_by(|&a, &b| spans[a].bytes.cmp(&spans[b].bytes));
        if order.iter().enumerate().any(|(i, &j)| i != j) {
            let reordered: Vec<Span> = order
                .into_iter()
                .map(|j| Span {
                    nlmsg_type: spans[j].nlmsg_type,
                    nlmsg_len: spans[j].nlmsg_len,
                    bytes: spans[j].bytes.clone(),
                })
                .collect();
            for (slot, span) in spans[..diag_count].iter_mut().zip(reordered) {
                *slot = span;
            }
            modified = true;
        }
    }

    if !modified {
        return None;
    }
    let mut out = Vec::with_capacity(buf.len());
    for span in &spans {
        out.extend_from_slice(&span.bytes);
    }
    // Reordering whole spans must preserve the exact length; bail out otherwise.
    if out.len() != buf.len() {
        return None;
    }
    Some(out)
}

/// Rewrite the supported identity fields of one socket-diag body. Every field
/// is independently bounds-checked, preserving the existing inode behavior for
/// truncated messages while never partially rewriting a cookie.
fn rewrite_family_identities(out: &mut [u8], body: usize, end: usize, modified: &mut bool) {
    let family = out[body] as i32;
    match family {
        libc::AF_UNIX => {
            zero_u32(out, body + UNIX_DIAG_INO_OFFSET, end, modified);
            write_no_cookie(out, body + UNIX_DIAG_COOKIE_OFFSET, end, modified);
            zero_unix_peer_attrs(out, body + UNIX_DIAG_MSG_LEN, end, modified);
        }
        libc::AF_NETLINK => {
            zero_u32(out, body + NETLINK_DIAG_INO_OFFSET, end, modified);
        }
        // `ss -t`/`ss -u` and anything else reading the socket table. Both
        // families share `struct inet_diag_msg`, so they share the offset.
        libc::AF_INET | libc::AF_INET6 => {
            zero_u32(out, body + INET_DIAG_INO_OFFSET, end, modified);
        }
        // AF_VSOCK and AF_XDP also register socket-diag handlers on this
        // kernel, and their bodies also carry a host-assigned inode
        // (`vdiag_ino`, `xdiag_ino`). They are deliberately absent: no dump on
        // this host returns a single message for either, so the offsets could
        // not be checked against a real reply. Rewrites are bounds-checked, so
        // a wrong offset would not fault -- it would silently canonicalize the
        // wrong field and fail open, which is worse than an acknowledged gap.
        // Add them alongside a populated dump, not before.
        _ => {}
    }
}

/// Walk the routing attributes trailing a `unix_diag_msg`, zeroing the peer
/// inode carried by any `UNIX_DIAG_PEER` attribute. Stops on any inconsistency;
/// because it only zeroes, an early stop is safe.
fn zero_unix_peer_attrs(out: &mut [u8], mut attr: usize, end: usize, modified: &mut bool) {
    while attr + RTA_HDRLEN <= end {
        let rta_len = u16::from_ne_bytes([out[attr], out[attr + 1]]) as usize;
        let rta_type = u16::from_ne_bytes([out[attr + 2], out[attr + 3]]);
        if rta_len < RTA_HDRLEN || attr + rta_len > end {
            break;
        }
        if rta_type == UNIX_DIAG_PEER && rta_len >= RTA_HDRLEN + 4 {
            zero_u32(out, attr + RTA_HDRLEN, end, modified);
        }
        let advance = align_up(rta_len, RTA_ALIGNTO);
        if advance == 0 {
            break;
        }
        attr += advance;
    }
}

/// Zero the `u32` at `at` when it is fully in bounds and currently non-zero.
fn zero_u32(out: &mut [u8], at: usize, end: usize, modified: &mut bool) {
    if at + 4 <= end && at + 4 <= out.len() && out[at..at + 4] != [0, 0, 0, 0] {
        out[at..at + 4].copy_from_slice(&[0, 0, 0, 0]);
        *modified = true;
    }
}

/// Replace a complete two-word cookie with Linux's explicit no-cookie sentinel.
fn write_no_cookie(out: &mut [u8], at: usize, end: usize, modified: &mut bool) {
    let Some(cookie_end) = at.checked_add(std::mem::size_of_val(&SOCK_DIAG_NOCOOKIE)) else {
        return;
    };
    if cookie_end > end || cookie_end > out.len() {
        return;
    }

    if out[at..cookie_end].iter().any(|&byte| byte != u8::MAX) {
        out[at..cookie_end].fill(u8::MAX);
        *modified = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Append an `nlmsghdr` + body as one message, padded to `NLMSG_ALIGNTO`.
    fn push_message(buf: &mut Vec<u8>, nlmsg_type: u16, body: &[u8]) {
        let len = NLMSG_HDRLEN + body.len();
        buf.extend_from_slice(&(len as u32).to_ne_bytes()); // nlmsg_len
        buf.extend_from_slice(&nlmsg_type.to_ne_bytes()); // nlmsg_type
        buf.extend_from_slice(&2u16.to_ne_bytes()); // nlmsg_flags (NLM_F_MULTI)
        buf.extend_from_slice(&0u32.to_ne_bytes()); // nlmsg_seq
        buf.extend_from_slice(&0u32.to_ne_bytes()); // nlmsg_pid
        buf.extend_from_slice(body);
        while !buf.len().is_multiple_of(NLMSG_ALIGNTO) {
            buf.push(0);
        }
    }

    const NETLINK_DIAG_COOKIE_OFFSET: usize = NETLINK_DIAG_INO_OFFSET + 4;
    const INET_DIAG_COOKIE_OFFSET: usize = 44;
    const INET_DIAG_MSG_LEN: usize = INET_DIAG_INO_OFFSET + 4;

    /// A `unix_diag_msg` (16 bytes) followed by a `UNIX_DIAG_PEER` attribute.
    fn unix_diag_body(ino: u32, cookie: [u32; 2], peer_ino: u32) -> Vec<u8> {
        let mut body = vec![
            libc::AF_UNIX as u8, // udiag_family
            0x5a,                // udiag_type sentinel
            1,                   // udiag_state
            0xa5,                // pad sentinel
        ];
        body.extend_from_slice(&ino.to_ne_bytes()); // udiag_ino
        for word in cookie {
            body.extend_from_slice(&word.to_ne_bytes()); // udiag_cookie[2]
        }
        assert_eq!(body.len(), UNIX_DIAG_MSG_LEN);
        let rta_len = (RTA_HDRLEN + 4) as u16;
        body.extend_from_slice(&rta_len.to_ne_bytes());
        body.extend_from_slice(&UNIX_DIAG_PEER.to_ne_bytes());
        body.extend_from_slice(&peer_ino.to_ne_bytes());
        body
    }

    fn netlink_diag_body(ino: u32, cookie: [u32; 2]) -> Vec<u8> {
        let mut body = vec![libc::AF_NETLINK as u8, 0x5a, 0xa5, 1];
        body.extend_from_slice(&0x4000_0000u32.to_ne_bytes());
        body.extend_from_slice(&0x2233_4455u32.to_ne_bytes());
        body.extend_from_slice(&0x6677_8899u32.to_ne_bytes());
        body.extend_from_slice(&ino.to_ne_bytes());
        for word in cookie {
            body.extend_from_slice(&word.to_ne_bytes());
        }
        assert_eq!(body.len(), NETLINK_DIAG_COOKIE_OFFSET + 8);
        body
    }

    fn inet_diag_body(family: i32, ino: u32, cookie: [u32; 2]) -> Vec<u8> {
        let mut body = vec![0xa5; INET_DIAG_MSG_LEN];
        body[0] = family as u8;
        for (index, word) in cookie.into_iter().enumerate() {
            let at = INET_DIAG_COOKIE_OFFSET + index * 4;
            body[at..at + 4].copy_from_slice(&word.to_ne_bytes());
        }
        body[INET_DIAG_INO_OFFSET..INET_DIAG_INO_OFFSET + 4].copy_from_slice(&ino.to_ne_bytes());
        body
    }

    fn read_u32(buf: &[u8], at: usize) -> u32 {
        u32::from_ne_bytes(buf[at..at + 4].try_into().unwrap())
    }

    fn rewrite_body(mut body: Vec<u8>) -> (Vec<u8>, bool) {
        let mut modified = false;
        let end = body.len();
        rewrite_family_identities(&mut body, 0, end, &mut modified);
        (body, modified)
    }

    #[test]
    fn rewrites_only_unix_cookie_and_all_existing_inode_fields() {
        let unix_cookie = [0x0102_0304, 0x0506_0708];
        let netlink_cookie = [0x1112_1314, 0x1516_1718];
        let mut buf = Vec::new();
        push_message(
            &mut buf,
            SOCK_DIAG_BY_FAMILY,
            &unix_diag_body(0x1122_3344, unix_cookie, 0x5566_7788),
        );
        let unix_body = NLMSG_HDRLEN;
        let netlink_start = buf.len();
        push_message(
            &mut buf,
            SOCK_DIAG_BY_FAMILY,
            &netlink_diag_body(0x99aa_bbcc, netlink_cookie),
        );
        push_message(&mut buf, 3, &0i32.to_ne_bytes());

        assert!(sanitize_sock_diag_identities(&mut buf));

        assert_eq!(read_u32(&buf, unix_body + UNIX_DIAG_INO_OFFSET), 0);
        assert_eq!(
            read_u32(&buf, unix_body + UNIX_DIAG_COOKIE_OFFSET),
            u32::MAX
        );
        assert_eq!(
            read_u32(&buf, unix_body + UNIX_DIAG_COOKIE_OFFSET + 4),
            u32::MAX
        );
        assert_eq!(
            read_u32(&buf, unix_body + UNIX_DIAG_MSG_LEN + RTA_HDRLEN),
            0
        );
        assert_eq!(
            read_u32(&buf, netlink_start + NLMSG_HDRLEN + NETLINK_DIAG_INO_OFFSET),
            0
        );
        assert_eq!(
            read_u32(
                &buf,
                netlink_start + NLMSG_HDRLEN + NETLINK_DIAG_COOKIE_OFFSET,
            ),
            netlink_cookie[0]
        );
        assert_eq!(
            read_u32(
                &buf,
                netlink_start + NLMSG_HDRLEN + NETLINK_DIAG_COOKIE_OFFSET + 4,
            ),
            netlink_cookie[1]
        );
    }

    #[test]
    fn preserves_nonidentity_sentinels_for_every_supported_family() {
        let unix = unix_diag_body(0x1122_3344, [0x0102_0304, 0x0506_0708], 0x5566_7788);
        let mut expected_unix = unix.clone();
        expected_unix[UNIX_DIAG_INO_OFFSET..UNIX_DIAG_INO_OFFSET + 4].fill(0);
        expected_unix[UNIX_DIAG_COOKIE_OFFSET..UNIX_DIAG_COOKIE_OFFSET + 8].fill(u8::MAX);
        expected_unix[UNIX_DIAG_MSG_LEN + RTA_HDRLEN..UNIX_DIAG_MSG_LEN + RTA_HDRLEN + 4].fill(0);
        let (unix, modified) = rewrite_body(unix);
        assert!(modified);
        assert_eq!(unix, expected_unix);

        let netlink_cookie = [0x0102_0304, 0x0506_0708];
        let netlink = netlink_diag_body(0x1122_3344, netlink_cookie);
        let mut expected_netlink = netlink.clone();
        expected_netlink[NETLINK_DIAG_INO_OFFSET..NETLINK_DIAG_INO_OFFSET + 4].fill(0);
        let (netlink, modified) = rewrite_body(netlink);
        assert!(modified);
        assert_eq!(netlink, expected_netlink, "AF_NETLINK cookie changed");

        for family in [libc::AF_INET, libc::AF_INET6] {
            let inet = inet_diag_body(family, 0x1122_3344, netlink_cookie);
            let mut expected_inet = inet.clone();
            expected_inet[INET_DIAG_INO_OFFSET..INET_DIAG_INO_OFFSET + 4].fill(0);
            let (inet, modified) = rewrite_body(inet);
            assert!(modified);
            assert_eq!(inet, expected_inet, "family {family} cookie changed");
        }
    }

    #[test]
    fn truncated_unix_cookie_is_untouched_while_complete_inode_is_zeroed() {
        let full = unix_diag_body(0x1122_3344, [0x0102_0304, 0x0506_0708], 0);
        let mut truncated = full[..UNIX_DIAG_MSG_LEN - 1].to_vec();
        let mut expected = truncated.clone();
        expected[UNIX_DIAG_INO_OFFSET..UNIX_DIAG_INO_OFFSET + 4].fill(0);
        let mut modified = false;
        let end = truncated.len();
        rewrite_family_identities(&mut truncated, 0, end, &mut modified);
        assert!(modified);
        assert_eq!(truncated, expected, "partial cookie was rewritten");

        let mut inode_truncated = full[..UNIX_DIAG_INO_OFFSET + 3].to_vec();
        let original = inode_truncated.clone();
        let mut modified = false;
        let end = inode_truncated.len();
        rewrite_family_identities(&mut inode_truncated, 0, end, &mut modified);
        assert!(!modified);
        assert_eq!(inode_truncated, original);
    }

    #[test]
    fn canonicalizes_unix_message_order_after_identity_rewrite() {
        let mut hi = unix_diag_body(0x1111_1111, [1, 2], 0x3333_3333);
        let mut lo = unix_diag_body(0x4444_4444, [5, 6], 0x7777_7777);
        hi[1] = 0x20;
        lo[1] = 0x10;

        let build = |first: &[u8], second: &[u8]| {
            let mut b = Vec::new();
            push_message(&mut b, SOCK_DIAG_BY_FAMILY, first);
            push_message(&mut b, SOCK_DIAG_BY_FAMILY, second);
            push_message(&mut b, 3, &0i32.to_ne_bytes());
            b
        };

        let mut forward = build(&lo, &hi);
        let mut reverse = build(&hi, &lo);
        assert!(sanitize_sock_diag_identities(&mut forward));
        assert!(sanitize_sock_diag_identities(&mut reverse));
        assert_eq!(forward, reverse);
        assert_eq!(forward.len(), reverse.len());
    }

    #[test]
    fn already_canonical_reports_no_change() {
        let mut buf = Vec::new();
        push_message(
            &mut buf,
            SOCK_DIAG_BY_FAMILY,
            &unix_diag_body(0, SOCK_DIAG_NOCOOKIE, 0),
        );
        assert!(!sanitize_sock_diag_identities(&mut buf));
    }

    #[test]
    fn fail_open_on_truncated_header() {
        let mut buf = vec![0xAB; NLMSG_HDRLEN - 1];
        let original = buf.clone();
        assert!(!sanitize_sock_diag_identities(&mut buf));
        assert_eq!(buf, original);
    }

    #[test]
    fn fail_open_on_inconsistent_length() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&999u32.to_ne_bytes());
        buf.extend_from_slice(&SOCK_DIAG_BY_FAMILY.to_ne_bytes());
        buf.extend_from_slice(&0u16.to_ne_bytes());
        buf.extend_from_slice(&0u32.to_ne_bytes());
        buf.extend_from_slice(&0u32.to_ne_bytes());
        buf.extend_from_slice(&unix_diag_body(0x1234, [0x5678, 0x9abc], 0));
        let original = buf.clone();
        assert!(!sanitize_sock_diag_identities(&mut buf));
        assert_eq!(buf, original);
    }

    #[test]
    fn empty_buffer_is_noop() {
        let mut buf: Vec<u8> = Vec::new();
        assert!(!sanitize_sock_diag_identities(&mut buf));
    }

    #[test]
    fn non_diag_messages_are_untouched() {
        let mut buf = Vec::new();
        let payload = unix_diag_body(0x1122_3344, [0x0102_0304, 0x0506_0708], 0x5566_7788);
        push_message(&mut buf, 2, &payload);
        let original = buf.clone();
        assert!(!sanitize_sock_diag_identities(&mut buf));
        assert_eq!(buf, original);
    }
}
