/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Types and parsers shared by procfs producers and consumers.

use std::collections::BTreeSet;

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-873): Review private mount-root normalization.
pub use reverie_procfs::mountinfo::MOUNT_PEER_PREFIXES;
/// The mountinfo and fdinfo parsers live in `reverie-procfs` (single
/// grammar owner, PR-3a/PR-3b of the virtual-/proc factoring);
/// re-exported here so producers and consumers keep one path. The
/// seed-mount exclusion and mount-identity provenance rules below are
/// Detcore policy and stay.
pub use reverie_procfs::mountinfo::MountInfoRow;
pub use reverie_procfs::mountinfo::parse_fdinfo_mount_id;
pub use reverie_procfs::mountinfo::parse_mountinfo;

/// Whether captured mount-identity provenance is well formed: the
/// namespace order names each raw mount ID once, and the assignment order
/// numbers each raw ID once and names raw 0 only where the namespace order
/// does (an fdinfo zero is the anonymous class and is never numbered).
/// Detcore's mount-ID pool accepts exactly these, and a producer that hands
/// provenance to a backend without a second capture checks them first.
pub fn mount_identity_provenance_is_well_formed(order: &[u64], assignment: &[u64]) -> bool {
    let mut rows = std::collections::BTreeSet::new();
    if !order.iter().all(|raw| rows.insert(*raw)) {
        return false;
    }
    if assignment.contains(&0) && !rows.contains(&0) {
        return false;
    }
    let mut numbered = std::collections::BTreeSet::new();
    assignment.iter().all(|raw| numbered.insert(*raw))
}

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

#[cfg(test)]
mod tests {
    use super::exclude_ephemeral_host_seed_mounts;
    use super::is_ephemeral_host_seed_mount;
    use super::mount_ids_are_ordered_subset;

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
