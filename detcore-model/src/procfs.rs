/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Types and parsers shared by procfs producers and consumers.

use std::collections::BTreeSet;

/// Identity-bearing fields from one Linux `/proc/*/mountinfo` row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MountInfoRow {
    pub raw_mount_id: u64,
    pub raw_parent_id: u64,
    pub raw_device: u64,
    pub root: Vec<u8>,
    pub raw_peer_groups: Vec<u64>,
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-873): Review private mount-root normalization.
pub const MOUNT_PEER_PREFIXES: [&[u8]; 3] = [b"shared:", b"master:", b"propagate_from:"];

/// True for one ephemeral host FUSE seed mount row in `/proc/<pid>/mountinfo`
/// grammar: filesystem type `fuse.squashfuse_ll` and a mount point whose last
/// path component is a host seed name, `<hex>-seed-<seed>-ns-<digits>`, where
/// `<seed>` is a non-empty run of ASCII letters, digits, `_` and `-`.
///
/// The host's squashfuse infrastructure creates these mounts, normally at
/// `/mnt/xarfuse/uid-<uid>/<seed name>`, either one per host process (seed
/// `nspid<digits>_cgpid<digits>`) or one per named host tool (for example
/// seed `chef`, `fb-pcie-error-log` or `devserver-cleanup_hg_cache`). Those
/// rows are other processes' runtime state imported into the guest namespace
/// by shared mount propagation: they are not created by Hermit, by the guest
/// session, or by any ancestor the guest can name, and they appear, disappear
/// and are remounted under new mount IDs asynchronously as unrelated host
/// processes and tool runs start and stop (named-tool seeds were observed
/// living four to five seconds, and the `chef` seed was remounted under a new
/// mount ID). Passing their membership through made `/proc/<pid>/mountinfo`
/// (and the length of every read of it) a host-timing observation: in the
/// `procfs-sanitized-paths` divergence, one seed row changed the tail `read`
/// length between two strict runs.
///
/// This is a chosen determinism fidelity trade. Seed rows are real mounts in
/// the guest namespace (imported by shared propagation and traversable), and
/// Linux omits no real mount from mountinfo. Hermit nevertheless excludes the
/// class, long-lived named seeds included, because its membership is owned by
/// unrelated host processes and changes asynchronously, the same scope choice
/// `DETERMINISM_ARGUMENT.md` makes for other changing host inputs. The class is
/// decided by the seed name, not by the directory it is shown under, so other
/// SquashFUSE mounts (for example a long-lived `/mnt/xarfuse/stable-release`)
/// stay visible, and a guest whose root makes the displayed path
/// `/xarfuse/uid-<uid>/<seed>` still excludes the same rows the launch-time
/// capture excluded. Other host mount churn, such as logind's
/// `/run/user/<uid>` tmpfs mounts, is not in this class and stays visible.
///
/// Only mountinfo rows are classified. `/proc/<pid>/mounts` is passed through
/// unchanged: <https://github.com/rrnewton/hermit/issues/3719>.
pub fn is_ephemeral_host_seed_mount(line: &[u8]) -> bool {
    let fields: Vec<&[u8]> = line.split(|byte| *byte == b' ').collect();
    let Some(separator) = fields.iter().position(|field| *field == b"-") else {
        return false;
    };
    let mount_point = fields.get(4).copied().unwrap_or_default();
    let fs_type = fields.get(separator + 1).copied().unwrap_or_default();
    is_ephemeral_host_seed_fields(mount_point, fs_type)
}

/// [`is_ephemeral_host_seed_mount`] for a row already split into its
/// mountpoint and filesystem type (fields 5 and 9).
pub fn is_ephemeral_host_seed_fields(mount_point: &[u8], fs_type: &[u8]) -> bool {
    let name = mount_point
        .rsplit(|byte| *byte == b'/')
        .next()
        .unwrap_or_default();
    fs_type == b"fuse.squashfuse_ll" && is_host_seed_name(name)
}

/// Drop the rows `is_ephemeral_host_seed_mount` classifies from raw mountinfo
/// contents, keeping every other row, newline included, in its original order.
/// Detcore's guest-read capture, Detcore's fdinfo `mnt_id` capture and
/// hermit-cli's launch-time identity capture all use this one filter, so they
/// agree on the guest mount membership.
pub fn exclude_ephemeral_host_seed_mounts(contents: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(contents.len());
    for line in contents.split_inclusive(|byte| *byte == b'\n') {
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        if !is_ephemeral_host_seed_mount(body) {
            out.extend_from_slice(line);
        }
    }
    out
}

/// `<hex>-seed-<seed>-ns-<digits>`, with a non-empty hex run, a non-empty
/// seed of ASCII letters, digits, `_` and `-`, and a non-empty digit run.
fn is_host_seed_name(name: &[u8]) -> bool {
    fn parse(name: &[u8]) -> Option<()> {
        let hex = name
            .iter()
            .take_while(|byte| byte.is_ascii_hexdigit())
            .count();
        let rest = name
            .get(hex..)
            .filter(|_| hex > 0)?
            .strip_prefix(b"-seed-")?;
        let digits = rest
            .iter()
            .rev()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        let seed = rest
            .get(..rest.len().checked_sub(digits).filter(|_| digits > 0)?)?
            .strip_suffix(b"-ns-")?;
        (!seed.is_empty()
            && seed
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')))
        .then_some(())
    }
    parse(name).is_some()
}

/// Whether every visible mount ID occurs once and in the same relative order
/// as the producer-captured namespace.
pub fn mount_ids_are_ordered_subset(visible: &[u64], captured: &[u64]) -> bool {
    let mut seen = BTreeSet::new();
    if !visible.iter().all(|raw| seen.insert(*raw)) {
        return false;
    }
    let mut captured = captured.iter();
    visible
        .iter()
        .all(|raw| captured.by_ref().any(|candidate| candidate == raw))
}

fn decimal(field: &[u8]) -> Option<u64> {
    if field.is_empty() || field.iter().any(|byte| !byte.is_ascii_digit()) {
        return None;
    }
    std::str::from_utf8(field).ok()?.parse().ok()
}

fn mount_peer_group(field: &[u8]) -> Option<Option<u64>> {
    for prefix in MOUNT_PEER_PREFIXES {
        if let Some(raw) = field.strip_prefix(prefix) {
            return Some(Some(decimal(raw)?));
        }
    }
    Some(None)
}

fn parse_mountinfo_row(line: &[u8]) -> Option<MountInfoRow> {
    let fields = line.split(|byte| *byte == b' ').collect::<Vec<_>>();
    if fields.iter().any(|field| field.is_empty()) {
        return None;
    }
    let separator = fields.iter().position(|field| *field == b"-")?;
    if separator < 6 || separator + 4 != fields.len() {
        return None;
    }
    let mut device = fields[2].split(|byte| *byte == b':');
    let major = u32::try_from(decimal(device.next()?)?).ok()?;
    let minor = u32::try_from(decimal(device.next()?)?).ok()?;
    if device.next().is_some() {
        return None;
    }
    let mut raw_peer_groups = Vec::new();
    for field in &fields[6..separator] {
        if let Some(raw) = mount_peer_group(field)? {
            raw_peer_groups.push(raw);
        } else if field
            .iter()
            .position(|byte| *byte == b':')
            .is_some_and(|separator| decimal(&field[separator + 1..]).is_some())
        {
            // Unknown numeric optional fields can carry mount-namespace
            // identity just like the peer-group fields above. Passing one
            // through would expose an unsanitized host identifier, so refuse
            // until its Linux semantics and deterministic mapping are known.
            return None;
        }
    }
    Some(MountInfoRow {
        raw_mount_id: decimal(fields[0])?,
        raw_parent_id: decimal(fields[1])?,
        raw_device: libc::makedev(major, minor),
        root: fields[3].to_vec(),
        raw_peer_groups,
    })
}

/// Strictly parse a Linux `/proc/*/mountinfo` snapshot.
///
/// Empty files are valid empty snapshots. Malformed rows and duplicate mount
/// IDs are rejected so every producer and consumer accepts the same grammar.
pub fn parse_mountinfo(contents: &[u8]) -> Option<Vec<MountInfoRow>> {
    if contents.is_empty() {
        return Some(Vec::new());
    }
    let body = contents.strip_suffix(b"\n").unwrap_or(contents);
    if body.is_empty() {
        return None;
    }
    let rows = body
        .split(|byte| *byte == b'\n')
        .map(parse_mountinfo_row)
        .collect::<Option<Vec<_>>>()?;
    let mut seen = BTreeSet::new();
    rows.iter()
        .all(|row| seen.insert(row.raw_mount_id))
        .then_some(rows)
}

/// Parse the one decimal `mnt_id` field required by Linux fdinfo.
///
/// Missing, duplicate, malformed, signed, or out-of-range values are rejected.
/// Keeping this byte parser below both `hermit-cli` and `detcore` prevents the
/// container capture path from accepting input the guest-visible sanitizer
/// would later refuse.
pub fn parse_fdinfo_mount_id(contents: &[u8]) -> Option<u64> {
    let mut mount_id = None;
    for line in contents.split(|byte| *byte == b'\n') {
        let Some(value) = line.strip_prefix(b"mnt_id:") else {
            continue;
        };
        let value = value
            .strip_prefix(b"\t")
            .or_else(|| value.strip_prefix(b" "))?;
        if value.is_empty() || value.iter().any(|byte| !byte.is_ascii_digit()) || mount_id.is_some()
        {
            return None;
        }
        let text = std::str::from_utf8(value).ok()?;
        mount_id = Some(text.parse().ok()?);
    }
    mount_id
}

#[cfg(test)]
mod tests {
    use super::exclude_ephemeral_host_seed_mounts;
    use super::is_ephemeral_host_seed_mount;
    use super::mount_ids_are_ordered_subset;
    use super::parse_fdinfo_mount_id;
    use super::parse_mountinfo;

    #[test]
    fn fdinfo_mount_id_is_strict_and_unique() {
        assert_eq!(parse_fdinfo_mount_id(b"pos:\t0\nmnt_id:\t37\n"), Some(37));
        assert_eq!(parse_fdinfo_mount_id(b"mnt_id: 0\n"), Some(0));
        for malformed in [
            b"pos:\t0\n".as_slice(),
            b"mnt_id:\tbad\nmnt_id:\t37\n".as_slice(),
            b"mnt_id:\t37\nmnt_id:\t38\n".as_slice(),
            b"mnt_id:\t37 trailing\n".as_slice(),
            b"mnt_id:\t18446744073709551616\n".as_slice(),
            b"mnt_id:37\n".as_slice(),
        ] {
            assert_eq!(parse_fdinfo_mount_id(malformed), None, "{malformed:?}");
        }
    }

    #[test]
    fn mountinfo_parser_accepts_empty_and_refuses_malformed_or_duplicate_rows() {
        assert_eq!(parse_mountinfo(b""), Some(Vec::new()));
        assert_eq!(parse_mountinfo(b"\n"), None);
        let row = b"37 1 8:1 / / rw shared:9 - ext4 /dev/root rw\n";
        let parsed = parse_mountinfo(row).expect("valid mountinfo row");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].raw_mount_id, 37);
        assert_eq!(parsed[0].raw_parent_id, 1);
        assert_eq!(parsed[0].root, b"/");
        assert_eq!(parsed[0].raw_peer_groups, [9]);
        assert!(parse_mountinfo(b"37 1 bad / / rw - ext4 /dev/root rw\n").is_none());
        assert!(
            parse_mountinfo(b"37 1 8:1 / / rw unbindable nosymfollow - ext4 /dev/root rw\n")
                .is_some(),
            "known bare optional flags must remain accepted"
        );
        assert!(
            parse_mountinfo(b"37 1 8:1 / / rw future_peer:19 - ext4 /dev/root rw\n").is_none(),
            "unknown numeric optional fields must fail closed"
        );
        assert!(
            parse_mountinfo(b"37 1 8:1 / / rw future_flag:value - ext4 /dev/root rw\n").is_some(),
            "unknown nonnumeric flags carry no raw numeric identity"
        );
        let duplicate = [row.as_slice(), row.as_slice()].concat();
        assert!(parse_mountinfo(&duplicate).is_none());
    }

    #[test]
    fn mount_id_subset_requires_unique_members_in_captured_order() {
        assert!(mount_ids_are_ordered_subset(&[], &[10, 20, 30]));
        assert!(mount_ids_are_ordered_subset(&[10, 30], &[10, 20, 30]));
        assert!(!mount_ids_are_ordered_subset(&[30, 10], &[10, 20, 30]));
        assert!(!mount_ids_are_ordered_subset(&[10, 99], &[10, 20, 30]));
        assert!(!mount_ids_are_ordered_subset(&[10, 10], &[10, 20, 30]));
    }

    const SEED: &[u8] = b"76 1 0:50 / /mnt/xarfuse/uid-1000/e62a203d-seed-nspid4026531836_cgpid16161-ns-4026531832 rw,nosuid,nodev,relatime master:48 - fuse.squashfuse_ll squashfuse_ll rw,user_id=1000,group_id=100,allow_other";

    /// The class is the seed name on a SquashFUSE mount, not the directory:
    /// a long-lived SquashFUSE mount under the same prefix stays, a changed
    /// guest root that shortens the displayed path still excludes the seed,
    /// and a seed-named mount of another filesystem type stays.
    #[test]
    fn ephemeral_host_seed_mount_class_is_the_seed_name() {
        assert!(is_ephemeral_host_seed_mount(SEED));
        assert!(!is_ephemeral_host_seed_mount(
            b"77 1 0:51 / /mnt/xarfuse/stable-release rw,relatime - fuse.squashfuse_ll squashfuse_ll rw"
        ));
        assert!(is_ephemeral_host_seed_mount(
            b"76 1 0:50 / /xarfuse/uid-1/e62a203d-seed-nspid4026531836_cgpid16161-ns-4026531832 rw - fuse.squashfuse_ll squashfuse_ll rw"
        ));
        assert!(!is_ephemeral_host_seed_mount(
            b"76 1 0:50 / /mnt/xarfuse/uid-1/e62a203d-seed-nspid4026531836_cgpid16161-ns-4026531832 rw - tmpfs none rw"
        ));
        for near_miss in [
            b"76 1 0:50 / /var/releases/www rw - fuse.squashfuse_ll squashfuse_ll rw".as_slice(),
            b"76 1 0:50 / /mnt/xarfuse/uid-1/e62a203d-seed-nspid1_cgpid2-ns-3x rw - fuse.squashfuse_ll squashfuse_ll rw".as_slice(),
            b"76 1 0:50 / /mnt/xarfuse/uid-1/-seed-nspid1_cgpid2-ns-3 rw - fuse.squashfuse_ll squashfuse_ll rw".as_slice(),
            b"76 1 0:50 / /mnt/xarfuse/uid-1/e62a203d-seed--ns-3 rw - fuse.squashfuse_ll squashfuse_ll rw".as_slice(),
            b"76 1 0:50 / /mnt/xarfuse/uid-1/e62a203d-seed-chef-ns- rw - fuse.squashfuse_ll squashfuse_ll rw".as_slice(),
            b"76 1 0:50 / /mnt/xarfuse/uid-1/e62a203d-seed-a.b-ns-3 rw - fuse.squashfuse_ll squashfuse_ll rw".as_slice(),
            b"76 1 0:50 / /mnt/xarfuse/uid-1/e62a203d-chef-ns-3 rw - fuse.squashfuse_ll squashfuse_ll rw".as_slice(),
            b"76 1 0:50 / /mnt/xarfuse/uid-1/e62a203d-seed-nspid1_cgpid2-ns-3/sub rw - fuse.squashfuse_ll squashfuse_ll rw".as_slice(),
            b"18 1 0:21 / /proc rw,nosuid - proc proc rw".as_slice(),
            b"squashfuse_ll /mnt/xarfuse/uid-1/e62a203d-seed-nspid1_cgpid2-ns-3 fuse.squashfuse_ll rw 0 0".as_slice(),
        ] {
            assert!(!is_ephemeral_host_seed_mount(near_miss), "{near_miss:?}");
        }
    }

    /// Named-tool seeds are the same class as per-process seeds. The rows are
    /// copied from a 2026-10-04 host mount monitor (two lived four to five
    /// seconds; `chef` was remounted under a new mount ID); the host is
    /// recorded in docs/TESTING_ENVIRONMENTS.md under "Named measurement hosts".
    /// A long-lived SquashFUSE mount and a seed-named mount of another type stay.
    #[test]
    fn named_host_seeds_are_excluded() {
        for named_seed in [
            b"683 2845 0:111 / /mnt/xarfuse/uid-0/8cce7402-seed-fb-pcie-error-log-ns-4026531832 rw,nosuid,nodev,relatime shared:3548 - fuse.squashfuse_ll squashfuse_ll rw,user_id=0,group_id=0".as_slice(),
            b"405 2845 0:52 / /mnt/xarfuse/uid-0/de637de8-seed-devserver-cleanup_hg_cache-ns-4026531832 rw,nosuid,nodev,relatime shared:246 - fuse.squashfuse_ll squashfuse_ll rw,user_id=0,group_id=0,allow_other".as_slice(),
            b"401 2845 0:52 / /mnt/xarfuse/uid-0/06178150-seed-chef-ns-4026531832 rw,nosuid,nodev,relatime shared:246 - fuse.squashfuse_ll squashfuse_ll rw,user_id=0,group_id=0,allow_other".as_slice(),
        ] {
            assert!(is_ephemeral_host_seed_mount(named_seed), "{named_seed:?}");
        }
        for kept in [
            b"77 2845 0:51 / /mnt/xarfuse/stable-release rw,relatime - fuse.squashfuse_ll squashfuse_ll rw".as_slice(),
            b"401 2845 0:52 / /mnt/xarfuse/uid-0/06178150-seed-chef-ns-4026531832 rw,nosuid,nodev,relatime shared:246 - tmpfs tmpfs rw".as_slice(),
        ] {
            assert!(!is_ephemeral_host_seed_mount(kept), "{kept:?}");
        }
    }

    #[test]
    fn seed_filter_drops_only_seed_rows_and_keeps_order() {
        let contents = [
            b"18 1 0:21 / /proc rw - proc proc rw\n".as_slice(),
            SEED,
            b"\n401 1 0:52 / /mnt/xarfuse/uid-0/06178150-seed-chef-ns-4026531832 rw - fuse.squashfuse_ll squashfuse_ll rw".as_slice(),
            b"\n100 1 0:70 / /test rw - tmpfs none rw\n".as_slice(),
            b"77 1 0:51 / /mnt/xarfuse/stable-release rw - fuse.squashfuse_ll squashfuse_ll rw"
                .as_slice(),
        ]
        .concat();
        assert_eq!(
            exclude_ephemeral_host_seed_mounts(&contents),
            b"18 1 0:21 / /proc rw - proc proc rw\n100 1 0:70 / /test rw - tmpfs none rw\n77 1 0:51 / /mnt/xarfuse/stable-release rw - fuse.squashfuse_ll squashfuse_ll rw"
        );
    }
}
