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
//! `F_OFD_SETLKW`, `flock` and `pidfd_getfd` call, and for a descriptor
//! received over `SCM_RIGHTS` that Detcore models, Detcore reads that
//! descriptor's fdinfo through the recorder ([`held_locks_in_fdinfo`]). Its
//! global state keeps a [`LockLedger`]: which locks each such description
//! holds and which guest descriptor tables reference it, with a slot count
//! per table, kept as Linux keeps the references (close, replacement, fork,
//! `CLONE_FILES`, a successful exec, exit). A description's locks are the
//! guest's while some table it knows references it. If the fdinfo cannot be
//! opened or read (a full descriptor table, no `/proc` in a chroot, a seccomp
//! filter), the description's locks are unknown rather than taken to be none:
//! the call itself goes on as Linux runs it, and a `/proc/locks` read made
//! while a guest table still references that description stops the run by
//! name ([`LockLedger::unknown`]), since its rows could not be told from a
//! host's. A run that never reads `/proc/locks` is not stopped.
//!
//! Before the snapshot's identities are numbered, Detcore keeps only the
//! groups of rows the guest owns ([`guest_lock_rows`]). A group is a granted
//! lock and the waiters queued behind it, which share one sequence number. A
//! group stays when its granted row stays:
//! - an OFD or flock row whose class, mode, access, object and range are, field
//!   for field, a lock a guest description some guest table references holds,
//!   at most one row per distinct such lock (a row a guest process owns claims
//!   its match first);
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
//! What still differs from Linux, named rather than hidden. One case can show
//! a host row; every other case hides a row of the guest's own, and none
//! shows a host row:
//! - a host lock whose row is, field for field, a lock a guest description
//!   holds now (in practice a shared read lock on the same range of the same
//!   file, or a file in another btrfs subvolume, whose rows print the one
//!   superblock device, with the same inode number, range and access) takes
//!   the place of the guest's row, at most once per distinct such lock: the
//!   row shown is the same either way;
//! - two guest descriptions holding identical shared read OFD locks (same
//!   file, range and access) show one row, where Linux shows two;
//! - the guest's own OFD waiter rows (`->` with owner -1) are not shown:
//!   fdinfo lists no waiters, so Detcore cannot tell them from a host's;
//! - a lock the guest's descriptions held before Detcore read it is not shown
//!   until a call through a modelled descriptor of it: a description
//!   inherited already locked at the start; one received over `SCM_RIGHTS`
//!   where Detcore keeps no model of the receiver's descriptor (ptrace), once
//!   the sender's tables let go of it; one received through `recvmmsg`; one an
//!   in-guest backend's exec rebuilds under a new identity;
//! - fork before lock, for OFD locks: a table that referenced a description
//!   before Detcore first read its locks (the parent of a child that then
//!   locks it) is not followed, so once the tables Detcore did see let go of
//!   it (the child exits), its lock is hidden though Linux keeps it. A flock
//!   taken that way is shown while its taker lives, by its pid; after the
//!   taker is reaped, Linux hides it too under a PID namespace;
//! - a process whose descriptor table the ledger cannot follow contributes no
//!   lock while it lives, nor does a process forked from it: one sharing its
//!   table with another process (`CLONE_FILES` without `CLONE_THREAD`, whose
//!   slots in-guest backends also count in separate per-process copies), and
//!   one with a thread that has a private table (`CLONE_THREAD` without
//!   `CLONE_FILES`), whose calls the ledger cannot tell from the leader's;
//! - once a lock or unlock goes through a descriptor whose description the
//!   ledger may know under another identity (one Detcore keeps no model of,
//!   such as a descriptor received over `SCM_RIGHTS` on ptrace, or one an
//!   in-guest backend registered on reception), no lock on that inode number,
//!   on any device, is shown again; if that descriptor's fdinfo cannot be
//!   read, no OFD or flock lock is shown again;
//! - a lease set by a process that was then reaped, under `--no-namespace`,
//!   is not shown; leases are kept by their owner's pid only;
//! - the guest's own `F_GETLK`, `F_OFD_GETLK` and conflicting lock calls still
//!   see host locks on files it shares with the host: this filter changes only
//!   what `/proc/locks` shows.
//!
//! Remaining ways a host row can still appear, until `/proc/locks` is decided
//! from the kernel's state at the snapshot: a description an in-guest
//! backend's exec rebuilds under a new identity, whose lock changes through
//! the new identity leave the old one's entry stale in a table forked before
//! the exec; and a lock change the guest makes without Detcore seeing the call
//! at all (DBT refuses the one known case, a copied `vfork` child, while the
//! parent's descriptions may hold an OFD lock).

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use serde::Deserialize;
use serde::Serialize;

use crate::types::DetPid;
use crate::types::OpenFileId;

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
/// lock a guest description some guest table references holds; each distinct
/// lock admits at most one row, so a lock listed by two descriptions the
/// ledger cannot tell are one (an in-guest backend's receiver registers a
/// description it received under an identity of its own) admits no host row
/// beside the guest's. `guest_process` answers for a positive owner.
pub(crate) fn guest_lock_rows(
    rows: &[LockRowFacts],
    mut guest_process: impl FnMut(i64) -> bool,
    held: &[HeldLock],
) -> Vec<bool> {
    let mut unclaimed: BTreeMap<&HeldLock, usize> = BTreeMap::new();
    for lock in held {
        unclaimed.insert(lock, 1);
    }
    // A granted row a guest process owns is the guest's whatever it matches,
    // and it claims its match first, so that a row another owner holds with
    // the same fields (two shared flocks on one file, one the guest's and one
    // a host process's, under `--no-namespace`) cannot claim it too.
    let owned = rows
        .iter()
        .map(|row| !row.waiter && row.owner > 0 && guest_process(row.owner))
        .collect::<Vec<_>>();
    for (row, owned) in rows.iter().zip(&owned) {
        if *owned
            && row.lock.belongs_to_a_description()
            && let Some(count) = unclaimed.get_mut(&row.lock)
        {
            *count = count.saturating_sub(1);
        }
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
            || owned[index]
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

/// A descriptor table, as the ledger names it: a token it hands out, not the
/// kernel's `files_struct` or Detcore's local `FilesId`, so that nothing in
/// the ledger depends on a guest process computing a table's identity the
/// same way the global state does.
type Table = u64;

/// Which guest descriptor tables reference each open file description whose
/// OFD or flock locks Detcore has read, and which locks it holds
/// (`GlobalState::lock_ledger`).
///
/// A description's locks count for `/proc/locks` only while some table
/// references it. Every change comes from the guest's own turns:
/// - a call through a descriptor that reads its fdinfo
///   ([`LockLedger::note_locks`]) records the description's locks and how many
///   slots of the caller's table name it;
/// - a close, `dup2`/`dup3` over a slot or `close_range` of a description
///   Detcore has read the locks of reports how many slots of that table still
///   name it ([`LockLedger::release`]);
/// - a fork copies the parent's table, and `CLONE_FILES` shares it
///   ([`LockLedger::fork`]);
/// - a successful exec leaves the process's table ([`LockLedger::exec`]) and
///   then names the descriptions that survived it ([`LockLedger::exec_kept`]);
///   a failed exec changes nothing;
/// - a table any of whose processes has exited, reaped or not, is dropped
///   ([`LockLedger::prune`]).
///
/// A description whose fdinfo could not be read is followed the same way,
/// with its locks unknown ([`LockLedger::note_unknown`]) until a later read
/// succeeds or no table references it.
///
/// Where it cannot follow a reference exactly, the ledger forgets it, which
/// hides the guest's own row; it never keeps one it has not seen, which would
/// show a host's.
#[derive(Debug, Default)]
pub(crate) struct LockLedger {
    next_table: Table,
    process_table: BTreeMap<DetPid, Table>,
    owners: BTreeMap<Table, BTreeSet<DetPid>>,
    references: BTreeMap<OpenFileId, BTreeMap<Table, u32>>,
    locks: BTreeMap<OpenFileId, DescriptionLocks>,
    after_exec: BTreeMap<DetPid, BTreeMap<OpenFileId, DescriptionLocks>>,
    /// Processes whose descriptor table the ledger cannot follow
    /// ([`LockLedger::mark_opaque`]): a description any of their tables
    /// references contributes no lock.
    opaque: BTreeSet<DetPid>,
    /// Inode numbers, as `/proc/locks` prints them, on which no lock counts
    /// ([`LockLedger::alias_inode`]).
    aliased_inodes: BTreeSet<String>,
    /// No lock counts at all ([`LockLedger::alias_inode`] without an inode).
    aliased_everything: bool,
}

/// The locks a description holds, as its fdinfo last showed them, or why
/// that fdinfo could not be read.
type DescriptionLocks = Result<Vec<HeldLock>, String>;

impl LockLedger {
    /// The table `process` uses, made for it if it has none yet.
    fn table_of(&mut self, process: DetPid) -> Table {
        if let Some(table) = self.process_table.get(&process) {
            return *table;
        }
        let table = self.next_table;
        self.next_table += 1;
        self.process_table.insert(process, table);
        self.owners.entry(table).or_default().insert(process);
        table
    }

    fn set_references(&mut self, open_file: OpenFileId, table: Table, slots: u32) {
        let references = self.references.entry(open_file).or_default();
        if slots == 0 {
            references.remove(&table);
        } else {
            references.insert(table, slots);
        }
        if references.is_empty() {
            self.references.remove(&open_file);
            self.locks.remove(&open_file);
        }
    }

    /// `open_file` holds `locks` (perhaps none), as its fdinfo showed them
    /// after a call through a descriptor of `process`'s table, `slots` of
    /// which name it.
    pub(crate) fn note_locks(
        &mut self,
        process: DetPid,
        open_file: OpenFileId,
        locks: Vec<HeldLock>,
        slots: u32,
    ) {
        self.note(process, open_file, Ok(locks), slots);
    }

    /// As [`Self::note_locks`], for a description whose fdinfo could not be
    /// read, for `reason`: which locks it holds is unknown.
    pub(crate) fn note_unknown(
        &mut self,
        process: DetPid,
        open_file: OpenFileId,
        reason: String,
        slots: u32,
    ) {
        self.note(process, open_file, Err(reason), slots);
    }

    fn note(
        &mut self,
        process: DetPid,
        open_file: OpenFileId,
        locks: DescriptionLocks,
        slots: u32,
    ) {
        let table = self.table_of(process);
        self.set_references(open_file, table, slots);
        if self.references.contains_key(&open_file) {
            self.locks.insert(open_file, locks);
        }
    }

    /// `remaining` slots of `process`'s table name `open_file` after a close
    /// or replacement there. A table the ledger has no record of has no
    /// reference to drop.
    pub(crate) fn release(&mut self, process: DetPid, open_file: OpenFileId, remaining: u32) {
        if let Some(table) = self.process_table.get(&process).copied() {
            self.set_references(open_file, table, remaining);
        }
    }

    /// A new process `child` starts with a copy of `parent`'s table, or,
    /// with `shares_table` (`CLONE_FILES`), with that table itself. Whatever an
    /// earlier process with the same number left is not the child's.
    ///
    /// A table two processes share is one the ledger cannot follow (in-guest
    /// backends count its slots in separate per-process copies), so both
    /// become opaque, as does a child of an opaque process, whose copy may
    /// hold the wrong slots ([`Self::mark_opaque`]).
    pub(crate) fn fork(&mut self, parent: DetPid, child: DetPid, shares_table: bool) {
        self.forget_process(child);
        self.after_exec.remove(&child);
        self.opaque.remove(&child);
        if shares_table {
            self.opaque.insert(parent);
        }
        if shares_table || self.opaque.contains(&parent) {
            self.opaque.insert(child);
        }
        let Some(table) = self.process_table.get(&parent).copied() else {
            return;
        };
        if shares_table {
            self.owners.entry(table).or_default().insert(child);
            self.process_table.insert(child, table);
            return;
        }
        let copy = self.table_of(child);
        for references in self.references.values_mut() {
            if let Some(slots) = references.get(&table).copied() {
                references.insert(copy, slots);
            }
        }
    }

    /// `process` completed an exec: it leaves its table (Linux unshares it
    /// and closes its close-on-exec slots), and only the descriptions
    /// [`Self::exec_kept`] names next remain referenced through it.
    pub(crate) fn exec(&mut self, process: DetPid) {
        let mut kept = BTreeMap::new();
        if let Some(table) = self.process_table.get(&process).copied() {
            for (open_file, references) in &self.references {
                if references.contains_key(&table)
                    && let Some(locks) = self.locks.get(open_file)
                {
                    kept.insert(*open_file, locks.clone());
                }
            }
        }
        self.forget_process(process);
        self.after_exec.insert(process, kept);
    }

    /// The descriptions whose locks Detcore read that `process`'s table names
    /// after the exec [`Self::exec`] recorded, with their slot counts.
    pub(crate) fn exec_kept(&mut self, process: DetPid, surviving: &[(OpenFileId, u32)]) {
        let kept = self.after_exec.remove(&process).unwrap_or_default();
        for (open_file, slots) in surviving {
            let Some(locks) = self
                .locks
                .get(open_file)
                .or_else(|| kept.get(open_file))
                .cloned()
            else {
                continue;
            };
            let table = self.table_of(process);
            self.set_references(*open_file, table, *slots);
            if self.references.contains_key(open_file) {
                self.locks.insert(*open_file, locks);
            }
        }
    }

    /// Drop every table one of whose processes is no longer `alive` (has
    /// exited, reaped or not; a process blocked in a call is alive). A
    /// table processes share (`CLONE_FILES`) goes when the first of them
    /// exits, which can only hide a row the survivors still hold.
    pub(crate) fn prune(&mut self, mut alive: impl FnMut(DetPid) -> bool) {
        self.opaque.retain(|process| alive(*process));
        let gone = self
            .owners
            .iter()
            .filter(|(_, owners)| owners.iter().any(|process| !alive(*process)))
            .map(|(table, _)| *table)
            .collect::<Vec<_>>();
        for table in gone {
            self.drop_table(table);
        }
        let processes = self.after_exec.keys().copied().collect::<Vec<_>>();
        for process in processes {
            if !alive(process) {
                self.after_exec.remove(&process);
            }
        }
    }

    fn forget_process(&mut self, process: DetPid) {
        let Some(table) = self.process_table.remove(&process) else {
            return;
        };
        let empty = self.owners.get_mut(&table).is_none_or(|owners| {
            owners.remove(&process);
            owners.is_empty()
        });
        if empty {
            self.drop_table(table);
        }
    }

    fn drop_table(&mut self, table: Table) {
        if let Some(owners) = self.owners.remove(&table) {
            for process in owners {
                if self.process_table.get(&process) == Some(&table) {
                    self.process_table.remove(&process);
                }
            }
        }
        let open_files = self.references.keys().copied().collect::<Vec<_>>();
        for open_file in open_files {
            self.set_references(open_file, table, 0);
        }
    }

    /// `process`'s descriptor table cannot be followed (one of its threads
    /// has a private table, `CLONE_THREAD` without `CLONE_FILES`, whose calls
    /// the ledger counts as the leader's): while it lives, a description any
    /// of its tables references contributes no lock, which can hide the
    /// guest's row but never admits a host's.
    pub(crate) fn mark_opaque(&mut self, process: DetPid) {
        self.opaque.insert(process);
    }

    /// A lock or unlock went through a descriptor whose description the
    /// ledger may know under another identity, on the file with inode number
    /// `inode` (from that descriptor's fdinfo), so another identity's entry
    /// may be stale: from now on no lock on that inode number counts, on any
    /// device. Without an inode (its fdinfo could not be read), no lock
    /// counts at all.
    pub(crate) fn alias_inode(&mut self, inode: Option<String>) {
        match inode {
            Some(inode) => {
                self.aliased_inodes.insert(inode);
            }
            None => self.aliased_everything = true,
        }
    }

    /// Every lock a description some table references holds, repeats
    /// included. A description whose locks are unknown contributes none, nor
    /// does one an opaque process's table references, nor a lock on an
    /// aliased inode.
    pub(crate) fn held(&self) -> Vec<HeldLock> {
        if self.aliased_everything {
            return Vec::new();
        }
        let opaque_tables = self
            .owners
            .iter()
            .filter(|(_, owners)| owners.iter().any(|process| self.opaque.contains(process)))
            .map(|(table, _)| *table)
            .collect::<BTreeSet<_>>();
        self.locks
            .iter()
            .filter(|(open_file, _)| {
                self.references.get(*open_file).is_some_and(|references| {
                    references
                        .keys()
                        .all(|table| !opaque_tables.contains(table))
                })
            })
            .filter_map(|(_, locks)| locks.as_ref().ok())
            .flat_map(|locks| locks.iter().cloned())
            .filter(|lock| {
                let inode = lock.object.rsplit(':').next().unwrap_or_default();
                !self.aliased_inodes.contains(inode)
            })
            .collect()
    }

    /// Why the locks of a description some table references are unknown, for
    /// the first such description: a `/proc/locks` snapshot taken now cannot
    /// tell that description's rows from a host's, so it is refused. `None`
    /// when every referenced description's locks are known.
    pub(crate) fn unknown(&self) -> Option<&str> {
        self.locks
            .iter()
            .filter(|(open_file, _)| self.references.contains_key(open_file))
            .find_map(|(_, locks)| locks.as_ref().err())
            .map(String::as_str)
    }
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
    fn a_distinct_lock_admits_one_row() {
        // Two shared read locks with the same range on the same file: one the
        // guest's, one a host's. Linux prints the same row twice.
        let contents = b"\
1: OFDLCK ADVISORY READ -1 00:18:7 0 EOF
2: OFDLCK ADVISORY READ -1 00:18:7 0 EOF
";
        let rows = lock_rows(contents).unwrap();
        let held = held_locks_in_fdinfo(b"lock:\t1: OFDLCK ADVISORY READ -1 00:18:7 0 EOF\n");
        assert_eq!(guest_lock_rows(&rows, guest, &held), [true, false]);
        // Listed twice (two guest descriptions, or one the ledger knows under
        // two identities), it still admits one row: never a host's beside it,
        // at the cost of hiding a second identical guest lock.
        let twice = [held.clone(), held].concat();
        assert_eq!(guest_lock_rows(&rows, guest, &twice), [true, false]);
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
    fn a_guest_processs_row_claims_its_match_before_another_owners() {
        // Two shared flocks on one file, the guest's (pid 3) and a host
        // process's (pid 4242), which `--no-namespace` shows both of.
        let held = held_locks_in_fdinfo(b"lock:\t1: FLOCK ADVISORY READ 3 00:2a:5 0 EOF\n");
        for (contents, want) in [
            (
                &b"1: FLOCK ADVISORY READ 3 00:2a:5 0 EOF\n2: FLOCK ADVISORY READ 4242 00:2a:5 0 EOF\n"[..],
                [true, false],
            ),
            (
                &b"1: FLOCK ADVISORY READ 4242 00:2a:5 0 EOF\n2: FLOCK ADVISORY READ 3 00:2a:5 0 EOF\n"[..],
                [false, true],
            ),
        ] {
            let rows = lock_rows(contents).unwrap();
            assert_eq!(guest_lock_rows(&rows, guest, &held), want);
        }
    }

    fn lock(start: &str) -> HeldLock {
        held_locks_in_fdinfo(
            format!("lock:\t1: OFDLCK ADVISORY READ -1 00:18:7 {start} {start}\n").as_bytes(),
        )
        .remove(0)
    }

    fn description(sequence: u64) -> OpenFileId {
        OpenFileId::new(DetPid::from_raw(3), sequence)
    }

    #[test]
    fn a_description_counts_while_a_table_references_it() {
        let [parent, child] = [3, 4].map(DetPid::from_raw);
        let mut ledger = LockLedger::default();
        ledger.note_locks(parent, description(0), vec![lock("0")], 1);
        assert_eq!(ledger.held(), [lock("0")]);
        // An unlock keeps the description referenced, holding nothing.
        ledger.note_locks(parent, description(0), Vec::new(), 1);
        assert!(ledger.held().is_empty());
        ledger.note_locks(parent, description(0), vec![lock("0")], 2);
        // One of two slots closed: still referenced.
        ledger.release(parent, description(0), 1);
        assert_eq!(ledger.held(), [lock("0")]);
        // The fork's copy keeps it after the parent's last close...
        ledger.fork(parent, child, false);
        ledger.release(parent, description(0), 0);
        assert_eq!(ledger.held(), [lock("0")]);
        // ...until the child has exited, reaped or not.
        ledger.prune(|process| process != child);
        assert!(ledger.held().is_empty());
        // A release from a table the ledger never saw changes nothing.
        ledger.release(DetPid::from_raw(9), description(0), 1);
        assert!(ledger.held().is_empty());
    }

    #[test]
    fn a_shared_table_closes_for_every_process_that_shares_it() {
        let [parent, sibling] = [3, 4].map(DetPid::from_raw);
        let mut ledger = LockLedger::default();
        ledger.note_locks(parent, description(0), vec![lock("0")], 1);
        ledger.fork(parent, sibling, true);
        ledger.release(sibling, description(0), 0);
        assert!(ledger.held().is_empty());
        // A shared table goes when the first process sharing it exits.
        ledger.note_locks(parent, description(1), vec![lock("1")], 1);
        ledger.prune(|process| process != sibling);
        assert!(ledger.held().is_empty());
    }

    #[test]
    fn an_exec_keeps_only_the_descriptions_that_survive_it() {
        let process = DetPid::from_raw(3);
        let mut ledger = LockLedger::default();
        ledger.note_locks(process, description(0), vec![lock("0")], 1);
        ledger.note_locks(process, description(1), vec![lock("1")], 1);
        ledger.exec(process);
        assert!(ledger.held().is_empty());
        // Description 1 was close-on-exec.
        ledger.exec_kept(process, &[(description(0), 1)]);
        assert_eq!(ledger.held(), [lock("0")]);
        // An exec that names nothing surviving leaves nothing.
        ledger.exec(process);
        ledger.exec_kept(process, &[]);
        assert!(ledger.held().is_empty());
    }

    #[test]
    fn a_reused_process_number_starts_with_nothing() {
        let [parent, child] = [3, 4].map(DetPid::from_raw);
        let mut ledger = LockLedger::default();
        ledger.note_locks(child, description(0), vec![lock("0")], 1);
        // A new process takes the number of one that held a lock; its parent
        // has no table the ledger knows.
        ledger.fork(parent, child, false);
        assert!(ledger.held().is_empty());
    }

    #[test]
    fn a_reference_taken_before_any_lock_is_not_followed() {
        // The parent opened the file before forking and never called through
        // it; the child locks it and exits. Linux keeps the lock while the
        // parent's slot stays open; the ledger never saw that slot, so the
        // lock is hidden (a named residual).
        let [parent, child] = [3, 4].map(DetPid::from_raw);
        let mut ledger = LockLedger::default();
        ledger.fork(parent, child, false);
        ledger.note_locks(child, description(0), vec![lock("0")], 1);
        assert_eq!(ledger.held(), [lock("0")]);
        ledger.prune(|process| process != child);
        assert!(ledger.held().is_empty());
    }

    #[test]
    fn a_table_the_ledger_cannot_follow_contributes_no_lock() {
        let [parent, sibling, child, other] = [3, 4, 5, 6].map(DetPid::from_raw);
        let mut ledger = LockLedger::default();
        // CLONE_FILES before any lock: both sharers are opaque.
        ledger.fork(parent, sibling, true);
        ledger.note_locks(parent, description(0), vec![lock("0")], 1);
        assert!(ledger.held().is_empty());
        // So is a child forked from an opaque process.
        ledger.fork(parent, child, false);
        ledger.note_locks(child, description(1), vec![lock("1")], 1);
        assert!(ledger.held().is_empty());
        // A process with a private-table thread is opaque until it exits.
        ledger.note_locks(other, description(2), vec![lock("2")], 1);
        assert_eq!(ledger.held(), [lock("2")]);
        ledger.mark_opaque(other);
        assert!(ledger.held().is_empty());
        // A reused number starts transparent.
        ledger.prune(|process| process != other);
        ledger.fork(DetPid::from_raw(9), other, false);
        ledger.note_locks(other, description(3), vec![lock("3")], 1);
        assert_eq!(ledger.held(), [lock("3")]);
    }

    #[test]
    fn an_aliased_inode_contributes_no_lock() {
        let process = DetPid::from_raw(3);
        let mut ledger = LockLedger::default();
        ledger.note_locks(process, description(0), vec![lock("0")], 1);
        ledger.alias_inode(Some("8".to_string()));
        assert_eq!(ledger.held(), [lock("0")]);
        ledger.alias_inode(Some("7".to_string()));
        assert!(ledger.held().is_empty());
        let mut ledger = LockLedger::default();
        ledger.note_locks(process, description(0), vec![lock("0")], 1);
        ledger.alias_inode(None);
        assert!(ledger.held().is_empty());
    }

    #[test]
    fn unknown_locks_count_while_a_table_references_them() {
        let [parent, child] = [3, 4].map(DetPid::from_raw);
        let mut ledger = LockLedger::default();
        ledger.note_unknown(parent, description(0), "EMFILE".to_string(), 1);
        assert_eq!(ledger.unknown(), Some("EMFILE"));
        assert!(ledger.held().is_empty());
        // A fork's copy references it too; the parent's close leaves it
        // unknown until the child's table goes as well.
        ledger.fork(parent, child, false);
        ledger.release(parent, description(0), 0);
        assert_eq!(ledger.unknown(), Some("EMFILE"));
        ledger.prune(|process| process != child);
        assert_eq!(ledger.unknown(), None);
        // A later read that succeeds makes the locks known again.
        ledger.note_unknown(parent, description(1), "ENOENT".to_string(), 1);
        ledger.note_locks(parent, description(1), vec![lock("0")], 1);
        assert_eq!(ledger.unknown(), None);
        assert_eq!(ledger.held(), [lock("0")]);
        // An exec keeps an unknown description its table still names.
        ledger.note_unknown(parent, description(1), "EACCES".to_string(), 1);
        ledger.exec(parent);
        assert_eq!(ledger.unknown(), None);
        ledger.exec_kept(parent, &[(description(1), 1)]);
        assert_eq!(ledger.unknown(), Some("EACCES"));
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
