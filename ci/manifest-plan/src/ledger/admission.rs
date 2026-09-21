//! Retained admission-floor transport. These values preserve a driver's already
//! authenticated proof; parsing a value or checking its digest grants no authority.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;

use super::HistoryRow;

pub const ADMISSION_CONTEXT_CONTRACT: &str = "ci-hub-admission-floor/v1";
pub const ADMISSION_CONTEXT_V2_CONTRACT: &str = "ci-hub-admission-floor/v2";
pub const ADMISSION_CONTEXT_MAX_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionFloorV1 {
    pub kind: String,
    pub sha: String,
    pub tree: String,
    pub observed_main_sha: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionAuthorityV1 {
    pub slot: u64,
    pub kind: String,
    pub target: String,
    pub host: String,
    pub owner_host: String,
    pub owner_boot_id: String,
    pub owner_pid: u32,
    pub owner_start_ticks: u64,
    pub response_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionPinBindingV1 {
    pub node: String,
    pub base_sha: String,
    pub canonical_command_sha256: String,
    pub execution_command_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionContextV1 {
    pub contract: String,
    pub observed_at: String,
    pub target_sha: String,
    pub target_tree: String,
    pub host: String,
    pub run_id: String,
    pub started_at: String,
    pub log_file: String,
    pub authority: AdmissionAuthorityV1,
    pub floor: AdmissionFloorV1,
    pub canonical_plan_sha256: String,
    pub execution_plan_sha256: String,
    pub pin_bindings: Vec<AdmissionPinBindingV1>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionContextArtifactV1 {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionFloorEvidenceV1 {
    pub context: AdmissionContextV1,
    pub artifact: AdmissionContextArtifactV1,
}

/// A portable name, not a filesystem capability. Consumers must separately
/// bind their verified state root and use regular-file, no-follow resolution.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceLocatorV2 {
    pub scope: String,
    pub path: String,
}

impl WorkspaceLocatorV2 {
    pub fn validate(&self) -> Result<(), String> {
        if self.scope != "workspace"
            || self.path.trim().is_empty()
            || self
                .path
                .split('/')
                .any(|part| matches!(part, "" | "." | ".."))
            || self.path.contains('\\')
            || self.path.chars().any(char::is_control)
            || !transport_safe_string(&self.path)
        {
            return Err("invalid portable workspace locator".into());
        }
        Ok(())
    }

    /// The narrower production retention boundary. This checks components,
    /// not file existence, root authority, or absence of symlink traversal.
    pub fn validate_retained_path(&self) -> Result<(), String> {
        self.validate()?;
        if !self.path.starts_with("ignored/validate/") {
            return Err("admission locator is outside ignored/validate/".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionContextV2 {
    pub contract: String,
    pub observed_at: String,
    pub target_sha: String,
    pub target_tree: String,
    pub host: String,
    pub run_id: String,
    pub started_at: String,
    pub log_identity: WorkspaceLocatorV2,
    pub authority: AdmissionAuthorityV1,
    pub floor: AdmissionFloorV1,
    pub canonical_plan_sha256: String,
    pub execution_plan_sha256: String,
    pub pin_bindings: Vec<AdmissionPinBindingV1>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionContextArtifactV2 {
    pub locator: WorkspaceLocatorV2,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionFloorEvidenceV2 {
    pub context: AdmissionContextV2,
    pub artifact: AdmissionContextArtifactV2,
}

/// Wire serialization has no enum tag; the required context contract selects
/// exactly one strict schema. V1 is never normalized into V2.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum AdmissionEvidence {
    V1(AdmissionFloorEvidenceV1),
    V2(AdmissionFloorEvidenceV2),
}

fn transport_safe_string(value: &str) -> bool {
    !["{{WORKSPACE_ROOT}}", "{{HOME}}", "/home/"]
        .iter()
        .any(|token| value.contains(token))
}

fn transport_safe_value(value: &Value) -> bool {
    match value {
        Value::String(value) => transport_safe_string(value),
        Value::Array(values) => values.iter().all(transport_safe_value),
        Value::Object(values) => values
            .iter()
            .all(|(key, value)| transport_safe_string(key) && transport_safe_value(value)),
        _ => true,
    }
}

/// Capture duplicates before any Value map discards them. The ordinary value
/// retains historical last-value semantics when no V2 identity is claimed.
struct CapturedValue {
    value: Value,
    duplicate: bool,
}

/// The historical extension map plus private original-input metadata. The
/// serialized form is exactly the BTreeMap; duplicate refusal state is exposed
/// only by HistoryRow::admission_log_identity for checked transient delegation.
/// Ordinary map mutation deliberately does not clear original bad metadata.
#[derive(Clone, Debug, Default)]
pub struct HistoryExtensions {
    values: BTreeMap<String, Value>,
    duplicate_log_identities: Option<Vec<Value>>,
}

impl std::ops::Deref for HistoryExtensions {
    type Target = BTreeMap<String, Value>;
    fn deref(&self) -> &Self::Target {
        &self.values
    }
}

impl std::ops::DerefMut for HistoryExtensions {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.values
    }
}

impl PartialEq for HistoryExtensions {
    fn eq(&self, other: &Self) -> bool {
        self.values == other.values
    }
}
impl Eq for HistoryExtensions {}

impl Serialize for HistoryExtensions {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.values.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for HistoryExtensions {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_extensions(deserializer)
    }
}

impl<'de> Deserialize<'de> for CapturedValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Capture;
        impl<'de> serde::de::Visitor<'de> for Capture {
            type Value = CapturedValue;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("JSON value with original duplicate membership")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Self::Value, M::Error> {
                let mut fields = serde_json::Map::new();
                let mut duplicate = false;
                while let Some(key) = map.next_key::<String>()? {
                    let captured = map.next_value::<CapturedValue>()?;
                    duplicate |= captured.duplicate || fields.contains_key(&key);
                    fields.insert(key, captured.value);
                }
                Ok(CapturedValue {
                    value: Value::Object(fields),
                    duplicate,
                })
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                let mut duplicate = false;
                while let Some(captured) = seq.next_element::<CapturedValue>()? {
                    duplicate |= captured.duplicate;
                    values.push(captured.value);
                }
                Ok(CapturedValue {
                    value: Value::Array(values),
                    duplicate,
                })
            }
            fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(CapturedValue {
                    value: value.into(),
                    duplicate: false,
                })
            }
            fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(CapturedValue {
                    value: value.into(),
                    duplicate: false,
                })
            }
            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(CapturedValue {
                    value: value.into(),
                    duplicate: false,
                })
            }
            fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Self::Value, E> {
                let number = serde_json::Number::from_f64(value)
                    .ok_or_else(|| E::custom("nonfinite JSON number"))?;
                Ok(CapturedValue {
                    value: Value::Number(number),
                    duplicate: false,
                })
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(CapturedValue {
                    value: value.into(),
                    duplicate: false,
                })
            }
            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(CapturedValue {
                    value: value.into(),
                    duplicate: false,
                })
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(CapturedValue {
                    value: Value::Null,
                    duplicate: false,
                })
            }
        }
        deserializer.deserialize_any(Capture)
    }
}

impl<'de> Deserialize<'de> for AdmissionEvidence {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let captured = CapturedValue::deserialize(deserializer)?;
        if captured.duplicate {
            return Err(serde::de::Error::custom(
                "duplicate admission evidence member",
            ));
        }
        match captured
            .value
            .get("context")
            .and_then(|c| c.get("contract"))
            .and_then(Value::as_str)
        {
            Some(ADMISSION_CONTEXT_CONTRACT) => serde_json::from_value(captured.value)
                .map(Self::V1)
                .map_err(serde::de::Error::custom),
            Some(ADMISSION_CONTEXT_V2_CONTRACT) => serde_json::from_value(captured.value)
                .map(Self::V2)
                .map_err(serde::de::Error::custom),
            _ => Err(serde::de::Error::custom(
                "unknown admission context contract",
            )),
        }
    }
}

pub fn admission_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

pub fn admission_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Exact driver timestamp grammar, including Gregorian calendar validity.
pub fn admission_timestamp(value: &str) -> bool {
    let b = value.as_bytes();
    if b.len() != 20
        || [4, 7, 10, 13, 16, 19]
            .into_iter()
            .zip(b"--T::Z")
            .any(|(i, c)| b[i] != *c)
        || b.iter()
            .enumerate()
            .any(|(i, c)| ![4, 7, 10, 13, 16, 19].contains(&i) && !c.is_ascii_digit())
    {
        return false;
    }
    let number = |start: usize, end: usize| {
        b[start..end]
            .iter()
            .fold(0_u32, |n, c| n * 10 + u32::from(c - b'0'))
    };
    let year = number(0, 4);
    let month = number(5, 7);
    let day = number(8, 10);
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        _ => return false,
    };
    year != 0
        && (1..=days).contains(&day)
        && number(11, 13) < 24
        && number(14, 16) < 60
        && number(17, 19) < 60
}

/// This canonicalization is scoped to context v1. In particular it must never
/// replace HistoryRow's existing struct-order receipt serialization.
pub fn admission_context_bytes(context: &AdmissionContextV1) -> Result<Vec<u8>, String> {
    context.validate()?;
    canonical_context_bytes(&serde_json::to_value(context).map_err(|e| e.to_string())?)
}

fn canonical_context_bytes(value: &Value) -> Result<Vec<u8>, String> {
    fn encode(value: &Value, out: &mut Vec<u8>) -> Result<(), String> {
        match value {
            Value::Object(fields) => {
                let sorted: BTreeMap<_, _> = fields.iter().collect();
                out.push(b'{');
                for (index, (key, value)) in sorted.into_iter().enumerate() {
                    if index != 0 {
                        out.push(b',');
                    }
                    serde_json::to_writer(&mut *out, key).map_err(|e| e.to_string())?;
                    out.push(b':');
                    encode(value, out)?;
                }
                out.push(b'}');
            }
            Value::Array(values) => {
                out.push(b'[');
                for (index, value) in values.iter().enumerate() {
                    if index != 0 {
                        out.push(b',');
                    }
                    encode(value, out)?;
                }
                out.push(b']');
            }
            Value::Number(n) if !n.is_u64() => {
                return Err("context v1 forbids floats and negative integers".into());
            }
            _ => serde_json::to_writer(&mut *out, value).map_err(|e| e.to_string())?,
        }
        Ok(())
    }
    let mut bytes = Vec::new();
    encode(value, &mut bytes)?;
    if bytes.len() > ADMISSION_CONTEXT_MAX_BYTES {
        return Err("admission context exceeds its 1 MiB wire bound".into());
    }
    Ok(bytes)
}

/// Parse the actual retained bytes, before any generic JSON map can collapse
/// duplicate fields. A digest of noncanonical or malformed bytes is no proof.
pub fn admission_context_from_bytes(bytes: &[u8]) -> Result<AdmissionContextV1, String> {
    if bytes.len() > ADMISSION_CONTEXT_MAX_BYTES {
        return Err("admission context exceeds its 1 MiB wire bound".into());
    }
    let context: AdmissionContextV1 = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if admission_context_bytes(&context)? != bytes {
        return Err("admission context is not canonical v1 JSON".into());
    }
    Ok(context)
}

pub fn admission_receipt_echo(bytes: &[u8]) -> Result<AdmissionFloorEvidenceV1, String> {
    #[derive(Deserialize)]
    struct Echo {
        admission_floor_evidence: AdmissionFloorEvidenceV1,
    }
    let evidence = serde_json::from_slice::<Echo>(bytes)
        .map_err(|e| e.to_string())?
        .admission_floor_evidence;
    evidence.validate()?;
    Ok(evidence)
}

// Preserve the original four-field decoder, including defaults, duplicate-field
// refusals (also after null), and positional input. The run-ID extension is
// captured beside it, never inserted into the historical field sequence.
#[derive(Deserialize)]
struct CoverageFields {
    #[serde(default)]
    planned_test_nodes: u64,
    #[serde(default)]
    executed_test_nodes: u64,
    #[serde(default)]
    zero_executed_nodes: Option<Vec<String>>,
    #[serde(default)]
    absent_nodes: Option<Vec<String>>,
}

impl CoverageFields {
    fn with_admission_run_id(self, admission_run_id: Option<Value>) -> super::CoverageRow {
        super::CoverageRow {
            admission_run_id,
            planned_test_nodes: self.planned_test_nodes,
            executed_test_nodes: self.executed_test_nodes,
            zero_executed_nodes: self.zero_executed_nodes,
            absent_nodes: self.absent_nodes,
        }
    }
}

impl<'de> Deserialize<'de> for super::CoverageRow {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Coverage;
        struct Capture<'a, M> {
            map: M,
            ids: &'a mut Vec<Value>,
        }
        impl<'de, M: serde::de::MapAccess<'de>> serde::de::MapAccess<'de> for Capture<'_, M> {
            type Error = M::Error;
            fn next_key_seed<K: serde::de::DeserializeSeed<'de>>(
                &mut self,
                seed: K,
            ) -> Result<Option<K::Value>, Self::Error> {
                while let Some(key) = self.map.next_key::<String>()? {
                    if key == "run_id" {
                        self.ids.push(self.map.next_value()?);
                    } else {
                        return seed
                            .deserialize(serde::de::value::StringDeserializer::<M::Error>::new(key))
                            .map(Some);
                    }
                }
                Ok(None)
            }
            fn next_value_seed<V: serde::de::DeserializeSeed<'de>>(
                &mut self,
                seed: V,
            ) -> Result<V::Value, Self::Error> {
                self.map.next_value_seed(seed)
            }
        }
        impl<'de> serde::de::Visitor<'de> for Coverage {
            type Value = super::CoverageRow;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("CoverageRow")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                map: M,
            ) -> Result<Self::Value, M::Error> {
                let mut ids = Vec::new();
                let fields = CoverageFields::deserialize(
                    serde::de::value::MapAccessDeserializer::new(Capture { map, ids: &mut ids }),
                )?;
                let identity = match ids.len() {
                    0 => None,
                    1 => ids.pop(),
                    _ => Some(Value::Array(ids)),
                };
                Ok(fields.with_admission_run_id(identity))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                seq: A,
            ) -> Result<Self::Value, A::Error> {
                CoverageFields::deserialize(serde::de::value::SeqAccessDeserializer::new(seq))
                    .map(|fields| fields.with_admission_run_id(None))
            }
        }
        deserializer.deserialize_struct(
            "CoverageRow",
            &[
                "planned_test_nodes",
                "executed_test_nodes",
                "zero_executed_nodes",
                "absent_nodes",
            ],
            Coverage,
        )
    }
}

/// Preserve extension ordering and historical shapes while decoding this new
/// claim at the raw boundary, where duplicate keys have not been erased.
pub(super) fn deserialize_extensions<'de, D>(deserializer: D) -> Result<HistoryExtensions, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct Extensions;
    impl<'de> serde::de::Visitor<'de> for Extensions {
        type Value = HistoryExtensions;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("history row extensions")
        }
        fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
        where
            M: serde::de::MapAccess<'de>,
        {
            let mut result = BTreeMap::new();
            let mut log_identities = Vec::new();
            let mut duplicate_log_identity = false;
            while let Some(key) = map.next_key::<String>()? {
                let value = if key == "admission_floor_evidence" {
                    if result.contains_key(&key) {
                        return Err(serde::de::Error::duplicate_field(
                            "admission_floor_evidence",
                        ));
                    }
                    let evidence = map.next_value::<AdmissionEvidence>()?;
                    evidence.validate().map_err(serde::de::Error::custom)?;
                    serde_json::to_value(evidence).map_err(serde::de::Error::custom)?
                } else if key == "log_identity" {
                    let captured = map.next_value::<CapturedValue>()?;
                    duplicate_log_identity |= captured.duplicate || !log_identities.is_empty();
                    log_identities.push(captured.value.clone());
                    captured.value
                } else {
                    map.next_value()?
                };
                result.insert(key, value);
            }
            Ok(HistoryExtensions {
                values: result,
                duplicate_log_identities: duplicate_log_identity.then_some(log_identities),
            })
        }
    }
    deserializer.deserialize_map(Extensions)
}

// Shared checks are identical across versions; only the log representation and
// transport constraints differ. Never encode a V2 context as a fabricated V1.
struct AdmissionIdentity<'a> {
    observed_at: &'a str,
    target_sha: &'a str,
    target_tree: &'a str,
    host: &'a str,
    run_id: &'a str,
    started_at: &'a str,
    log_valid: bool,
    authority: &'a AdmissionAuthorityV1,
    floor: &'a AdmissionFloorV1,
    canonical_plan_sha256: &'a str,
    execution_plan_sha256: &'a str,
    pin_bindings: &'a [AdmissionPinBindingV1],
}

impl AdmissionIdentity<'_> {
    fn validate(&self) -> Result<(), String> {
        let a = self.authority;
        let f = self.floor;
        if !admission_timestamp(self.observed_at) || !admission_timestamp(self.started_at) {
            return Err("admission context requires actual UTC-second timestamps".into());
        }
        if [
            self.target_sha,
            self.target_tree,
            a.target.as_str(),
            f.sha.as_str(),
            f.tree.as_str(),
            f.observed_main_sha.as_str(),
        ]
        .iter()
        .any(|v| !admission_hex(v, 40))
            || [
                self.canonical_plan_sha256,
                self.execution_plan_sha256,
                a.response_sha256.as_str(),
            ]
            .iter()
            .any(|v| !admission_hex(v, 64))
        {
            return Err("admission context has an invalid object ID or digest".into());
        }
        if [self.host, self.run_id, a.owner_boot_id.as_str()]
            .iter()
            .any(|s| {
                s.trim_matches(|c: char| {
                    c.is_whitespace() || ('\u{001c}'..='\u{001f}').contains(&c)
                })
                .is_empty()
                    || s.contains('\0')
            })
            || !self.log_valid
            || a.target != self.target_sha
            || a.host != self.host
            || a.owner_host != self.host
            || a.owner_pid <= 1
            || a.owner_start_ticks == 0
        {
            return Err("admission context has inconsistent run/owner identity".into());
        }
        match (a.kind.as_str(), f.kind.as_str()) {
            ("validate", "current-main") if f.sha == f.observed_main_sha => {}
            ("frozen-validate", "frozen-target")
                if f.sha == self.target_sha && f.tree == self.target_tree => {}
            _ => return Err("admission floor disagrees with its authority kind/identity".into()),
        }
        let mut nodes = BTreeSet::new();
        for binding in self.pin_bindings {
            if !matches!(
                binding.node.as_str(),
                "pre.reverie_pin" | "pre.reverie_pin_on_host" | "check.lint_checks"
            ) || !nodes.insert(&binding.node)
                || binding.base_sha != f.sha
                || !admission_hex(&binding.canonical_command_sha256, 64)
                || !admission_hex(&binding.execution_command_sha256, 64)
                || binding.canonical_command_sha256 == binding.execution_command_sha256
            {
                return Err("admission context has an invalid or duplicate pin binding".into());
            }
        }
        Ok(())
    }
}

impl AdmissionContextV1 {
    pub fn validate(&self) -> Result<(), String> {
        if self.contract != ADMISSION_CONTEXT_CONTRACT {
            return Err("unknown admission context contract".into());
        }
        AdmissionIdentity {
            observed_at: &self.observed_at,
            target_sha: &self.target_sha,
            target_tree: &self.target_tree,
            host: &self.host,
            run_id: &self.run_id,
            started_at: &self.started_at,
            authority: &self.authority,
            floor: &self.floor,
            canonical_plan_sha256: &self.canonical_plan_sha256,
            execution_plan_sha256: &self.execution_plan_sha256,
            pin_bindings: &self.pin_bindings,
            log_valid: self.log_file.starts_with('/') && !self.log_file.contains('\0'),
        }
        .validate()
    }
}

impl AdmissionContextV2 {
    pub fn validate(&self) -> Result<(), String> {
        if self.contract != ADMISSION_CONTEXT_V2_CONTRACT {
            return Err("unknown admission context contract".into());
        }
        self.log_identity.validate()?;
        if !transport_safe_value(&serde_json::to_value(self).map_err(|e| e.to_string())?) {
            return Err(
                "V2 context contains a nonportable substitution token or owner path".into(),
            );
        }
        AdmissionIdentity {
            observed_at: &self.observed_at,
            target_sha: &self.target_sha,
            target_tree: &self.target_tree,
            host: &self.host,
            run_id: &self.run_id,
            started_at: &self.started_at,
            authority: &self.authority,
            floor: &self.floor,
            canonical_plan_sha256: &self.canonical_plan_sha256,
            execution_plan_sha256: &self.execution_plan_sha256,
            pin_bindings: &self.pin_bindings,
            log_valid: true,
        }
        .validate()
    }
}

impl AdmissionFloorEvidenceV1 {
    pub fn validate(&self) -> Result<(), String> {
        let bytes = admission_context_bytes(&self.context)?;
        if !self.artifact.path.starts_with('/')
            || self.artifact.path.contains('\0')
            || self.artifact.bytes != bytes.len() as u64
            || self.artifact.sha256 != admission_sha256(&bytes)
        {
            return Err("admission artifact does not match the inline context bytes".into());
        }
        Ok(())
    }

    pub fn validate_for_row(&self, row: &HistoryRow) -> Result<(), String> {
        self.validate()?;
        let c = &self.context;
        validate_original_row(
            row,
            &c.target_sha,
            &c.target_tree,
            &c.host,
            &c.run_id,
            &c.started_at,
            Some(&c.log_file),
        )
    }
}

fn validate_original_row(
    row: &HistoryRow,
    target_sha: &str,
    target_tree: &str,
    host: &str,
    expected_run_id: &str,
    started_at: &str,
    log_file: Option<&str>,
) -> Result<(), String> {
    if row.commit.as_deref() != Some(target_sha)
        || row.tree()? != Some(target_tree)
        || row.host.as_deref() != Some(host)
        || row.run_id.as_deref() != Some(expected_run_id)
        || row.started_at.as_deref() != Some(started_at)
        || log_file.is_some_and(|log| row.log_file.as_deref() != Some(log))
    {
        return Err("admission context does not belong to the selected original row".into());
    }
    if let Some(run_id) = row
        .coverage
        .as_ref()
        .and_then(|coverage| coverage.admission_run_id.as_ref())
    {
        if run_id.as_str() != Some(expected_run_id) {
            return Err("admission context disagrees with coverage.run_id".into());
        }
    }
    for (name, run_id) in [
        (
            "cell_results",
            row.cell_results
                .as_ref()
                .and_then(super::CellResultsValue::admission_run_id),
        ),
        (
            "test_results",
            row.test_results
                .as_ref()
                .and_then(super::TestResultsValue::admission_run_id),
        ),
    ] {
        if let Some(run_id) = run_id {
            if run_id.as_str() != Some(expected_run_id) {
                return Err(format!("admission context disagrees with {name}.run_id"));
            }
        }
    }
    Ok(())
}

pub fn admission_context_v2_bytes(context: &AdmissionContextV2) -> Result<Vec<u8>, String> {
    context.validate()?;
    canonical_context_bytes(&serde_json::to_value(context).map_err(|e| e.to_string())?)
}

pub fn admission_context_v2_from_bytes(bytes: &[u8]) -> Result<AdmissionContextV2, String> {
    if bytes.len() > ADMISSION_CONTEXT_MAX_BYTES {
        return Err("admission context exceeds its 1 MiB wire bound".into());
    }
    let context: AdmissionContextV2 = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if admission_context_v2_bytes(&context)? != bytes {
        return Err("admission context is not canonical v2 JSON".into());
    }
    Ok(context)
}

pub fn admission_evidence_receipt_echo(bytes: &[u8]) -> Result<AdmissionEvidence, String> {
    #[derive(Deserialize)]
    struct Echo {
        admission_floor_evidence: AdmissionEvidence,
    }
    let evidence = serde_json::from_slice::<Echo>(bytes)
        .map_err(|e| e.to_string())?
        .admission_floor_evidence;
    evidence.validate()?;
    Ok(evidence)
}

impl AdmissionFloorEvidenceV2 {
    pub fn validate(&self) -> Result<(), String> {
        let bytes = admission_context_v2_bytes(&self.context)?;
        self.artifact.locator.validate()?;
        if self.artifact.bytes != bytes.len() as u64
            || self.artifact.sha256 != admission_sha256(&bytes)
        {
            return Err("admission artifact does not match the inline context bytes".into());
        }
        Ok(())
    }

    pub fn validate_for_row(&self, row: &HistoryRow) -> Result<(), String> {
        self.validate()?;
        let c = &self.context;
        validate_original_row(
            row,
            &c.target_sha,
            &c.target_tree,
            &c.host,
            &c.run_id,
            &c.started_at,
            None,
        )?;
        let identity: WorkspaceLocatorV2 = serde_json::from_value(
            row.admission_log_identity()
                .ok_or("V2 selected row has no log_identity")?,
        )
        .map_err(|e| format!("invalid original row log_identity: {e}"))?;
        identity.validate()?;
        if identity != c.log_identity {
            return Err("admission context disagrees with original row log_identity".into());
        }
        Ok(())
    }
}

impl AdmissionEvidence {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::V1(e) => e.validate(),
            Self::V2(e) => e.validate(),
        }
    }
    pub fn validate_for_row(&self, row: &HistoryRow) -> Result<(), String> {
        match self {
            Self::V1(e) => e.validate_for_row(row),
            Self::V2(e) => e.validate_for_row(row),
        }
    }
    pub fn context_bytes(&self) -> Result<Vec<u8>, String> {
        match self {
            Self::V1(e) => admission_context_bytes(&e.context),
            Self::V2(e) => admission_context_v2_bytes(&e.context),
        }
    }
    pub fn floor(&self) -> &AdmissionFloorV1 {
        match self {
            Self::V1(e) => &e.context.floor,
            Self::V2(e) => &e.context.floor,
        }
    }
    pub fn authority(&self) -> &AdmissionAuthorityV1 {
        match self {
            Self::V1(e) => &e.context.authority,
            Self::V2(e) => &e.context.authority,
        }
    }
    pub fn is_canonical(&self) -> bool {
        self.validate().is_ok() && self.floor().kind == "current-main"
    }
    pub fn log_identity(&self) -> Option<&WorkspaceLocatorV2> {
        match self {
            Self::V1(_) => None,
            Self::V2(e) => Some(&e.context.log_identity),
        }
    }
    pub fn artifact_locator(&self) -> Option<&WorkspaceLocatorV2> {
        match self {
            Self::V1(_) => None,
            Self::V2(e) => Some(&e.artifact.locator),
        }
    }
    pub fn artifact_sha256(&self) -> &str {
        match self {
            Self::V1(e) => &e.artifact.sha256,
            Self::V2(e) => &e.artifact.sha256,
        }
    }
    pub fn artifact_bytes(&self) -> u64 {
        match self {
            Self::V1(e) => e.artifact.bytes,
            Self::V2(e) => e.artifact.bytes,
        }
    }
}

impl HistoryRow {
    /// Versioned original-row check. Parsing/digests preserve an already held
    /// proof; they do not supply live admission or local artifact availability.
    pub fn admission_evidence(&self) -> Result<Option<AdmissionEvidence>, String> {
        let Some(value) = self.extra.get("admission_floor_evidence") else {
            return Ok(None);
        };
        let evidence: AdmissionEvidence = serde_json::from_value(value.clone())
            .map_err(|e| format!("invalid admission evidence: {e}"))?;
        evidence.validate_for_row(self)?;
        Ok(Some(evidence))
    }

    /// Original portable identity, including a nonobject duplicate sentinel for
    /// a V2 claim. Delegation must not turn that sentinel into a valid locator.
    pub fn admission_log_identity(&self) -> Option<Value> {
        self.extra
            .duplicate_log_identities
            .as_ref()
            .map(|identities| Value::Array(identities.clone()))
            .or_else(|| self.extra.get("log_identity").cloned())
    }

    /// Historical absence is unknown. A malformed present claim is an error,
    /// including null; no current lock or sibling row may fill it in.
    pub fn admission_floor_evidence(&self) -> Result<Option<AdmissionFloorEvidenceV1>, String> {
        let Some(value) = self.extra.get("admission_floor_evidence") else {
            return Ok(None);
        };
        let evidence: AdmissionFloorEvidenceV1 = serde_json::from_value(value.clone())
            .map_err(|e| format!("invalid admission floor evidence: {e}"))?;
        evidence.validate_for_row(self)?;
        Ok(Some(evidence))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> AdmissionContextV1 {
        AdmissionContextV1 {
            contract: ADMISSION_CONTEXT_CONTRACT.into(),
            observed_at: "2026-09-18T15:00:00Z".into(),
            target_sha: "1".repeat(40),
            target_tree: "2".repeat(40),
            host: "fixture-host".into(),
            run_id: "actual-e2e-fixture-run".into(),
            started_at: "2026-09-18T15:00:01Z".into(),
            log_file: "/retained/fixture/run.log".into(),
            authority: AdmissionAuthorityV1 {
                slot: 0,
                kind: "validate".into(),
                target: "1".repeat(40),
                host: "fixture-host".into(),
                owner_host: "fixture-host".into(),
                owner_boot_id: "11111111-2222-3333-4444-555555555555".into(),
                owner_pid: 123,
                owner_start_ticks: 456,
                response_sha256: "3".repeat(64),
            },
            floor: AdmissionFloorV1 {
                kind: "current-main".into(),
                sha: "4".repeat(40),
                tree: "5".repeat(40),
                observed_main_sha: "4".repeat(40),
            },
            canonical_plan_sha256: "6".repeat(64),
            execution_plan_sha256: "7".repeat(64),
            pin_bindings: vec![AdmissionPinBindingV1 {
                node: "pre.reverie_pin".into(),
                base_sha: "4".repeat(40),
                canonical_command_sha256: "8".repeat(64),
                execution_command_sha256: "9".repeat(64),
            }],
        }
    }

    fn context_v2() -> AdmissionContextV2 {
        let c = context();
        AdmissionContextV2 {
            contract: ADMISSION_CONTEXT_V2_CONTRACT.into(),
            observed_at: c.observed_at,
            target_sha: c.target_sha,
            target_tree: c.target_tree,
            host: c.host,
            run_id: c.run_id,
            started_at: c.started_at,
            log_identity: WorkspaceLocatorV2 {
                scope: "workspace".into(),
                path: "ignored/validate/exact-run.log".into(),
            },
            authority: c.authority,
            floor: c.floor,
            canonical_plan_sha256: c.canonical_plan_sha256,
            execution_plan_sha256: c.execution_plan_sha256,
            pin_bindings: c.pin_bindings,
        }
    }

    fn evidence_v2(c: AdmissionContextV2) -> AdmissionFloorEvidenceV2 {
        let bytes = admission_context_v2_bytes(&c).unwrap();
        AdmissionFloorEvidenceV2 {
            context: c,
            artifact: AdmissionContextArtifactV2 {
                locator: WorkspaceLocatorV2 {
                    scope: "workspace".into(),
                    path: "ignored/validate/admission/exact-run/context.json".into(),
                },
                sha256: admission_sha256(&bytes),
                bytes: bytes.len() as u64,
            },
        }
    }

    fn row_v2(e: &AdmissionFloorEvidenceV2) -> Value {
        let c = &e.context;
        serde_json::json!({"schema_version":9,"result":"fail","commit":c.target_sha,
            "tree":c.target_tree,"host":c.host,"run_id":c.run_id,"started_at":c.started_at,
            "log_file":"/home/example/work/ignored/validate/exact-run.log",
            "log_identity":c.log_identity,"admission_floor_evidence":e})
    }

    #[test]
    fn portable_admission_v2_golden_bytes_and_frozen_status() {
        let basic = context_v2();
        let mut escaped = basic.clone();
        escaped.run_id = "quote\"/backslash\\/tab\t/newline\n/control\u{0001}/é/雪/😀".into();
        let mut large = basic.clone();
        large.authority.slot = u64::MAX;
        large.authority.owner_start_ticks = u64::MAX;
        large.authority.owner_pid = u32::MAX;
        let mut frozen = basic.clone();
        frozen.authority.kind = "frozen-validate".into();
        frozen.floor.kind = "frozen-target".into();
        frozen.floor.sha = frozen.target_sha.clone();
        frozen.floor.tree = frozen.target_tree.clone();
        frozen.pin_bindings[0].base_sha = frozen.target_sha.clone();
        for (name, c) in [
            ("basic", basic),
            ("escaped", escaped),
            ("large-u64", large),
            ("frozen", frozen),
        ] {
            let bytes = admission_context_v2_bytes(&c).unwrap();
            assert_eq!(admission_context_v2_from_bytes(&bytes).unwrap(), c);
            assert!(!bytes.ends_with(b"\n"));
            let e = AdmissionEvidence::V2(evidence_v2(c));
            assert_eq!(e.context_bytes().unwrap(), bytes);
            assert_eq!(e.is_canonical(), name != "frozen");
            assert_eq!(e.artifact_sha256(), admission_sha256(&bytes));
            assert_eq!(e.artifact_bytes(), bytes.len() as u64);
            e.artifact_locator()
                .unwrap()
                .validate_retained_path()
                .unwrap();
            e.log_identity().unwrap().validate_retained_path().unwrap();
            let raw =
                serde_json::to_vec(&serde_json::json!({"admission_floor_evidence": e})).unwrap();
            assert_eq!(admission_evidence_receipt_echo(&raw).unwrap(), e);
            assert!(admission_receipt_echo(&raw).is_err());
            if let Some(root) = std::env::var_os("HERMIT_ADMISSION_V2_GOLDEN_DIR") {
                use std::io::Write;
                let path = std::path::Path::new(&root).join(format!("{name}.json"));
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
                    .unwrap();
                file.write_all(&bytes).unwrap();
                file.sync_all().unwrap();
            }
        }
    }

    #[test]
    fn portable_locator_refuses_traversal_tokens_and_distinguishes_local_boundary() {
        let locator = |path: &str| WorkspaceLocatorV2 {
            scope: "workspace".into(),
            path: path.into(),
        };
        for path in [
            "ignored/validate/run.log",
            "ignored/validate/é-雪-😀.log",
            "relative/run.log",
        ] {
            locator(path).validate().unwrap();
        }
        assert!(
            locator("relative/run.log")
                .validate_retained_path()
                .is_err()
        );
        assert!(
            locator("ignored/validate-other/run.log")
                .validate_retained_path()
                .is_err()
        );
        for path in [
            "",
            " ",
            "\u{00a0}",
            "\u{2003}",
            "/absolute",
            "../run",
            "a/../run",
            "a/./run",
            "a//run",
            "run/",
            "./run",
            "a\\run",
            "a\0run",
            "a\nrun",
            "a\u{0085}run",
            "{{WORKSPACE_ROOT}}/run",
            "a/{{HOME}}/run",
            "a/home/example/run",
        ] {
            assert!(locator(path).validate().is_err(), "{path:?}");
        }
        let mut bad = locator("ignored/validate/run.log");
        bad.scope = "host".into();
        assert!(bad.validate().is_err());
        for token in ["{{WORKSPACE_ROOT}}", "{{HOME}}", "/home/example"] {
            let mut c = context_v2();
            c.run_id = token.into();
            assert!(admission_context_v2_bytes(&c).is_err());
            let mut c = context_v2();
            c.authority.owner_boot_id = token.into();
            assert!(admission_context_v2_bytes(&c).is_err());
        }
    }

    #[test]
    fn portable_context_raw_version_and_schema_are_not_fallbacks() {
        let e = evidence_v2(context_v2());
        let raw = serde_json::to_string(&e).unwrap();
        for (old, new) in [
            (ADMISSION_CONTEXT_V2_CONTRACT, ADMISSION_CONTEXT_CONTRACT),
            (ADMISSION_CONTEXT_V2_CONTRACT, "ci-hub-admission-floor/v99"),
            ("\"slot\":0", "\"slot\":true"),
            ("\"slot\":0", "\"slot\":0.0"),
            ("\"slot\":0", "\"slot\":0,\"slot\":0"),
            (
                "\"scope\":\"workspace\"",
                "\"scope\":\"workspace\",\"scope\":\"workspace\"",
            ),
            (
                "\"scope\":\"workspace\"",
                "\"scope\":\"workspace\",\"unknown\":0",
            ),
        ] {
            let bad = raw.replacen(old, new, 1);
            assert_ne!(bad, raw);
            assert!(
                serde_json::from_str::<AdmissionEvidence>(&bad).is_err(),
                "{bad}"
            );
        }
        let canonical = admission_context_v2_bytes(&e.context).unwrap();
        let mut newline = canonical.clone();
        newline.push(b'\n');
        assert!(admission_context_v2_from_bytes(&newline).is_err());
        let c = context();
        let bytes = admission_context_bytes(&c).unwrap();
        let old = AdmissionFloorEvidenceV1 {
            context: c,
            artifact: AdmissionContextArtifactV1 {
                path: "/retained/context.json".into(),
                sha256: admission_sha256(&bytes),
                bytes: bytes.len() as u64,
            },
        };
        let versioned: AdmissionEvidence =
            serde_json::from_value(serde_json::to_value(&old).unwrap()).unwrap();
        assert_eq!(versioned, AdmissionEvidence::V1(old.clone()));
        assert_eq!(versioned.context_bytes().unwrap(), bytes);
        assert_eq!(
            serde_json::to_vec(&versioned).unwrap(),
            serde_json::to_vec(&old).unwrap()
        );
        assert!(admission_context_v2_from_bytes(&bytes).is_err());
        if let Some(path) = std::env::var_os("HERMIT_ADMISSION_V2_INTEROP_MANIFEST") {
            let path = std::path::Path::new(&path);
            let manifest: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            let cases = manifest["cases"].as_array().unwrap();
            assert!(!cases.is_empty());
            for case in cases {
                let raw =
                    std::fs::read(path.parent().unwrap().join(case["file"].as_str().unwrap()))
                        .unwrap();
                assert_eq!(raw.len() as u64, case["bytes"].as_u64().unwrap());
                assert_eq!(admission_sha256(&raw), case["sha256"].as_str().unwrap());
                let accepted = match case["kind"].as_str().unwrap() {
                    "context" => admission_context_v2_from_bytes(&raw).is_ok(),
                    "row" => serde_json::from_slice::<HistoryRow>(&raw).is_ok_and(|row| {
                        row.admission_evidence().is_ok_and(|evidence| {
                            matches!(evidence, Some(AdmissionEvidence::V2(_)))
                        })
                    }),
                    kind => panic!("unknown interop case kind {kind}"),
                };
                assert_eq!(
                    accepted,
                    case["expected_accept"].as_bool().unwrap(),
                    "{}",
                    case["name"]
                );
            }
            println!(
                "actual V2 raw interop corpus: {} cases checked",
                cases.len()
            );
        }
    }

    #[test]
    fn portable_original_row_duplicate_identity_is_retained_until_validation() {
        let e = evidence_v2(context_v2());
        let mut row = row_v2(&e);
        row.as_object_mut().unwrap().remove("log_identity");
        let mut prefix = serde_json::to_string(&row).unwrap();
        prefix.pop();
        let good = serde_json::to_string(&e.context.log_identity).unwrap();
        let wrong = r#"{"scope":"workspace","path":"ignored/validate/other/run.log"}"#;
        for members in [
            format!("\"log_identity\":{wrong},\"log_identity\":{good}"),
            format!("\"log_identity\":{good},\"log_identity\":{good}"),
            format!("\"log_identity\":null,\"log_identity\":{good}"),
            format!("\"log_identity\":{good},\"log_identity\":{wrong}"),
            r#""log_identity":{"scope":"wrong","scope":"workspace","path":"ignored/validate/exact-run.log"}"#.into(),
            r#""log_identity":{"scope":"workspace","path":"ignored/validate/wrong","path":"ignored/validate/exact-run.log"}"#.into(),
        ] {
            let raw = format!("{prefix},{members}}}");
            let original: HistoryRow = serde_json::from_str(&raw).unwrap();
            assert!(original.admission_log_identity().unwrap().is_array(), "{raw}");
            assert!(original.admission_evidence().is_err(), "{raw}");
            // Historical serialization keeps last-value bytes. Only the
            // original-row guard and transient request retain duplicate proof.
            let collapsed: HistoryRow = serde_json::from_value(serde_json::from_str::<Value>(&raw).unwrap()).unwrap();
            assert_eq!(serde_json::to_vec(&original).unwrap(), serde_json::to_vec(&collapsed).unwrap());
            let mut cloned = original.clone();
            cloned.extra.insert("log_identity".into(), serde_json::to_value(&e.context.log_identity).unwrap());
            assert!(cloned.admission_evidence().is_err());
            let mut delegated = serde_json::to_value(&original).unwrap();
            delegated["log_identity"] = original.admission_log_identity().unwrap();
            assert!(serde_json::from_value::<HistoryRow>(delegated).unwrap().admission_evidence().is_err());
            let legacy_raw = format!("{{{members}}}");
            let legacy: HistoryRow = serde_json::from_str(&legacy_raw).unwrap();
            let last_value: HistoryRow = serde_json::from_value(serde_json::from_str::<Value>(&legacy_raw).unwrap()).unwrap();
            assert!(legacy.admission_evidence().unwrap().is_none());
            assert_eq!(serde_json::to_vec(&legacy).unwrap(),serde_json::to_vec(&last_value).unwrap());
        }
        for identity in [
            Value::Null,
            serde_json::json!(1),
            serde_json::json!([]),
            serde_json::json!({"scope":"workspace","path":"ignored/validate/different/exact-run.log"}),
        ] {
            let mut row = row_v2(&e);
            row["log_identity"] = identity;
            let row: HistoryRow = serde_json::from_value(row).unwrap();
            assert!(row.admission_evidence().is_err());
        }
    }

    #[test]
    fn portable_context_preserves_original_row_and_nested_run_bindings() {
        let e = evidence_v2(context_v2());
        let original = row_v2(&e);
        let row: HistoryRow = serde_json::from_value(original.clone()).unwrap();
        assert_eq!(
            row.admission_evidence().unwrap(),
            Some(AdmissionEvidence::V2(e.clone()))
        );
        assert!(row.admission_floor_evidence().is_err());
        for (key, value) in [
            ("commit", Value::from("a".repeat(40))),
            ("tree", Value::from("b".repeat(40))),
            ("host", Value::from("other")),
            ("run_id", Value::from("other")),
            ("started_at", Value::from("2026-09-18T15:00:02Z")),
        ] {
            let mut bad = original.clone();
            bad[key] = value;
            assert!(
                serde_json::from_value::<HistoryRow>(bad)
                    .unwrap()
                    .admission_evidence()
                    .is_err(),
                "{key}"
            );
        }
        for name in ["coverage", "cell_results", "test_results"] {
            for value in [
                Value::Null,
                Value::from("wrong"),
                Value::from(1),
                serde_json::json!([]),
            ] {
                let mut bad = original.clone();
                bad[name] = serde_json::json!({"run_id":value});
                assert!(
                    serde_json::from_value::<HistoryRow>(bad)
                        .unwrap()
                        .admission_evidence()
                        .is_err(),
                    "{name}"
                );
            }
            let mut raw = serde_json::to_string(&original).unwrap();
            raw.pop();
            raw.push_str(&format!(
                ",\"{name}\":{{\"run_id\":\"wrong\",\"run_id\":\"actual-e2e-fixture-run\"}}}}"
            ));
            assert!(
                serde_json::from_str::<HistoryRow>(&raw)
                    .unwrap()
                    .admission_evidence()
                    .is_err(),
                "{name}"
            );
        }
        let mut bad = e;
        bad.artifact.sha256 = "0".repeat(64);
        assert!(bad.validate().is_err());
    }

    #[test]
    fn portable_context_identity_survives_raw_and_expanded_display_paths() {
        let e = evidence_v2(context_v2());
        let bytes = admission_context_v2_bytes(&e.context).unwrap();
        let mut serialized_rows = BTreeSet::new();
        for display in [
            "{{WORKSPACE_ROOT}}/ignored/validate/exact-run.log",
            "/home/example/work/ignored/validate/exact-run.log",
            "/srv/reader/ignored/validate/exact-run.log",
        ] {
            let mut value = row_v2(&e);
            value["log_file"] = display.into();
            let row: HistoryRow = serde_json::from_value(value).unwrap();
            let evidence = row.admission_evidence().unwrap().unwrap();
            assert_eq!(evidence.context_bytes().unwrap(), bytes);
            assert_eq!(evidence.artifact_sha256(), e.artifact.sha256);
            serialized_rows.insert(serde_json::to_string(&row).unwrap());
        }
        // Only the context's immutable identity is root-independent. Existing
        // HistoryRow serialization still includes its local display fields.
        assert_eq!(serialized_rows.len(), 3);
        let mut bad = row_v2(&e);
        bad["log_identity"]["path"] = "ignored/validate/other/exact-run.log".into();
        assert!(
            serde_json::from_value::<HistoryRow>(bad)
                .unwrap()
                .admission_evidence()
                .is_err()
        );
    }

    #[test]
    fn admission_context_canonical_golden_bytes() {
        let basic = context();
        let mut escaped = basic.clone();
        escaped.log_file =
            "/retained/quote\"/backslash\\/tab\t/newline\n/control\u{0001}/é/雪/😀/run.log".into();
        let mut large = basic.clone();
        large.authority.slot = u64::MAX;
        large.authority.owner_start_ticks = u64::MAX;
        large.authority.owner_pid = u32::MAX;
        let mut frozen = basic.clone();
        frozen.authority.kind = "frozen-validate".into();
        frozen.floor.kind = "frozen-target".into();
        frozen.floor.sha = frozen.target_sha.clone();
        frozen.floor.tree = frozen.target_tree.clone();
        frozen.pin_bindings[0].base_sha = frozen.target_sha.clone();
        for (name, value) in [
            ("basic", basic),
            ("escaped", escaped),
            ("large-u64", large),
            ("frozen", frozen),
        ] {
            let bytes = admission_context_bytes(&value).unwrap();
            assert_eq!(
                serde_json::from_slice::<AdmissionContextV1>(&bytes).unwrap(),
                value
            );
            assert!(!bytes.ends_with(b"\n"));
            assert!(bytes.starts_with(b"{\"authority\":{\"host\":"));
            if name == "escaped" {
                let text = std::str::from_utf8(&bytes).unwrap();
                assert!(text.contains("é/雪/😀"));
                assert!(text.contains(r"\u0001"));
                assert!(text.contains(r"\t"));
            }
            if name == "large-u64" {
                assert!(
                    std::str::from_utf8(&bytes)
                        .unwrap()
                        .contains("18446744073709551615")
                );
            }
            // Optional evidence export from the actual Rust serializer; ordinary
            // library tests need no host path or pre-existing artifact.
            if let Some(root) = std::env::var_os("HERMIT_ADMISSION_GOLDEN_DIR") {
                use std::io::Write;
                let path = std::path::Path::new(&root).join(format!("{name}.json"));
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
                    .unwrap();
                file.write_all(&bytes).unwrap();
                file.sync_all().unwrap();
            }
        }
    }

    #[test]
    fn admission_context_refuses_malformed_present_claims() {
        let original = context();
        let raw = String::from_utf8(admission_context_bytes(&original).unwrap()).unwrap();
        for malformed in [
            raw.replacen("\"slot\":0", "\"slot\":true", 1),
            raw.replacen("\"slot\":0", "\"slot\":0.0", 1),
            raw.replacen("\"slot\":0", "\"slot\":-1", 1),
            raw.replacen("\"slot\":0", "\"slot\":0,\"slot\":0", 1),
            raw.replacen("\"slot\":0", "\"slot\":0,\"unknown\":0", 1),
        ] {
            assert!(
                serde_json::from_str::<AdmissionContextV1>(&malformed).is_err(),
                "{malformed}"
            );
        }
        let mut bad = original.clone();
        bad.floor.sha = "a".repeat(40);
        assert!(admission_context_bytes(&bad).is_err());
        let mut bad = original.clone();
        bad.pin_bindings.push(bad.pin_bindings[0].clone());
        assert!(admission_context_bytes(&bad).is_err());
        let mut bad = original.clone();
        bad.pin_bindings[0].execution_command_sha256 =
            bad.pin_bindings[0].canonical_command_sha256.clone();
        assert!(admission_context_bytes(&bad).is_err());
        for invalid in ["", " \t\n", "\u{001c}", "\u{2003}", "bad\0identity"] {
            let mut bad = original.clone();
            bad.run_id = invalid.into();
            assert!(admission_context_bytes(&bad).is_err());
            let mut bad = original.clone();
            bad.authority.owner_boot_id = invalid.into();
            assert!(admission_context_bytes(&bad).is_err());
            let mut bad = original.clone();
            bad.host = invalid.into();
            bad.authority.host = invalid.into();
            bad.authority.owner_host = invalid.into();
            assert!(admission_context_bytes(&bad).is_err());
        }
        let mut bad = original.clone();
        bad.log_file = "relative/run.log".into();
        assert!(admission_context_bytes(&bad).is_err());
        let mut bad = original;
        bad.observed_at = "unknown".into();
        assert!(admission_context_bytes(&bad).is_err());
    }

    #[test]
    fn admission_raw_context_and_row_reject_duplicates_before_map_collapse() {
        let context = context();
        let bytes = admission_context_bytes(&context).unwrap();
        assert_eq!(admission_context_from_bytes(&bytes).unwrap(), context);
        let mut newline = bytes.clone();
        newline.push(b'\n');
        assert!(admission_context_from_bytes(&newline).is_err());
        let evidence = AdmissionFloorEvidenceV1 {
            context,
            artifact: AdmissionContextArtifactV1 {
                path: "/retained/context.json".into(),
                sha256: admission_sha256(&bytes),
                bytes: bytes.len() as u64,
            },
        };
        let raw = serde_json::to_string(&serde_json::json!({"admission_floor_evidence":evidence}))
            .unwrap();
        assert!(serde_json::from_str::<HistoryRow>(&raw).is_ok());
        let duplicate = raw.replacen("\"slot\":0", "\"slot\":0,\"slot\":0", 1);
        assert_ne!(duplicate, raw);
        assert!(serde_json::from_str::<HistoryRow>(&duplicate).is_err());
        let duplicate = format!(
            "{{\"admission_floor_evidence\":{},\"admission_floor_evidence\":{}}}",
            serde_json::to_string(&evidence).unwrap(),
            serde_json::to_string(&evidence).unwrap()
        );
        assert!(serde_json::from_str::<HistoryRow>(&duplicate).is_err());
        assert!(
            serde_json::from_str::<HistoryRow>(r#"{"admission_floor_evidence":null}"#).is_err()
        );
    }

    #[test]
    fn admission_raw_cross_language_corpus() {
        // The ordinary test still exercises the real raw parser without a host
        // artifact. Explicit interop runs additionally consume EVERY sealed case.
        let bytes = admission_context_bytes(&context()).unwrap();
        assert!(admission_context_from_bytes(&bytes).is_ok());
        if let Some(path) = std::env::var_os("HERMIT_ADMISSION_INTEROP_MANIFEST") {
            let path = std::path::Path::new(&path);
            let manifest: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            let cases = manifest["cases"].as_array().unwrap();
            assert!(!cases.is_empty());
            for case in cases {
                let raw =
                    std::fs::read(path.parent().unwrap().join(case["file"].as_str().unwrap()))
                        .unwrap();
                assert_eq!(raw.len() as u64, case["bytes"].as_u64().unwrap());
                assert_eq!(admission_sha256(&raw), case["sha256"].as_str().unwrap());
                assert_eq!(
                    admission_context_from_bytes(&raw).is_ok(),
                    case["expected_accept"].as_bool().unwrap(),
                    "{}",
                    case["name"]
                );
            }
            println!("actual raw interop corpus: {} cases checked", cases.len());
        }
    }

    #[test]
    fn legacy_coverage_fields_keep_raw_parser_and_serialization_contract() {
        let empty = r#"{"planned_test_nodes":0,"executed_test_nodes":0,"zero_executed_nodes":null,"absent_nodes":null}"#;
        for (raw, expected) in [
            ("{}", empty),
            ("[]", empty),
            (r#"{"run_id":"one","run_id":"two"}"#, empty),
            (r#"{"future":null,"future":{},"run_id":null}"#, empty),
            (
                "[1]",
                r#"{"planned_test_nodes":1,"executed_test_nodes":0,"zero_executed_nodes":null,"absent_nodes":null}"#,
            ),
            (
                "[1,2,null,null]",
                r#"{"planned_test_nodes":1,"executed_test_nodes":2,"zero_executed_nodes":null,"absent_nodes":null}"#,
            ),
            (
                "[1,2,[],[]]",
                r#"{"planned_test_nodes":1,"executed_test_nodes":2,"zero_executed_nodes":[],"absent_nodes":[]}"#,
            ),
        ] {
            let row: super::super::CoverageRow = serde_json::from_str(raw).unwrap();
            assert_eq!(serde_json::to_string(&row).unwrap(), expected, "{raw}");
        }
        for raw in [
            "[null]",
            "[1,2,[],[],0]",
            r#"{"planned_test_nodes":null}"#,
            r#"{"executed_test_nodes":true}"#,
            r#"{"zero_executed_nodes":null,"zero_executed_nodes":[]}"#,
            r#"{"absent_nodes":null,"absent_nodes":[]}"#,
            r#"{"planned_test_nodes":0,"planned_test_nodes":0}"#,
            r#"{"executed_test_nodes":0,"executed_test_nodes":0}"#,
        ] {
            assert!(
                serde_json::from_str::<super::super::CoverageRow>(raw).is_err(),
                "{raw}"
            );
        }
    }

    #[test]
    fn new_admission_refuses_duplicate_coverage_ids_even_if_they_match() {
        let c = context();
        let bytes = admission_context_bytes(&c).unwrap();
        let evidence = AdmissionFloorEvidenceV1 {
            context: c.clone(),
            artifact: AdmissionContextArtifactV1 {
                path: "/retained/context.json".into(),
                sha256: admission_sha256(&bytes),
                bytes: bytes.len() as u64,
            },
        };
        let row = serde_json::json!({"commit":c.target_sha,"tree":c.target_tree,"host":c.host,
            "run_id":c.run_id,"started_at":c.started_at,"log_file":c.log_file,
            "admission_floor_evidence":evidence});
        let mut raw = serde_json::to_string(&row).unwrap();
        raw.pop();
        raw.push_str(r#", "coverage":{"run_id":"actual-e2e-fixture-run","run_id":"actual-e2e-fixture-run"}}"#);
        let decoded: HistoryRow = serde_json::from_str(&raw).unwrap();
        assert!(decoded.admission_floor_evidence().is_err());
        assert_eq!(
            decoded
                .coverage
                .as_ref()
                .unwrap()
                .admission_run_id
                .as_ref()
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let legacy: HistoryRow =
            serde_json::from_str(r#"{"coverage":{"run_id":"one","run_id":"two"}}"#).unwrap();
        assert!(legacy.admission_floor_evidence().unwrap().is_none());
        let without: HistoryRow = serde_json::from_str(r#"{"coverage":{}}"#).unwrap();
        assert_eq!(
            serde_json::to_vec(&legacy).unwrap(),
            serde_json::to_vec(&without).unwrap()
        );
    }

    fn nested_identity_row(kind: &str, ids: &[Value], claim: bool) -> String {
        let c = context();
        let bytes = admission_context_bytes(&c).unwrap();
        let mut row = serde_json::json!({"schema_version":7,"commit":c.target_sha,
            "tree":c.target_tree,"host":c.host,"run_id":c.run_id,
            "started_at":c.started_at,"log_file":c.log_file,"result":"fail"});
        if claim {
            row["admission_floor_evidence"] = serde_json::to_value(AdmissionFloorEvidenceV1 {
                context: c.clone(),
                artifact: AdmissionContextArtifactV1 {
                    path: "/retained/fixture/context.json".into(),
                    sha256: admission_sha256(&bytes),
                    bytes: bytes.len() as u64,
                },
            })
            .unwrap();
        }
        let mut nested = match kind {
            "raw_tests" => serde_json::json!({"future_shape":true}),
            "typed_tests" => serde_json::json!({"path":"full","hermit_sha":c.target_sha,
                "source_tree_dirty":false,"selected_count":0,"recorded_count":0,
                "population_sha256":"b".repeat(64),"selected":{"nodes":[],"compatibility":false},
                "nodes":[],"compatibility":null,
                "totals":{"executed_tests":0,"passed_tests":0,"failed_tests":0,"filtered_tests":0},
                "artifact":{"path":"ignored/validate/artifacts/fixture/tests.jsonl","sha256":"c".repeat(64),"row_count":0}}),
            _ => serde_json::json!({"hermit_sha":c.target_sha,"source_tree_dirty":false,
                "selected_count":0,"recorded_count":0,"population_sha256":"b".repeat(64),
                "artifact":{"path":"ignored/validate/artifacts/fixture/cells.jsonl","sha256":"c".repeat(64),"row_count":0},
                "selected":[],"cells":[]}),
        };
        if kind == "schema8_cells" {
            row["schema_version"] = serde_json::json!(8);
            nested["path"] = serde_json::json!("quick");
        }
        let name = if kind.ends_with("tests") {
            "test_results"
        } else {
            "cell_results"
        };
        let mut raw_nested = serde_json::to_string(&nested).unwrap();
        raw_nested.pop();
        for id in ids {
            raw_nested.push_str(",\"run_id\":");
            raw_nested.push_str(&serde_json::to_string(id).unwrap());
        }
        raw_nested.push('}');
        let mut raw = serde_json::to_string(&row).unwrap();
        raw.pop();
        raw.push_str(&format!(",{name:?}:{raw_nested}}}"));
        raw
    }

    fn nested_duplicate_ids() -> Vec<Vec<Value>> {
        let right = serde_json::json!(context().run_id);
        vec![
            vec![serde_json::json!("wrong"), right.clone()],
            vec![right.clone(), serde_json::json!("wrong")],
            vec![right.clone(), right.clone()],
            vec![Value::Null, right],
        ]
    }

    #[test]
    fn raw_nested_run_id_duplicates_are_preserved_for_admission() {
        for kind in ["typed_cells", "schema8_cells", "raw_tests", "typed_tests"] {
            for ids in nested_duplicate_ids() {
                let mut row: HistoryRow =
                    serde_json::from_str(&nested_identity_row(kind, &ids, true)).unwrap();
                assert!(row.admission_floor_evidence().is_err(), "{kind}: {ids:?}");
                let retained = if kind.ends_with("tests") {
                    row.test_results.as_ref().unwrap().admission_run_id()
                } else {
                    row.cell_results.as_ref().unwrap().admission_run_id()
                };
                assert_eq!(retained, Some(Value::Array(ids.clone())));
                assert!(row.clone().admission_floor_evidence().is_err());
                if kind == "typed_cells" {
                    let cell = row.cell_results.as_mut().unwrap();
                    cell.typed_mut().unwrap().run_id = context().run_id;
                    assert_eq!(cell.admission_run_id(), Some(Value::Array(ids.clone())));
                    assert!(
                        row.admission_floor_evidence().is_err(),
                        "typed_mut erased original duplicate evidence"
                    );
                }
            }
            for ids in [vec![], vec![serde_json::json!(context().run_id)]] {
                let row: HistoryRow =
                    serde_json::from_str(&nested_identity_row(kind, &ids, true)).unwrap();
                assert!(
                    row.admission_floor_evidence().unwrap().is_some(),
                    "{kind}: {ids:?}"
                );
            }
            for invalid in [
                Value::Null,
                serde_json::json!(42),
                serde_json::json!("wrong"),
                serde_json::json!([]),
                serde_json::json!({}),
            ] {
                let row: HistoryRow =
                    serde_json::from_str(&nested_identity_row(kind, &[invalid], true)).unwrap();
                assert!(row.admission_floor_evidence().is_err());
            }
        }
        for name in ["cell_results", "test_results"] {
            for first in ["null", "{}"] {
                assert!(
                    serde_json::from_str::<HistoryRow>(&format!(
                        "{{{name:?}:{first},{name:?}:{{}}}}"
                    ))
                    .is_err()
                );
            }
        }
    }

    #[test]
    fn legacy_nested_run_id_metadata_preserves_public_behavior() {
        let mut cases = nested_duplicate_ids();
        cases.extend([
            vec![],
            vec![serde_json::json!(context().run_id)],
            vec![Value::Null],
            vec![serde_json::json!(42)],
            vec![serde_json::json!([])],
            vec![serde_json::json!({})],
        ]);
        for kind in ["typed_cells", "schema8_cells", "raw_tests", "typed_tests"] {
            for ids in &cases {
                let raw = nested_identity_row(kind, ids, false);
                let row: HistoryRow = serde_json::from_str(&raw).unwrap();
                let normalized: HistoryRow =
                    serde_json::from_value(serde_json::from_str::<Value>(&raw).unwrap()).unwrap();
                assert!(row.admission_floor_evidence().unwrap().is_none());
                assert_eq!(
                    serde_json::to_vec(&row).unwrap(),
                    serde_json::to_vec(&normalized).unwrap(),
                    "{kind}: {ids:?}"
                );
                assert_eq!(row.cell_results, normalized.cell_results);
                assert_eq!(row.test_results, normalized.test_results);
                assert_eq!(
                    row.cell_results_evidence(),
                    normalized.cell_results_evidence()
                );
                assert_eq!(
                    row.cell_results_validate_path(),
                    normalized.cell_results_validate_path()
                );
                if kind == "typed_tests" && ids.last().is_some_and(Value::is_string) {
                    let actual = row.test_results.as_ref().unwrap().schema9().unwrap();
                    let expected = normalized.test_results.as_ref().unwrap().schema9().unwrap();
                    assert_eq!(actual, expected);
                }
                if ids.len() < 2 {
                    let actual = if kind.ends_with("tests") {
                        row.test_results.as_ref().unwrap().admission_run_id()
                    } else {
                        row.cell_results.as_ref().unwrap().admission_run_id()
                    };
                    assert_eq!(actual, ids.first().cloned());
                    if kind == "typed_cells" && ids.last().is_some_and(Value::is_string) {
                        assert!(matches!(
                            row.cell_results,
                            Some(super::super::CellResultsValue::Typed(_))
                        ));
                    }
                }
            }
        }
        for raw in [
            "null",
            "true",
            "42",
            "-4",
            "2.5",
            "\"text\"",
            "[]",
            "{\"unknown\":1,\"unknown\":2}",
        ] {
            let expected: Value = serde_json::from_str(raw).unwrap();
            let test: super::super::TestResultsValue = serde_json::from_str(raw).unwrap();
            let cell: super::super::CellResultsValue = serde_json::from_str(raw).unwrap();
            assert_eq!(serde_json::to_value(test).unwrap(), expected);
            assert_eq!(serde_json::to_value(cell).unwrap(), expected);
        }
        assert!(
            serde_json::from_str::<super::super::CellResultsValue>(
                r#"{"binding_contract":"x","binding_contract":"x"}"#
            )
            .is_err()
        );
        let test: super::super::TestResultsValue =
            serde_json::from_str(r#"{"binding_contract":"x","binding_contract":"x"}"#).unwrap();
        assert_eq!(
            serde_json::to_value(test).unwrap(),
            serde_json::json!({"binding_contract":"x"})
        );
    }

    #[test]
    fn admission_context_calendar_and_selected_row_binding() {
        for valid in ["2000-02-29T00:00:00Z", "2026-09-18T23:59:59Z"] {
            assert!(admission_timestamp(valid));
        }
        for invalid in [
            "1900-02-29T00:00:00Z",
            "2026-02-29T00:00:00Z",
            "2026-04-31T00:00:00Z",
            "2026-01-01T24:00:00Z",
            "2026-01-01T00:00:60Z",
            "0000-01-01T00:00:00Z",
            "2026-01-01T00:00:00+00:00",
        ] {
            assert!(!admission_timestamp(invalid), "{invalid}");
        }
        let c = context();
        let bytes = admission_context_bytes(&c).unwrap();
        let evidence = AdmissionFloorEvidenceV1 {
            context: c.clone(),
            artifact: AdmissionContextArtifactV1 {
                path: "/retained/fixture/context.json".into(),
                sha256: admission_sha256(&bytes),
                bytes: bytes.len() as u64,
            },
        };
        let row = serde_json::json!({"commit":c.target_sha,"tree":c.target_tree,"host":c.host,
            "run_id":c.run_id,"started_at":c.started_at,"log_file":c.log_file,
            "result":"fail","admission_floor_evidence":evidence});
        let decoded: HistoryRow = serde_json::from_value(row.clone()).unwrap();
        assert_eq!(decoded.result.as_deref(), Some("fail"));
        assert!(decoded.admission_floor_evidence().unwrap().is_some());
        for key in ["commit", "tree", "host", "run_id", "started_at", "log_file"] {
            let mut wrong = row.clone();
            wrong[key] = Value::String("another-row".into());
            assert!(
                serde_json::from_value::<HistoryRow>(wrong)
                    .unwrap()
                    .admission_floor_evidence()
                    .is_err()
            );
        }
        let mut wrong = row.clone();
        wrong["admission_floor_evidence"]["artifact"]["sha256"] = Value::String("0".repeat(64));
        assert!(serde_json::from_value::<HistoryRow>(wrong).is_err());
        let mut wrong = row.clone();
        wrong["admission_floor_evidence"]["artifact"]["path"] =
            Value::String("relative/context.json".into());
        assert!(serde_json::from_value::<HistoryRow>(wrong).is_err());
        for name in ["cell_results", "coverage", "test_results"] {
            let mut wrong = row.clone();
            wrong[name] = serde_json::json!({"run_id":"sibling-run"});
            // A structurally malformed typed container may refuse during row
            // decoding; a readable container must refuse the mismatched ID.
            if let Ok(decoded) = serde_json::from_value::<HistoryRow>(wrong) {
                assert!(decoded.admission_floor_evidence().is_err(), "{name}");
            }
        }
        let mut matching = row.clone();
        matching["coverage"] = serde_json::json!({"run_id":c.run_id});
        assert!(
            serde_json::from_value::<HistoryRow>(matching)
                .unwrap()
                .admission_floor_evidence()
                .unwrap()
                .is_some()
        );
        for invalid in [
            Value::Null,
            serde_json::json!(42),
            serde_json::json!("sibling-run"),
            serde_json::json!({}),
            serde_json::json!([]),
        ] {
            let mut wrong = row.clone();
            wrong["coverage"] = serde_json::json!({"run_id":invalid});
            assert!(
                serde_json::from_value::<HistoryRow>(wrong)
                    .unwrap()
                    .admission_floor_evidence()
                    .is_err()
            );
        }
        let mut legacy = row;
        legacy
            .as_object_mut()
            .unwrap()
            .remove("admission_floor_evidence");
        legacy["coverage"] = serde_json::json!({});
        let old_bytes =
            serde_json::to_vec(&serde_json::from_value::<HistoryRow>(legacy.clone()).unwrap())
                .unwrap();
        for retained in [
            Value::Null,
            serde_json::json!(42),
            serde_json::json!("historical-run"),
            serde_json::json!({}),
            serde_json::json!([]),
        ] {
            let mut value = legacy.clone();
            value["coverage"]["run_id"] = retained;
            let decoded = serde_json::from_value::<HistoryRow>(value).unwrap();
            assert!(decoded.admission_floor_evidence().unwrap().is_none());
            assert_eq!(
                serde_json::to_vec(&decoded).unwrap(),
                old_bytes,
                "historical receipt bytes changed"
            );
        }
    }
}
