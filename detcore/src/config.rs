/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Detcore configuration and widely used types.
pub use detcore_model::config::CONFIG_FINGERPRINT_ENV;
pub use detcore_model::config::LITEINST_CONFIG_FINGERPRINT_ENV;
pub use detcore_model::config::*;

/// [`detcore_model::config::config_wire_fingerprint`], extended with the
/// revision of Detcore's global RPC (`crate::tool_global::GLOBAL_RPC_REVISION`).
///
/// A runtime that embeds Detcore (SaBRe's plugin, the LiteInst runtime that
/// in-guest-trap also loads, DBT's) sends `GlobalRequest`s to its coordinator
/// as positional bincode and decodes the `GlobalResponse`s. Both enums live
/// in this crate, outside the sources the configuration's fingerprint hashes,
/// so a variant one build had and the other lacked went unnoticed: a newer
/// runtime's request failed to decode in the middle of a run, and an older
/// runtime never sent it (and so, for one, kept the host's inotify rename
/// cookies). Every such runtime compares this fingerprint with its
/// coordinator's before it decodes anything, so the pair is refused by name.
/// The revision stands for the enums' source, which, unlike the
/// configuration's, is not embedded: these runtimes are loaded into the guest,
/// where every byte they grow by moves the guest's own mappings.
pub fn config_wire_fingerprint() -> String {
    // The material is immutable for this loaded image: the model overrides
    // environment-dependent defaults, and the RPC revision is compiled in.
    // Reuse it when a forked in-guest tool reconnects on the guest's clone
    // stack; rebuilding Clap's canonical Config there can exhaust that stack.
    // SaBRe's parent compatibility check finishes before an intercepted clone
    // can run, so its child inherits an initialized value. Each connection
    // still compares the coordinator's fingerprint with this local value.
    static FINGERPRINT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    FINGERPRINT
        .get_or_init(|| {
            fingerprint_with_revision(
                &detcore_model::config::config_wire_fingerprint(),
                crate::tool_global::GLOBAL_RPC_REVISION,
            )
        })
        .clone()
}

/// What differs when a coordinator announced `expected` as its
/// [`config_wire_fingerprint`] and this build's is another, or `None` when
/// they are equal. The runtimes report this before they decode the
/// coordinator's configuration (<https://github.com/rrnewton/hermit/issues/3986>).
pub fn config_fingerprint_mismatch(expected: &std::ffi::OsStr) -> Option<String> {
    let ours = config_wire_fingerprint();
    (expected.to_str() != Some(ours.as_str())).then(|| {
        format!(
            "its configuration and clock RPC fingerprint is {ours}, Hermit's is {}",
            expected.to_string_lossy()
        )
    })
}

/// FNV-1a over `bytes`, each part length-prefixed in a domain of its own,
/// as `detcore_model`'s fingerprint hashes its material. A mismatch
/// detector, not a security boundary.
fn fnv_domains(parts: &[(u8, &[u8])]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for (domain, bytes) in parts {
        for byte in std::iter::once(domain)
            .chain((bytes.len() as u64).to_le_bytes().iter())
            .chain(bytes.iter())
        {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x1000_0000_01b3);
        }
    }
    format!("{hash:016x}")
}

/// The fingerprint of configuration fingerprint `base` at global RPC
/// revision `revision`.
fn fingerprint_with_revision(base: &str, revision: u32) -> String {
    fnv_domains(&[(0, base.as_bytes()), (1, &revision.to_le_bytes())])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Revisions reserved for a change that is not on this base yet, which
    /// `GLOBAL_RPC_REVISIONS` therefore skips: 33 to 39 and 42 to 44 retain
    /// the coordinator's reserved allocation numbers. A landed change uses
    /// the next available row; its earlier reservation remains separate
    /// until the allocation is released. At this head 28 is
    /// https://github.com/rrnewton/hermit/pull/4789's, 29 is
    /// https://github.com/rrnewton/hermit/pull/4884's, 30 is
    /// https://github.com/rrnewton/hermit/pull/4728's, 31 is
    /// https://github.com/rrnewton/hermit/pull/4867's, 32 is the stopless
    /// layout on top of 31's, 40 is the `CpuClockProcess` pair allocated
    /// to https://github.com/rrnewton/hermit/pull/4885, 41 is the
    /// `HostFileIdentity::content` layout of
    /// https://github.com/rrnewton/hermit/pull/4791, 45 is
    /// https://github.com/rrnewton/hermit/pull/4796's
    /// `TraceSchedEvent`/`WithWatchOrder` revision, 46 is
    /// https://github.com/rrnewton/hermit/pull/4769's
    /// `PublishInstructionOffset` revision, 47 is
    /// https://github.com/rrnewton/hermit/pull/4735's `StoreExclusive`
    /// payload layout. 48 adds `WithCpuTime` and `RecordExitCode` from
    /// package K item 2b slice 2b-1. 49 adds `Rewrite::not_written` from
    /// https://github.com/rrnewton/hermit/pull/5222, numbered 48 on its
    /// source base and renumbered to 49 at landing. 50 adds
    /// `Found::unverified_pairs` to the `StoreExclusive` payload from
    /// https://github.com/rrnewton/hermit/pull/5230 (source revision 44,
    /// carried and landed as 50). 51 adds `SuspendHoldsDiscarded` and its
    /// response from https://github.com/rrnewton/hermit/pull/5175 (carried
    /// as 50 and renumbered to 51 at landing). 52 adds
    /// `TraceSchedEventResponse::backstop` to the `GlobalResponse::TraceSchedEvent`
    /// replay reply from https://github.com/rrnewton/hermit/pull/4759 (carried
    /// as 49 and renumbered to 52 at landing).
    /// 53 appends `RecordExecStrings` and `ProcessExecStrings` (package K item
    /// 2b slice 2b-3; 42 on its old base, then 43, 47 and 49 at its rebases onto
    /// 2b-1, 51 on S2b's unlanded head, 52 at its rebase onto main's 51, and
    /// mechanically 53 at its rebase onto main's 52) and is the current revision.
    /// The table below records the landed history; the reservation list
    /// is unchanged.
    const RESERVED_GLOBAL_RPC_REVISIONS: &[u32] = &[33, 34, 35, 36, 37, 38, 39, 42, 43, 44];

    /// Every revision of the global RPC, oldest first, with the digest of the
    /// code of its two enums and its `Resources` payload. A change to any of
    /// these appends a row and sets
    /// `GLOBAL_RPC_REVISION` to its revision.
    const GLOBAL_RPC_REVISIONS: &[(u32, &str, &str)] = &[
        // GlobalRequest::VirtualInotifyCookies and its response, the first
        // revision the fingerprint carries.
        (1, "a61c88ecb1abcb5e", "9b7e11bf5f08e6ae"),
        // GlobalRequest::RegisterDescriptorDirectory and DescriptorDirectory and
        // their responses (package K slice F1).
        (2, "caee2c37469869d4", "9b7e11bf5f08e6ae"),
        // GlobalRequest::RegisterPlaceholder, Placeholder, PlaceholderFlock,
        // PlaceholderWriteHint and PlaceholderCopies and their responses, in
        // place of F1's two (package K slice P1).
        (3, "b3d7410a78b56ae0", "9b7e11bf5f08e6ae"),
        // Resources::priority_row extends the positional request payload.
        (4, "b3d7410a78b56ae0", "4e10b2de93d47d64"),
        // Adds `GlobalRequest::CpuAffinity`, `GlobalRequest::SetCpuAffinity`; adds `GlobalResponse::CpuAffinity` (#4629).
        (5, "5a4ad14e74501e11", "4e10b2de93d47d64"),
        // Adds `Resources.slice_expired` (#4653).
        (6, "5a4ad14e74501e11", "6a1b176897e67771"),
        // Retypes `GlobalRequest::RecordHostInput`
        // (https://github.com/rrnewton/hermit/pull/4611).
        (7, "6b6c7521a6620f94", "6a1b176897e67771"),
        // Adds `Resources.zero_length_sleep`
        // (https://github.com/rrnewton/hermit/pull/4658).
        (8, "6b6c7521a6620f94", "dafaa96d8b8cb3ad"),
        // Adds `GlobalResponse::WithLoneGrant`
        // (https://github.com/rrnewton/hermit/pull/4662).
        (9, "ebfd2b59a97e1d7a", "dafaa96d8b8cb3ad"),
        // Adds `GlobalRequest::ProcessSignalReceiver`; adds `GlobalResponse::ProcessSignalReceiver`
        // (https://github.com/rrnewton/hermit/pull/4625, renumbered to 10 at landing).
        (10, "1899cf7ab14698ae", "dafaa96d8b8cb3ad"),
        // Adds `GlobalRequest::StoreExclusive` and `GlobalResponse::StoreExclusive`
        // (https://github.com/rrnewton/hermit/pull/4740, renumbered to 11 at landing).
        (11, "8bb87305b1bd45a3", "dafaa96d8b8cb3ad"),
        // Adds `GlobalRequest::CommitFatalDelivery`; removes `GlobalRequest::RecordFatalSignal`; adds `GlobalResponse::CommitFatalDelivery`; removes `GlobalResponse::RecordFatalSignal`
        // (https://github.com/rrnewton/hermit/pull/4676, renumbered to 12 at landing).
        (12, "ab01c6abb5daa6d7", "dafaa96d8b8cb3ad"), // Adds `GlobalRequest::SigkillDyingThreads`, `GlobalRequest::RecordSigkillRobustLists`, `GlobalRequest::TakeSigkillRobustList` and the matching responses
        // (https://github.com/rrnewton/hermit/pull/4760, renumbered to 13 at landing).
        (13, "78e513d0fe279099", "dafaa96d8b8cb3ad"),
        (14, "edaba1a0186b437c", "dafaa96d8b8cb3ad"), // Adds `GlobalRequest::NextGuestId`, `GlobalRequest::GaveHostId`; adds `GlobalResponse::NextGuestId`, `GlobalResponse::GaveHostId`
        // (https://github.com/rrnewton/hermit/pull/4725, renumbered to 14 at integration; digest measured at this head).
        // Adds `Resources::passes_descriptors_out`, which revokes the scheduler's
        // guest-held marks
        // (https://github.com/rrnewton/hermit/pull/4862, renumbered to 15 at
        // integration; digests measured at this head).
        (15, "edaba1a0186b437c", "846052997d97693f"),
        // Retypes `GlobalRequest::TraceSchedEvent`, including its nested
        // AccessPayload signal observation (Handler/Ignored/Default/Unknown).
        // The definition digest below covers the outer enums, not nested types.
        // (https://github.com/rrnewton/hermit/pull/4738, renumbered to 16 at
        // integration; digests measured at this head).
        (16, "182b80da8855a429", "846052997d97693f"),
        // Adds `GlobalRequest::ProcessSignalSent` and
        // `GlobalRequest::OwnProcessSignalChoice` and their responses
        // (https://github.com/rrnewton/hermit/pull/4918, allocated revision 17
        // at integration; digests measured at this head).
        (17, "5e3c31a8fe54f2a1", "846052997d97693f"),
        // Adds `Resources.polls_a_named_fifo`
        // (https://github.com/rrnewton/hermit/pull/4692, allocated revision 18
        // at integration; digests measured at this head).
        (18, "c413f205051a27d4", "f013e36743d8d9e6"),
        // Adds the replay-departure findings to the global request/response
        // pair (PR 4860 rework, allocated revision 19 at integration;
        // digests measured at this head).
        (19, "c19f0246a9e94f4a", "f013e36743d8d9e6"),
        // Adds `GlobalRequest::DescriptionLocksTaken`, `LockOwners`,
        // `LedgerLockRows`, `SecondIdentityLocks`, `HoldTable` and
        // `RecordLocksHeldAsOfd` and their responses, and `HeldTable`, defined
        // outside the digested enums, which encodes as nothing (package K
        // slice K8-h of https://github.com/rrnewton/hermit/issues/4213; its
        // revisions 17 to 19, never on main, became this one when main took
        // 17 to 19).
        (20, "3fb2388350b1a2fa", "f013e36743d8d9e6"),
        // Adds `GlobalRequest::HostDiskstats` and `GlobalResponse::HostDiskstats`:
        // the host's /proc/diskstats as the run first read it, which every
        // rendered /proc/diskstats starts from;
        // `GlobalRequest::UserNamespaceRelation` and
        // `GlobalRequest::UserNamespaceOwnerMayDiffer` and their responses,
        // Detcore's record of user namespaces; and
        // `GlobalRequest::ProcfsSequence` and
        // `GlobalRequest::StoreProcfsSequence` and their responses, the record
        // sequence of a per-process file a fork or a send shares where the
        // tool runs in each guest process. Serialized fields this digest does
        // not cover: `ProcfsFile::sequence_shared`, which a shared open file
        // model carries; the open file model's `record_sequence_unknown`; and
        // `ExecFdState::procfs_records`, which `GlobalRequest::PrepareExec`
        // carries (the kvm lane's slice N3, revision 17 on its own base,
        // renumbered to 21 at landing after main's 20; digests measured at
        // this head).
        (21, "9d583ef0f9243c61", "f013e36743d8d9e6"),
        // Adds `GlobalRequest::TakeSleepInterruption` and its response, and
        // `ObservationFinish::InterruptForCaught`'s `dequeue`, which the
        // digest does not cover (https://github.com/rrnewton/hermit/issues/4931).
        (22, "a26a19716aa915db", "f013e36743d8d9e6"),
        (23, "7c34f0ecd356155a", "f013e36743d8d9e6"),
        // `Report::Pairs` carries `ScannedMapping`s with a `Found` in place
        // of tuples (#4779), and the digest now covers the StoreExclusive
        // request's payload types. Renumbered to 24 at landing on top of
        // Train 6's revision 23; digest measured at this combined head.
        (24, "b31c27eec3d0d5b4", "f013e36743d8d9e6"),
        // Adds `GlobalRequest::Flock`, `FlockPoll`, `FlockCancel`,
        // `FlockCloseCheck`, `FlockHostDecided`, `FlockMapped`,
        // `FlockUnfollowable`, `FlockTeardownPending` and `FlockHeldBy` and
        // their responses, after slice N3's, and retypes
        // `GlobalResponse::LedgerLockRows` and `GlobalResponse::GuestLockRows`
        // (the kvm lane's slice SP2, Detcore's flock model; revision 21 on its
        // own base, renumbered to 22 at its rebase onto main's 21, and to 25,
        // appended after main's 22 to 24, at its rebase onto batch5a; digests
        // measured at this head). Serialized fields this digest does not cover:
        // `procfs::locks::FlockRequest`, `FlockOutcome` and `FlockPoll`, whose
        // `Waiting` carries whether a retry deleted the description's lock
        // (SP2's review round, before any release of revision 25, which also
        // made `GlobalResponse::FlockCancel` a unit answer) and whether it woke
        // other requests (SP2's third review round, still before any release).
        (25, "5c9d613c4fb323bd", "f013e36743d8d9e6"),
        // Adds `GlobalRequest::ChildSubreaper` and `GlobalResponse::ChildSubreaper`:
        // Detcore's record of the caller's subreaper flag, which answers
        // `PR_GET_CHILD_SUBREAPER` on a backend that places orphans where
        // Detcore names (PRCTL phase 2 of the kvm lane), after SP2's variants:
        // revision 22 was reserved for SP2 while PRCTL's base lacked them, and
        // the pair became 25 and 26, appended after main's 22 to 24, at their
        // rebase onto batch5a (digests measured at this head, above SP2).
        (26, "b5fc7394708905c9", "f013e36743d8d9e6"),
        // Adds `GlobalRequest::PidfdSelfQualificationRefusal`: Detcore's
        // record of a self-sentinel `pidfd_send_signal` refusal its backend
        // could not qualify, filed with the coordinator before the Tool
        // image returns it (the release-05 pidfd pair, renumbered to 27 at
        // its rebase onto C2's 25 and 26; digest measured at this head).
        (27, "74ceb8dd663f2297", "f013e36743d8d9e6"),
        // Retypes `GlobalResponse::StartNewThread` to carry the starting
        // thread's replayed first timeslice
        // (https://github.com/rrnewton/hermit/pull/4789; renumbered to 28 at
        // its rebase onto main's pidfd revision 27; digest measured at this
        // head).
        (28, "a23bc7a0c1215843", "f013e36743d8d9e6"),
        // Retypes the `StoreExclusive` payload definitions the digest covers
        // in `store_exclusive.rs` (PR 4884's in-memory exclusive-rewrite
        // run tally and reporting; renumbered to 29 at its rebase onto
        // main's 27, skipping 28 reserved for the pending 4789 change;
        // digest measured at this head).
        (29, "05a9f7a4ca16cde1", "f013e36743d8d9e6"),
        // `SchedEvent::stopless`, which the event's bincode always writes, and
        // the `ReplayedSliceEnd` that `TraceSchedEventResponse` now carries
        // (https://github.com/rrnewton/hermit/pull/4728; revision 30, reserved
        // for this change by the D2 seal while 29 and 31 landed first, so it
        // sits in numeric order between them; digest measured at the head
        // where this change stood on cf600eb1). The digest now also
        // covers the event `TraceSchedEvent` carries (domain 4: its fields and
        // its `Serialize`) and the reply's fields (domain 5). #4780 defines
        // domain 4 for the same event; whichever of the two lands second merges
        // the item lists and records the next revision.
        (30, "8b2876baa9a75d3b", "f013e36743d8d9e6"),
        // Adds `GlobalRequest::PostCrash`, `GlobalResponse::PostCrash` and
        // `GlobalResponse::WithPostCrashBound`
        // (https://github.com/rrnewton/hermit/pull/4867; renumbered to 31 at
        // this rebase onto main's pidfd revision 27, skipping reserved
        // 30 (4728); digest measured at this head).
        (31, "b4bbaec490287ee5", "f013e36743d8d9e6"),
        // This change's layout on top of revision 31's: row 30 above names
        // the definitions as they stood on cf600eb1, before 29 and 31
        // landed, so the combined definitions append their own revision
        // here rather than relabel 31 (revopus1's R1 on this change).
        // Digest measured at this head.
        (32, "e178ac33707f23b5", "f013e36743d8d9e6"),
        // Adds `GlobalRequest::CpuClockProcess` and `GlobalResponse::CpuClockProcess`
        // (https://github.com/rrnewton/hermit/pull/4885; revision 20 at its base,
        // allocated revision 40 at the drain rebase after main's landed 32
        // (the stopless layout on top of 31), skipping reserved 33 to 39;
        // digests measured at this head).
        (40, "3919013da0508f27", "f013e36743d8d9e6"),
        // Adds `HostFileIdentity::content` (JSON key `snapshot_content`),
        // always encoded, which `GlobalRequest::RecordHostInput` carries in
        // its `identity`. The digest now also covers that nested type, in
        // domain 6: its attributes and fields
        // (https://github.com/rrnewton/hermit/pull/4791; allocated 25 above
        // main's 24, renumbered to 36 at its rebase onto main's 27, and
        // renumbered mechanically to 41 at its rebase onto main's 40, the
        // next revision after main's last row, skipping only the reserved
        // 33 to 39 (36 is among them on this base); its domain moved from
        // 4 to 6 because main's 30 took domain 4 for the scheduled event
        // and domain 5 for its reply; digests measured at this head).
        (41, "aeb38079447d34fb", "f013e36743d8d9e6"),
        // Retypes `GlobalRequest::TraceSchedEvent`; adds
        // `GlobalResponse::WithWatchOrder` (#4796). Live-computed as 45 at
        // its rebase onto main's 41, skipping reserved 42 to 44; digest
        // measured at the sealed head.
        (45, "5e346726bb0f7c7d", "f013e36743d8d9e6"),
        // Adds `GlobalRequest::PublishInstructionOffset` and
        // `GlobalResponse::PublishInstructionOffset`, appended after the
        // `CpuClockProcess` pair at the tail of each enum
        // (https://github.com/rrnewton/hermit/pull/4769; 26 above
        // https://github.com/rrnewton/hermit/pull/4728's 25 on its own
        // base, renumbered to 46 at this rebase: the next revision after
        // main's landed 45 (https://github.com/rrnewton/hermit/pull/4796's
        // `TraceSchedEvent`/`WithWatchOrder` revision), skipping only
        // the reserved 33 to 39 and 42 to 44; digests measured at this
        // head).
        (46, "0dbbe4f192bc0983", "f013e36743d8d9e6"),
        // The `StoreExclusive` payload's `Rewrite` says whether a mapping's
        // file holds loops no mapping symbol proves (`unmarked`), which the
        // run names in its summary; its fields cross the wire positionally.
        // Numbered 42 at this change's rebase onto main's 41 and renumbered
        // to 47, the next revision after main's landed 46
        // (https://github.com/rrnewton/hermit/pull/4769); digest re-measured
        // at this head.
        (47, "3626cafe4551d074", "f013e36743d8d9e6"),
        // Adds `GlobalRequest::WithCpuTime`, which carries a thread's CPU
        // accounting on its request (`CpuPublication`), and
        // `GlobalRequest::RecordExitCode` and its response, and gives
        // `ThreadDeregistration` its `wait_status` (package K item 2b slice
        // 2b-1, revision 39 allocated by the kvm lane's coordinator, renumbered
        // 41 at its rebase onto main's 40, skipping reserved 33 to 39, and
        // renumbered mechanically to 42 at its rebase onto main's 41, to 46
        // at its rebase onto main's 45, and to 48 at its rebase onto main's
        // 47, each time the next revision after main's last row). The digest
        // covers those payload types too (domain 7: `ThreadDeregistration`,
        // `CpuPublication`, `TaskCpuTimes`), as row 32 extended it to its
        // own; domain 6 on its old base, moved to 7 because main's 41 took
        // domain 6 for `HostFileIdentity`. Digest measured at this head.
        (48, "e40e244635167e09", "f013e36743d8d9e6"),
        // The `StoreExclusive` payload's `Rewrite` says why loops were not
        // written (`not_written`); its fields cross the wire positionally.
        // Carried after main's 47; digest measured by the revision guard.
        // (renumbered from 48 to 49 at landing by the merge queue; digests measured on the combined
        // tree)
        (49, "cc10df0910192a63", "f013e36743d8d9e6"),
        // Adds `Found::unverified_pairs` to the positional `StoreExclusive`
        // payload. Landed as 50 via https://github.com/rrnewton/hermit/pull/5230,
        // from source revision 44, after main's landed 49. Both digests
        // were printed by the revision guard on the carried tree.
        (50, "317db02ddad70067", "f013e36743d8d9e6"),
        // Adds `GlobalRequest::SuspendHoldsDiscarded` and its response
        // (SIGSHARE S2b, https://github.com/rrnewton/hermit/pull/5175): after(this
        // base's last row); renumbered mechanically at each rebase: 41 on
        // its old base, 42 after main's 41, 46 after main's 45, 48 after
        // main's 47, 49 after main's 48, 50 after main's 49.
        // (renumbered from 50 to 51 at landing by the merge queue; digests measured on the combined
        // tree)
        (51, "ad4bb7658a751c72", "f013e36743d8d9e6"),
        // Adds `TraceSchedEventResponse::backstop`, the replay backstop of a
        // schedule replay of a record whose slices ran without the
        // `--max-timeslice` bound, which `GlobalResponse::TraceSchedEvent`
        // carries. The reply's fields are digested as `TRACE_REPLY_WIRE`
        // items, so the field changes the digest in domain 5
        // (https://github.com/rrnewton/hermit/pull/4759, allocated revision
        // 25 on its own base after main's 24, assigned 42 at its rebase
        // onto 41, renumbered to 49 at its rebase onto main's 48, skipping
        // reserved revisions; digest measured at this head).
        // (renumbered from 49 to 52 at landing by the merge queue; digests measured on the combined
        // tree)
        (52, "18b49ae6affb5c77", "f013e36743d8d9e6"),
        // Adds `GlobalRequest::RecordExecStrings` and
        // `GlobalRequest::ProcessExecStrings` and their responses, which
        // carry an image's argument and environment strings (`ExecStrings`,
        // package K item 2b slice 2b-3; after(this base's last row),
        // renumbered mechanically at each rebase: 42 on its old base, then
        // 43, 47 and 49 at its rebases onto 2b-1, 51 after SIGSHARE S2b's
        // provisional 50, 52 after main's 51, and 53 after main's 52) and,
        // in `ProcessExecStrings`'s, the image's number. The digest covers
        // that payload type too (domain 8; domain 7 on its old base, moved to
        // 8 because main's 41 took domain 6 and 2b-1's payload types moved
        // to 7). Digest measured at this head.
        (53, "21665db7c6c7db51", "f013e36743d8d9e6"),
        // Adds `GlobalResponse::WithReplayInstructionOffset` and
        // `SchedEvent::instruction_offset`, which the event's bincode always
        // writes and domain 4 covers (#4845; 23 on its own base, renumbered
        // to 27 in the PR stack, to 53 at its rebase onto main's 52, and to
        // 54 at this rebase onto main's 53 (RecordExecStrings), skipping
        // reserved 33 to 39 and 42 to 44; digest measured at this head).
        (54, "4a8fdaad9b3c03b7", "f013e36743d8d9e6"),
    ];

    /// The lines of code, without comments or blank lines, of the type that
    /// `source` declares on the line `header`, through its closing brace.
    fn definition(source: &str, header: &str) -> Vec<String> {
        let mut lines = source.lines().skip_while(|line| *line != header);
        assert_eq!(lines.next(), Some(header), "{header} is declared");
        let mut code: Vec<String> = lines
            .take_while(|line| *line != "}")
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with("//"))
            .map(str::to_owned)
            .collect();
        code.insert(0, header.to_owned());
        code
    }

    /// The types `GlobalRequest::StoreExclusive` carries, declared in
    /// `store_exclusive.rs` inside its modules. Their fields cross the RPC in
    /// positional bincode as the enums' own do.
    const STORE_EXCLUSIVE_PAYLOAD: [&str; 5] = [
        "pub enum Report {",
        "pub struct ScannedMapping {",
        "pub struct Found {",
        "pub struct Rewrite {",
        "pub struct LiveMapping {",
    ];

    /// The lines of code, as [`definition`] gives them, of the type that
    /// `source` declares, indented, on the line `header`, through the brace
    /// at that indentation that closes it.
    fn nested_definition(source: &str, header: &str) -> Vec<String> {
        let mut lines = source.lines().skip_while(|line| line.trim() != header);
        let first = lines
            .next()
            .unwrap_or_else(|| panic!("{header} is declared"));
        let close = format!("{}}}", &first[..first.len() - first.trim_start().len()]);
        let mut code: Vec<String> = lines
            .take_while(|line| *line != close)
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with("//"))
            .map(str::to_owned)
            .collect();
        code.insert(0, header.to_owned());
        code
    }

    /// The items of detcore-model's schedule.rs that decide how
    /// `GlobalRequest::TraceSchedEvent` carries its event: the event's
    /// fields and its hand-written `Serialize`, which decides which of them
    /// the RPC's bincode writes.
    const SCHED_EVENT_WIRE: [&str; 2] =
        ["pub struct SchedEvent {", "impl Serialize for SchedEvent {"];

    /// The items that decide how `GlobalResponse::TraceSchedEvent` carries
    /// its reply: the reply's fields, in tool_global.rs, and the replayed
    /// slice end one of them holds, in scheduler/replayer.rs.
    const TRACE_REPLY_WIRE: [&str; 1] = ["pub struct TraceSchedEventResponse {"];
    const REPLAYED_SLICE_END_WIRE: [&str; 1] = ["pub struct ReplayedSliceEnd {"];

    /// The nested types, outside the two enums, whose fields decide a
    /// message's positional (bincode) layout, in detcore-model's
    /// host_input.rs: the `HostFileIdentity` that
    /// `GlobalRequest::RecordHostInput` carries.
    const HOST_INPUT_WIRE: [&str; 1] = ["pub struct HostFileIdentity {"];

    /// The payload types package K item 2b slice 2b-1 added or changed and
    /// its variants carry: `DeregisterThread`'s `ThreadDeregistration` (its
    /// `wait_status`) and `WithCpuTime`'s `CpuPublication`, in tool_global.rs,
    /// and the `TaskCpuTimes` that holds, in procfs/task_stat.rs. Listed by
    /// hand, as revision 32 listed its own; covering every serialized
    /// payload's layout mechanically is minibeads dh-uxyyj.
    const PROCFS_PAYLOAD_WIRE: [&str; 2] = [
        "pub struct ThreadDeregistration {",
        "pub struct CpuPublication {",
    ];
    const TASK_CPU_TIMES_WIRE: [&str; 1] = ["pub struct TaskCpuTimes {"];

    /// The payload type package K item 2b slice 2b-3 added, which
    /// `RecordExecStrings` and `ProcessExecStrings`'s response carry, in
    /// tool_global.rs.
    const EXEC_STRINGS_WIRE: [&str; 1] = ["pub struct ExecStrings {"];

    /// The lines of code of the item `source` declares, unindented, on the
    /// line `header`: the attributes just above it (its derives and serde
    /// attributes decide its encoding), the header, and its body through
    /// the closing brace without comments or blank lines.
    fn wire_item(source: &str, header: &str) -> Vec<String> {
        let lines: Vec<&str> = source.lines().collect();
        let at = lines
            .iter()
            .position(|line| *line == header)
            .unwrap_or_else(|| panic!("{header} is declared"));
        let attributes = lines[..at]
            .iter()
            .rev()
            .take_while(|line| line.starts_with("#["))
            .count();
        let mut code: Vec<String> = lines[at - attributes..=at]
            .iter()
            .map(|line| (*line).to_owned())
            .collect();
        code.extend(
            lines[at + 1..]
                .iter()
                .take_while(|line| **line != "}")
                .map(|line| line.trim())
                .filter(|line| !line.is_empty() && !line.starts_with("//"))
                .map(str::to_owned),
        );
        code
    }

    /// The wire items `headers` of `source`, one after another.
    fn wire_items(source: &str, headers: &[&str]) -> String {
        headers
            .iter()
            .map(|header| wire_item(source, header).join("\n"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A digest of the code of `GlobalRequest` and `GlobalResponse` in
    /// `source`, of the StoreExclusive payload's types in `payload`, of the
    /// scheduled event's wire items in `schedule`, of the trace reply's
    /// wire items in `source` and `replayer`, of the nested wire types
    /// in `host_input` (`HOST_INPUT_WIRE`), of 2b-1's payload types in
    /// `source` and `task_stat`, and of 2b-3's in `source`: their variants,
    /// fields, payload types, order
    /// and attributes, not their documentation.
    fn definition_digest(
        source: &str,
        payload: &str,
        schedule: &str,
        replayer: &str,
        host_input: &str,
        task_stat: &str,
    ) -> String {
        let request = definition(source, "pub enum GlobalRequest {").join("\n");
        let response = definition(source, "pub enum GlobalResponse {").join("\n");
        let payload = STORE_EXCLUSIVE_PAYLOAD
            .iter()
            .map(|header| nested_definition(payload, header).join("\n"))
            .collect::<Vec<_>>()
            .join("\n");
        let event = wire_items(schedule, &SCHED_EVENT_WIRE);
        let reply = [
            wire_items(source, &TRACE_REPLY_WIRE),
            wire_items(replayer, &REPLAYED_SLICE_END_WIRE),
        ]
        .join("\n");
        let nested = wire_items(host_input, &HOST_INPUT_WIRE);
        let procfs = [
            wire_items(source, &PROCFS_PAYLOAD_WIRE),
            wire_items(task_stat, &TASK_CPU_TIMES_WIRE),
        ]
        .join("\n");
        let exec_strings = wire_items(source, &EXEC_STRINGS_WIRE);
        fnv_domains(&[
            (0, request.as_bytes()),
            (1, response.as_bytes()),
            (3, payload.as_bytes()),
            (4, event.as_bytes()),
            (5, reply.as_bytes()),
            (6, nested.as_bytes()),
            (7, procfs.as_bytes()),
            (8, exec_strings.as_bytes()),
        ])
    }

    fn resources_definition_digest(source: &str) -> String {
        let resources = definition(source, "pub struct Resources {").join("\n");
        fnv_domains(&[(2, resources.as_bytes())])
    }

    /// The global RPC's revision names its enums and Resources as they are:
    /// a variant or field added, removed, reordered or retyped changes the digest,
    /// and the test then
    /// fails until a new revision is recorded, which changes the fingerprint
    /// every runtime compares with its coordinator's.
    #[test]
    fn the_global_rpc_revision_matches_its_definitions() {
        let source = include_str!("tool_global.rs");
        let payload = include_str!("store_exclusive.rs");
        let schedule = detcore_model::SCHEDULE_RS_SOURCE;
        let replayer = include_str!("scheduler/replayer.rs");
        let host_input = include_str!("../../detcore-model/src/host_input.rs");
        let task_stat = include_str!("procfs/task_stat.rs");
        let digest = definition_digest(source, payload, schedule, replayer, host_input, task_stat);
        let resources = include_str!("resources.rs");
        let resources_digest = resources_definition_digest(resources);
        let revision = crate::tool_global::GLOBAL_RPC_REVISION;
        let after = |revision: u32| {
            let mut next = revision + 1;
            while RESERVED_GLOBAL_RPC_REVISIONS.contains(&next) {
                next += 1;
            }
            next
        };
        let next = GLOBAL_RPC_REVISIONS
            .last()
            .map_or(1, |(last, _, _)| after(*last));
        assert_eq!(
            GLOBAL_RPC_REVISIONS.last(),
            Some(&(revision, digest.as_str(), resources_digest.as_str())),
            "GlobalRequest, GlobalResponse or Resources changed: append ({next}, \
             \"{digest}\", \"{resources_digest}\") to GLOBAL_RPC_REVISIONS and set \
             GLOBAL_RPC_REVISION to {next}, so \
             that runtimes and coordinators from either side of the change refuse each other"
        );
        for pair in GLOBAL_RPC_REVISIONS.windows(2) {
            assert_eq!(
                pair[1].0,
                after(pair[0].0),
                "revisions count up by one, skipping only a reserved revision"
            );
            assert_ne!(
                (pair[1].1, pair[1].2),
                (pair[0].1, pair[0].2),
                "each revision changed the wire definitions"
            );
        }

        // What the digest sees, and what it does not.
        let documented = source.replacen(
            "pub enum GlobalRequest {\n",
            "pub enum GlobalRequest {\n    /// Documentation only.\n\n",
            1,
        );
        assert_eq!(
            definition_digest(
                &documented,
                payload,
                schedule,
                replayer,
                host_input,
                task_stat
            ),
            digest
        );
        let added = source.replacen(
            "pub enum GlobalRequest {\n",
            "pub enum GlobalRequest {\n    Added(u32),\n",
            1,
        );
        assert_ne!(
            definition_digest(&added, payload, schedule, replayer, host_input, task_stat),
            digest
        );
        // Retype GlobalRequest's VirtualInotifyCookies wherever it sits: later
        // revisions append variants after it.
        let retyped = source.replacen(
            "    VirtualInotifyCookies(Vec<u32>),\n",
            "    VirtualInotifyCookies(Vec<u64>),\n",
            1,
        );
        assert_ne!(retyped, source);
        assert_ne!(
            definition_digest(&retyped, payload, schedule, replayer, host_input, task_stat),
            digest
        );
        // A field added to a payload type changes it too, as this change's
        // own move from tuples to `ScannedMapping` and `Found` did; its
        // documentation does not.
        let grown = payload.replacen(
            "    pub struct Found {\n",
            "    pub struct Found {\n        pub added: u64,\n",
            1,
        );
        assert_ne!(grown, payload);
        assert_ne!(
            definition_digest(source, &grown, schedule, replayer, host_input, task_stat),
            digest
        );
        let documented = payload.replacen(
            "    pub struct Found {\n",
            "    pub struct Found {\n        /// Documentation only.\n",
            1,
        );
        assert_ne!(documented, payload);
        assert_eq!(
            definition_digest(
                source,
                &documented,
                schedule,
                replayer,
                host_input,
                task_stat
            ),
            digest
        );
        // The event `TraceSchedEvent` carries is detcore-model's: a field
        // added to it, or a change to which fields its `Serialize` writes,
        // changes the digest; a doc line does not.
        let grown = schedule.replacen(
            "    pub instruction_offset: Option<InstructionOffset>,\n}",
            "    pub instruction_offset: Option<InstructionOffset>,\n    pub added: u32,\n}",
            1,
        );
        assert_ne!(grown, schedule);
        assert_ne!(
            definition_digest(source, payload, &grown, replayer, host_input, task_stat),
            digest
        );
        let carried = schedule.replacen(
            "let written_stopless = *stopless || binary;",
            "let written_stopless = *stopless;",
            1,
        );
        assert_ne!(carried, schedule);
        assert_ne!(
            definition_digest(source, payload, &carried, replayer, host_input, task_stat),
            digest
        );
        let documented = schedule.replacen(
            "pub struct SchedEvent {\n",
            "pub struct SchedEvent {\n    /// Documentation only.\n",
            1,
        );
        assert_ne!(documented, schedule);
        assert_eq!(
            definition_digest(
                source,
                payload,
                &documented,
                replayer,
                host_input,
                task_stat
            ),
            digest
        );
        // So is the reply: a field added to the replayed slice end it holds,
        // or its timeslice retyped, changes the digest; a doc line does not.
        let grown = replayer.replacen(
            "pub struct ReplayedSliceEnd {\n",
            "pub struct ReplayedSliceEnd {\n    pub added: u32,\n",
            1,
        );
        assert_ne!(grown, replayer);
        assert_ne!(
            definition_digest(source, payload, schedule, &grown, host_input, task_stat),
            digest
        );
        let retyped = source.replacen(
            "    timeslice: Option<ReplayedSliceEnd>,\n",
            "    timeslice: Option<LogicalTime>,\n",
            1,
        );
        assert_ne!(retyped, source);
        assert_ne!(
            definition_digest(&retyped, payload, schedule, replayer, host_input, task_stat),
            digest
        );
        let documented = replayer.replacen(
            "pub struct ReplayedSliceEnd {\n",
            "pub struct ReplayedSliceEnd {\n    /// Documentation only.\n",
            1,
        );
        assert_ne!(documented, replayer);
        assert_eq!(
            definition_digest(
                source,
                payload,
                schedule,
                &documented,
                host_input,
                task_stat
            ),
            digest
        );
        // The nested `HostFileIdentity`: a field added, or its encoding's
        // attributes changed, moves the digest; a doc line does not.
        let documented = host_input.replacen(
            "pub struct HostFileIdentity {\n",
            "pub struct HostFileIdentity {\n    /// Documentation only.\n",
            1,
        );
        assert_ne!(documented, host_input);
        assert_eq!(
            definition_digest(source, payload, schedule, replayer, &documented, task_stat),
            digest
        );
        let added = host_input.replacen(
            "pub struct HostFileIdentity {\n",
            "pub struct HostFileIdentity {\n    pub added: u32,\n",
            1,
        );
        assert_ne!(
            definition_digest(source, payload, schedule, replayer, &added, task_stat),
            digest
        );
        let renamed = host_input.replacen(
            "#[serde(default, rename = \"snapshot_content\")]",
            "#[serde(default)]",
            1,
        );
        assert_ne!(renamed, host_input);
        assert_ne!(
            definition_digest(source, payload, schedule, replayer, &renamed, task_stat),
            digest
        );
        let rederived = host_input.replacen(
            "#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]\npub struct HostFileIdentity {",
            "#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]\npub struct HostFileIdentity {",
            1,
        );
        assert_ne!(rederived, host_input);
        assert_ne!(
            definition_digest(source, payload, schedule, replayer, &rederived, task_stat),
            digest
        );
        // And 2b-1's payload types: a field added to `ThreadDeregistration` or
        // to `TaskCpuTimes` changes the digest; a doc line does not.
        let grown = source.replacen(
            "pub struct ThreadDeregistration {\n",
            "pub struct ThreadDeregistration {\n    pub(crate) added: u32,\n",
            1,
        );
        assert_ne!(grown, source);
        assert_ne!(
            definition_digest(&grown, payload, schedule, replayer, host_input, task_stat),
            digest
        );
        let grown = task_stat.replacen(
            "pub struct TaskCpuTimes {\n",
            "pub struct TaskCpuTimes {\n    pub added: LogicalTime,\n",
            1,
        );
        assert_ne!(grown, task_stat);
        assert_ne!(
            definition_digest(source, payload, schedule, replayer, host_input, &grown),
            digest
        );
        let documented = source.replacen(
            "pub struct CpuPublication {\n",
            "pub struct CpuPublication {\n    /// Documentation only.\n",
            1,
        );
        assert_ne!(documented, source);
        assert_eq!(
            definition_digest(
                &documented,
                payload,
                schedule,
                replayer,
                host_input,
                task_stat
            ),
            digest
        );
        // And 2b-3's: a field added to `ExecStrings` or one retyped changes
        // the digest; a doc line does not.
        let grown = source.replacen(
            "pub struct ExecStrings {\n",
            "pub struct ExecStrings {\n    pub added: u32,\n",
            1,
        );
        assert_ne!(grown, source);
        assert_ne!(
            definition_digest(&grown, payload, schedule, replayer, host_input, task_stat),
            digest
        );
        let retyped = source.replacen("    pub envp: Vec<u8>,\n", "    pub envp: String,\n", 1);
        assert_ne!(retyped, source);
        assert_ne!(
            definition_digest(&retyped, payload, schedule, replayer, host_input, task_stat),
            digest
        );
        let documented = source.replacen(
            "pub struct ExecStrings {\n",
            "pub struct ExecStrings {\n    /// Documentation only.\n",
            1,
        );
        assert_ne!(documented, source);
        assert_eq!(
            definition_digest(
                &documented,
                payload,
                schedule,
                replayer,
                host_input,
                task_stat
            ),
            digest
        );

        // The reply this change extends is covered the same way: a field
        // added to `TraceSchedEventResponse`, or its backstop removed,
        // changes the digest (domain 5); the positional response a runtime
        // decodes changes with it.
        let nested_added = source.replacen(
            "pub struct TraceSchedEventResponse {\n",
            "pub struct TraceSchedEventResponse {\n    added: bool,\n",
            1,
        );
        assert_ne!(nested_added, source);
        assert_ne!(
            definition_digest(
                &nested_added,
                payload,
                schedule,
                replayer,
                host_input,
                task_stat
            ),
            digest
        );
        let nested_removed = source.replacen("    backstop: Option<LogicalTime>,\n", "", 1);
        assert_ne!(nested_removed, source);
        assert_ne!(
            definition_digest(
                &nested_removed,
                payload,
                schedule,
                replayer,
                host_input,
                task_stat
            ),
            digest
        );

        let documented = resources.replacen(
            "pub struct Resources {\n",
            "pub struct Resources {\n    /// Documentation only.\n\n",
            1,
        );
        assert_ne!(documented, resources);
        assert_eq!(resources_definition_digest(&documented), resources_digest);
        let removed = resources.replacen("    pub(crate) priority_row: Option<u64>,\n", "", 1);
        assert_ne!(removed, resources);
        assert_ne!(resources_definition_digest(&removed), resources_digest);
        let retyped =
            resources.replacen("priority_row: Option<u64>", "priority_row: Option<u32>", 1);
        assert_ne!(retyped, resources);
        assert_ne!(resources_definition_digest(&retyped), resources_digest);
    }

    /// The published fingerprint is the configuration's extended with the
    /// revision, so another revision is a mismatch, named with both values.
    #[test]
    fn the_fingerprint_carries_the_global_rpc_revision() {
        let model = detcore_model::config::config_wire_fingerprint();
        let ours = config_wire_fingerprint();
        let revision = crate::tool_global::GLOBAL_RPC_REVISION;
        assert_eq!(ours.len(), 16);
        assert_eq!(ours, fingerprint_with_revision(&model, revision));
        assert_ne!(ours, model);
        let other = fingerprint_with_revision(&model, revision + 1);
        assert_ne!(other, ours);
        // A build at the previous revision, from before the newest wire
        // change, is refused too.
        let previous = fingerprint_with_revision(&model, revision - 1);
        assert!(
            config_fingerprint_mismatch(std::ffi::OsStr::new(&previous)).is_some(),
            "the previous revision is a mismatch"
        );
        assert_eq!(
            config_fingerprint_mismatch(std::ffi::OsStr::new(&ours)),
            None
        );
        let mismatch = config_fingerprint_mismatch(std::ffi::OsStr::new(&other))
            .expect("another revision is a mismatch");
        assert!(
            mismatch.contains(&ours) && mismatch.contains(&other),
            "{mismatch}"
        );
    }
}
