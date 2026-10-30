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
        (24, "86768711e86e29fd", "f013e36743d8d9e6"),
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
    const STORE_EXCLUSIVE_PAYLOAD: [&str; 3] = [
        "pub enum Report {",
        "pub struct ScannedMapping {",
        "pub struct Found {",
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

    /// A digest of the code of `GlobalRequest` and `GlobalResponse` in
    /// `source`, and of the StoreExclusive payload's types in `payload`:
    /// their variants, fields, payload types, order and attributes, not
    /// their documentation.
    fn definition_digest(source: &str, payload: &str) -> String {
        let request = definition(source, "pub enum GlobalRequest {").join("\n");
        let response = definition(source, "pub enum GlobalResponse {").join("\n");
        let payload = STORE_EXCLUSIVE_PAYLOAD
            .iter()
            .map(|header| nested_definition(payload, header).join("\n"))
            .collect::<Vec<_>>()
            .join("\n");
        fnv_domains(&[
            (0, request.as_bytes()),
            (1, response.as_bytes()),
            (3, payload.as_bytes()),
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
        let digest = definition_digest(source, payload);
        let resources = include_str!("resources.rs");
        let resources_digest = resources_definition_digest(resources);
        let revision = crate::tool_global::GLOBAL_RPC_REVISION;
        let next = GLOBAL_RPC_REVISIONS
            .last()
            .map_or(1, |(last, _, _)| last + 1);
        assert_eq!(
            GLOBAL_RPC_REVISIONS.last(),
            Some(&(revision, digest.as_str(), resources_digest.as_str())),
            "GlobalRequest, GlobalResponse or Resources changed: append ({next}, \
             \"{digest}\", \"{resources_digest}\") to GLOBAL_RPC_REVISIONS and set \
             GLOBAL_RPC_REVISION to {next}, so \
             that runtimes and coordinators from either side of the change refuse each other"
        );
        for pair in GLOBAL_RPC_REVISIONS.windows(2) {
            assert_eq!(pair[1].0, pair[0].0 + 1, "revisions count up by one");
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
        assert_eq!(definition_digest(&documented, payload), digest);
        let added = source.replacen(
            "pub enum GlobalRequest {\n",
            "pub enum GlobalRequest {\n    Added(u32),\n",
            1,
        );
        assert_ne!(definition_digest(&added, payload), digest);
        // Retype GlobalRequest's VirtualInotifyCookies wherever it sits: later
        // revisions append variants after it.
        let retyped = source.replacen(
            "    VirtualInotifyCookies(Vec<u32>),\n",
            "    VirtualInotifyCookies(Vec<u64>),\n",
            1,
        );
        assert_ne!(retyped, source);
        assert_ne!(definition_digest(&retyped, payload), digest);
        // A field added to a payload type changes it too, as this change's
        // own move from tuples to `ScannedMapping` and `Found` did; its
        // documentation does not.
        let grown = payload.replacen(
            "    pub struct Found {\n",
            "    pub struct Found {\n        pub added: u64,\n",
            1,
        );
        assert_ne!(grown, payload);
        assert_ne!(definition_digest(source, &grown), digest);
        let documented = payload.replacen(
            "    pub struct Found {\n",
            "    pub struct Found {\n        /// Documentation only.\n",
            1,
        );
        assert_ne!(documented, payload);
        assert_eq!(definition_digest(source, &documented), digest);

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
