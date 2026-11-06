//! Source ownership of manifest cells. This selects no backend build recipe.
//! A partition keeps the original cell identity and assertions; a missing owner
//! is an error, never an unreported successful cell.
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;

use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    schema: u64,
    backends: BTreeMap<String, String>,
    run_type_owners: BTreeMap<String, String>,
}

pub struct ExecutionOwners {
    backends: BTreeMap<String, String>,
    run_type_owners: BTreeMap<String, String>,
}
impl ExecutionOwners {
    pub fn load(root: &Path) -> Result<Self, String> {
        let path = root.join("ci/execution-owners.json");
        let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if bytes.len() > 65536 {
            return Err("execution owner document exceeds bound".into());
        }
        Self::from_slice(&bytes)
    }
    pub fn from_slice(bytes: &[u8]) -> Result<Self, String> {
        // serde_json::Value would silently collapse duplicate object keys. Use
        // a recursive strict JSON front door before decoding typed fields.
        reject_duplicate_keys(bytes)?;
        let document: Document = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        if document.schema != 1
            || document.backends.is_empty()
            || document.backends.iter().any(|(backend, owner)| {
                backend.is_empty()
                    || owner.is_empty()
                    || !backend.bytes().all(|c| c.is_ascii_lowercase() || c == b'-')
                    || !owner.bytes().all(|c| c.is_ascii_lowercase() || c == b'-')
            })
        {
            return Err("closed nonempty execution owner schema1 required".into());
        }
        let owners = Self {
            backends: document.backends,
            run_type_owners: document.run_type_owners,
        };
        if owners.run_type_owners
            != BTreeMap::from([("sabre-compat-only".into(), "patching".into())])
        {
            return Err("retained focused run-type source ownership changed".into());
        }
        if owners.backends("core")? != core_backends() {
            return Err(
                "core execution owner differs from the actual first-party frontend set".into(),
            );
        }
        Ok(owners)
    }
    pub fn backends(&self, owner: &str) -> Result<BTreeSet<String>, String> {
        let selected = self
            .backends
            .iter()
            .filter_map(|(backend, value)| (value == owner).then_some(backend.clone()))
            .collect::<BTreeSet<_>>();
        if selected.is_empty() {
            return Err(format!("execution owner {owner:?} names no backend"));
        }
        Ok(selected)
    }
    /// A complete labelled run type can own its first-party comparator cells.
    /// The backend mapping remains disjoint for ordinary mixed buckets.
    pub fn run_type_owner(&self, label: &str) -> &str {
        self.run_type_owners
            .get(label)
            .map(String::as_str)
            .unwrap_or("core")
    }
    pub fn backends_for_selection(
        &self,
        owner: &str,
        labels: &[String],
    ) -> Result<BTreeSet<String>, String> {
        self.backends(owner)?;
        let focused = labels
            .iter()
            .filter_map(|label| self.run_type_owners.get(label))
            .collect::<BTreeSet<_>>();
        if !focused.is_empty() {
            if focused.len() != 1
                || labels.len() != 1
                || focused.iter().any(|value| value.as_str() != owner)
            {
                return Err("run-type selection crosses declared source owners".into());
            }
            return Ok(self.backends.keys().cloned().collect());
        }
        self.backends(owner)
    }
    /// Whether `label` names a focused run type with a declared
    /// source owner (as opposed to an unowned run type such as
    /// `super`, whose cells remain the backend owner's).
    pub fn is_run_type_owned(&self, label: &str) -> bool {
        self.run_type_owners.contains_key(label)
    }
    /// The execution owner an exact cell on `backend` carrying
    /// `run_types` must be selected under: a focused run type with a
    /// declared owner wins (its selection spans backends, per
    /// [`Self::backends_for_selection`]); otherwise the backend's own
    /// owner. Callers that build one exact-cell command per cell
    /// (pressure-test) pass the result as `--execution-owner`, so a
    /// cell on a non-core backend is not dropped by the default
    /// core-only execution filter.
    pub fn owner_for_cell(
        &self,
        backend: &str,
        run_types: &BTreeSet<String>,
    ) -> Result<String, String> {
        let focused = run_types
            .iter()
            .filter_map(|label| self.run_type_owners.get(label))
            .collect::<BTreeSet<_>>();
        if focused.len() > 1 {
            return Err("run-type selection crosses declared source owners".into());
        }
        if let Some(owner) = focused.into_iter().next() {
            return Ok(owner.clone());
        }
        Ok(self.owner(Some(backend))?.to_string())
    }
    pub fn validate_manifest_backends(
        &self,
        manifests: &crate::runner::ManifestSet,
    ) -> Result<(), String> {
        for document in &manifests.documents {
            for test in &document.test {
                for recipe in test.modes.values() {
                    for backend in &recipe.backends_enabled {
                        self.owner(Some(backend))?;
                    }
                }
            }
        }
        Ok(())
    }
    pub fn owner(&self, backend: Option<&str>) -> Result<&str, String> {
        let backend = backend.unwrap_or("native");
        self.backends
            .get(backend)
            .map(String::as_str)
            .ok_or_else(|| format!("manifest backend {backend:?} has no execution owner"))
    }
    pub fn names(&self) -> BTreeSet<&str> {
        self.backends.values().map(String::as_str).collect()
    }
}

pub(crate) fn reject_duplicate_keys(bytes: &[u8]) -> Result<(), String> {
    struct Unique;
    impl<'de> serde::de::DeserializeSeed<'de> for Unique {
        type Value = ();
        fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
            deserializer.deserialize_any(self)
        }
    }
    impl<'de> serde::de::Visitor<'de> for Unique {
        type Value = ();
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("JSON without duplicate object keys")
        }
        fn visit_map<M: serde::de::MapAccess<'de>>(self, mut map: M) -> Result<(), M::Error> {
            let mut keys = BTreeSet::new();
            while let Some(key) = map.next_key::<String>()? {
                if !keys.insert(key) {
                    return Err(serde::de::Error::custom("duplicate JSON object key"));
                }
                map.next_value_seed(Unique)?;
            }
            Ok(())
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
            while seq.next_element_seed(Unique)?.is_some() {}
            Ok(())
        }
        fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<(), E> {
            Ok(())
        }
        fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<(), E> {
            Ok(())
        }
        fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<(), E> {
            Ok(())
        }
        fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<(), E> {
            Ok(())
        }
        fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<(), E> {
            Ok(())
        }
        fn visit_none<E: serde::de::Error>(self) -> Result<(), E> {
            Ok(())
        }
        fn visit_unit<E: serde::de::Error>(self) -> Result<(), E> {
            Ok(())
        }
    }
    use serde::de::DeserializeSeed;
    let mut parser = serde_json::Deserializer::from_slice(bytes);
    Unique.deserialize(&mut parser).map_err(|e| e.to_string())?;
    parser.end().map_err(|e| e.to_string())
}

pub fn core_backends() -> BTreeSet<String> {
    ["native", "ptrace", "kvm", "liteinst", "in-guest-trap"]
        .into_iter()
        .map(String::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::ManifestSet;
    use crate::runner::Population;
    use crate::runner::Selection;
    #[test]
    fn actual_manifest_owner_partitions_are_disjoint_and_preserve_every_original_cell() {
        let root = crate::git_environment::checkout_root();
        let owners = ExecutionOwners::load(&root).unwrap();
        let manifests = ManifestSet::load(&root).unwrap();
        for all_run_types in [false, true] {
            let baseline = Selection {
                population: Some(Population::Required),
                all_run_types,
                ..Default::default()
            };
            let all = manifests.select(&baseline).unwrap();
            let expected = all
                .iter()
                .map(|cell| cell.id.clone())
                .collect::<BTreeSet<_>>();
            for cell in &all {
                owners.owner(cell.id.backend.as_deref()).unwrap();
            }
            let mut union = BTreeSet::new();
            for owner in owners.names() {
                let mut selection = baseline.clone();
                selection.execution_backends = Some(owners.backends(owner).unwrap());
                for cell in manifests.select(&selection).unwrap() {
                    assert_eq!(owners.owner(cell.id.backend.as_deref()).unwrap(), owner);
                    assert!(union.insert(cell.id), "a cell has two execution owners");
                }
            }
            assert_eq!(
                union, expected,
                "an owned cell disappeared or changed identity"
            );
        }
    }
    #[test]
    fn owner_for_cell_names_the_declared_owner_per_backend_and_run_type() {
        let source = include_bytes!("../../execution-owners.json");
        let owners = ExecutionOwners::from_slice(source).unwrap();
        let full = BTreeSet::from(["full".to_string()]);
        assert_eq!(owners.owner_for_cell("dbt", &full).unwrap(), "dynamorio");
        assert_eq!(owners.owner_for_cell("sabre", &full).unwrap(), "patching");
        assert_eq!(owners.owner_for_cell("ptrace", &full).unwrap(), "core");
        assert_eq!(owners.owner_for_cell("liteinst", &full).unwrap(), "core");
        let archived = BTreeSet::from(["archived-nightly".to_string()]);
        assert_eq!(owners.owner_for_cell("sabre", &archived).unwrap(), "patching");
        let focused = BTreeSet::from(["sabre-compat-only".to_string()]);
        assert_eq!(owners.owner_for_cell("ptrace", &focused).unwrap(), "patching");
        assert!(owners.owner_for_cell("unknown", &full).is_err());
    }
    #[test]
    fn exact_disabled_dbt_and_sabre_cells_select_exactly_one_under_their_owner() {
        // Regression: pressure-test exact cells ran test-harness with the
        // default core execution owner, whose backend set excludes dbt
        // and sabre, so every exact dbt/sabre selection came back empty
        // ("filters selected no cells") and the nightly survey parts
        // recorded all cells INCOMPLETE (2026-11-05/06 sweeps).
        let root = crate::git_environment::checkout_root();
        let owners = ExecutionOwners::load(&root).unwrap();
        let manifests = ManifestSet::load(&root).unwrap();
        for (backend, labels) in [
            ("dbt", Vec::new()),
            ("sabre", vec!["archived-nightly".to_string()]),
        ] {
            let run_types = if labels.is_empty() {
                BTreeSet::from([crate::runner::DEFAULT_RUN_TYPE.to_string()])
            } else {
                labels.iter().cloned().collect()
            };
            let owner = owners.owner_for_cell(backend, &run_types).unwrap();
            let exact = Selection {
                population: Some(Population::Disabled),
                test: Some("applications/git-repository-workflow".into()),
                mode: Some("verify".into()),
                backend: Some(backend.into()),
                labels: labels.clone(),
                execution_backends: Some(
                    owners.backends_for_selection(&owner, &labels).unwrap(),
                ),
                ..Default::default()
            };
            let cells = manifests.select(&exact).unwrap();
            assert_eq!(
                cells.len(),
                1,
                "exact {backend} cell must select exactly one cell under owner {owner}"
            );
            assert_eq!(cells[0].id.backend.as_deref(), Some(backend));
            let mut core_only = exact.clone();
            core_only.execution_backends = Some(owners.backends("core").unwrap());
            assert!(
                manifests.select(&core_only).unwrap().is_empty(),
                "the core-only execution filter must select no {backend} cell"
            );
        }
    }
    #[test]
    fn execution_owner_refuses_unknown_empty_duplicate_and_changed_core_population() {
        let source = include_bytes!("../../execution-owners.json");
        let owners = ExecutionOwners::from_slice(source).unwrap();
        assert!(owners.backends("unknown").is_err());
        assert!(owners.owner(Some("unknown")).is_err());
        let focused = vec!["sabre-compat-only".to_string()];
        assert!(owners.backends_for_selection("core", &focused).is_err());
        let root = crate::git_environment::checkout_root();
        let manifests = ManifestSet::load(&root).unwrap();
        owners.validate_manifest_backends(&manifests).unwrap();
        let original = Selection {
            population: Some(Population::Required),
            labels: focused.clone(),
            ..Default::default()
        };
        let mut selected = original.clone();
        selected.execution_backends =
            Some(owners.backends_for_selection("patching", &focused).unwrap());
        let original = manifests.select(&original).unwrap();
        let actual = manifests.select(&selected).unwrap();
        assert_eq!(
            original.iter().map(|cell| &cell.id).collect::<Vec<_>>(),
            actual.iter().map(|cell| &cell.id).collect::<Vec<_>>()
        );
        assert!(
            actual
                .iter()
                .any(|cell| cell.id.backend.as_deref() == Some("ptrace")),
            "focused comparison cells are required too"
        );
        assert!(
            actual
                .iter()
                .any(|cell| cell.id.backend.as_deref() == Some("sabre"))
        );

        let valid: serde_json::Value = serde_json::from_slice(source).unwrap();
        let mut empty = valid.clone();
        empty["backends"] = serde_json::json!({});
        assert!(
            ExecutionOwners::from_slice(&serde_json::to_vec(&empty).unwrap())
                .err()
                .unwrap()
                .contains("closed nonempty")
        );
        let mut changed = valid.clone();
        changed["backends"]["dbt"] = serde_json::json!("core");
        let error = ExecutionOwners::from_slice(&serde_json::to_vec(&changed).unwrap())
            .err()
            .unwrap();
        assert!(
            error.contains("core execution owner differs from the actual first-party frontend set"),
            "{error}"
        );
        let duplicate = String::from_utf8(source.to_vec()).unwrap().replacen(
            "\"schema\": 1",
            "\"schema\": 1, \"schema\": 1",
            1,
        );
        assert!(
            ExecutionOwners::from_slice(duplicate.as_bytes())
                .err()
                .unwrap()
                .contains("duplicate JSON object key")
        );
    }
}
