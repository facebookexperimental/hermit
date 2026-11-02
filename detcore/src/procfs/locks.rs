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
//! - an OFD row whose class, mode, access, object and range are, field for
//!   field, a lock a guest description some guest table references holds, at
//!   most one row per distinct such lock. An OFD row has no owning task (owner
//!   `-1` from every namespace), so this field match is the only way to tell
//!   the guest's from a host's;
//! - a row owned by a positive pid that names a process of the guest, as the
//!   container's `/proc` lists it (a live process, or a zombie until it is
//!   reaped), translated from the kernel's numbering where a backend's differs
//!   (`Scheduler::procfs_ids`). This is the ONLY rule that keeps a POSIX or a
//!   flock row: Linux prints a flock row with the pid of the process that took
//!   it, so a flock row is kept when that pid is a guest process's. This is
//!   guest-pid membership, not proof of identity: a pid says who holds that
//!   number now, not who took the lock (see the host-row cases below). A
//!   field match against a description's recorded flock is not
//!   used: that record can go stale where Detcore does not see the unlock (a
//!   copied DBT `vfork` child runs `LOCK_UN` natively), and a stale record
//!   would admit a host process's identical flock where the guest's procfs is
//!   not namespace-private (`--no-namespace`). This also hides another
//!   process's POSIX and flock rows there;
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
//! What still differs from Linux, named rather than hidden. Three cases can
//! show a host row (the first, and the two pid-identity cases right after
//! it); every other case hides a row of the guest's own, and none shows a
//! host row:
//! - a host OFD lock whose row is, field for field, an OFD lock a guest
//!   description holds now (in practice a shared read lock on the same range
//!   of the same file, or a file in another btrfs subvolume, whose rows print
//!   the one superblock device, with the same inode number, range and access)
//!   takes the place of the guest's row, at most once per distinct such lock:
//!   the row shown is the same either way;
//! - a host flock whose taker has exited, its description kept by another
//!   host process, prints the taker's freed pid under `--no-namespace`; when
//!   that number is later given to a guest process, the row is kept as the
//!   guest's. When the timing of that reuse is the host's, so is the row;
//! - on DBT, when a guest process is killed from outside and reaped
//!   automatically before Detcore retires its pid, a host process can take
//!   that number while it is still in Detcore's list of guest processes, and
//!   that host process's POSIX or flock rows are kept until it is retired;
//! - two guest descriptions holding identical shared read OFD locks (same
//!   file, range and access) show one row, where Linux shows two;
//! - the guest's own OFD waiter rows (`->` with owner -1) are not shown:
//!   fdinfo lists no waiters, so Detcore cannot tell them from a host's;
//! - an OFD lock the guest's descriptions held before Detcore read it is not
//!   shown until a call through a modelled descriptor of it: a description
//!   inherited already locked at the start; one received over `SCM_RIGHTS`
//!   where Detcore keeps no model of the receiver's descriptor (not recorded
//!   under record and replay, or without sequentialized threads on ptrace
//!   and KVM), once the sender's tables let go of it; one received through
//!   `recvmmsg`; one an
//!   in-guest backend's exec rebuilds under a new identity;
//! - a flock whose taker is not a guest process, while a guest description
//!   holds it: a taker that has exited (its descriptor was inherited), or one
//!   outside the guest from the start (a launcher that took the flock and
//!   handed the guest that description). Linux keeps the original taker's
//!   pid in the row, even after the guest itself takes the same mode on the
//!   description (a same-mode flock changes nothing), so the row names no
//!   guest process and is hidden, whatever call the guest makes. Inside a
//!   PID namespace Linux hides such a row itself (the taker is outside the
//!   namespace, or reaped), so the visible difference is only under
//!   `--no-namespace`; a zombie taker, which the guest's `/proc` still lists,
//!   stays shown;
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
//!   such as a descriptor received over `SCM_RIGHTS` and not recorded, or one
//!   registered on reception, a second model of the sender's description), no
//!   lock on that inode number,
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
//! the exec; and an OFD lock change the guest makes without Detcore seeing the
//! call at all. DBT refuses the one known case, a copied `vfork` child's
//! `F_OFD_SETLK`/`F_OFD_SETLKW`, at the call, and also refuses the `vfork`
//! up front while the parent's own descriptions may hold an OFD lock. A
//! flock change Detcore does not see (a copied child's native `LOCK_UN` or
//! `LOCK_NB`) can leave a description's recorded flock stale, but that record
//! admits no row: flock rows are kept by their taker's pid.

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
        self.class == "OFDLCK" || self.is_flock()
    }

    /// Whether this is a flock, which Linux prints with its taker's pid.
    fn is_flock(&self) -> bool {
        self.class == "FLOCK"
    }

    /// Whether a row of this lock is kept by matching it, field for field,
    /// against a lock a guest description holds: an OFD lock only, whose row
    /// has no owner to decide by. A flock row is kept only by its taker's pid
    /// (see the module comment).
    fn kept_by_field_match(&self) -> bool {
        self.belongs_to_a_description() && !self.is_flock()
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
    // the same fields cannot claim it too. Only an OFD lock is kept by field
    // match, and an OFD row has no owner, so in practice this claims nothing;
    // it keeps the rule whole should another field-matched class appear.
    let owned = rows
        .iter()
        .map(|row| !row.waiter && row.owner > 0 && guest_process(row.owner))
        .collect::<Vec<_>>();
    for (row, owned) in rows.iter().zip(&owned) {
        if *owned
            && row.lock.kept_by_field_match()
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
            || (row.lock.kept_by_field_match()
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
/// - a successful exec leaves the process's table ([`LockLedger::exec`]) for
///   a new one, which holds the descriptions the exec names as survivors
///   next ([`LockLedger::exec_kept`]) and, until then, every description of
///   the old table that holds a `flock` on a placeholder directory, so that
///   no such lock a survivor holds is let go on the way; a failed exec
///   changes nothing;
/// - a table any of whose processes has exited, reaped or not, counts no
///   longer for `/proc/locks`, and goes once every process that holds it has
///   exited ([`LockLedger::prune`]).
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
    /// The locks the descriptions of a process's table held when it
    /// completed an exec ([`LockLedger::exec`]), for those its new table names
    /// as survivors ([`LockLedger::exec_kept`]).
    after_exec: BTreeMap<DetPid, BTreeMap<OpenFileId, DescriptionLocks>>,
    /// Processes whose new table, after an exec ([`LockLedger::exec`]),
    /// still references the descriptions of the old one that hold a `flock`
    /// on a placeholder directory, until the exec names its survivors
    /// ([`LockLedger::exec_kept`]), which releases the others then.
    exec_pending: BTreeSet<DetPid>,
    /// Processes whose descriptor table the ledger cannot follow
    /// ([`LockLedger::mark_opaque`]): a description any of their tables
    /// references contributes no lock.
    opaque: BTreeSet<DetPid>,
    /// Inode numbers, as `/proc/locks` prints them, on which no lock counts
    /// ([`LockLedger::alias_inode`]).
    aliased_inodes: BTreeSet<String>,
    /// No lock counts at all ([`LockLedger::alias_inode`] without an inode).
    aliased_everything: bool,
    /// Why no `/proc/locks` snapshot can be decided for the rest of the run
    /// ([`LockLedger::refuse`]): set once, never cleared.
    refused: Option<String>,
    /// Tables one of whose processes has exited while another still holds
    /// them (`CLONE_FILES` without `CLONE_THREAD`): they count no longer for
    /// `/proc/locks` ([`LockLedger::held`], [`LockLedger::unknown`]), which
    /// can only hide a row the survivors hold, and their references still
    /// hold the descriptions' `flock` locks on placeholder directories, as
    /// the survivors' table holds them on Linux, until they exit too
    /// ([`LockLedger::prune`]).
    departed: BTreeSet<Table>,
    /// The `flock` locks held on the `/proc` directories Detcore serves on
    /// placeholder descriptors ([`LockLedger::placeholder_flock`]), by
    /// directory, each held by an open file description some table
    /// references, followed as the ledger follows every description's
    /// references: a description no table references any more holds none,
    /// as Linux's last `fput` of it releases its locks (`locks_remove_file`).
    placeholder_flocks:
        BTreeMap<crate::procfs::fd_directory::PlaceholderLockKey, BTreeMap<OpenFileId, FlockMode>>,
    /// Processes whose descriptor tables the ledger cannot tell apart, for
    /// their whole life: one of their threads got a table of its own
    /// (`CLONE_THREAD` without `CLONE_FILES`, [`LockLedger::mark_private_tables`]),
    /// whose calls the ledger counts against the process's one table, or they
    /// were forked from such a process, whose copied table may be either's.
    /// Their placeholder `flock` requests are refused by name, their releases
    /// of a description's last reference are not taken (a slot of another of
    /// their tables may still hold it), and a lock that conflicts with one a
    /// description their tables reference holds is refused by name rather
    /// than answered ([`PlaceholderFlockOutcome::Unfollowable`]).
    tainted: BTreeSet<DetPid>,
    /// Copies of placeholder descriptions in flight: sent over `SCM_RIGHTS`
    /// and not received yet, by description ([`LockLedger::placeholder_copies`]).
    /// Linux keeps a description, and its `flock` locks, while a message
    /// that holds it is queued; the ledger lets no description hold a
    /// placeholder `flock` while a copy of it is in flight (a send of a
    /// locked one, and a lock of one in flight, are refused by name), so no
    /// lock rests on a reference only a queued message holds.
    in_flight: BTreeMap<OpenFileId, u32>,
    /// The write hint of each placeholder directory's inode whose hint a
    /// guest set (`F_SET_RW_HINT`, [`LockLedger::placeholder_write_hint`]).
    placeholder_write_hints: BTreeMap<crate::procfs::fd_directory::PlaceholderLockKey, u64>,
}

/// A change to the copies of placeholder descriptions in flight over
/// `SCM_RIGHTS` ([`LockLedger::placeholder_copies`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlaceholderCopies {
    /// A send of these descriptions is about to be made (one per copy).
    Sending(Vec<OpenFileId>),
    /// That send failed: its copies are not in flight.
    Unsent(Vec<OpenFileId>),
    /// A receive delivered a copy of this description.
    Received(OpenFileId),
}

/// The mode of a `flock` lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FlockMode {
    /// `LOCK_SH`.
    Shared,
    /// `LOCK_EX`.
    Exclusive,
}

/// A `flock` through a placeholder descriptor
/// ([`LockLedger::placeholder_flock`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaceholderFlock {
    /// The directory the descriptor stands for.
    pub key: crate::procfs::fd_directory::PlaceholderLockKey,
    /// The descriptor's open file description, which owns the lock.
    pub open_file: OpenFileId,
    /// How many slots of the caller's table name the description.
    pub slots: u32,
    /// The lock asked for, or `None` for `LOCK_UN`.
    pub mode: Option<FlockMode>,
    /// `LOCK_NB`.
    pub nonblocking: bool,
}

/// What [`LockLedger::placeholder_flock`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlaceholderFlockOutcome {
    /// Granted, released, or already held in that mode.
    Done,
    /// Another description holds a conflicting lock and the caller asked not
    /// to wait: `EWOULDBLOCK`. A lock of another mode the description held
    /// is gone, as Linux's conversion deletes it before it looks for a
    /// conflict (`flock_lock_inode`).
    WouldBlock,
    /// Another description holds a conflicting lock and the caller asked to
    /// wait, which Detcore does not model: refused, with the description's
    /// earlier lock kept, as `Detcore::handle_flock` refuses a contended
    /// blocking `flock`.
    Contended,
    /// The caller's process holds descriptor tables the ledger cannot tell
    /// apart, or the lock conflicts with one held by a description such a
    /// process's tables reference ([`LockLedger::tainted`]), whose last
    /// reference the ledger could not place: refused by name.
    Unfollowable,
    /// A copy of the description is in flight over `SCM_RIGHTS`
    /// ([`LockLedger::in_flight`]): refused by name.
    InFlight,
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
            // Linux releases a description's `flock` locks with its last
            // reference (`locks_remove_file`, from `__fput`).
            self.placeholder_flocks.retain(|_, holders| {
                holders.remove(&open_file);
                !holders.is_empty()
            });
        }
    }

    /// A `flock` through a descriptor of `process`'s table that Detcore
    /// serves as a `/proc` directory on a placeholder
    /// (`procfs::fd_directory::PlaceholderDirectory`). Every placeholder
    /// descriptor is on the one inode of the placeholder directory, where
    /// Linux gives procfs's root and each task's descriptor directory an
    /// inode of its own, so the locks are kept here, by directory, as Linux's
    /// `flock_lock_inode` keeps them on each inode: a description holds at
    /// most one lock per directory; asking again for the mode it holds
    /// changes nothing; another mode deletes the one it holds first; a lock
    /// conflicts with another description's exclusive lock, and an exclusive
    /// one with any. The description is followed as every description whose
    /// locks the ledger knows (it holds no `/proc/locks` row), so its locks go
    /// with its last reference, and survive an exec its descriptor survives.
    pub(crate) fn placeholder_flock(
        &mut self,
        process: DetPid,
        request: PlaceholderFlock,
    ) -> PlaceholderFlockOutcome {
        if request.mode.is_some() && self.tainted.contains(&process) {
            return PlaceholderFlockOutcome::Unfollowable;
        }
        if request.mode.is_some() && self.in_flight.contains_key(&request.open_file) {
            return PlaceholderFlockOutcome::InFlight;
        }
        self.note(process, request.open_file, Ok(Vec::new()), request.slots);
        let held = self
            .placeholder_flocks
            .get(&request.key)
            .and_then(|holders| holders.get(&request.open_file))
            .copied();
        let outcome = match request.mode {
            None => {
                if let Some(holders) = self.placeholder_flocks.get_mut(&request.key) {
                    holders.remove(&request.open_file);
                }
                PlaceholderFlockOutcome::Done
            }
            Some(mode) if held == Some(mode) => PlaceholderFlockOutcome::Done,
            Some(mode) => {
                let conflicting = self
                    .placeholder_flocks
                    .get(&request.key)
                    .into_iter()
                    .flatten()
                    .filter(|(open_file, other)| {
                        **open_file != request.open_file
                            && (mode == FlockMode::Exclusive || **other == FlockMode::Exclusive)
                    })
                    .map(|(open_file, _)| *open_file)
                    .collect::<Vec<_>>();
                if conflicting
                    .iter()
                    .any(|open_file| self.referenced_by_tainted(*open_file))
                {
                    // Whether that description still holds its lock is
                    // unknown: its last reference may have gone with a table
                    // the ledger cannot follow. Nothing changes.
                    return PlaceholderFlockOutcome::Unfollowable;
                }
                let holders = self.placeholder_flocks.entry(request.key).or_default();
                holders.remove(&request.open_file);
                if conflicting.is_empty() {
                    holders.insert(request.open_file, mode);
                    PlaceholderFlockOutcome::Done
                } else if request.nonblocking {
                    PlaceholderFlockOutcome::WouldBlock
                } else {
                    if let Some(held) = held {
                        holders.insert(request.open_file, held);
                    }
                    PlaceholderFlockOutcome::Contended
                }
            }
        };
        self.placeholder_flocks
            .retain(|_, holders| !holders.is_empty());
        outcome
    }

    /// Whether a table of a process the ledger cannot follow
    /// ([`Self::tainted`]) references `open_file`.
    fn referenced_by_tainted(&self, open_file: OpenFileId) -> bool {
        self.references.get(&open_file).is_some_and(|tables| {
            tables.keys().any(|table| {
                self.owners
                    .get(table)
                    .is_some_and(|owners| owners.iter().any(|owner| self.tainted.contains(owner)))
            })
        })
    }

    /// A change to the copies of placeholder descriptions in flight over
    /// `SCM_RIGHTS` ([`Self::in_flight`]): a send counts its copies, unless a
    /// description it sends holds a placeholder `flock`, which it names
    /// (`Err`) for the sender to refuse by name; a failed send uncounts them,
    /// and a receive its copy. A copy that is never received stays counted,
    /// so that its description takes no placeholder lock for the rest of the
    /// run (refused by name).
    pub(crate) fn placeholder_copies(
        &mut self,
        change: PlaceholderCopies,
    ) -> Result<(), OpenFileId> {
        match change {
            PlaceholderCopies::Sending(open_files) => {
                if let Some(locked) = open_files
                    .iter()
                    .find(|open_file| self.holds_placeholder_flock(**open_file))
                {
                    return Err(*locked);
                }
                for open_file in open_files {
                    *self.in_flight.entry(open_file).or_default() += 1;
                }
            }
            PlaceholderCopies::Unsent(open_files) => {
                for open_file in open_files {
                    self.uncount_copy(open_file);
                }
            }
            PlaceholderCopies::Received(open_file) => self.uncount_copy(open_file),
        }
        Ok(())
    }

    fn uncount_copy(&mut self, open_file: OpenFileId) {
        if let Some(count) = self.in_flight.get_mut(&open_file) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.in_flight.remove(&open_file);
            }
        }
    }

    /// Set the write hint of the inode of the placeholder directory `key` to
    /// `hint`, or read it with `None`; the hint after the request. Linux keeps
    /// a write hint on the inode (`inode->i_write_hint`, `fcntl_rw_hint`),
    /// shared by every open of the directory, 0 (`RWH_WRITE_LIFE_NOT_SET`) on
    /// a new one; Detcore keeps it for the directory's task incarnation (its
    /// key), where Linux keeps it while the inode is cached (named
    /// approximation).
    pub(crate) fn placeholder_write_hint(
        &mut self,
        key: crate::procfs::fd_directory::PlaceholderLockKey,
        hint: Option<u64>,
    ) -> u64 {
        if let Some(hint) = hint {
            self.placeholder_write_hints.insert(key, hint);
        }
        self.placeholder_write_hints.get(&key).copied().unwrap_or(0)
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
        // A process whose tables the ledger cannot tell apart may hold the
        // description in another of its tables: its last-reference releases
        // are not taken, and the description stays referenced until the
        // process exits ([`Self::tainted`]; a conflict with a lock it holds is
        // then refused by name, never answered from a reference that may be
        // gone).
        if remaining == 0 && self.tainted.contains(&process) {
            return;
        }
        if let Some(table) = self.process_table.get(&process).copied() {
            self.set_references(open_file, table, remaining);
        }
    }

    /// Whether `open_file` holds a `flock` on a placeholder directory.
    fn holds_placeholder_flock(&self, open_file: OpenFileId) -> bool {
        self.placeholder_flocks
            .values()
            .any(|holders| holders.contains_key(&open_file))
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
        self.exec_pending.remove(&child);
        self.opaque.remove(&child);
        self.tainted.remove(&child);
        if self.tainted.contains(&parent) {
            self.tainted.insert(child);
        }
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
    /// and closes its close-on-exec slots) for a new one, through which only
    /// the descriptions [`Self::exec_kept`] names next stay referenced. A
    /// description of the old table that holds a `flock` on a placeholder
    /// directory is referenced through the new one until then, with its slot
    /// count, so that a survivor's lock is never let go between the two and
    /// another process cannot take a conflicting one (on KVM, without
    /// sequentialized threads, two exclusive locks); `exec_kept` releases the
    /// ones the exec closed. Any other description's locks are kept aside for
    /// `exec_kept`: a backend that rediscovers its descriptor table after an
    /// exec (the in-guest backends, where no placeholder is served) names no
    /// survivor, and the exec then leaves none of them referenced.
    pub(crate) fn exec(&mut self, process: DetPid) {
        let mut kept = BTreeMap::new();
        let mut held = Vec::new();
        if let Some(table) = self.process_table.get(&process).copied() {
            for (open_file, references) in &self.references {
                let Some(slots) = references.get(&table) else {
                    continue;
                };
                if self.holds_placeholder_flock(*open_file) {
                    held.push((*open_file, *slots));
                } else if let Some(locks) = self.locks.get(open_file) {
                    kept.insert(*open_file, locks.clone());
                }
            }
        }
        let table = self.next_table;
        self.next_table += 1;
        for (open_file, slots) in &held {
            if let Some(references) = self.references.get_mut(open_file) {
                references.insert(table, *slots);
            }
        }
        self.forget_process(process);
        self.process_table.insert(process, table);
        self.owners.entry(table).or_default().insert(process);
        self.after_exec.insert(process, kept);
        if held.is_empty() {
            self.exec_pending.remove(&process);
        } else {
            self.exec_pending.insert(process);
        }
    }

    /// The descriptions whose locks Detcore follows that `process`'s table
    /// names after the exec [`Self::exec`] recorded, with their slot counts:
    /// each is referenced through the new table with its locks, and a
    /// description that held a placeholder `flock` and is not among them,
    /// one the exec closed, is released.
    pub(crate) fn exec_kept(&mut self, process: DetPid, surviving: &[(OpenFileId, u32)]) {
        let kept = self.after_exec.remove(&process).unwrap_or_default();
        let table = self.table_of(process);
        if self.exec_pending.remove(&process) {
            let closed = self
                .references
                .iter()
                .filter(|(open_file, references)| {
                    references.contains_key(&table)
                        && !surviving
                            .iter()
                            .any(|(survivor, slots)| survivor == *open_file && *slots > 0)
                })
                .map(|(open_file, _)| *open_file)
                .collect::<Vec<_>>();
            for open_file in closed {
                self.set_references(open_file, table, 0);
            }
        }
        for (open_file, slots) in surviving {
            let Some(locks) = self
                .locks
                .get(open_file)
                .or_else(|| kept.get(open_file))
                .cloned()
            else {
                continue;
            };
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
        self.tainted.retain(|process| alive(*process));
        self.after_exec.retain(|process, _| alive(*process));
        self.exec_pending.retain(|process| alive(*process));
        let mut gone = Vec::new();
        for (table, owners) in &self.owners {
            let living = owners.iter().filter(|process| alive(**process)).count();
            if living == 0 {
                gone.push(*table);
            } else if living < owners.len() {
                self.departed.insert(*table);
            }
        }
        for table in gone {
            self.drop_table(table);
        }
        // A process that exited holds its table no longer; a later process
        // with its number starts with a table of its own.
        let exited = self
            .process_table
            .keys()
            .copied()
            .filter(|process| !alive(*process))
            .collect::<Vec<_>>();
        for process in exited {
            self.forget_process(process);
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
        self.departed.remove(&table);
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

    /// `process` has a thread with a descriptor table of its own
    /// (`CLONE_THREAD` without `CLONE_FILES`): its tables cannot be told
    /// apart for the rest of its life, for `/proc/locks`
    /// ([`Self::mark_opaque`]) and for the placeholder `flock` locks its
    /// descriptions hold ([`Self::tainted`]).
    pub(crate) fn mark_private_tables(&mut self, process: DetPid) {
        self.mark_opaque(process);
        self.tainted.insert(process);
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
                    let counted = references
                        .keys()
                        .filter(|table| !self.departed.contains(table))
                        .collect::<Vec<_>>();
                    !counted.is_empty()
                        && counted.iter().all(|table| !opaque_tables.contains(table))
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

    /// A lock or unlock went through a descriptor whose description the
    /// ledger may know under another identity, and its fdinfo could not be
    /// read for `reason` (ambient or resource state): which identities it
    /// changed is unknown, so every later `/proc/locks` snapshot is refused,
    /// by the first such reason. Hiding every lock instead would make the
    /// guest's rows depend on a transient host state.
    pub(crate) fn refuse(&mut self, reason: String) {
        self.refused.get_or_insert(reason);
    }

    /// Why a `/proc/locks` snapshot taken now cannot be decided: the run's
    /// refusal ([`Self::refuse`]), or else the reason the locks of a
    /// description some table references are unknown, for the first such
    /// description, whose rows a snapshot cannot tell from a host's. `None`
    /// when every referenced description's locks are known.
    pub(crate) fn unknown(&self) -> Option<&str> {
        if let Some(reason) = &self.refused {
            return Some(reason);
        }
        self.locks
            .iter()
            .filter(|(open_file, _)| {
                self.references.get(*open_file).is_some_and(|references| {
                    references
                        .keys()
                        .any(|table| !self.departed.contains(table))
                })
            })
            .find_map(|(_, locks)| locks.as_ref().err())
            .map(String::as_str)
    }
}

/// The smallest buffer a kernel `seq_file` such as `/proc/locks` fills in one
/// `read`: one page (`seq_read_iter` starts with `PAGE_SIZE` and only grows).
/// 4096 is the smallest page Linux uses, so a larger one only makes the rule
/// in [`locks_capture_without_repeats`] keep more.
const SEQ_FILE_MIN_BUFFER_BYTES: usize = 4096;

/// A `/proc/locks` capture, `contents`, without the records that the kernel
/// printed again after the end of its list, given the lengths of the
/// capture's nonzero reads, in order, each through a buffer of `chunk` bytes,
/// and the read that returned 0 after them.
///
/// After the first read whose length shows it reached the end of the
/// kernel's list in its own walk, each record a later read returned is
/// dropped, its waiter lines with it, when its granted lock is a lock already
/// in the capture ([`lock_identity`]). Every other record is kept, where it
/// came. When no read passes the length test, or the bytes after it are not
/// whole records, the capture is returned as the kernel gave it.
///
/// Why repeats appear. Each `read` of a `seq_file` restarts the walk of the
/// kernel's global lock list at the index where the last one stopped
/// (`start(m->index)`). The list is a list per CPU, walked in CPU order, each
/// newest lock first. When the host adds locks ahead of that index between
/// two reads, the restarted walk prints the records that slid past it: the
/// last records the previous read returned, renumbered, and any added lock
/// that landed between the index and them (a lock added on the same CPU as
/// the guest's, ahead of the guest's own rows). The added locks are real and
/// stay; the repeats go. A repeat need not be byte for byte the record it
/// repeats: between the reads the host can queue a waiter behind the lock or
/// give one up, a lease can start breaking, and a waiter's owner can leave
/// the reader's view. None of that moves the list's index, and it is still
/// the one lock, so a repeat is recognized by its granted lock alone, and
/// the earlier record, which one walk printed, is the one kept.
///
/// The length test. One read stops before the end of the list only when the
/// caller's buffer is full (it returns `chunk` bytes) or when the next record
/// does not fit in the kernel's buffer of at least
/// [`SEQ_FILE_MIN_BUFFER_BYTES`]. A record is a granted lock and every waiter
/// queued behind it, so it has no fixed bound; but that record is the first
/// thing the next read prints, so while the list stays still, the reads after
/// a read of `n < chunk` bytes that stopped there return at least
/// `SEQ_FILE_MIN_BUFFER_BYTES - n` bytes.
///
/// Why the length test alone is not enough. The list does not stay still: the
/// record that stopped a read can lose its waiters or go between the reads,
/// and a removal ahead of the index moves the walk. The next read then
/// returns records nobody has read yet, few enough bytes to pass the length
/// test. So only repeats are dropped.
///
/// What this can still get wrong, named. A later record whose granted lock
/// prints the same identity as a lock already captured, and which is
/// genuinely another lock, is dropped, whenever the bytes after the end are
/// shorter than the length test allows: either the list changed between the
/// reads in a way the length test cannot see and the record was never read,
/// or the host added it. Identical identities of distinct locks exist
/// natively, per class:
/// - POSIX: one owner's locks on two files in two btrfs subvolumes with the
///   same inode number (every subvolume prints its one superblock device);
///   two descriptor tables of one thread group (identical shared locks);
/// - FLOCK: two descriptions under one pid holding identical shared flocks,
///   and the btrfs case;
/// - OFD: two descriptions holding identical shared ranges (the guest filter
///   already shows such a lock once), and the btrfs case;
/// - LEASE and DELEG: two descriptions under one pid holding identical
///   leases, and the btrfs case.
///
/// The snapshot then shows one of two identical rows. A record added by the
/// host that matches a guest lock is the host's, and the guest filter would
/// keep at most one row for it anyway. Content the kernel returns over
/// several reads stays exposed to the host's changes at every boundary before
/// the last, and duplicates are kept whenever no read passes the length test.
///
/// Matching only the records that could have slid past the index (the last
/// few of the capture) would narrow the residual, but it is not provably
/// enough: a removal between the reads can drop one of those records and let
/// an earlier one slide in, so the whole capture is searched.
pub(crate) fn locks_capture_without_repeats(
    contents: &[u8],
    reads: &[usize],
    chunk: usize,
) -> Vec<u8> {
    if reads.iter().sum::<usize>() != contents.len() {
        return contents.to_vec();
    }
    let mut later = contents.len();
    let mut end = 0;
    for &read in reads {
        end += read;
        later -= read;
        if later > 0 && read < chunk && read + later < SEQ_FILE_MIN_BUFFER_BYTES {
            return without_repeats(contents, end).unwrap_or_else(|| contents.to_vec());
        }
    }
    contents.to_vec()
}

/// `contents` with each record past `end` dropped whose granted lock is one
/// an earlier record holds; `None` when the bytes are not whole records.
fn without_repeats(contents: &[u8], end: usize) -> Option<Vec<u8>> {
    let mut seen = lock_records(&contents[..end])?
        .into_iter()
        .map(|(identity, _)| identity)
        .collect::<BTreeSet<_>>();
    let mut out = contents[..end].to_vec();
    for (identity, bytes) in lock_records(&contents[end..])? {
        if seen.insert(identity) {
            out.extend_from_slice(bytes.as_bytes());
        }
    }
    Some(out)
}

/// `contents` as whole `/proc/locks` records, each a granted lock's line and
/// the lines of the waiters queued behind it: the identity of the record's
/// granted lock ([`lock_identity`]) and the record's bytes. A waiter's line is
/// `SEQ: -> ...`, indented between the colon and the arrow by its depth in the
/// waiter tree (`lock_get_status`). `None` when the bytes are not whole lines
/// that each begin with a sequence number, or begin with a waiter.
fn lock_records(contents: &[u8]) -> Option<Vec<(Vec<String>, &str)>> {
    let text = std::str::from_utf8(contents).ok()?;
    if !text.is_empty() && !text.ends_with('\n') {
        return None;
    }
    let mut records: Vec<(Vec<String>, std::ops::Range<usize>)> = Vec::new();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let (sequence, rest) = line.split_once(':')?;
        if sequence.is_empty() || !sequence.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let span = offset..offset + line.len();
        offset = span.end;
        if rest.trim_start().starts_with("->") {
            records.last_mut()?.1.end = span.end;
        } else {
            records.push((lock_identity(rest), span));
        }
    }
    Some(
        records
            .into_iter()
            .map(|(identity, bytes)| (identity, &text[bytes]))
            .collect(),
    )
}

/// The fields of a granted lock's `/proc/locks` line, its `SEQ:` removed,
/// that stay the same while it is the one lock: every field
/// (`CLASS MODE ACCESS PID MAJOR:MINOR:INODE START END`) for POSIX, OFD,
/// FLOCK and the rest; for a lease or delegation (`LEASE`/`DELEG`), all but
/// its state (`ACTIVE`, `BREAKING`, `BREAKER`) and its access, which a break
/// changes to the type it is breaking to (`target_leasetype`).
fn lock_identity(line: &str) -> Vec<String> {
    let mut fields = line
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if matches!(fields.first().map(String::as_str), Some("LEASE" | "DELEG")) && fields.len() > 3 {
        fields.drain(1..3);
    }
    fields
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
                // The flock a guest description holds, whose taker (4242) is
                // gone: hidden, since a flock row is kept only by its taker's
                // pid. A declared residual (module comment); Linux hides it too
                // under a PID namespace once the taker is reaped.
                false, // that flock
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

    /// A description's recorded flock does not admit a row by its fields: a
    /// copied DBT `vfork` child can release it natively, leaving the record
    /// stale, and a host process's identical shared flock (shown under
    /// `--no-namespace`) would then pass for the guest's. Only the taker's
    /// pid keeps a flock row.
    #[test]
    fn a_recorded_flock_admits_no_row_whose_taker_is_not_the_guests() {
        let held = held_locks_in_fdinfo(b"lock:\t1: FLOCK ADVISORY READ 3 00:2a:5 0 EOF\n");
        // The guest's lock is gone (released natively); the host's remains.
        let rows = lock_rows(b"1: FLOCK ADVISORY READ 4242 00:2a:5 0 EOF\n").unwrap();
        assert_eq!(guest_lock_rows(&rows, guest, &held), [false]);
        // A flock its guest taker holds is shown whether or not it was recorded.
        let rows = lock_rows(b"1: FLOCK ADVISORY READ 3 00:2a:5 0 EOF\n").unwrap();
        assert_eq!(guest_lock_rows(&rows, guest, &held), [true]);
        assert_eq!(guest_lock_rows(&rows, guest, &[]), [true]);
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

    /// `flock` through placeholder descriptors locks each directory apart,
    /// as Linux's separate inodes do: two descriptions conflict only on the
    /// same directory, a description re-asking for its mode keeps it, a
    /// refused blocking conversion keeps the earlier lock, a refused
    /// nonblocking one loses it, and the last close releases the lock.
    #[test]
    fn placeholder_flocks_lock_each_directory_apart() {
        use crate::procfs::fd_directory::DescriptorDirectoryTable;
        use crate::procfs::fd_directory::PlaceholderLockKey;
        let process = DetPid::from_raw(3);
        let fd_directory = PlaceholderLockKey::Descriptors {
            table: DescriptorDirectoryTable::Process,
            pid: 3,
            process_incarnation: 1,
            thread: None,
        };
        let flock = |key, open_file, slots, mode, nonblocking| PlaceholderFlock {
            key,
            open_file,
            slots,
            mode,
            nonblocking,
        };
        let (shared, exclusive) = (Some(FlockMode::Shared), Some(FlockMode::Exclusive));
        let mut ledger = LockLedger::default();
        let root = PlaceholderLockKey::Root;
        assert_eq!(
            ledger.placeholder_flock(process, flock(root, description(0), 1, exclusive, true)),
            PlaceholderFlockOutcome::Done
        );
        // Another directory is another inode: no conflict.
        assert_eq!(
            ledger.placeholder_flock(
                process,
                flock(fd_directory, description(1), 1, exclusive, true)
            ),
            PlaceholderFlockOutcome::Done
        );
        // The same directory through another description conflicts.
        assert_eq!(
            ledger.placeholder_flock(process, flock(root, description(2), 1, shared, true)),
            PlaceholderFlockOutcome::WouldBlock
        );
        assert_eq!(
            ledger.placeholder_flock(process, flock(root, description(2), 1, shared, false)),
            PlaceholderFlockOutcome::Contended
        );
        // The holder asking again changes nothing.
        assert_eq!(
            ledger.placeholder_flock(process, flock(root, description(0), 2, exclusive, true)),
            PlaceholderFlockOutcome::Done
        );
        // Shared locks share.
        assert_eq!(
            ledger.placeholder_flock(process, flock(root, description(0), 2, shared, true)),
            PlaceholderFlockOutcome::Done
        );
        assert_eq!(
            ledger.placeholder_flock(process, flock(root, description(2), 1, shared, true)),
            PlaceholderFlockOutcome::Done
        );
        // A refused blocking conversion keeps the shared lock...
        assert_eq!(
            ledger.placeholder_flock(process, flock(root, description(2), 1, exclusive, false)),
            PlaceholderFlockOutcome::Contended
        );
        assert_eq!(
            ledger.placeholder_flock(process, flock(root, description(0), 2, exclusive, true)),
            PlaceholderFlockOutcome::WouldBlock,
            "description 2 still holds its shared lock"
        );
        // ...and description 0 lost its own by asking without waiting. Once
        // description 2's last slot closes, nothing holds the root.
        ledger.release(process, description(2), 0);
        assert_eq!(
            ledger.placeholder_flock(process, flock(root, description(3), 1, exclusive, true)),
            PlaceholderFlockOutcome::Done
        );
        // An unlock releases; the placeholder flocks add no /proc/locks row.
        assert_eq!(
            ledger.placeholder_flock(process, flock(root, description(3), 1, None, true)),
            PlaceholderFlockOutcome::Done
        );
        assert!(ledger.held().is_empty());
        assert_eq!(ledger.unknown(), None);
    }

    #[test]
    fn a_shared_table_closes_for_every_process_that_shares_it() {
        let [parent, sibling] = [3, 4].map(DetPid::from_raw);
        let mut ledger = LockLedger::default();
        ledger.note_locks(parent, description(0), vec![lock("0")], 1);
        ledger.fork(parent, sibling, true);
        ledger.release(sibling, description(0), 0);
        assert!(ledger.held().is_empty());
        // A shared table counts no longer for /proc/locks once the first
        // process sharing it exits.
        ledger.note_locks(parent, description(1), vec![lock("1")], 1);
        ledger.prune(|process| process != sibling);
        assert!(ledger.held().is_empty());
    }

    /// A placeholder `flock` is the description's, and goes with its last
    /// reference, as Linux's last `fput` releases it: not when the taker
    /// closes a description a child forked before the lock still holds, not
    /// at an exec its descriptor survives, but at the exit of the last
    /// process whose table holds it, also without a close, and with the last
    /// process sharing a table. A process whose tables the ledger cannot tell
    /// apart is refused a lock for the rest of its life, keeps every
    /// description it holds until it exits, and a lock that conflicts with
    /// one such a description holds is refused by name, not answered.
    #[test]
    fn a_placeholder_flock_follows_its_descriptions_references() {
        use crate::procfs::fd_directory::PlaceholderLockKey;
        let [parent, child, other, sibling] = [3, 4, 5, 6].map(DetPid::from_raw);
        let lock = |ledger: &mut LockLedger, process, open_file, mode| {
            ledger.placeholder_flock(
                process,
                PlaceholderFlock {
                    key: PlaceholderLockKey::Root,
                    open_file,
                    slots: 1,
                    mode,
                    nonblocking: true,
                },
            )
        };
        let exclusive = Some(FlockMode::Exclusive);
        let mut ledger = LockLedger::default();
        // The parent opens the directory (its description noted with no
        // lock) and forks before it locks; then it locks and closes.
        ledger.note_locks(parent, description(0), Vec::new(), 1);
        ledger.fork(parent, child, false);
        assert_eq!(
            lock(&mut ledger, parent, description(0), exclusive),
            PlaceholderFlockOutcome::Done
        );
        ledger.release(parent, description(0), 0);
        assert_eq!(
            lock(&mut ledger, other, description(1), exclusive),
            PlaceholderFlockOutcome::WouldBlock,
            "the child's copy still holds the lock"
        );
        // An exec the child's descriptor survives keeps it.
        ledger.exec(child);
        ledger.exec_kept(child, &[(description(0), 1)]);
        assert_eq!(
            lock(&mut ledger, other, description(1), exclusive),
            PlaceholderFlockOutcome::WouldBlock,
            "the descriptor survived the exec"
        );
        // The child exits without closing it: its last reference goes.
        ledger.prune(|process| process != child);
        assert_eq!(
            lock(&mut ledger, other, description(1), exclusive),
            PlaceholderFlockOutcome::Done
        );
        assert_eq!(
            lock(&mut ledger, other, description(1), None),
            PlaceholderFlockOutcome::Done
        );
        // An exec the descriptor does not survive lets the lock go.
        ledger.note_locks(sibling, description(2), Vec::new(), 1);
        assert_eq!(
            lock(&mut ledger, sibling, description(2), exclusive),
            PlaceholderFlockOutcome::Done
        );
        ledger.exec(sibling);
        ledger.exec_kept(sibling, &[]);
        assert_eq!(
            lock(&mut ledger, other, description(1), exclusive),
            PlaceholderFlockOutcome::Done
        );
        assert_eq!(
            lock(&mut ledger, other, description(1), None),
            PlaceholderFlockOutcome::Done
        );
        // A table two processes share holds the lock until both exit.
        let mut ledger = LockLedger::default();
        ledger.note_locks(parent, description(0), Vec::new(), 1);
        ledger.fork(parent, sibling, true);
        assert_eq!(
            lock(&mut ledger, parent, description(0), exclusive),
            PlaceholderFlockOutcome::Done
        );
        ledger.prune(|process| process != parent);
        assert_eq!(
            lock(&mut ledger, other, description(1), exclusive),
            PlaceholderFlockOutcome::WouldBlock,
            "the sibling still holds the shared table"
        );
        ledger.prune(|process| process != parent && process != sibling);
        assert_eq!(
            lock(&mut ledger, other, description(1), exclusive),
            PlaceholderFlockOutcome::Done
        );
        // A process that got a thread with a table of its own: its tables
        // cannot be told apart for the rest of its life. It is refused a new
        // lock; its close of the lock's description is not taken (the
        // thread's copy may still hold it, and Linux releases the lock only
        // at the last reference, which may go with that thread unseen); and
        // another process's lock that conflicts with it is refused by name,
        // as is a lock in a process forked from it, until both have exited.
        let mut ledger = LockLedger::default();
        ledger.note_locks(parent, description(0), Vec::new(), 1);
        assert_eq!(
            lock(&mut ledger, parent, description(0), exclusive),
            PlaceholderFlockOutcome::Done
        );
        ledger.mark_private_tables(parent);
        let request = PlaceholderFlock {
            key: PlaceholderLockKey::Root,
            open_file: description(3),
            slots: 1,
            mode: Some(FlockMode::Shared),
            nonblocking: true,
        };
        assert_eq!(
            ledger.placeholder_flock(parent, request),
            PlaceholderFlockOutcome::Unfollowable
        );
        ledger.release(parent, description(0), 0);
        assert_eq!(
            lock(&mut ledger, other, description(1), exclusive),
            PlaceholderFlockOutcome::Unfollowable,
            "the conflicting lock's last reference may be gone unseen"
        );
        ledger.fork(parent, child, false);
        assert_eq!(
            ledger.placeholder_flock(child, request),
            PlaceholderFlockOutcome::Unfollowable,
            "a process forked from it cannot be followed either"
        );
        assert_eq!(
            lock(&mut ledger, other, description(1), None),
            PlaceholderFlockOutcome::Done,
            "an unlock is answered"
        );
        ledger.prune(|process| process != parent && process != child);
        assert_eq!(
            lock(&mut ledger, other, description(1), exclusive),
            PlaceholderFlockOutcome::Done
        );
    }

    /// No placeholder `flock` rests on a reference only a message queued
    /// over `SCM_RIGHTS` holds: a send of a locked description is refused
    /// (it names the description), a lock of one with a copy in flight is
    /// refused, and the receive (or the send's failure) lets the description
    /// lock again.
    #[test]
    fn a_placeholder_lock_never_rests_on_a_copy_in_flight() {
        use crate::procfs::fd_directory::PlaceholderLockKey;
        let [sender, receiver] = [3, 4].map(DetPid::from_raw);
        let lock = |ledger: &mut LockLedger, process, open_file, mode| {
            ledger.placeholder_flock(
                process,
                PlaceholderFlock {
                    key: PlaceholderLockKey::Root,
                    open_file,
                    slots: 1,
                    mode,
                    nonblocking: true,
                },
            )
        };
        let exclusive = Some(FlockMode::Exclusive);
        let mut ledger = LockLedger::default();
        ledger.note_locks(sender, description(0), Vec::new(), 1);
        assert_eq!(
            lock(&mut ledger, sender, description(0), exclusive),
            PlaceholderFlockOutcome::Done
        );
        assert_eq!(
            ledger.placeholder_copies(PlaceholderCopies::Sending(vec![description(0)])),
            Err(description(0))
        );
        assert_eq!(
            lock(&mut ledger, sender, description(0), None),
            PlaceholderFlockOutcome::Done
        );
        assert_eq!(
            ledger.placeholder_copies(PlaceholderCopies::Sending(vec![description(0)])),
            Ok(())
        );
        assert_eq!(
            lock(&mut ledger, sender, description(0), exclusive),
            PlaceholderFlockOutcome::InFlight
        );
        // The receiver's copy is the same description: once it is received,
        // a lock through either is the description's one lock.
        assert_eq!(
            ledger.placeholder_copies(PlaceholderCopies::Received(description(0))),
            Ok(())
        );
        assert_eq!(
            lock(&mut ledger, receiver, description(0), exclusive),
            PlaceholderFlockOutcome::Done
        );
        assert_eq!(
            lock(&mut ledger, sender, description(0), exclusive),
            PlaceholderFlockOutcome::Done,
            "the same description holds it already"
        );
        // A failed send uncounts its copies.
        assert_eq!(
            lock(&mut ledger, receiver, description(0), None),
            PlaceholderFlockOutcome::Done
        );
        assert_eq!(
            ledger.placeholder_copies(PlaceholderCopies::Sending(vec![description(0)])),
            Ok(())
        );
        assert_eq!(
            ledger.placeholder_copies(PlaceholderCopies::Unsent(vec![description(0)])),
            Ok(())
        );
        assert_eq!(
            lock(&mut ledger, sender, description(0), exclusive),
            PlaceholderFlockOutcome::Done
        );
    }

    /// An exec never lets a placeholder `flock` a survivor holds go: the new
    /// table references every description of the old one that holds such a
    /// lock until the exec names what survived, so another process never
    /// finds a surviving description's lock gone in between, and then a
    /// close-on-exec one is released.
    #[test]
    fn an_exec_never_lets_a_surviving_lock_go() {
        use crate::procfs::fd_directory::PlaceholderLockKey;
        let [process, other] = [3, 4].map(DetPid::from_raw);
        let lock = |ledger: &mut LockLedger, process, open_file, mode| {
            ledger.placeholder_flock(
                process,
                PlaceholderFlock {
                    key: PlaceholderLockKey::Root,
                    open_file,
                    slots: 1,
                    mode,
                    nonblocking: true,
                },
            )
        };
        let exclusive = Some(FlockMode::Exclusive);
        let mut ledger = LockLedger::default();
        ledger.note_locks(process, description(0), Vec::new(), 1);
        assert_eq!(
            lock(&mut ledger, process, description(0), exclusive),
            PlaceholderFlockOutcome::Done
        );
        ledger.exec(process);
        assert_eq!(
            lock(&mut ledger, other, description(1), exclusive),
            PlaceholderFlockOutcome::WouldBlock,
            "the survivor's lock holds between the exec and its survivors"
        );
        ledger.exec_kept(process, &[(description(0), 1)]);
        assert_eq!(
            lock(&mut ledger, other, description(1), exclusive),
            PlaceholderFlockOutcome::WouldBlock,
            "and after"
        );
        // A close-on-exec description's lock goes with the old table.
        ledger.exec(process);
        ledger.exec_kept(process, &[]);
        assert_eq!(
            lock(&mut ledger, other, description(1), exclusive),
            PlaceholderFlockOutcome::Done
        );
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
        // Nor does one that names nothing at all (a backend that rediscovers
        // its table after the exec), whatever it held.
        ledger.note_locks(process, description(2), vec![lock("2")], 1);
        ledger.exec(process);
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
    fn a_refusal_holds_for_the_rest_of_the_run_by_its_first_reason() {
        let process = DetPid::from_raw(3);
        let mut ledger = LockLedger::default();
        ledger.note_locks(process, description(0), vec![lock("0")], 1);
        assert_eq!(ledger.unknown(), None);
        ledger.refuse("ENFILE".to_string());
        ledger.refuse("EIO".to_string());
        assert_eq!(ledger.unknown(), Some("ENFILE"));
        // Neither a later successful read nor the end of every table clears it.
        ledger.note_locks(process, description(0), Vec::new(), 1);
        ledger.prune(|_| false);
        assert_eq!(ledger.unknown(), Some("ENFILE"));
        // It does not hide what the ledger knows: the decision is refused.
        ledger.note_locks(process, description(1), vec![lock("1")], 1);
        assert_eq!(ledger.held(), [lock("1")]);
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
        ledger.note_unknown(parent, description(1), "EIO".to_string(), 1);
        ledger.note_locks(parent, description(1), vec![lock("0")], 1);
        assert_eq!(ledger.unknown(), None);
        assert_eq!(ledger.held(), [lock("0")]);
        // An exec keeps an unknown description its table still names, and
        // drops one it does not.
        ledger.note_unknown(parent, description(1), "ENFILE".to_string(), 1);
        ledger.exec(parent);
        assert_eq!(ledger.unknown(), None);
        ledger.exec_kept(parent, &[(description(1), 1)]);
        assert_eq!(ledger.unknown(), Some("ENFILE"));
        ledger.exec(parent);
        ledger.exec_kept(parent, &[]);
        assert_eq!(ledger.unknown(), None);
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

    /// `rows` rows of 50 bytes each, as a host's OFD locks print, numbered
    /// from `first`: the part of a capture other tests do not care about.
    fn filler(first: usize, rows: usize) -> String {
        let mut out = String::new();
        for sequence in first..first + rows {
            let base = format!("{sequence}: OFDLCK ADVISORY  WRITE -1 00:2a: 0 EOF\n").len();
            out.push_str(&format!(
                "{sequence}: OFDLCK ADVISORY  WRITE -1 00:2a:{inode:0width$} 0 EOF\n",
                inode = 7000 + sequence,
                width = 50 - base,
            ));
        }
        assert_eq!(out.len(), rows * 50);
        out
    }

    const CHUNK: usize = 64 * 1024;

    /// What the capture keeps of `parts`, each part one read.
    fn kept(parts: &[&str]) -> String {
        let contents = parts.concat();
        let reads = parts.iter().map(|part| part.len()).collect::<Vec<_>>();
        String::from_utf8(locks_capture_without_repeats(
            contents.as_bytes(),
            &reads,
            CHUNK,
        ))
        .expect("the capture stays text")
    }

    /// The guest's own rows, in one read.
    const GUEST: &str = "\
238: OFDLCK ADVISORY  WRITE -1 00:2a:3843038210 0 EOF
239: POSIX  ADVISORY  WRITE 3 00:2a:3843038209 0 EOF
";
    /// The guest's POSIX row printed again by a closing read, renumbered.
    const REPEAT: &str = "243: POSIX  ADVISORY  WRITE 3 00:2a:3843038209 0 EOF\n";
    /// A lock the host added between the reads.
    const ADDED: &str = "241: OFDLCK ADVISORY  WRITE -1 00:2a:3843038208 0 0\n";

    #[test]
    fn the_race_repeats_are_dropped_and_an_added_lock_stays() {
        // The shape the pinned host_locks test caught (reads of 107 and 159
        // bytes): the host added four locks on the guest's CPU, ahead of the
        // guest's rows; the closing read printed the one visible added lock
        // and both guest rows again, renumbered.
        let closing =
            format!("{ADDED}242: OFDLCK ADVISORY  WRITE -1 00:2a:3843038210 0 EOF\n{REPEAT}");
        assert_eq!((GUEST.len(), closing.len()), (107, 159));
        assert_eq!(kept(&[GUEST, &closing]), format!("{GUEST}{ADDED}"));
        // One addition behind the guest's rows: the last row again; or
        // several closing reads, several times.
        assert_eq!(kept(&[GUEST, REPEAT]), GUEST);
        assert_eq!(kept(&[GUEST, REPEAT, REPEAT, REPEAT]), GUEST);
        // An added lock repeated by a later closing read goes too.
        assert_eq!(
            kept(&[GUEST, ADDED, REPEAT, ADDED]),
            format!("{GUEST}{ADDED}")
        );
        // Nothing after the read: nothing to drop.
        assert_eq!(kept(&[GUEST]), GUEST);
        assert_eq!(kept(&[]), "");
    }

    #[test]
    fn a_row_nobody_read_is_kept_although_the_lengths_pass() {
        // Codex's case: a 3000-byte read stops at a guest lock whose host
        // waiters need at least 1096 bytes; they cancel before the next read,
        // which returns the guest's root row alone, 80 bytes. The lengths
        // pass (3000 + 80 < 4096), but the row was never read, so it stays.
        let prefix = filler(1, 60);
        let shrunk = format!("61: POSIX  ADVISORY  WRITE 1 00:2a:{:038} 0 EOF\n", 1002);
        assert_eq!((prefix.len(), shrunk.len()), (3000, 80));
        assert_eq!(kept(&[&prefix, &shrunk]), format!("{prefix}{shrunk}"));
        // DS's case: a 4050-byte read stops at a host record the host then
        // releases, and the next read returns the guest's last POSIX row,
        // about 40 bytes (4050 + 41 < 4096).
        let prefix = filler(1, 81);
        let row = "82: POSIX  ADVISORY  WRITE 1 00:2a:1 0 0\n";
        assert_eq!((prefix.len(), row.len()), (4050, 41));
        assert_eq!(kept(&[&prefix, row]), format!("{prefix}{row}"));
        // A row nobody read beside a repeat: the row stays, the repeat goes.
        let new = "4: FLOCK  ADVISORY  WRITE 1 00:2a:1003 0 EOF\n";
        assert_eq!(kept(&[GUEST, REPEAT, new]), format!("{GUEST}{new}"));
        assert_eq!(kept(&[GUEST, new, REPEAT]), format!("{GUEST}{new}"));
    }

    #[test]
    fn an_identical_row_is_dropped_only_as_a_repeat_after_a_provable_end() {
        // Two identical shared OFD rows exist natively (two descriptions, one
        // range of one file). When the second stopped the first read by not
        // fitting in the page, the reads after it return at least what the
        // page had left, so the lengths keep it though it repeats the first.
        let row = "OFDLCK ADVISORY  READ  -1 00:2a:1009 0 EOF\n";
        let second = format!("82: {row}");
        let first_read = format!("{}81: {row}", filler(1, 80));
        // Widen one filler row so that the first read and the second row
        // together are exactly a page: the second row did not fit.
        let pad = 4096 - first_read.len() - second.len();
        let widened = format!(
            "{}80: OFDLCK ADVISORY  WRITE -1 00:2a:{:0width$} 0 EOF\n81: {row}",
            filler(1, 79),
            1,
            width = 50 + pad - "80: OFDLCK ADVISORY  WRITE -1 00:2a: 0 EOF\n".len(),
        );
        assert_eq!(widened.len() + second.len(), 4096);
        assert_eq!(kept(&[&widened, &second]), format!("{widened}{second}"));
        // Below the page, the same row after the same prefix can only be a
        // repeat past the end, which is what the closing read prints: dropped.
        // Were it genuine, the list changed between the reads (the residual
        // the function documents): the snapshot shows one of the two rows.
        assert!(first_read.len() + second.len() < 4096);
        assert_eq!(kept(&[&first_read, &second]), first_read);
    }

    #[test]
    fn a_repeat_is_its_granted_lock_whatever_its_waiters() {
        let tree = "\
5: POSIX  ADVISORY  WRITE 3 00:2a:1004 0 EOF
5: -> POSIX  ADVISORY  WRITE 0 00:2a:1004 0 EOF
5:  -> POSIX  ADVISORY  WRITE 0 00:2a:1004 0 EOF
";
        // The tree again, renumbered.
        let repeat = tree.replace("5:", "9:");
        assert_eq!(kept(&[tree, &repeat]), tree);
        // The same granted lock with fewer waiters (one gave up between the
        // reads), or none, or more: still the one lock, so a repeat, and the
        // tree one walk printed is the one kept.
        let fewer = "9: POSIX  ADVISORY  WRITE 3 00:2a:1004 0 EOF\n\
                     9: -> POSIX  ADVISORY  WRITE 0 00:2a:1004 0 EOF\n";
        assert_eq!(kept(&[tree, fewer]), tree);
        let root = "9: POSIX  ADVISORY  WRITE 3 00:2a:1004 0 EOF\n";
        assert_eq!(kept(&[tree, root]), tree);
        assert_eq!(kept(&[root, tree]), root);
        // A repeated tree beside a lock nobody read: the tree goes, the new
        // lock stays.
        let other = "10: POSIX  ADVISORY  WRITE 3 00:2a:1005 0 EOF\n";
        assert_eq!(
            kept(&[tree, &format!("{fewer}{other}")]),
            format!("{tree}{other}")
        );
        // A different range, access or owner is another lock: kept.
        for changed in [
            "9: POSIX  ADVISORY  WRITE 3 00:2a:1004 0 9\n",
            "9: POSIX  ADVISORY  READ  3 00:2a:1004 0 EOF\n",
            "9: POSIX  ADVISORY  WRITE 4 00:2a:1004 0 EOF\n",
            "9: FLOCK  ADVISORY  WRITE 3 00:2a:1004 0 EOF\n",
        ] {
            assert_eq!(kept(&[tree, changed]), format!("{tree}{changed}"));
        }
        // A tail that begins with a waiter is not whole records: all kept.
        let orphan = "9: -> POSIX  ADVISORY  WRITE 0 00:2a:1004 0 EOF\n";
        assert_eq!(kept(&[tree, orphan]), format!("{tree}{orphan}"));
    }

    #[test]
    fn a_lease_that_started_breaking_is_a_repeat() {
        // A host open of the file starts a break between the reads: the
        // lease prints BREAKING, the access it breaks to, and the breaker
        // queued behind it. It is the same lease.
        let active = "7: LEASE  ACTIVE    WRITE 3 00:2a:1006 0 EOF\n";
        let breaking = "\
11: LEASE  BREAKING  READ  3 00:2a:1006 0 EOF
11: -> LEASE  BREAKER   UNLCK 0 00:2a:1006 0 EOF
";
        assert_eq!(kept(&[active, breaking]), active);
        // Another file's lease is another lease.
        let elsewhere = "11: LEASE  ACTIVE    WRITE 3 00:2a:1007 0 EOF\n";
        assert_eq!(kept(&[active, elsewhere]), format!("{active}{elsewhere}"));
    }

    #[test]
    fn a_guest_root_whose_host_waiters_gave_up_shows_once_through_the_filter() {
        // Codex's round-2 case: read 1 reaches the end with the guest's POSIX
        // root (pid 3) and two host waiters (pid 0, outside the namespace);
        // the waiters give up and the host adds hidden locks ahead of the
        // root, so the closing read prints the root again without them.
        let first = "\
239: POSIX  ADVISORY  WRITE 3 00:2a:1004 0 EOF
239: -> POSIX  ADVISORY  WRITE 0 00:2a:1004 0 EOF
239:  -> POSIX  ADVISORY  WRITE 0 00:2a:1004 0 EOF
";
        let closing = "243: POSIX  ADVISORY  WRITE 3 00:2a:1004 0 EOF\n";
        let capture = kept(&[first, closing]);
        let rows = lock_rows(capture.as_bytes()).expect("the capture parses");
        let keep = guest_lock_rows(&rows, guest, &[]);
        let shown = String::from_utf8(keep_lock_rows(capture.as_bytes(), Some(keep.as_slice())))
            .expect("the rows stay text");
        assert_eq!(shown, "239: POSIX  ADVISORY  WRITE 3 00:2a:1004 0 EOF\n");
    }

    #[test]
    fn only_a_short_read_below_a_page_can_end_the_capture() {
        let capture = |contents: &str, reads: &[usize], chunk: usize| {
            String::from_utf8(locks_capture_without_repeats(
                contents.as_bytes(),
                reads,
                chunk,
            ))
            .expect("the capture stays text")
        };
        // A read that filled the caller's buffer may have stopped anywhere.
        let contents = format!("{GUEST}{REPEAT}");
        assert_eq!(
            capture(&contents, &[GUEST.len(), REPEAT.len()], GUEST.len()),
            contents
        );
        // A host list over several pages, each read stopping at a page, whose
        // last read reached the end, followed by a repeat of its last row.
        let (one, two) = (filler(1, 81), filler(82, 81));
        let three = format!("{}{GUEST}", filler(163, 70));
        let all = format!("{one}{two}{three}");
        assert_eq!(kept(&[&one, &two, &three, REPEAT]), all);
        assert_eq!(kept(&[&one, &two, &three]), all);
        // A burst of additions too large for the length test, in one closing
        // read with the repeat: all kept.
        let burst = format!("{}{REPEAT}", filler(400, 80));
        assert_eq!(kept(&[GUEST, &burst]), format!("{GUEST}{burst}"));
        // Lengths that disagree with the bytes decide nothing.
        assert_eq!(capture(&contents, &[GUEST.len()], CHUNK), contents);
        // Bytes that are not whole lock rows are never dropped.
        assert_eq!(kept(&[GUEST, "garbage\n"]), format!("{GUEST}garbage\n"));
        let unterminated = REPEAT.trim_end();
        assert_eq!(
            kept(&[GUEST, unterminated]),
            format!("{GUEST}{unterminated}")
        );
    }
}
