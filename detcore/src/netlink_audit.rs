/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Determinize the volatile fields carried by `NETLINK_AUDIT` status replies,
//! and classify audit request types for write containment.
//!
//! libaudit (reached from PAM stacks, e.g. inside an rpmbuild guest) exchanges
//! `AUDIT_GET` for an `audit_status` reply on a `NETLINK_AUDIT` raw socket.
//! Nothing flagged audit sockets before this module, so the reply -- 232 bytes
//! measured in the pam rebuild under `--verify` (claude-provrepro p06) --
//! reached the guest byte-for-byte. `audit_status` carries the host audit
//! subsystem's LIVE COUNTERS and the host auditd pid (linux/audit.h):
//!
//! ```text
//!   audit_status field        offset  why it moves
//!   pid                         12    host auditd's pid: host identity
//!   lost                        24    records dropped: advances with host load
//!   backlog                     28    messages queued right now
//!   backlog_wait_time_actual    40    cumulative seconds of queue-wait
//! ```
//!
//! The differing bytes in the observed 232-byte reply were those fields: the
//! two runs saw different backlog (716 vs 196) and lost (112 vs 100) counts.
//!
//! The volatile fields are ZEROED, the same "freeze, don't virtualize" choice
//! `crate::netlink_route` and `crate::sock_diag` make for their live counters
//! and identities: a guest under Hermit has no deterministic clock or host
//! audit daemon against which advancing host counters would mean anything.
//! Configuration fields (enabled, failure mode, rate and backlog limits,
//! feature bitmap) pass through untouched.
//!
//! Write containment is judged separately from reads: an AUDIT_SET or
//! AUDIT_USER-class message is an instruction to the *host* audit subsystem.
//! Whether the host kernel would honour it depends on capabilities the guest
//! happens to hold (measured on this host: the kernel acks -ECONNREFUSED when
//! no audit consumer is registered, and CAP_AUDIT_CONTROL in a child user
//! namespace cannot reach the init namespace's configuration) -- but Detcore
//! must not let that be the only barrier, so typed sends are classified here
//! and mutating types are refused by name at the send entry points.
//!
//! Known residual, shared with `NETLINK_ROUTE`/`NETLINK_SOCK_DIAG`:
//! sockets are flagged when Detcore sees them created, so an audit
//! socket it did not see created (an inherited initial descriptor, or
//! one adopted from outside the guest) is neither sanitized nor
//! write-contained. Descriptors passed inside the guest join the
//! sender's open-file description and keep the flag.

/// `nlmsghdr` is 16 bytes: len(u32) type(u16) flags(u16) seq(u32) pid(u32).
const NLMSG_HDRLEN: usize = 16;
const NLMSG_ALIGNTO: usize = 4;

/// `AUDIT_GET`, the libaudit status request; the kernel reuses the request
/// type on the status reply (linux/audit.h).
const AUDIT_GET: u16 = 1000;

/// `AUDIT_SET`: sets the fields named by audit_status.mask.
#[cfg(test)]
const AUDIT_SET: u16 = 1001;

/// `audit_status` (linux/audit.h) is eleven 32-bit words.
#[cfg(test)]
const AUDIT_STATUS_LEN: usize = 44;

// TODO-HUMAN-REVIEW(D133211314): Determinizing audit replies hides the host
// audit subsystem's identity and load from guest userspace; review that
// choice together with the audit-write refusal.

/// Whether an outgoing netlink message of this type only *reads* the
/// audit subsystem and so may be sent on to the host kernel. Every other
/// audit-domain type (1000 and above) either mutates host audit
/// configuration (AUDIT_SET and family: rules, TTY/module/logging
/// controls) or emits a record into the host audit log -- the kernel's
/// user-message ranges 1100-1199 (`AUDIT_FIRST_USER_MSG..=LAST`) and
/// 2100-2999 (`..MSG2`) are record emission gated only by
/// `CAP_AUDIT_WRITE` in `audit_netlink_ok`, and `AUDIT_USER_TTY` (1124)
/// also drives `tty_audit_push()`. Unknown and kernel-record types are
/// refused too: new write verbs must not acquire a pass-through by
/// default. Types below 1000 are generic netlink control (`NLMSG_*`),
/// never audit requests.
pub fn audit_request_mutates(msg_type: u16) -> bool {
    if msg_type < 1000 {
        return false;
    }
    !matches!(
        msg_type,
        AUDIT_GET | AUDIT_LIST_RULES | AUDIT_GET_FEATURE | AUDIT_TTY_GET | AUDIT_SIGNAL_INFO
    )
}

/// `AUDIT_LIST_RULES`, dump the kernel audit rules (no state change).
const AUDIT_LIST_RULES: u16 = 1013;
/// `AUDIT_SIGNAL_INFO`, query signal-info state.
const AUDIT_SIGNAL_INFO: u16 = 1010;
/// `AUDIT_TTY_GET`, query TTY auditing state.
const AUDIT_TTY_GET: u16 = 1016;
/// `AUDIT_GET_FEATURE`, query the enabled-feature bitmap.
const AUDIT_GET_FEATURE: u16 = 1019;

/// Field offsets within `audit_status` that move on a live host. Zeroing
/// exactly these keeps the reply byte-stable without touching the
/// configuration the guest asked about (enabled, failure mode, limits).
const VOLATILE_FIELDS: [(usize, usize); 4] = [
    (12, 16), // pid: host identity (zeroed like sock_diag inodes)
    (24, 28), // lost
    (28, 32), // backlog
    (40, 44), // backlog_wait_time_actual
];

fn align_up(value: usize, align: usize) -> usize {
    value.div_ceil(align) * align
}

/// The first message type in an outgoing NETLINK_AUDIT buffer that would
/// act on the host kernel's audit subsystem, if any. Walks the buffer the
/// way the kernel does -- sequentially by `nlmsg_len` -- so a mutating
/// message cannot hide behind a read-only one; a malformed or truncated
/// tail ends the walk, as it would the kernel's parse.
pub fn audit_send_mutating_type(buf: &[u8]) -> Option<u16> {
    let mut offset = 0usize;
    while offset + NLMSG_HDRLEN <= buf.len() {
        let len = u32::from_ne_bytes(buf[offset..offset + 4].try_into().ok()?) as usize;
        let msg_type = u16::from_ne_bytes(buf[offset + 4..offset + 6].try_into().ok()?);
        if len < NLMSG_HDRLEN || offset + len > buf.len() {
            return None;
        }
        if audit_request_mutates(msg_type) {
            return Some(msg_type);
        }
        offset += align_up(len, NLMSG_ALIGNTO);
    }
    None
}

/// The first mutating message type across a batch of datagrams
/// (`sendmmsg`). Each datagram is classified on its own: the kernel
/// parses every skb independently (`nlmsg_ok`/`nlmsg_next` per skb in
/// `audit_receive`), so concatenating a batch into one buffer would let
/// padding or an overlong header in one datagram hide the next.
pub fn audit_sends_mutating_type<'a, I>(datagrams: I) -> Option<u16>
where
    I: IntoIterator<Item = &'a [u8]>,
{
    datagrams.into_iter().find_map(audit_send_mutating_type)
}

/// Zero the volatile `audit_status` fields in a NETLINK_AUDIT GET reply,
/// in place. Returns whether the buffer was modified.
///
/// The walk sanitizes each complete message, then stops at a malformed or
/// truncated tail. A final message the receive truncated (header present,
/// body short) still has the volatile bytes it *did* capture zeroed: a
/// short read must not leak the host auditd pid just because the guest
/// asked for fewer bytes than a whole status.
pub fn sanitize_audit_reply(buf: &mut [u8]) -> bool {
    let mut modified = false;
    let mut offset = 0usize;
    while offset + NLMSG_HDRLEN <= buf.len() {
        let len = match buf[offset..offset + 4].try_into() {
            Ok(bytes) => u32::from_ne_bytes(bytes) as usize,
            Err(_) => return modified,
        };
        let msg_type = match buf[offset + 4..offset + 6].try_into() {
            Ok(bytes) => u16::from_ne_bytes(bytes),
            Err(_) => return modified,
        };
        if len < NLMSG_HDRLEN {
            return modified;
        }
        // The part of this message the receive actually captured.
        let end = (offset + len).min(buf.len());
        if msg_type == AUDIT_GET {
            let status = offset + NLMSG_HDRLEN;
            for (from, to) in VOLATILE_FIELDS {
                let from = status + from;
                let to = (status + to).min(end);
                if from < to {
                    for byte in &mut buf[from..to] {
                        if *byte != 0 {
                            *byte = 0;
                            modified = true;
                        }
                    }
                }
            }
        }
        if offset + len > buf.len() {
            return modified;
        }
        offset += align_up(len, NLMSG_ALIGNTO);
    }
    modified
}

#[cfg(test)]
mod test {
    use super::*;

    fn status_reply(pid: u32, lost: u32, backlog: u32, wait_actual: u32) -> Vec<u8> {
        let len = NLMSG_HDRLEN + AUDIT_STATUS_LEN;
        let mut out = vec![0u8; align_up(len, NLMSG_ALIGNTO)];
        out[0..4].copy_from_slice(&(len as u32).to_ne_bytes());
        out[4..6].copy_from_slice(&AUDIT_GET.to_ne_bytes());
        let s = &mut out[NLMSG_HDRLEN..];
        s[0..4].copy_from_slice(&0xffu32.to_ne_bytes()); // mask
        s[4..8].copy_from_slice(&1u32.to_ne_bytes()); // enabled
        s[12..16].copy_from_slice(&pid.to_ne_bytes());
        s[16..20].copy_from_slice(&1000u32.to_ne_bytes()); // rate_limit
        s[24..28].copy_from_slice(&lost.to_ne_bytes());
        s[28..32].copy_from_slice(&backlog.to_ne_bytes());
        s[40..44].copy_from_slice(&wait_actual.to_ne_bytes());
        out
    }

    #[test]
    fn volatile_fields_are_zeroed_and_config_is_kept() {
        let mut buf = status_reply(1234, 112, 716, 9);
        let before = buf.clone();
        assert!(sanitize_audit_reply(&mut buf));
        // Expected ranges are the audit_status layout itself (header 16 +
        // pid@12, lost@24, backlog@28, backlog_wait_time_actual@40), written
        // out in full so a wrong offset in the code cannot hide behind the
        // constant under test.
        let mut expected = before;
        for (from, to) in [(28, 32), (40, 44), (44, 48), (56, 60)] {
            expected[from..to].fill(0);
        }
        assert_eq!(buf, expected);
        // A second pass is a no-op.
        assert!(!sanitize_audit_reply(&mut buf));
    }

    #[test]
    fn every_message_in_a_reply_is_sanitized() {
        let mut buf = status_reply(11, 12, 13, 14);
        buf.extend_from_slice(&status_reply(21, 22, 23, 24));
        assert!(sanitize_audit_reply(&mut buf));
        // Second status starts at byte 60; its pid is at 60+28.
        assert_eq!(&buf[28..32], &[0, 0, 0, 0]);
        assert_eq!(&buf[88..92], &[0, 0, 0, 0]);
        assert_eq!(&buf[116..120], &[0, 0, 0, 0]);
    }

    #[test]
    fn unlike_replies_are_untouched() {
        // NLMSG_ERROR (type 2): an 80-byte ack echoing the request, as the
        // kernel sends; its payload bytes sit where a status's volatile
        // fields would, and must survive untouched.
        let mut ack = vec![0xabu8; 80];
        ack[0..4].copy_from_slice(&80u32.to_ne_bytes());
        ack[4..6].copy_from_slice(&2u16.to_ne_bytes());
        ack[16..20].copy_from_slice(&(-111i32).to_ne_bytes());
        let before = ack.clone();
        assert!(!sanitize_audit_reply(&mut ack));
        assert_eq!(ack, before);
    }

    #[test]
    fn short_reply_still_zeroes_captured_volatile_bytes() {
        // A 32-byte capture of the 60-byte reply holds the pid (bytes
        // 28..32) but nothing past it: exactly those bytes are zeroed.
        let mut buf = status_reply(1234, 112, 716, 9);
        buf.truncate(32);
        let mut expected = buf.clone();
        expected[28..32].fill(0);
        assert!(sanitize_audit_reply(&mut buf));
        assert_eq!(buf, expected);
        // A capture too short to reach any volatile byte stays untouched.
        let mut buf = status_reply(1, 2, 3, 4);
        buf.truncate(20);
        let before = buf.clone();
        assert!(!sanitize_audit_reply(&mut buf));
        assert_eq!(buf, before);
    }

    #[test]
    fn write_requests_are_classified_as_mutating() {
        assert!(audit_request_mutates(AUDIT_SET));
        assert!(audit_request_mutates(1011)); // AUDIT_ADD_RULE
        assert!(audit_request_mutates(1012)); // AUDIT_DEL_RULE
        assert!(audit_request_mutates(1018)); // AUDIT_SET_FEATURE
        assert!(audit_request_mutates(1017)); // AUDIT_TTY_SET
        assert!(audit_request_mutates(1005)); // AUDIT_USER (emit a record)
        assert!(audit_request_mutates(1042)); // unknown user-range type
        // The kernel's user-record ranges: emission gated only by
        // CAP_AUDIT_WRITE in audit_netlink_ok.
        assert!(audit_request_mutates(1100)); // AUDIT_FIRST_USER_MSG
        assert!(audit_request_mutates(1107)); // AUDIT_USER_AVC
        assert!(audit_request_mutates(1124)); // AUDIT_USER_TTY (tty_audit_push)
        assert!(audit_request_mutates(1199)); // AUDIT_LAST_USER_MSG
        assert!(audit_request_mutates(2100)); // AUDIT_FIRST_USER_MSG2
        assert!(audit_request_mutates(2400));
        assert!(audit_request_mutates(2999)); // AUDIT_LAST_USER_MSG2
        assert!(audit_request_mutates(1300)); // kernel record type, never a request
        assert!(!audit_request_mutates(AUDIT_GET));
        assert!(!audit_request_mutates(AUDIT_LIST_RULES));
        assert!(!audit_request_mutates(AUDIT_GET_FEATURE));
        assert!(!audit_request_mutates(AUDIT_TTY_GET));
        assert!(!audit_request_mutates(AUDIT_SIGNAL_INFO));
        assert!(!audit_request_mutates(2)); // NLMSG_ERROR is not a request
        assert!(!audit_request_mutates(3)); // NLMSG_DONE
        assert!(!audit_request_mutates(999)); // below the audit domain
    }

    fn request(msg_type: u16, payload_len: usize) -> Vec<u8> {
        let len = NLMSG_HDRLEN + payload_len;
        let mut out = vec![0u8; align_up(len, NLMSG_ALIGNTO)];
        out[0..4].copy_from_slice(&(len as u32).to_ne_bytes());
        out[4..6].copy_from_slice(&msg_type.to_ne_bytes());
        out
    }

    #[test]
    fn send_walk_finds_mutating_type_behind_reads() {
        let mut buf = request(AUDIT_GET, AUDIT_STATUS_LEN);
        buf.extend_from_slice(&request(AUDIT_SET, AUDIT_STATUS_LEN));
        assert_eq!(audit_send_mutating_type(&buf), Some(AUDIT_SET));
        assert_eq!(
            audit_send_mutating_type(&request(AUDIT_LIST_RULES, 0)),
            None
        );
        assert_eq!(audit_send_mutating_type(&[]), None);
        assert_eq!(audit_send_mutating_type(&[0u8; 8]), None);
        // A header whose length runs past the buffer ends the walk.
        let mut truncated = request(AUDIT_SET, AUDIT_STATUS_LEN);
        truncated.truncate(NLMSG_HDRLEN + 4);
        assert_eq!(audit_send_mutating_type(&truncated), None);
    }

    #[test]
    fn sendmmsg_datagrams_are_classified_separately() {
        // A padded read-only first datagram must not hide a mutating second
        // one: the kernel parses each skb on its own.
        let mut first = request(AUDIT_GET, AUDIT_STATUS_LEN);
        first.extend_from_slice(&[0; 4]);
        let second = request(AUDIT_SET, AUDIT_STATUS_LEN);
        assert_eq!(
            audit_sends_mutating_type([first.as_slice(), second.as_slice()]),
            Some(AUDIT_SET)
        );
        assert_eq!(
            audit_sends_mutating_type([first.as_slice(), first.as_slice()]),
            None
        );
        // Flattened into one buffer the same batch hides the second message
        // behind the padding -- exactly why sendmmsg keeps its boundaries.
        let mut flat = first.clone();
        flat.extend_from_slice(&second);
        assert_eq!(audit_send_mutating_type(&flat), None);
    }
}
