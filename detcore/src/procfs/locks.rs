/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Which rows of the kernel's `/proc/locks` belong to the guest.
//!
//! Detcore renders `/proc/locks` from the kernel's file, read through the
//! guest's own descriptor. In the guest's PID namespace Linux hides every
//! POSIX, flock and lease row whose owner the namespace cannot see
//! (`locks_show` drops a row whose `locks_translate_pid` is 0), but it cannot
//! hide an open file description (OFD) lock: such a lock has no owning task,
//! and Linux prints it as owner `-1` from every namespace. Any process on the
//! host that holds an OFD lock, on any file, therefore added rows to the
//! guest's snapshot, at moments the host decided.
//!
//! The guest's own OFD and flock locks belong to its open file descriptions,
//! not to its tasks, and the kernel lists exactly the locks held through one
//! description in that description's `/proc/<pid>/fdinfo/<fd>` `lock:` lines,
//! spelled as `/proc/locks` spells them. After every `F_OFD_SETLK`,
//! `F_OFD_SETLKW` and `flock` call, Detcore reads the calling descriptor's
//! fdinfo through the recorder ([`held_locks_in_fdinfo`]) and keeps, in its
//! global state, which locks each description holds and which guest processes
//! hold the description: a process stops holding it at its last close there,
//! at its exit, and a fork's child starts holding what its parent holds. A
//! description sent over `SCM_RIGHTS` stays held after its sender closes it,
//! until a call through it shows it holds nothing.
//!
//! Before the snapshot's identities are numbered, Detcore keeps only the
//! groups of rows the guest owns ([`guest_lock_rows`]). A group is a granted
//! lock and the waiters queued behind it, which share one sequence number. A
//! group stays when its granted row stays:
//! - an OFD or flock row whose class, mode, access, object and range are, field
//!   for field, a lock a live guest description holds, no more such rows than
//!   guest descriptions hold;
//! - a row owned by a positive pid that names a process of the guest, as the
//!   container's `/proc` lists it (a live process, or a zombie until it is
//!   reaped), translated from the kernel's numbering where a backend's differs
//!   (`Scheduler::procfs_ids`). This also hides another process's POSIX and
//!   flock rows where the guest's procfs is not namespace-private;
//! - TEMPORARY EXEMPTION, for KVM only in practice: a row whose object is on
//!   device `00:00`. KVM keeps a backend-local copy of `/proc/locks`, built
//!   from the supervisor's fdinfo `lock:` lines for the guest's own files, and
//!   prints every row there as `00:00:N` with owner `-1`, so no guest
//!   description's lock line (which names the real device) matches it, and
//!   this filter would otherwise drop all of them. Linux itself never prints
//!   `00:00` for a lockable object (an anonymous device's minor starts at 1),
//!   so no kernel row is kept by this rule. This is not Linux behaviour being
//!   modelled: it is an exemption for one backend's local file, to be removed
//!   when package K replaces KVM's local `/proc/locks` with Detcore's own
//!   rendering (slice K8).
//!
//! Within a group that stays, a waiter row stays only when a guest process
//! owns it. A group whose granted row goes, goes whole, as Linux drops a whole
//! group whose granted row's owner it cannot see.
//!
//! What still differs from Linux, named rather than hidden:
//! - a host lock whose row is, field for field, one a guest description holds
//!   (in practice a shared read lock on the same range of the same file, or a
//!   file in another btrfs subvolume, whose rows print the one superblock
//!   device, with the same inode number, range and access) is shown in place
//!   of nothing, up to the number of such guest locks;
//! - the guest's own OFD waiter rows (`->` with owner -1) are not shown:
//!   fdinfo lists no waiters, so Detcore cannot tell them from a host's;
//! - a lock the guest's descriptions held before Detcore saw a call through
//!   them (a description inherited already locked at the start, one received
//!   over `SCM_RIGHTS` on a backend where Detcore keeps no model of it, or one
//!   rebuilt by an in-guest backend's exec) is not shown until a call through
//!   a modelled descriptor of it;
//! - a lease set by a process that was then reaped, under `--no-namespace`,
//!   is not shown; leases are kept by their owner's pid only;
//! - the guest's own `F_GETLK`, `F_OFD_GETLK` and conflicting lock calls still
//!   see host locks on files it shares with the host: this filter changes only
//!   what `/proc/locks` shows.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use serde::Deserialize;
use serde::Serialize;

/// One row of `/proc/locks`, split into fields:
///     `SEQ: [->] CLASS MODE RW PID MAJOR:MINOR:INODE START END`
#[derive(Clone)]
pub(super) struct LockLine {
    pub(super) sequence: String,
    pub(super) waiter: bool,
    pub(super) fields: Vec<String>,
    pub(super) owner_index: usize,
    pub(super) object_index: usize,
}

/// `line` as a `/proc/locks` row, or `None` when it is not one. The owner is
/// a signed integer and the object has three `:`-separated parts.
pub(super) fn parse_line(line: &str) -> Option<LockLine> {
    let fields = line
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let waiter = fields.get(1).is_some_and(|field| field == "->");
    let details = usize::from(waiter) + 1;
    let owner_index = details + 3;
    let object_index = details + 4;
    let sequence = fields
        .first()
        .and_then(|field| field.strip_suffix(':'))
        .filter(|field| !field.is_empty() && field.bytes().all(|byte| byte.is_ascii_digit()))?
        .to_owned();
    if fields.len() != details + 7
        || fields[owner_index].parse::<i64>().is_err()
        || fields[object_index].split(':').count() != 3
    {
        return None;
    }
    Some(LockLine {
        sequence,
        waiter,
        fields,
        owner_index,
        object_index,
    })
}

/// One granted lock as `/proc/locks` and fdinfo spell it, its owner aside:
/// class (`OFDLCK`, `FLOCK`, `POSIX`, ...), mode (`ADVISORY`), access
/// (`READ`, `WRITE`), object (`MAJOR:MINOR:INODE`, hexadecimal device numbers
/// and a decimal inode, `%02x:%02x:%lu`) and range.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct HeldLock {
    /// The lock's class.
    pub class: String,
    /// The lock's mode.
    pub mode: String,
    /// The lock's access.
    pub access: String,
    /// The locked object, as the kernel prints it.
    pub object: String,
    /// The range's first byte.
    pub start: String,
    /// The range's last byte, or `EOF`.
    pub end: String,
}

impl HeldLock {
    fn from_line(row: &LockLine) -> Self {
        let first = usize::from(row.waiter) + 1;
        let field = |index: usize| row.fields[index].clone();
        Self {
            class: field(first),
            mode: field(first + 1),
            access: field(first + 2),
            object: field(row.object_index),
            start: field(row.object_index + 1),
            end: field(row.object_index + 2),
        }
    }

    /// Whether the object is on device `00:00`: the temporary KVM exemption
    /// (see the module comment), removed when package K slice K8 replaces
    /// KVM's local `/proc/locks`. Linux never prints that device for a
    /// lockable object.
    fn on_null_device(&self) -> bool {
        let mut parts = self.object.split(':');
        let number = |part: Option<&str>| part.and_then(|part| u32::from_str_radix(part, 16).ok());
        number(parts.next()) == Some(0) && number(parts.next()) == Some(0)
    }

    /// Whether Linux ties this lock to an open file description rather than
    /// to a task: an OFD lock or a flock.
    fn belongs_to_a_description(&self) -> bool {
        self.class == "OFDLCK" || self.class == "FLOCK"
    }
}

/// The fields of one `/proc/locks` row that decide whether the guest owns it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockRowFacts {
    /// The group's sequence number, which a granted lock and its waiters share.
    pub sequence: String,
    /// Whether this is a waiter (`->`) row.
    pub waiter: bool,
    /// The owner field: a pid, `-1` for an OFD lock, `0` for a task the
    /// reader's PID namespace cannot see, or a remote lock's own value.
    pub owner: i64,
    /// The lock, its owner aside.
    pub lock: HeldLock,
}

/// The facts of every row of `contents`, in row order, or `None` when any row
/// is not a `/proc/locks` row (the sanitizer then refuses the whole snapshot).
pub(crate) fn lock_rows(contents: &[u8]) -> Option<Vec<LockRowFacts>> {
    let text = std::str::from_utf8(contents).ok()?;
    text.lines()
        .map(|line| {
            let row = parse_line(line)?;
            Some(LockRowFacts {
                owner: row.fields[row.owner_index].parse().ok()?,
                lock: HeldLock::from_line(&row),
                sequence: row.sequence,
                waiter: row.waiter,
            })
        })
        .collect()
}

/// The OFD and flock locks a description holds, from its
/// `/proc/<pid>/fdinfo/<fd>` text: its `lock:` lines, which the kernel spells
/// as `/proc/locks` rows. Lines of other classes (a POSIX lock this process
/// took on the same file) and lines that do not parse are left out.
pub(crate) fn held_locks_in_fdinfo(contents: &[u8]) -> Vec<HeldLock> {
    let text = String::from_utf8_lossy(contents);
    text.lines()
        .filter_map(|line| line.strip_prefix("lock:"))
        .filter_map(|row| parse_line(row.trim()))
        .filter(|row| !row.waiter)
        .map(|row| HeldLock::from_line(&row))
        .filter(HeldLock::belongs_to_a_description)
        .collect()
}

/// Which of `rows` the guest owns (see the module comment). `held` is every
/// lock a live guest description holds, repeats included; `guest_process`
/// answers for a positive owner.
pub(crate) fn guest_lock_rows(
    rows: &[LockRowFacts],
    mut guest_process: impl FnMut(i64) -> bool,
    held: &[HeldLock],
) -> Vec<bool> {
    let mut unclaimed: BTreeMap<&HeldLock, usize> = BTreeMap::new();
    for lock in held {
        *unclaimed.entry(lock).or_default() += 1;
    }
    let mut kept_groups = BTreeSet::new();
    let mut keep = vec![false; rows.len()];
    for (index, row) in rows.iter().enumerate() {
        if row.waiter {
            continue;
        }
        // TEMPORARY EXEMPTION for KVM's backend-local `/proc/locks`
        // (`HeldLock::on_null_device`); removed with package K slice K8.
        keep[index] = row.lock.on_null_device()
            || (row.owner > 0 && guest_process(row.owner))
            || (row.lock.belongs_to_a_description()
                && unclaimed.get_mut(&row.lock).is_some_and(|count| {
                    let available = *count > 0;
                    *count = count.saturating_sub(1);
                    available
                }));
        if keep[index] {
            kept_groups.insert(row.sequence.as_str());
        }
    }
    for (index, row) in rows.iter().enumerate() {
        if row.waiter {
            keep[index] = kept_groups.contains(row.sequence.as_str())
                && (row.lock.on_null_device() || (row.owner > 0 && guest_process(row.owner)));
        }
    }
    keep
}

/// The rows of `contents` that `kept` keeps, in order. With no decision (a
/// snapshot whose rows do not parse, which the sanitizer refuses anyway, or a
/// caller that made none) every row is kept. A decision for a different
/// number of rows keeps none.
pub(crate) fn keep_lock_rows(contents: &[u8], kept: Option<&[bool]>) -> Vec<u8> {
    let Some(kept) = kept else {
        return contents.to_vec();
    };
    let Ok(text) = std::str::from_utf8(contents) else {
        return Vec::new();
    };
    let lines = text.lines().collect::<Vec<_>>();
    if lines.len() != kept.len() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (line, keep) in lines.into_iter().zip(kept) {
        if *keep {
            out.extend_from_slice(line.as_bytes());
            out.push(b'\n');
        }
    }
    if !text.ends_with('\n') && !out.is_empty() {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Rows as Linux prints them, from a host where a guest in a PID namespace
    // holds a POSIX lock (namespace pid 3), an OFD lock and a flock, and the
    // host holds others. `00:2a` is a btrfs superblock device.
    const SNAPSHOT: &[u8] = b"\
1: POSIX  ADVISORY  WRITE 3 00:2a:1001 0 EOF
2: OFDLCK ADVISORY  WRITE -1 00:2a:1002 0 0
3: OFDLCK ADVISORY  WRITE -1 00:18:588084 1073741824 1073741824
4: OFDLCK ADVISORY  WRITE -1 00:2a:1002 10 10
5: FLOCK  ADVISORY  WRITE 4242 00:2a:1001 0 EOF
6: POSIX  ADVISORY  WRITE 4243 00:2a:1004 0 EOF
6: -> POSIX  ADVISORY  WRITE 3 00:2a:1004 0 EOF
7: OFDLCK ADVISORY  WRITE -1 00:2a:1003 0 EOF
7: -> OFDLCK ADVISORY  WRITE -1 00:2a:1003 0 EOF
7: -> POSIX  ADVISORY  WRITE 3 00:2a:1003 0 EOF
7: -> POSIX  ADVISORY  WRITE 0 00:2a:1003 0 EOF
";

    // What fdinfo shows for the guest's descriptions: one OFD lock on inode
    // 1002 at byte 0, one flock (taken by pid 4242, since reaped) and one OFD
    // lock on inode 1003. The POSIX line is this process's own POSIX lock on
    // the same file, which fdinfo lists too.
    const FDINFO: &[u8] = b"pos:\t0\nflags:\t02100002\nmnt_id:\t31\nino:\t1002\n\
lock:\t1: OFDLCK ADVISORY  WRITE -1 00:2a:1002 0 0\n\
lock:\t2: POSIX  ADVISORY  WRITE 3 00:2a:1002 5 5\n\
lock:\t3: FLOCK  ADVISORY  WRITE 4242 00:2a:1001 0 EOF\n\
lock:\t4: OFDLCK ADVISORY  WRITE -1 00:2a:1003 0 EOF\n";

    fn guest(owner: i64) -> bool {
        owner == 3
    }

    #[test]
    fn parses_every_row_and_refuses_a_malformed_one() {
        let rows = lock_rows(SNAPSHOT).unwrap();
        assert_eq!(rows.len(), 11);
        assert_eq!(rows[2].owner, -1);
        assert_eq!(rows[2].lock.object, "00:18:588084");
        assert_eq!(rows[2].lock.start, "1073741824");
        assert!(rows[6].waiter && rows[6].sequence == "6" && rows[6].lock.class == "POSIX");
        assert_eq!(rows[10].owner, 0);
        assert!(lock_rows(b"1: POSIX ADVISORY WRITE 3 00:2a:1001 0 EOF\nnot a row\n").is_none());
        assert!(lock_rows(b"").unwrap().is_empty());
    }

    #[test]
    fn fdinfo_names_the_descriptions_ofd_and_flock_locks() {
        let held = held_locks_in_fdinfo(FDINFO);
        assert_eq!(
            held.iter()
                .map(|lock| (
                    lock.class.as_str(),
                    lock.object.as_str(),
                    lock.start.as_str()
                ))
                .collect::<Vec<_>>(),
            [
                ("OFDLCK", "00:2a:1002", "0"),
                ("FLOCK", "00:2a:1001", "0"),
                ("OFDLCK", "00:2a:1003", "0"),
            ]
        );
        assert!(held_locks_in_fdinfo(b"pos:\t0\nflags:\t02\n").is_empty());
        assert!(held_locks_in_fdinfo(b"lock:\tnot a lock\n").is_empty());
    }

    #[test]
    fn host_rows_are_dropped_and_the_guests_are_kept() {
        let rows = lock_rows(SNAPSHOT).unwrap();
        let held = held_locks_in_fdinfo(FDINFO);
        assert_eq!(
            guest_lock_rows(&rows, guest, &held),
            [
                true,  // the guest's POSIX lock, by its pid
                true,  // the guest's OFD lock on 1002 at byte 0
                false, // a host OFD lock on another file
                false, // a host OFD lock on the guest's own file, another range
                true,  // the flock a guest description holds, whose taker is gone
                false, // a host POSIX lock
                false, // the guest's waiter behind it: the group goes whole
                true,  // the guest's OFD lock on 1003
                false, // an OFD waiter, which fdinfo cannot attribute
                true,  // the guest's POSIX waiter behind its own lock
                false, // a waiter the namespace cannot see
            ]
        );
    }

    #[test]
    fn with_no_description_locks_only_guest_processes_rows_stay() {
        let rows = lock_rows(SNAPSHOT).unwrap();
        assert_eq!(
            guest_lock_rows(&rows, guest, &[]),
            [
                true, false, false, false, false, false, false, false, false, false, false
            ]
        );
        assert!(
            guest_lock_rows(&rows, |_| false, &[])
                .iter()
                .all(|keep| !keep)
        );
    }

    #[test]
    fn identical_rows_are_kept_no_more_often_than_the_guest_holds_them() {
        // Two shared read locks with the same range on the same file: one the
        // guest's, one a host's. Linux prints the same row twice.
        let contents = b"\
1: OFDLCK ADVISORY READ -1 00:18:7 0 EOF
2: OFDLCK ADVISORY READ -1 00:18:7 0 EOF
";
        let rows = lock_rows(contents).unwrap();
        let held = held_locks_in_fdinfo(b"lock:\t1: OFDLCK ADVISORY READ -1 00:18:7 0 EOF\n");
        assert_eq!(guest_lock_rows(&rows, guest, &held), [true, false]);
        let twice = [held.clone(), held].concat();
        assert_eq!(guest_lock_rows(&rows, guest, &twice), [true, true]);
    }

    #[test]
    fn a_lock_on_another_device_with_the_same_inode_is_not_the_guests() {
        let contents = b"\
1: OFDLCK ADVISORY WRITE -1 00:18:7 0 EOF
2: OFDLCK ADVISORY WRITE -1 00:2a:7 0 EOF
";
        let rows = lock_rows(contents).unwrap();
        let held = held_locks_in_fdinfo(b"lock:\t1: OFDLCK ADVISORY WRITE -1 00:18:7 0 EOF\n");
        assert_eq!(guest_lock_rows(&rows, guest, &held), [true, false]);
        // Once the guest unlocks, nothing of its own remains to claim either.
        assert_eq!(guest_lock_rows(&rows, guest, &[]), [false, false]);
    }

    #[test]
    fn rows_a_backend_rendered_on_the_null_device_are_kept() {
        // KVM's backend-local copy of the file (the temporary exemption,
        // removed with package K slice K8).
        let contents = b"\
1: OFDLCK ADVISORY WRITE -1 00:00:1 0 EOF
2: FLOCK ADVISORY WRITE 98765 00:00:2 0 EOF
";
        let rows = lock_rows(contents).unwrap();
        assert_eq!(guest_lock_rows(&rows, |_| false, &[]), [true, true]);
    }

    #[test]
    fn dropped_rows_shift_no_remaining_identity() {
        let rows = lock_rows(SNAPSHOT).unwrap();
        let kept = guest_lock_rows(&rows, guest, &[]);
        let mut file =
            super::super::ProcfsFile::from_path(std::path::Path::new("/proc/locks")).unwrap();
        file.initialize(
            SNAPSHOT.to_vec(),
            super::super::ProcfsSnapshotContext {
                lock_rows_kept: Some(kept),
                ..Default::default()
            },
        );
        let rendered = file.take(usize::MAX).unwrap();
        // Exactly what the guest's one row alone renders: the host's rows,
        // gone, numbered nothing.
        assert_eq!(
            rendered,
            super::super::sanitize_locks(b"1: POSIX  ADVISORY  WRITE 3 00:2a:1001 0 EOF\n")
        );
        assert_eq!(
            String::from_utf8(rendered).unwrap(),
            "1: POSIX ADVISORY WRITE 1 00:00:1 0 EOF\n"
        );
    }

    #[test]
    fn kept_rows_preserve_order_and_the_final_newline() {
        let contents = b"1: A\n2: B\n3: C\n";
        assert_eq!(
            keep_lock_rows(contents, Some(&[true, false, true][..])),
            b"1: A\n3: C\n"
        );
        assert_eq!(
            keep_lock_rows(b"1: A\n2: B", Some(&[false, true][..])),
            b"2: B"
        );
        assert_eq!(keep_lock_rows(contents, Some(&[false; 3][..])), b"");
        assert_eq!(keep_lock_rows(contents, None), contents);
        assert_eq!(keep_lock_rows(contents, Some(&[true][..])), b"");
    }
}
