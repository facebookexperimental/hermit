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
    fingerprint_with_revision(
        &detcore_model::config::config_wire_fingerprint(),
        crate::tool_global::GLOBAL_RPC_REVISION,
    )
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

    /// A digest of the code of `GlobalRequest` and `GlobalResponse` in
    /// `source`: their variants, payload types, order and attributes, not
    /// their documentation.
    fn definition_digest(source: &str) -> String {
        let request = definition(source, "pub enum GlobalRequest {").join("\n");
        let response = definition(source, "pub enum GlobalResponse {").join("\n");
        fnv_domains(&[(0, request.as_bytes()), (1, response.as_bytes())])
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
        let digest = definition_digest(source);
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
        assert_eq!(definition_digest(&documented), digest);
        let added = source.replacen(
            "pub enum GlobalRequest {\n",
            "pub enum GlobalRequest {\n    Added(u32),\n",
            1,
        );
        assert_ne!(definition_digest(&added), digest);
        // Retype GlobalRequest's VirtualInotifyCookies wherever it sits: later
        // revisions append variants after it.
        let retyped = source.replacen(
            "    VirtualInotifyCookies(Vec<u32>),\n",
            "    VirtualInotifyCookies(Vec<u64>),\n",
            1,
        );
        assert_ne!(retyped, source);
        assert_ne!(definition_digest(&retyped), digest);

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
