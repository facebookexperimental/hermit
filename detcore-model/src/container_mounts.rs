/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The guest's container mount table: the mounts `/proc/<pid>/mountinfo` and
//! `/proc/<pid>/mounts` show, which hermit-cli builds once, in the completed
//! container namespace before the guest starts, and Detcore renders on every
//! backend (package K slice K3a,
//! <https://github.com/rrnewton/hermit/issues/4213>).
//!
//! A container's mount namespace starts as a copy of the host's, so a table
//! read from the kernel has a row for every host mount that existed when
//! Hermit started: other users' FUSE checkouts, other agents' bind mounts,
//! logind's `/run/user/<uid>`. Which of those exist changes from one run to
//! the next, and a row added or removed renumbers every row after it. The
//! table below shows only the mounts a fixed rule selects
//! ([`ContainerMountTable::select`]), so host mounts outside that rule, made
//! or removed between two runs, change nothing the guest reads.
//!
//! The rows the rule shows are still host facts where they are host mounts:
//! - the base mountpoints;
//! - the host mounts above a mount Hermit installed (rule 3). A mount Hermit
//!   stages below `/tmp` is installed first in its private tmp directory,
//!   below `$TMPDIR` or `--tmp`, so rule 3 then shows the host mount holding
//!   that directory too;
//! - the copies a recursive bind of the plan makes of the host mounts below
//!   its source (every user `--bind` and `--mount type=bind` of a directory
//!   is recursive), which are the bind's own rows (rule 1), ephemeral seeds
//!   included, so they come and go with the host's mounts below that source.
//!
//! The kernel lists mounts in creation order (by unique mount ID since Linux
//! 6.8, in attach order before). The host rows are the copies made when the
//! container's namespace was created, so a host remount moves no row; only a
//! host unmount and a new mount at a shown row's path, between two runs,
//! moves that row.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use serde::Deserialize;
use serde::Serialize;

use crate::config::MountInfoRootRewrite;

/// The mountpoints the guest sees whenever its container has a mount at one:
/// the filesystems every Linux program may expect to find, modelled on the
/// minimal machine of gVisor's `/proc`. A host mount at any other path is
/// hidden unless it holds a mount Hermit installed ([`ContainerMountTable::select`]).
pub const BASE_MOUNTPOINTS: [&str; 9] = [
    "/",
    "/proc",
    "/dev",
    "/dev/pts",
    "/dev/shm",
    "/dev/mqueue",
    "/sys",
    "/sys/fs/cgroup",
    "/run",
];

/// One row of the guest's mount table: every field of a Linux
/// `/proc/<pid>/mountinfo` line, as the kernel wrote it in the completed
/// container namespace (paths and the source escaped as the kernel escapes
/// them), except the parent, which names the nearest mount the table shows
/// ([`ContainerMountTable::select`]). Mount IDs, devices and peer groups are
/// the kernel's raw values; Detcore numbers them for the guest through the
/// same pools that number `stat`, `statx` and fdinfo.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContainerMount {
    /// Field 1: the mount ID.
    pub raw_mount_id: u64,
    /// Field 2: the parent's mount ID.
    pub raw_parent_id: u64,
    /// Field 3: the device's major number.
    pub major: u32,
    /// Field 3: the device's minor number.
    pub minor: u32,
    /// Field 4: the root of the mount within its filesystem.
    pub root: Vec<u8>,
    /// Field 5: the mountpoint.
    pub mountpoint: Vec<u8>,
    /// Field 6: the per-mount options, `ro` or `rw` first.
    pub mount_options: Vec<u8>,
    /// The optional fields before the separator (`shared:N`, `master:N`,
    /// `propagate_from:N`, `unbindable`).
    pub optional_fields: Vec<Vec<u8>>,
    /// The filesystem type.
    pub fstype: Vec<u8>,
    /// The mount source.
    pub source: Vec<u8>,
    /// The superblock options, `ro` or `rw` first.
    pub super_options: Vec<u8>,
}

/// The superblock flags Linux's `show_sb_opts` prints, in its order, right
/// after the superblock's `ro` or `rw` and before the filesystem's own
/// options.
const SUPERBLOCK_FLAG_OPTIONS: [&[u8]; 4] = [b"sync", b"dirsync", b"mand", b"lazytime"];

impl ContainerMount {
    /// One `mountinfo` line, without its newline, in the grammar
    /// [`crate::procfs::parse_mountinfo`] accepts.
    fn parse(line: &[u8]) -> Option<Self> {
        let fields = line.split(|byte| *byte == b' ').collect::<Vec<_>>();
        let separator = fields.iter().position(|field| *field == b"-")?;
        if separator < 6 || separator + 4 != fields.len() {
            return None;
        }
        let decimal = |field: &[u8]| std::str::from_utf8(field).ok()?.parse::<u64>().ok();
        let (major, minor) = {
            let text = std::str::from_utf8(fields[2]).ok()?;
            let (major, minor) = text.split_once(':')?;
            (major.parse().ok()?, minor.parse().ok()?)
        };
        Some(Self {
            raw_mount_id: decimal(fields[0])?,
            raw_parent_id: decimal(fields[1])?,
            major,
            minor,
            root: fields[3].to_vec(),
            mountpoint: fields[4].to_vec(),
            mount_options: fields[5].to_vec(),
            optional_fields: fields[6..separator].iter().map(|f| f.to_vec()).collect(),
            fstype: fields[separator + 1].to_vec(),
            source: fields[separator + 2].to_vec(),
            super_options: fields[separator + 3].to_vec(),
        })
    }

    /// Whether this row is an ephemeral host FUSE seed
    /// (`crate::procfs::is_ephemeral_host_seed_mount`), which the table never
    /// shows unless Hermit installed it.
    pub fn is_ephemeral_host_seed(&self) -> bool {
        // From the fields, not a rendered line: a container's namespace can
        // hold hundreds of rows, and this runs for each at every launch.
        crate::procfs::is_ephemeral_host_seed_fields(&self.mountpoint, &self.fstype)
    }

    /// Append this row as Linux's `show_mountinfo` writes it.
    fn write_mountinfo(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(
            format!(
                "{} {} {}:{} ",
                self.raw_mount_id, self.raw_parent_id, self.major, self.minor
            )
            .as_bytes(),
        );
        for field in [&self.root, &self.mountpoint, &self.mount_options] {
            out.extend_from_slice(field);
            out.push(b' ');
        }
        for field in &self.optional_fields {
            out.extend_from_slice(field);
            out.push(b' ');
        }
        out.extend_from_slice(b"- ");
        out.extend_from_slice(&self.fstype);
        out.push(b' ');
        out.extend_from_slice(&self.source);
        out.push(b' ');
        out.extend_from_slice(&self.super_options);
        out.push(b'\n');
    }

    /// The options Linux's `show_vfsmnt` writes for this mount in
    /// `/proc/<pid>/mounts`: `ro` if the mount or its superblock is
    /// read-only (`__mnt_is_readonly`), else `rw`; then the superblock flags
    /// (`show_sb_opts`), the per-mount flags (`show_mnt_opts`) and the
    /// filesystem's own options (`show_options`). `mountinfo` splits the same
    /// options into its field 6 (the mount's `ro` or `rw` and its flags) and
    /// its superblock options (the superblock's `ro` or `rw`, its flags and the
    /// filesystem's options), so they are taken apart from there.
    ///
    /// A filesystem whose own options begin with one of the superblock flags'
    /// names would have it shown before the per-mount flags rather than after:
    /// no filesystem Linux ships prints one of those names first.
    fn mounts_options(&self) -> Vec<u8> {
        let mut mount = self.mount_options.split(|byte| *byte == b',');
        let mut superblock = self.super_options.split(|byte| *byte == b',');
        let mount_read_only = mount.next() == Some(b"ro".as_slice());
        let superblock_read_only = superblock.next() == Some(b"ro".as_slice());
        let superblock = superblock.collect::<Vec<_>>();
        let flags = superblock
            .iter()
            .take_while(|option| SUPERBLOCK_FLAG_OPTIONS.contains(option))
            .count();
        let mut options: Vec<&[u8]> = vec![if mount_read_only || superblock_read_only {
            b"ro"
        } else {
            b"rw"
        }];
        options.extend(&superblock[..flags]);
        options.extend(mount.filter(|option| !option.is_empty()));
        options.extend(
            superblock[flags..]
                .iter()
                .filter(|option| !option.is_empty()),
        );
        options.join(&b","[..])
    }
}

/// Parse a Linux `/proc/<pid>/mountinfo` snapshot into its rows, in the
/// grammar [`crate::procfs::parse_mountinfo`] accepts: `None` for a malformed
/// row, an optional field this crate cannot map, or a repeated mount ID.
pub fn parse_container_mounts(contents: &[u8]) -> Option<Vec<ContainerMount>> {
    crate::procfs::parse_mountinfo(contents)?;
    if contents.is_empty() {
        return Some(Vec::new());
    }
    contents
        .strip_suffix(b"\n")
        .unwrap_or(contents)
        .split(|byte| *byte == b'\n')
        .map(ContainerMount::parse)
        .collect()
}

/// The guest's container mount table (see the module documentation).
#[derive(Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContainerMountTable {
    /// The rows the guest sees, in the kernel's row order.
    pub mounts: Vec<ContainerMount>,
    /// For each mount of the namespace the table hides, the nearest mount
    /// above it that the table shows, as raw mount IDs: a file on a hidden
    /// mount reports that mount's ID in fdinfo `mnt_id` and `STATX_MNT_ID`.
    pub hidden_ancestors: Vec<(u64, u64)>,
}

/// Only the table's size: Detcore logs its whole configuration, this table
/// included, on one line at each run's start, and verbose verification
/// compares that line between two runs character by character. The rows'
/// raw mount IDs differ from run to run, and their bytes print as decimal
/// lists, so in a container with long mount options (an overlay root's layer
/// list) the line, and that comparison, grew without bound: the validate
/// node running hermit_modes hit its 4 GiB memory cap on it.
impl std::fmt::Debug for ContainerMountTable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ContainerMountTable")
            .field("rows", &self.mounts.len())
            .field("hidden", &self.hidden_ancestors.len())
            .finish()
    }
}

impl ContainerMountTable {
    // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/issues/4213): Review
    // the container mount table's membership rule, a new determinization
    // strategy: real host mounts outside the rule are hidden from the guest.
    /// The table of the namespace whose rows are `namespace` (in the kernel's
    /// order). A row is shown exactly when it is:
    ///
    /// 1. a mount Hermit's mount plan installed: hermit-cli records the mount
    ///    IDs each installation creates as it installs it ([`owned_mounts`]),
    ///    and passes those of every installation but a backend's own
    ///    (`installed`);
    /// 2. a mount at one of [`BASE_MOUNTPOINTS`], proven the same way: `base`
    ///    holds each base mountpoint, as the kernel escapes it, with the
    ///    mount ID of a descriptor held on it, and the row counts only if it
    ///    is mounted at that very path (not merely the mount containing it);
    /// 3. an ancestor of a row of (1), so that every mount Hermit installed
    ///    hangs from rows the table shows.
    ///
    /// A row in `never` (a backend's own mount, an ephemeral host FUSE seed)
    /// is not shown by rule 2 or 3, and a chain of parents passes through it.
    /// No other host mount is shown, whatever it is. A shown row's parent is
    /// its nearest shown ancestor; the namespace's root keeps its parent,
    /// which no row shows, as Linux shows it. Each hidden row is mapped to
    /// its nearest shown ancestor ([`Self::shown_mount_id`]).
    ///
    /// This is a chosen fidelity trade, generalizing the one made for host
    /// FUSE seed mounts (`crate::procfs::is_ephemeral_host_seed_mount`): a
    /// hidden mount is real and traversable, and Linux omits no real mount
    /// from these files. Hermit hides it because whether it exists is decided
    /// by unrelated host activity between runs, and a row added or removed
    /// renumbers every later row. Where Hermit is given an image root
    /// (`--image`), every mount below that root is Hermit's own, so nothing is
    /// hidden there. hermit-cli also leaves the `uid=` and `gid=` superblock
    /// options out of the rows of the filesystems its plan mounts afresh,
    /// which only the container's root can own: Linux prints a tmpfs's owner
    /// as the initial user namespace sees it, so they would name the host
    /// user who ran Hermit where the same mount by root names no one.
    pub fn select(
        namespace: &[ContainerMount],
        installed: &BTreeSet<u64>,
        base: &[(Vec<u8>, u64)],
        never: &BTreeSet<u64>,
    ) -> Self {
        let by_id = namespace
            .iter()
            .map(|row| (row.raw_mount_id, row))
            .collect::<BTreeMap<_, _>>();
        let mut shown = installed
            .iter()
            .copied()
            .filter(|id| by_id.contains_key(id))
            .collect::<BTreeSet<_>>();
        for (mountpoint, id) in base {
            if !never.contains(id)
                && by_id
                    .get(id)
                    .is_some_and(|row| row.mountpoint == *mountpoint)
            {
                shown.insert(*id);
            }
        }
        for id in installed {
            let mut visited = BTreeSet::new();
            let mut current = by_id.get(id).copied();
            while let Some(row) = current {
                if !visited.insert(row.raw_mount_id) {
                    break;
                }
                current = by_id.get(&row.raw_parent_id).copied();
                if let Some(parent) = current
                    && !never.contains(&parent.raw_mount_id)
                {
                    shown.insert(parent.raw_mount_id);
                }
            }
        }
        // The nearest shown mount above `row`, or `None` where its chain of
        // parents leaves the namespace (or loops) first, with the raw ID it
        // left by.
        let nearest_shown = |row: &ContainerMount| -> Result<u64, u64> {
            let mut visited = BTreeSet::from([row.raw_mount_id]);
            let mut parent = row.raw_parent_id;
            loop {
                if shown.contains(&parent) {
                    return Ok(parent);
                }
                match by_id.get(&parent) {
                    Some(above) if visited.insert(parent) => parent = above.raw_parent_id,
                    _ => return Err(parent),
                }
            }
        };
        let mut mounts = Vec::new();
        let mut hidden_ancestors = Vec::new();
        for row in namespace {
            if shown.contains(&row.raw_mount_id) {
                let mut row = row.clone();
                row.raw_parent_id = match nearest_shown(&row) {
                    Ok(parent) => parent,
                    // The namespace's root, or a row whose chain loops: its
                    // own parent, as the kernel wrote it.
                    Err(_) => row.raw_parent_id,
                };
                mounts.push(row);
            } else if let Ok(ancestor) = nearest_shown(row) {
                hidden_ancestors.push((row.raw_mount_id, ancestor));
            }
        }
        Self {
            mounts,
            hidden_ancestors,
        }
    }

    /// The raw mount ID the guest is shown for a file on mount `raw_mount_id`:
    /// the mount itself where the table shows it or it is no row of the
    /// namespace (pipefs, sockfs and the like, which no `mountinfo` lists),
    /// else its nearest shown ancestor.
    pub fn shown_mount_id(&self, raw_mount_id: u64) -> u64 {
        self.hidden_ancestors
            .iter()
            .find(|(hidden, _)| *hidden == raw_mount_id)
            .map_or(raw_mount_id, |(_, ancestor)| *ancestor)
    }

    /// The raw mount IDs of the rows the table shows.
    pub fn shown_mount_ids(&self) -> BTreeSet<u64> {
        self.mounts.iter().map(|row| row.raw_mount_id).collect()
    }

    /// The table as the kernel would write `/proc/<pid>/mountinfo` with only
    /// these rows: raw IDs, devices and peer groups, which Detcore's
    /// mountinfo sanitizer then numbers for the guest.
    pub fn mountinfo(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for row in &self.mounts {
            row.write_mountinfo(&mut out);
        }
        out
    }

    /// The table as Linux writes `/proc/<pid>/mounts` (`show_vfsmnt`): source,
    /// mountpoint, type, options and two zeros per row, in the table's order,
    /// with the mountpoints `rewrites` translate (Hermit's internal staging
    /// prefixes) written as the guest sees them, as its `mountinfo` shows them.
    pub fn mounts(&self, rewrites: &[MountInfoRootRewrite]) -> Vec<u8> {
        let mut prefixes = rewrites
            .iter()
            .filter_map(|rewrite| {
                Some((
                    rewrite.raw_mountpoint_prefix.as_deref()?,
                    rewrite.deterministic_mountpoint_prefix.as_deref()?,
                ))
            })
            .collect::<Vec<_>>();
        prefixes.sort_by_key(|(raw, _)| std::cmp::Reverse(raw.len()));
        let mut out = Vec::new();
        for row in &self.mounts {
            out.extend_from_slice(&row.source);
            out.push(b' ');
            out.extend_from_slice(&rewrite_prefix(&row.mountpoint, &prefixes));
            out.push(b' ');
            out.extend_from_slice(&row.fstype);
            out.push(b' ');
            out.extend_from_slice(&row.mounts_options());
            out.extend_from_slice(b" 0 0\n");
        }
        out
    }
}

/// One installation of the container's mount plan, as hermit-cli recorded it
/// while installing it, in the completed namespace before the guest starts.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Installation {
    /// The raw mount IDs of the rows the mount operation created: those the
    /// namespace listed after it and not before. A recursive bind creates a
    /// row for each mount below its source as well.
    pub ids: Vec<u64>,
    /// The mountpoints, as the kernel escapes them, at which the operation
    /// installed its mount: its target as named, as Hermit resolved it, and
    /// as the kernel resolved it. A created row at one of these is the
    /// installation's; one below one of these is a recursive bind's copy of
    /// a mount below its source; any other created row is not admitted.
    pub mountpoints: Vec<Vec<u8>>,
    /// A bind's source directory, as the kernel escapes it (as named and as
    /// resolved), whose mounts a recursive bind copies below the target.
    pub sources: Vec<Vec<u8>>,
    /// Whether the installation is a backend's own (DBT's statistics bind,
    /// e9patch's overlay of the rewritten program): plumbing,
    /// not part of the guest's machine, so never shown, and neither is a
    /// recursive bind's copy of one.
    pub backend: bool,
}

/// The rows of `namespace` the installations made, split into Hermit's (the
/// plan's own: rule 1 of [`ContainerMountTable::select`]) and a backend's own.
/// A created row counts only at, or below, a mountpoint its installation
/// names, so no row a later mount's symbolic link redirects a path onto can be
/// taken for an installation's. A recursive bind's copy of a mount below its
/// source takes the owner of the row it copies: a backend's own mount staged
/// below Hermit's private tmp directory stays a backend's in the bind of that
/// directory over `/tmp`.
pub fn owned_mounts(
    namespace: &[ContainerMount],
    installations: &[Installation],
) -> (BTreeSet<u64>, BTreeSet<u64>) {
    let by_id = namespace
        .iter()
        .map(|row| (row.raw_mount_id, row))
        .collect::<BTreeMap<_, _>>();
    let mut hermit = BTreeSet::new();
    let mut backend = BTreeSet::new();
    for installation in installations {
        let created = installation.ids.iter().copied().collect::<BTreeSet<_>>();
        for id in &installation.ids {
            let Some(row) = by_id.get(id) else {
                continue;
            };
            let owner_is_backend = if installation.is_at(row) {
                installation.backend
            } else if installation.below(row).is_some() {
                // The row a recursive bind copied: below the source, the same
                // filesystem and root, made before this installation.
                let original = installation
                    .original_of(namespace, row, |candidate| !created.contains(&candidate));
                installation.backend
                    || original.is_some_and(|original| backend.contains(&original.raw_mount_id))
            } else {
                continue;
            };
            if owner_is_backend {
                backend.insert(*id);
            } else {
                hermit.insert(*id);
            }
        }
    }
    (hermit, backend)
}

impl Installation {
    /// Whether `row` is mounted at one of the installation's mountpoints.
    fn is_at(&self, row: &ContainerMount) -> bool {
        self.mountpoints.contains(&row.mountpoint)
    }

    /// The rest of `row`'s mountpoint below one of the installation's (with
    /// its leading `/`), where `row` is strictly below one.
    fn below(&self, row: &ContainerMount) -> Option<Vec<u8>> {
        self.mountpoints.iter().find_map(|mountpoint| {
            if mountpoint.as_slice() == b"/" {
                return (row.mountpoint.as_slice() != b"/").then(|| row.mountpoint.clone());
            }
            let rest = row.mountpoint.strip_prefix(mountpoint.as_slice())?;
            (rest.first() == Some(&b'/')).then(|| rest.to_vec())
        })
    }

    /// The row of `namespace` a recursive bind of this installation's source
    /// copied as `row`: at the source plus `row`'s rest below the target, on
    /// the same filesystem with the same root, and admitted by `eligible`
    /// (by raw mount ID).
    fn original_of<'a>(
        &self,
        namespace: &'a [ContainerMount],
        row: &ContainerMount,
        eligible: impl Fn(u64) -> bool,
    ) -> Option<&'a ContainerMount> {
        let rest = self.below(row)?;
        self.sources.iter().find_map(|source| {
            let path = if source.as_slice() == b"/" {
                rest.clone()
            } else {
                let mut path = source.clone();
                path.extend_from_slice(&rest);
                path
            };
            namespace.iter().find(|candidate| {
                candidate.raw_mount_id != row.raw_mount_id
                    && eligible(candidate.raw_mount_id)
                    && candidate.mountpoint == path
                    && (candidate.major, candidate.minor, &candidate.root)
                        == (row.major, row.minor, &row.root)
            })
        })
    }
}

/// Assign each row the plan created to the installation that created it, for
/// a plan installed as the namespace's last mounts, one mount operation after
/// another, in a namespace whose propagation from the host was cut (made
/// private) before the plan's first mount, so that no host row can follow
/// it: `namespace` lists rows in creation order (the kernel's row order), so
/// the plan's rows are its end, each installation's together and in plan
/// order, each starting with the one row the operation created at its target
/// (a recursive bind's copies of the mounts below its source follow it).
/// Walking back from the last row, a row is
/// - the first row of the latest installation still open at whose
///   mountpoint it is, which closes that installation; or
/// - a copy the latest open installation's recursive bind made, if it is
///   below that installation's mountpoint and the row it copies exists; or
/// - no installation's (a row a symbolic link redirected, a propagated copy),
///   which is not admitted.
///
/// The walk ends at the first installation's row: every row before it is the
/// host's. Returns whether it got there; if not, the plan's rows cannot be
/// told from the host's. An installation with no mountpoint (a remount, which
/// creates no row) takes none.
pub fn assign_installed_rows(
    namespace: &[ContainerMount],
    installations: &mut [Installation],
) -> bool {
    if installations.is_empty() {
        return true;
    }
    // Installations below `open` may still take rows.
    let mut open = installations.len();
    for row in namespace.iter().rev() {
        let first_row = (0..open)
            .rev()
            .find(|&index| installations[index].is_at(row));
        let copy = (0..open).rev().find(|&index| {
            installations[index]
                .original_of(namespace, row, |_| true)
                .is_some()
        });
        // The later installation's, where the row could be either.
        let closes = match (first_row, copy) {
            (Some(index), Some(copy)) => (index > copy).then_some(index),
            (first_row, None) => first_row,
            (None, Some(_)) => None,
        };
        if let Some(index) = closes {
            installations[index].ids.push(row.raw_mount_id);
            if index == 0 {
                return true;
            }
            open = index;
        } else if let Some(index) = copy {
            installations[index].ids.push(row.raw_mount_id);
        }
    }
    false
}

/// Refuse a walk ([`assign_installed_rows`]) whose result does not show the
/// plan as installed: every installation that creates a row (one with a
/// mountpoint) must have been given a row at its own recorded target, and
/// the row given to the first installation must be the container's own
/// `/proc` (`is_plans_first_row`; a copy of the host's `/proc`, at the same
/// path but earlier, would mean the walk ran past the plan into the host's
/// rows). The reason names the first installation that fails.
pub fn check_installed_rows(
    namespace: &[ContainerMount],
    installations: &[Installation],
    is_plans_first_row: impl Fn(&ContainerMount) -> bool,
) -> Result<(), String> {
    let row_at_target = |installation: &Installation| {
        installation.ids.iter().find_map(|id| {
            namespace
                .iter()
                .find(|row| row.raw_mount_id == *id && installation.is_at(row))
        })
    };
    for (index, installation) in installations.iter().enumerate() {
        if installation.mountpoints.is_empty() {
            continue;
        }
        let Some(row) = row_at_target(installation) else {
            return Err(format!(
                "the plan's mount {index}, at {}, has no row at its target",
                String::from_utf8_lossy(&installation.mountpoints[0])
            ));
        };
        if index == 0 && !is_plans_first_row(row) {
            return Err(format!(
                "the row taken for the plan's first mount, {} at {}, is not the container's own",
                row.raw_mount_id,
                String::from_utf8_lossy(&row.mountpoint)
            ));
        }
    }
    Ok(())
}

/// `field` with the longest of `prefixes`' raw prefixes it starts with, at a
/// path component boundary, replaced by that prefix's guest-visible one, as
/// Detcore's mountinfo sanitizer rewrites a mountpoint.
fn rewrite_prefix(field: &[u8], prefixes: &[(&[u8], &[u8])]) -> Vec<u8> {
    for (raw, deterministic) in prefixes {
        let Some(suffix) = field.strip_prefix(*raw) else {
            continue;
        };
        if suffix.is_empty() || suffix.first() == Some(&b'/') {
            let mut rewritten = deterministic.to_vec();
            rewritten.extend_from_slice(suffix);
            return rewritten;
        }
    }
    field.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host namespace: its root, the base filesystems, two host mounts
    /// outside the base set (`/data` and a FUSE checkout below it), a host
    /// tmpfs at `/tmp`, and Hermit's mounts: `/etc/group`, a new `/proc` over
    /// the host's, the private `/tmp` and a user mount below `/data`.
    const HOST: &[u8] = b"\
20 1 259:4 / / rw,relatime shared:1 - btrfs /dev/nvme0n1p4 rw,ssd,subvol=/\n\
21 20 0:22 / /proc rw,nosuid,nodev,noexec,relatime shared:2 - proc proc rw\n\
22 20 0:5 / /dev rw,nosuid master:3 - devtmpfs devtmpfs rw,size=4096k,mode=755\n\
23 22 0:24 / /dev/pts rw,nosuid,noexec,relatime - devpts devpts rw,gid=5,mode=620,ptmxmode=000\n\
24 20 0:25 / /sys ro,nosuid,nodev,noexec,relatime - sysfs sysfs rw\n\
25 20 259:5 / /data rw,relatime - ext4 /dev/nvme1n1 rw\n\
26 25 0:40 / /data/a rw,nosuid,nodev,relatime - fuse.edenfs edenfs rw,user_id=0\n\
27 20 0:26 / /tmp rw,nosuid,nodev - tmpfs tmpfs rw,size=1024k\n\
28 20 259:4 /tmp/tmpAbC/group /etc/group ro,relatime - btrfs /dev/nvme0n1p4 rw,ssd,subvol=/\n\
29 21 0:41 / /proc rw,relatime - proc proc rw\n\
30 27 0:26 /tmpAbC /tmp rw,nosuid,nodev - tmpfs tmpfs rw,size=1024k\n\
31 25 0:42 / /data/work rw,relatime - tmpfs none rw\n";

    /// The base mountpoints with the mount ID a descriptor held on each
    /// reaches.
    fn base(top_at: &[(&str, u64)]) -> Vec<(Vec<u8>, u64)> {
        top_at
            .iter()
            .map(|(path, id)| (path.as_bytes().to_vec(), *id))
            .collect()
    }

    fn host_table(contents: &[u8]) -> ContainerMountTable {
        let rows = parse_container_mounts(contents).expect("the host table parses");
        // The descriptors hermit-cli would hold: the plan's targets reach
        // Hermit's mounts, and each base path reaches the top mount there
        // (`/run`, `/dev/shm`, `/dev/mqueue` and `/sys/fs/cgroup` are not
        // mounts here: their descriptors reach the mount containing them).
        let installed = BTreeSet::from([28, 29, 30, 31]);
        let base = base(&[
            ("/", 20),
            ("/proc", 29),
            ("/dev", 22),
            ("/dev/pts", 23),
            ("/dev/shm", 22),
            ("/dev/mqueue", 22),
            ("/sys", 24),
            ("/sys/fs/cgroup", 24),
            ("/run", 20),
        ]);
        ContainerMountTable::select(&rows, &installed, &base, &BTreeSet::new())
    }

    #[test]
    fn the_table_shows_hermits_mounts_the_base_set_and_their_ancestors_only() {
        let table = host_table(HOST);
        let shown = table
            .mounts
            .iter()
            .map(|row| row.raw_mount_id)
            .collect::<Vec<_>>();
        // Not 26, a host FUSE checkout. 21 (the host's /proc, which Hermit's
        // covers), 25 (/data, which holds Hermit's user mount) and 27 (the
        // host's /tmp, which holds Hermit's private /tmp) are ancestors of
        // Hermit's mounts.
        assert_eq!(shown, [20, 21, 22, 23, 24, 25, 27, 28, 29, 30, 31]);
        // Every shown row keeps its parent: each parent is shown.
        let named = parse_container_mounts(HOST).unwrap();
        for row in &table.mounts {
            let kernel = named
                .iter()
                .find(|kernel| kernel.raw_mount_id == row.raw_mount_id)
                .unwrap();
            assert_eq!(row, kernel);
        }
        assert_eq!(table.hidden_ancestors, [(26, 25)]);
    }

    #[test]
    fn a_shown_row_hangs_from_its_nearest_shown_ancestor() {
        let rows = parse_container_mounts(HOST).unwrap();
        // /dev/pts is a base mount, /dev here is not: /dev/pts hangs from the
        // root, and the root keeps the parent no row shows.
        let table = ContainerMountTable::select(
            &rows,
            &BTreeSet::new(),
            &base(&[("/", 20), ("/dev/pts", 23)]),
            &BTreeSet::new(),
        );
        let shown = table
            .mounts
            .iter()
            .map(|row| (row.raw_mount_id, row.raw_parent_id))
            .collect::<Vec<_>>();
        assert_eq!(shown, [(20, 1), (23, 20)]);
        assert_eq!(table.shown_mount_id(22), 20);
        assert_eq!(table.shown_mount_id(31), 20);
    }

    #[test]
    fn a_hidden_mount_reports_its_nearest_shown_ancestor() {
        let table = host_table(HOST);
        assert_eq!(
            table.shown_mount_id(26),
            25,
            "the FUSE checkout under /data"
        );
        assert_eq!(
            table.shown_mount_id(21),
            21,
            "the covered host /proc is shown"
        );
        assert_eq!(table.shown_mount_id(30), 30, "a shown mount is itself");
        assert_eq!(table.shown_mount_id(7), 7, "pipefs is no row");
    }

    #[test]
    fn the_rendering_is_a_pure_function_of_the_table() {
        let table = host_table(HOST);
        assert_eq!(table.mountinfo(), host_table(HOST).mountinfo());
        assert_eq!(
            table.mountinfo(),
            b"\
20 1 259:4 / / rw,relatime shared:1 - btrfs /dev/nvme0n1p4 rw,ssd,subvol=/\n\
21 20 0:22 / /proc rw,nosuid,nodev,noexec,relatime shared:2 - proc proc rw\n\
22 20 0:5 / /dev rw,nosuid master:3 - devtmpfs devtmpfs rw,size=4096k,mode=755\n\
23 22 0:24 / /dev/pts rw,nosuid,noexec,relatime - devpts devpts rw,gid=5,mode=620,ptmxmode=000\n\
24 20 0:25 / /sys ro,nosuid,nodev,noexec,relatime - sysfs sysfs rw\n\
25 20 259:5 / /data rw,relatime - ext4 /dev/nvme1n1 rw\n\
27 20 0:26 / /tmp rw,nosuid,nodev - tmpfs tmpfs rw,size=1024k\n\
28 20 259:4 /tmp/tmpAbC/group /etc/group ro,relatime - btrfs /dev/nvme0n1p4 rw,ssd,subvol=/\n\
29 21 0:41 / /proc rw,relatime - proc proc rw\n\
30 27 0:26 /tmpAbC /tmp rw,nosuid,nodev - tmpfs tmpfs rw,size=1024k\n\
31 25 0:42 / /data/work rw,relatime - tmpfs none rw\n"
                .as_slice()
        );
        // The rendering round-trips through the parser every consumer uses.
        assert_eq!(
            parse_container_mounts(&table.mountinfo()).unwrap(),
            table.mounts
        );
        // And the serialized table renders the same bytes.
        let restored: ContainerMountTable =
            serde_json::from_str(&serde_json::to_string(&table).unwrap()).unwrap();
        assert_eq!(restored.mountinfo(), table.mountinfo());
    }

    /// The measured divergence: one host mount appeared between two runs,
    /// around row 20 of about 100, and renumbered every row after it. A host
    /// table with an extra row outside the rule, even one under a shown
    /// mount, renders the same bytes, and only the extra row's own fdinfo
    /// mapping is new.
    #[test]
    fn a_host_mount_outside_the_rule_changes_no_byte() {
        let mut churned = Vec::new();
        for line in HOST.split_inclusive(|byte| *byte == b'\n') {
            churned.extend_from_slice(line);
            if line.starts_with(b"25 ") {
                churned.extend_from_slice(
                    b"95 25 0:93 / /data/b rw,nosuid,nodev,relatime - \
                      fuse.edenfs edenfs rw,user_id=1\n",
                );
                churned.extend_from_slice(
                    b"96 20 0:94 / /run/user/1000 rw,nosuid,nodev,relatime - tmpfs tmpfs rw\n",
                );
            }
        }
        let quiet = host_table(HOST);
        let churned = host_table(&churned);
        assert_eq!(churned.mountinfo(), quiet.mountinfo());
        assert_eq!(churned.mounts(&[]), quiet.mounts(&[]));
        assert_eq!(churned.shown_mount_id(95), 25);
        assert_eq!(churned.shown_mount_id(96), 20);
    }

    #[test]
    fn a_base_mountpoint_counts_only_where_its_descriptor_reaches_a_mount_there() {
        let rows = parse_container_mounts(HOST).unwrap();
        // A descriptor on /sys/fs/cgroup reaching the /sys mount does not
        // make /sys a mount at /sys/fs/cgroup, nor one named elsewhere.
        let table = ContainerMountTable::select(
            &rows,
            &BTreeSet::new(),
            &base(&[("/sys/fs/cgroup", 24), ("/data", 26)]),
            &BTreeSet::new(),
        );
        assert!(table.mounts.is_empty(), "{:?}", table.mounts);
        // Every row then maps to the nearest shown ancestor, of which there
        // is none.
        assert!(table.hidden_ancestors.is_empty());
    }

    /// An installation as hermit-cli records it: the rows it created, at
    /// `mountpoints`, copying from `sources`.
    fn installation(
        ids: &[u64],
        mountpoints: &[&str],
        sources: &[&str],
        backend: bool,
    ) -> Installation {
        Installation {
            ids: ids.to_vec(),
            mountpoints: mountpoints.iter().map(|m| m.as_bytes().to_vec()).collect(),
            sources: sources.iter().map(|m| m.as_bytes().to_vec()).collect(),
            backend,
        }
    }

    fn shown(table: &ContainerMountTable) -> Vec<u64> {
        table.mounts.iter().map(|row| row.raw_mount_id).collect()
    }

    /// Both reviews' case: `--mount bind,/srv/a:/opt/x` then
    /// `--mount bind,/srv/b:/opt`, where `/srv/b/x` is a symbolic link to
    /// `/data/foo`. Reopening `/opt/x` afterwards reaches the host's `/data`
    /// mount (and resolves to `/data/foo`, a host mount too); the recorded
    /// installations name only the two rows the binds made. Neither host row,
    /// nor any later host mount at `/data/foo`, becomes Hermit's.
    #[test]
    fn a_later_bind_redirecting_a_target_admits_no_host_row() {
        let rows = parse_container_mounts(
            b"20 1 8:1 / / rw - ext4 /dev/root rw\n\
              50 20 8:2 / /data rw - ext4 /dev/data rw\n\
              51 50 0:40 / /data/foo rw - fuse.edenfs edenfs rw\n\
              60 20 8:1 /srv/a /opt/x rw - ext4 /dev/root rw\n\
              61 20 8:1 /srv/b /opt rw - ext4 /dev/root rw\n",
        )
        .unwrap();
        let installations = [
            installation(&[60], &["/opt/x"], &["/srv/a"], false),
            installation(&[61], &["/opt"], &["/srv/b"], false),
            // A recorded ID at a mountpoint its installation does not name is
            // not admitted: no row a symbolic link reaches counts.
            installation(&[51], &["/opt/x"], &[], false),
        ];
        let (hermit, backend) = owned_mounts(&rows, &installations);
        assert_eq!(hermit, BTreeSet::from([60, 61]));
        assert!(backend.is_empty());
        let table = ContainerMountTable::select(&rows, &hermit, &[], &BTreeSet::new());
        assert_eq!(shown(&table), [20, 60, 61]);
        assert_eq!(table.shown_mount_id(50), 20);
        assert_eq!(table.shown_mount_id(51), 20);
    }

    /// A recursive bind of `/src`, which holds the host mount `/src/sub`,
    /// creates `/dst` and its copy `/dst/sub`: both are the installation's.
    #[test]
    fn a_recursive_bind_owns_the_copies_it_makes() {
        let rows = parse_container_mounts(
            b"20 1 8:1 / / rw - ext4 /dev/root rw\n\
              40 20 8:3 / /src rw - ext4 /dev/src rw\n\
              41 40 0:41 / /src/sub rw - tmpfs tmpfs rw\n\
              70 20 8:3 / /dst rw - ext4 /dev/src rw\n\
              71 70 0:41 / /dst/sub rw - tmpfs tmpfs rw\n",
        )
        .unwrap();
        let (hermit, backend) = owned_mounts(
            &rows,
            &[installation(&[70, 71], &["/dst"], &["/src"], false)],
        );
        assert_eq!(hermit, BTreeSet::from([70, 71]));
        assert!(backend.is_empty());
        let table = ContainerMountTable::select(&rows, &hermit, &[], &BTreeSet::new());
        assert_eq!(shown(&table), [20, 70, 71]);
        assert_eq!(
            table.shown_mount_id(41),
            20,
            "the host's /src/sub stays hidden"
        );
    }

    /// The ids each installation gets, in walk order (last row first), and
    /// whether the walk reached the first installation's row.
    fn assigned(rows: &[ContainerMount], plan: &[Installation]) -> (Vec<Vec<u64>>, bool) {
        let mut plan = plan.to_vec();
        let found = assign_installed_rows(rows, &mut plan);
        (
            plan.into_iter()
                .map(|installation| installation.ids)
                .collect(),
            found,
        )
    }

    /// Two installations at one target (a user's bind, then e9patch's
    /// overlay of the program on it) each get their own row, a remount (no
    /// mountpoint: it creates no row) gets none, and the rows before the
    /// plan's first (the host's copy of `/proc`, at the same path as the
    /// plan's `/proc`) are no installation's.
    #[test]
    fn rows_at_one_target_go_to_their_installations_in_order() {
        let rows = parse_container_mounts(
            b"20 1 8:1 / / rw - ext4 /dev/root rw\n\
              21 20 0:20 / /proc rw - proc proc rw\n\
              50 21 0:50 / /proc rw - proc proc rw\n\
              60 20 8:1 /p /tmp/prog rw - ext4 /dev/root rw\n\
              61 60 8:1 /q /tmp/prog ro - ext4 /dev/root rw\n",
        )
        .unwrap();
        let plan = [
            installation(&[], &["/proc"], &[], false),
            installation(&[], &["/tmp/prog"], &["/p"], false),
            installation(&[], &["/tmp/prog"], &["/q"], true),
            installation(&[], &[], &[], true),
        ];
        assert_eq!(
            assigned(&rows, &plan),
            (vec![vec![50], vec![60], vec![61], vec![]], true)
        );
    }

    /// A recursive bind's copies (here, of the staged mount below Hermit's
    /// private tmp directory, in its bind over `/tmp`) are the bind's, and
    /// keep the owner of the row they copy.
    #[test]
    fn a_recursive_binds_copies_are_assigned_to_it() {
        let rows = parse_container_mounts(
            b"20 1 8:1 / / rw - ext4 /dev/root rw\n\
              49 20 0:49 / /proc rw - proc proc rw\n\
              50 20 0:60 / /scratch/t/x rw - tmpfs tmpfs rw\n\
              51 20 8:1 /scratch/t /tmp rw - ext4 /dev/root rw\n\
              52 51 0:60 / /tmp/x rw - tmpfs tmpfs rw\n",
        )
        .unwrap();
        let plan = [
            installation(&[], &["/proc"], &[], false),
            installation(&[], &["/scratch/t/x"], &[], true),
            installation(&[], &["/tmp"], &["/scratch/t"], false),
        ];
        let (plan_ids, found) = assigned(&rows, &plan);
        assert!(found);
        assert_eq!(plan_ids, [vec![49], vec![50], vec![52, 51]]);
        let mut plan = plan.to_vec();
        for (installation, ids) in plan.iter_mut().zip(plan_ids) {
            installation.ids = ids;
        }
        let (hermit, backend) = owned_mounts(&rows, &plan);
        assert_eq!(hermit, BTreeSet::from([49, 51]));
        assert_eq!(
            backend,
            BTreeSet::from([50, 52]),
            "a backend's mount stays the backend's in the copy"
        );
    }

    /// A row stays its installation's where the kernel put it at install
    /// time, whatever its target names later: a bind at /opt/x crossed the
    /// symbolic link x -> /data/foo in the first bind at /opt, so it was
    /// recorded at /data/foo, and a later bind that replaces /opt (whose x
    /// links elsewhere) does not drop it. A row at no recorded target is no
    /// installation's, and no host row (before the plan's first) is ever
    /// assigned, even one at a plan target's path.
    #[test]
    fn a_row_stays_where_its_install_time_target_was() {
        let rows = parse_container_mounts(
            b"20 1 8:1 / / rw - ext4 /dev/root rw\n\
              30 20 8:2 / /data rw - ext4 /dev/data rw\n\
              31 30 8:2 /x /opt/x rw - ext4 /dev/data rw\n\
              49 20 0:49 / /proc rw - proc proc rw\n\
              60 20 8:1 /srv/a /opt rw - ext4 /dev/root rw\n\
              61 30 8:1 /item /data/foo rw - ext4 /dev/root rw\n\
              62 20 8:1 /stray /stray rw - ext4 /dev/root rw\n\
              63 60 8:1 /srv/c /opt rw - ext4 /dev/root rw\n",
        )
        .unwrap();
        let plan = [
            installation(&[], &["/proc"], &[], false),
            installation(&[], &["/opt"], &[], false),
            installation(&[], &["/data/foo"], &[], false),
            installation(&[], &["/opt"], &[], false),
        ];
        let (plan_ids, found) = assigned(&rows, &plan);
        assert!(found);
        assert_eq!(plan_ids, [vec![49], vec![60], vec![61], vec![63]]);
        let mut plan = plan.to_vec();
        for (installation, ids) in plan.iter_mut().zip(plan_ids) {
            installation.ids = ids;
        }
        assert_eq!(
            check_installed_rows(&rows, &plan, |row| row.raw_mount_id == 49),
            Ok(())
        );
        let (hermit, _) = owned_mounts(&rows, &plan);
        let table = ContainerMountTable::select(&rows, &hermit, &[], &BTreeSet::new());
        assert!(shown(&table).contains(&61), "the genuine row stays visible");
        assert!(!shown(&table).contains(&62) && !shown(&table).contains(&31));
    }

    /// A namespace whose plan rows lack the container's own /proc: the walk
    /// runs on into the host's rows and takes the host's /proc for it, and
    /// the check refuses that; a plan mount with no row at its target is
    /// refused too.
    #[test]
    fn a_walk_that_takes_the_hosts_proc_or_misses_a_mount_is_refused() {
        let rows = parse_container_mounts(
            b"20 1 8:1 / / rw - ext4 /dev/root rw\n\
              21 20 0:22 / /proc rw - proc proc rw\n\
              27 20 0:26 / /tmp rw - tmpfs tmpfs rw\n\
              70 27 8:1 /scratch/t /tmp rw - ext4 /dev/root rw\n",
        )
        .unwrap();
        let plan = [
            installation(&[], &["/proc"], &[], false),
            installation(&[], &["/tmp"], &[], false),
        ];
        let (plan_ids, found) = assigned(&rows, &plan);
        assert!(found, "the walk alone cannot tell");
        let mut walked = plan.to_vec();
        for (installation, ids) in walked.iter_mut().zip(plan_ids) {
            installation.ids = ids;
        }
        // Only a fresh procfs (device 0:4095 here) is the container's.
        let ours = |row: &ContainerMount| (row.major, row.minor) == (0, 4095);
        assert!(check_installed_rows(&rows, &walked, ours).is_err());
        let mut missing = walked.clone();
        missing[1].ids.clear();
        assert!(
            check_installed_rows(&rows, &missing, |_| true)
                .unwrap_err()
                .contains("no row at its target")
        );
    }

    /// Without the plan's first row, the walk cannot tell the plan's rows
    /// from the host's, and says so.
    #[test]
    fn a_plan_whose_first_row_is_missing_is_refused() {
        let rows = parse_container_mounts(
            b"20 1 8:1 / / rw - ext4 /dev/root rw\n\
              60 20 8:1 /srv/b /opt rw - ext4 /dev/root rw\n",
        )
        .unwrap();
        let plan = [
            installation(&[], &["/proc"], &[], false),
            installation(&[], &["/opt"], &["/srv/b"], false),
        ];
        assert!(!assigned(&rows, &plan).1);
    }

    /// A user's own tmpfs at `/test`, with a backend's tmpfs stacked on it:
    /// the user's is shown, the backend's is not, and a file on the
    /// backend's reports the user's, its nearest shown ancestor. Ownership is
    /// the installation's identity, not its path.
    #[test]
    fn a_user_mount_at_a_backend_target_stays_shown() {
        let rows = parse_container_mounts(
            b"20 1 8:1 / / rw - ext4 /dev/root rw\n\
              80 20 0:50 / /test rw - tmpfs tmpfs rw\n\
              81 80 0:51 / /test rw - tmpfs tmpfs rw\n",
        )
        .unwrap();
        let (hermit, backend) = owned_mounts(
            &rows,
            &[
                installation(&[80], &["/test"], &[], false),
                installation(&[81], &["/test"], &[], true),
            ],
        );
        assert_eq!(hermit, BTreeSet::from([80]));
        assert_eq!(backend, BTreeSet::from([81]));
        let table = ContainerMountTable::select(&rows, &hermit, &[], &backend);
        assert_eq!(shown(&table), [20, 80]);
        assert_eq!(table.shown_mount_id(81), 80);
    }

    /// Hermit's `/proc`, covered by a later user bind at `/`, stays shown,
    /// as Linux lists every mount of the namespace.
    #[test]
    fn a_covered_hermit_proc_stays_shown() {
        let rows = parse_container_mounts(
            b"20 1 8:1 / / rw - ext4 /dev/root rw\n\
              21 20 0:22 / /proc rw - proc proc rw\n\
              90 21 0:4095 / /proc rw - proc proc rw\n\
              91 20 8:4 /guest-root / rw - ext4 /dev/other rw\n",
        )
        .unwrap();
        let plan = [
            installation(&[], &["/proc"], &[], false),
            installation(&[], &["/"], &["/guest-root"], false),
        ];
        let (plan_ids, found) = assigned(&rows, &plan);
        assert_eq!((plan_ids.clone(), found), (vec![vec![90], vec![91]], true));
        let mut plan = plan.to_vec();
        for (installation, ids) in plan.iter_mut().zip(plan_ids) {
            installation.ids = ids;
        }
        assert_eq!(
            check_installed_rows(&rows, &plan, |row| row.raw_mount_id == 90),
            Ok(())
        );
        let (hermit, _) = owned_mounts(&rows, &plan);
        assert_eq!(hermit, BTreeSet::from([90, 91]));
        let table = ContainerMountTable::select(&rows, &hermit, &[], &BTreeSet::new());
        assert_eq!(shown(&table), [20, 21, 90, 91]);
    }

    /// A backend's bind staged below Hermit's private tmp directory, then
    /// copied by the recursive bind of that directory over `/tmp`: the copy
    /// is the backend's too, and the guest's `/tmp` is Hermit's.
    #[test]
    fn a_copy_of_a_backend_mount_is_the_backends() {
        let rows = parse_container_mounts(
            b"20 1 8:1 / / rw - ext4 /dev/root rw\n\
              27 20 0:26 / /tmp rw - tmpfs tmpfs rw\n\
              100 27 8:1 /stats /tmp/tmpX/.stats rw - ext4 /dev/root rw\n\
              101 27 0:26 /tmpX /tmp rw - tmpfs tmpfs rw\n\
              102 101 8:1 /stats /tmp/.stats rw - ext4 /dev/root rw\n",
        )
        .unwrap();
        let (hermit, backend) = owned_mounts(
            &rows,
            &[
                installation(&[100], &["/tmp/tmpX/.stats"], &["/stats"], true),
                installation(&[101, 102], &["/tmp"], &["/tmp/tmpX"], false),
            ],
        );
        assert_eq!(hermit, BTreeSet::from([101]));
        assert_eq!(backend, BTreeSet::from([100, 102]));
        let table = ContainerMountTable::select(&rows, &hermit, &[], &backend);
        assert_eq!(shown(&table), [20, 27, 101]);
        assert_eq!(table.shown_mount_id(102), 101);
    }

    /// An ephemeral host FUSE seed is hidden by visibility alone: it stays in
    /// the topology, so a mount below it maps to its nearest shown ancestor,
    /// and a mount Hermit installed is never dropped by its name.
    #[test]
    fn a_seed_is_hidden_but_kept_in_the_topology() {
        let rows = parse_container_mounts(
            b"20 1 8:1 / / rw - ext4 /dev/root rw\n\
              110 20 0:60 / /mnt/x/0a-seed-chef-ns-1 rw - fuse.squashfuse_ll s rw\n\
              111 110 0:61 / /mnt/x/0a-seed-chef-ns-1/in rw - tmpfs tmpfs rw\n\
              112 20 0:62 / /opt/0b-seed-plan-ns-2 rw - fuse.squashfuse_ll s rw\n",
        )
        .unwrap();
        let installed = BTreeSet::from([112]);
        let never = BTreeSet::from([110]);
        let table = ContainerMountTable::select(&rows, &installed, &base(&[("/", 20)]), &never);
        assert_eq!(shown(&table), [20, 112]);
        assert_eq!(table.shown_mount_id(110), 20);
        assert_eq!(table.shown_mount_id(111), 20);
    }

    #[test]
    fn proc_mounts_merges_the_options_as_show_vfsmnt_does() {
        let table = host_table(HOST);
        let mounts = table.mounts(&[]);
        let text = String::from_utf8(mounts).unwrap();
        let lines = text.lines().collect::<Vec<_>>();
        assert_eq!(
            lines[0],
            "/dev/nvme0n1p4 / btrfs rw,relatime,ssd,subvol=/ 0 0"
        );
        assert_eq!(
            lines[4], "sysfs /sys sysfs ro,nosuid,nodev,noexec,relatime 0 0",
            "a read-only mount of a writable superblock is ro"
        );
        let row = |mount_options: &[u8], super_options: &[u8]| ContainerMount {
            raw_mount_id: 1,
            raw_parent_id: 0,
            major: 0,
            minor: 1,
            root: b"/".to_vec(),
            mountpoint: b"/m".to_vec(),
            mount_options: mount_options.to_vec(),
            optional_fields: Vec::new(),
            fstype: b"ext4".to_vec(),
            source: b"/dev/x".to_vec(),
            super_options: super_options.to_vec(),
        };
        assert_eq!(
            row(b"rw,nosuid,noatime", b"rw,sync,lazytime,errors=remount-ro").mounts_options(),
            b"rw,sync,lazytime,nosuid,noatime,errors=remount-ro"
        );
        assert_eq!(
            row(b"rw,relatime", b"ro").mounts_options(),
            b"ro,relatime",
            "a writable mount of a read-only superblock is ro"
        );
    }

    #[test]
    fn proc_mounts_shows_staged_mountpoints_where_the_guest_sees_them() {
        let table = ContainerMountTable {
            mounts: parse_container_mounts(
                b"40 30 0:26 /tmpAbC/x /tmp/tmpAbC/x rw - tmpfs tmpfs rw\n\
                  41 30 0:26 /tmpAbC/xy /tmp/tmpAbCdef rw - tmpfs tmpfs rw\n",
            )
            .unwrap(),
            hidden_ancestors: Vec::new(),
        };
        let rewrite = MountInfoRootRewrite {
            raw_mount_id: 30,
            deterministic_root: b"/".to_vec(),
            raw_root_prefix: None,
            deterministic_root_prefix: None,
            raw_mountpoint_prefix: Some(b"/tmp/tmpAbC".to_vec()),
            deterministic_mountpoint_prefix: Some(b"/tmp".to_vec()),
        };
        assert_eq!(
            table.mounts(&[rewrite]),
            b"tmpfs /tmp/x tmpfs rw 0 0\ntmpfs /tmp/tmpAbCdef tmpfs rw 0 0\n"
        );
    }

    #[test]
    fn the_parser_refuses_what_parse_mountinfo_refuses() {
        assert_eq!(parse_container_mounts(b""), Some(Vec::new()));
        for malformed in [
            b"20 1 259:4 / / rw - btrfs\n".as_slice(),
            b"20 1 259:4 / / rw unknown:7 - btrfs /dev/x rw\n".as_slice(),
            b"20 1 259:4 / / rw - btrfs /dev/x rw\n20 1 259:4 / /a rw - btrfs /dev/x rw\n"
                .as_slice(),
        ] {
            assert_eq!(parse_container_mounts(malformed), None, "{malformed:?}");
        }
    }
}
