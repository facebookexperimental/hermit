// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Retired E2E manifest test ids and the live ids that replaced them.
//!
//! `tests/e2e/manifests/inventory/retired-ids.json` records every test id that
//! no manifest declares any more because its program moved to a different id,
//! for example when the backend-parity-c bucket was folded into c-programs
//! (<https://github.com/rrnewton/hermit/issues/3301>, slice S6). History is
//! keyed by test id, so a reader that must join a row recorded under an old id
//! with a row recorded under the new one resolves the old id through this file
//! rather than through a string rewrite of its own.
//!
//! The file must stay a bijection from its retired ids onto live ids: every
//! retired id is absent from the manifests, every successor is declared by one,
//! and no two retired ids share a successor. [`RetiredIds::check_live`] is the
//! check; the manifest expander runs it on every metadata validation.
//!
//! A successor must also be the retired id's own test: either the plain
//! bucket-prefix rename (`old-bucket/name` becomes `new-bucket/name`) or a
//! rename listed, with its reason, in the retirement's `collisions`.
//! [`RetiredIds::parse`] refuses any other successor, so a map that points a
//! retired id at an unrelated live test is refused even though that test is
//! live and unshared.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;

use serde::Deserialize;

/// Repository-relative path of the retired-id map.
pub const RETIRED_IDS_FILE: &str = "tests/e2e/manifests/inventory/retired-ids.json";

/// The only schema this reader accepts.
pub const RETIRED_IDS_SCHEMA: u64 = 1;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RetiredIds {
    pub schema: u64,
    pub description: String,
    pub retirements: Vec<Retirement>,
}

/// One retired bucket and the id each of its tests now carries.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Retirement {
    /// The manifest bucket that no longer exists.
    pub retired_bucket: String,
    /// The live bucket whose manifest now declares every successor id.
    pub successor_bucket: String,
    /// The last Hermit commit whose manifests declared the retired ids.
    pub last_live_commit: String,
    /// Why the ids moved, including every successor that is not a plain
    /// bucket-prefix rename.
    pub reason: String,
    /// Retired id to successor id.
    pub ids: BTreeMap<String, String>,
    /// Every retired id whose successor is not the plain bucket-prefix rename,
    /// mapped to why the plain rename could not be used. Required, and empty
    /// when every successor is the plain rename.
    pub collisions: BTreeMap<String, String>,
}

fn test_name<'a>(id: &'a str, bucket: &str) -> Option<&'a str> {
    id.strip_prefix(bucket)
        .and_then(|rest| rest.strip_prefix('/'))
        .filter(|name| {
            !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
                && !name.starts_with('-')
                && !name.ends_with('-')
        })
}

impl RetiredIds {
    /// Read and structurally validate the repository's retired-id map.
    pub fn load(root: &Path) -> Result<Self, String> {
        let path = root.join(RETIRED_IDS_FILE);
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        Self::parse(&text).map_err(|error| format!("{}: {error}", path.display()))
    }

    /// Parse and structurally validate a retired-id map.
    ///
    /// This refuses anything that would make a historical id resolve
    /// ambiguously: an unknown schema, an empty retirement, a retired id outside
    /// its retired bucket, a successor outside its successor bucket, a retired
    /// id listed twice, two retired ids sharing a successor, or a successor that
    /// is itself retired (a chain that a single lookup would leave half-done).
    ///
    /// It also refuses a successor that is not the retired id's own test: a
    /// successor must be the plain bucket-prefix rename unless the retired id is
    /// listed in `collisions` with a reason, and a listed collision must be a
    /// retired id of the same retirement whose successor is not the plain
    /// rename.
    pub fn parse(text: &str) -> Result<Self, String> {
        let map: Self = serde_json::from_str(text)
            .map_err(|error| format!("invalid retired-id map: {error}"))?;
        if map.schema != RETIRED_IDS_SCHEMA {
            return Err(format!(
                "retired-id map schema must be {RETIRED_IDS_SCHEMA}, found {}",
                map.schema
            ));
        }
        if map.description.trim().is_empty() {
            return Err("retired-id map has no description".into());
        }
        if map.retirements.is_empty() {
            return Err("retired-id map has no retirement".into());
        }
        let mut retired = BTreeSet::new();
        let mut successors = BTreeSet::new();
        let mut buckets = BTreeSet::new();
        for retirement in &map.retirements {
            let bucket = &retirement.retired_bucket;
            if bucket.is_empty() || retirement.successor_bucket.is_empty() {
                return Err("a retirement names an empty bucket".into());
            }
            if bucket == &retirement.successor_bucket {
                return Err(format!("retirement of {bucket} names itself as successor"));
            }
            if !buckets.insert(bucket.clone()) {
                return Err(format!("bucket {bucket} is retired twice"));
            }
            if retirement.last_live_commit.len() != 40
                || !retirement
                    .last_live_commit
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(format!(
                    "retirement of {bucket} needs a full lowercase last_live_commit SHA"
                ));
            }
            if retirement.reason.trim().is_empty() {
                return Err(format!("retirement of {bucket} gives no reason"));
            }
            if retirement.ids.is_empty() {
                return Err(format!("retirement of {bucket} retires no id"));
            }
            for (old, reason) in &retirement.collisions {
                if !retirement.ids.contains_key(old) {
                    return Err(format!(
                        "collision {old:?} is not a retired id of bucket {bucket}"
                    ));
                }
                if reason.trim().is_empty() {
                    return Err(format!("collision {old:?} gives no reason"));
                }
            }
            for (old, new) in &retirement.ids {
                let Some(old_name) = test_name(old, bucket) else {
                    return Err(format!(
                        "retired id {old:?} is not a test of bucket {bucket}"
                    ));
                };
                let Some(new_name) = test_name(new, &retirement.successor_bucket) else {
                    return Err(format!(
                        "successor {new:?} of {old:?} is not a test of bucket {}",
                        retirement.successor_bucket
                    ));
                };
                let listed = retirement.collisions.contains_key(old);
                if old_name == new_name && listed {
                    return Err(format!(
                        "collision {old:?} maps to the plain prefix rename {new:?}; list only successors that differ from it"
                    ));
                }
                if old_name != new_name && !listed {
                    return Err(format!(
                        "successor {new:?} of {old:?} is neither the prefix rename {}/{old_name} nor a listed collision",
                        retirement.successor_bucket
                    ));
                }
                if !retired.insert(old.clone()) {
                    return Err(format!("retired id {old:?} is listed twice"));
                }
                if !successors.insert(new.clone()) {
                    return Err(format!("successor {new:?} is shared by two retired ids"));
                }
            }
        }
        if let Some(chained) = retired.intersection(&successors).next() {
            return Err(format!(
                "{chained:?} is both a retired id and a successor; record the final id directly"
            ));
        }
        Ok(map)
    }

    /// The live id that carries `id`'s history: its successor if `id` is
    /// retired, otherwise `id` itself.
    pub fn resolve<'a>(&'a self, id: &'a str) -> &'a str {
        self.successor(id).unwrap_or(id)
    }

    /// The successor of a retired id, or `None` if `id` is not retired.
    pub fn successor(&self, id: &str) -> Option<&str> {
        self.retirements
            .iter()
            .find_map(|retirement| retirement.ids.get(id).map(String::as_str))
    }

    /// The retirement of `bucket`, if that bucket was retired.
    pub fn retirement(&self, bucket: &str) -> Option<&Retirement> {
        self.retirements
            .iter()
            .find(|retirement| retirement.retired_bucket == bucket)
    }

    /// Every successor id of the retired `bucket`, or an error if the bucket
    /// was never retired.
    pub fn successors_of(&self, bucket: &str) -> Result<BTreeSet<String>, String> {
        self.retirement(bucket)
            .map(|retirement| retirement.ids.values().cloned().collect())
            .ok_or_else(|| format!("bucket {bucket} has no retirement in {RETIRED_IDS_FILE}"))
    }

    /// Check the map against the ids and buckets the manifests declare now.
    ///
    /// No retired bucket may be live, no retired id may be live, and every
    /// successor must be live in its successor bucket. With the structural
    /// checks in [`RetiredIds::parse`], this makes the map a bijection from its
    /// retired ids onto a set of live ids.
    pub fn check_live(
        &self,
        live_ids: &BTreeSet<String>,
        live_buckets: &BTreeSet<String>,
    ) -> Result<(), String> {
        for retirement in &self.retirements {
            if live_buckets.contains(&retirement.retired_bucket) {
                return Err(format!(
                    "retired bucket {} still has a manifest",
                    retirement.retired_bucket
                ));
            }
            if !live_buckets.contains(&retirement.successor_bucket) {
                return Err(format!(
                    "successor bucket {} of {} has no manifest",
                    retirement.successor_bucket, retirement.retired_bucket
                ));
            }
            for (old, new) in &retirement.ids {
                if live_ids.contains(old) {
                    return Err(format!(
                        "retired id {old:?} is still declared by a manifest"
                    ));
                }
                if !live_ids.contains(new) {
                    return Err(format!(
                        "successor {new:?} of retired id {old:?} is not declared by any manifest"
                    ));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(items: &[(&str, &str)]) -> BTreeMap<String, String> {
        items
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn map_with(ids: &[(&str, &str)], collisions: &[(&str, &str)]) -> String {
        serde_json::json!({
            "schema": 1,
            "description": "test map",
            "retirements": [{
                "retired_bucket": "old",
                "successor_bucket": "new",
                "last_live_commit": "a".repeat(40),
                "reason": "folded",
                "ids": pairs(ids),
                "collisions": pairs(collisions),
            }],
        })
        .to_string()
    }

    fn map(ids: &[(&str, &str)]) -> String {
        map_with(ids, &[])
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn resolves_retired_ids_and_passes_live_ids_through() {
        let ids = RetiredIds::parse(&map_with(
            &[("old/a", "new/a"), ("old/b", "new/b-pair")],
            &[("old/b", "new/b names a different program")],
        ))
        .unwrap();
        assert_eq!(ids.resolve("old/a"), "new/a");
        assert_eq!(ids.resolve("old/b"), "new/b-pair");
        assert_eq!(ids.resolve("new/a"), "new/a");
        assert_eq!(ids.resolve("other/x"), "other/x");
        assert_eq!(
            ids.successors_of("old").unwrap(),
            set(&["new/a", "new/b-pair"])
        );
        assert!(ids.successors_of("new").is_err());
        ids.check_live(&set(&["new/a", "new/b-pair", "new/c"]), &set(&["new"]))
            .unwrap();
    }

    #[test]
    fn refuses_every_non_bijective_or_ambiguous_map() {
        for (ids, collisions, needle) in [
            (
                vec![("old/a", "new/x"), ("old/b", "new/x")],
                vec![("old/a", "new/a is taken"), ("old/b", "new/b is taken")],
                "shared by two",
            ),
            (
                vec![("other/a", "new/a")],
                vec![],
                "not a test of bucket old",
            ),
            (
                vec![("old/a", "other/a")],
                vec![],
                "not a test of bucket new",
            ),
            (vec![("old/A", "new/a")], vec![], "not a test of bucket old"),
            (vec![], vec![], "retires no id"),
            // A successor that is neither the prefix rename nor a listed
            // collision: a retired id pointed at an unrelated test.
            (
                vec![("old/a", "new/a"), ("old/b", "new/unrelated")],
                vec![],
                "neither the prefix rename new/b nor a listed collision",
            ),
            // A listed collision whose successor is the plain rename.
            (
                vec![("old/a", "new/a")],
                vec![("old/a", "new/a is taken")],
                "maps to the plain prefix rename",
            ),
            // A listed collision that the retirement does not retire.
            (
                vec![("old/a", "new/a-pair")],
                vec![("old/a", "new/a is taken"), ("old/z", "new/z is taken")],
                "is not a retired id of bucket old",
            ),
            // A listed collision with no reason.
            (
                vec![("old/a", "new/a-pair")],
                vec![("old/a", "  ")],
                "gives no reason",
            ),
        ] {
            let error = RetiredIds::parse(&map_with(&ids, &collisions)).unwrap_err();
            assert!(error.contains(needle), "{error}");
        }
        let mut undeclared: serde_json::Value =
            serde_json::from_str(&map(&[("old/a", "new/a")])).unwrap();
        undeclared["retirements"][0]
            .as_object_mut()
            .unwrap()
            .remove("collisions");
        assert!(
            RetiredIds::parse(&undeclared.to_string())
                .unwrap_err()
                .contains("missing field `collisions`")
        );
        let mut chained: serde_json::Value =
            serde_json::from_str(&map(&[("old/a", "new/a")])).unwrap();
        chained["retirements"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "retired_bucket": "new", "successor_bucket": "newer",
                "last_live_commit": "b".repeat(40), "reason": "again",
                "ids": {"new/a": "newer/a"}, "collisions": {},
            }));
        let error = RetiredIds::parse(&chained.to_string()).unwrap_err();
        assert!(
            error.contains("both a retired id and a successor"),
            "{error}"
        );
        let mut unknown: serde_json::Value =
            serde_json::from_str(&map(&[("old/a", "new/a")])).unwrap();
        unknown["extra"] = serde_json::json!(true);
        assert!(RetiredIds::parse(&unknown.to_string()).is_err());
        let mut short: serde_json::Value =
            serde_json::from_str(&map(&[("old/a", "new/a")])).unwrap();
        short["retirements"][0]["last_live_commit"] = serde_json::json!("abc");
        assert!(
            RetiredIds::parse(&short.to_string())
                .unwrap_err()
                .contains("last_live_commit")
        );
    }

    #[test]
    fn live_check_refuses_a_live_retired_id_or_a_missing_successor() {
        let ids = RetiredIds::parse(&map(&[("old/a", "new/a")])).unwrap();
        for (live, buckets, needle) in [
            (set(&["new/a", "old/a"]), set(&["new"]), "still declared"),
            (
                set(&["new/b"]),
                set(&["new"]),
                "not declared by any manifest",
            ),
            (
                set(&["new/a"]),
                set(&["new", "old"]),
                "still has a manifest",
            ),
            (set(&["new/a"]), set(&["other"]), "has no manifest"),
        ] {
            let error = ids.check_live(&live, &buckets).unwrap_err();
            assert!(error.contains(needle), "{error}");
        }
    }
}
