//! Hermit's typed M1 fixture bundle. The allocation oracle and ELF qualifier
//! live once in the exact pinned Reverie source, not in this envelope.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Leaf {
    pub runtime: PathBuf,
    pub receipt: PathBuf,
    pub profile_name: String,
    pub cargo_profile: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bundle {
    pub schema: u32,
    pub hermit_head: String,
    pub hermit_tree: String,
    pub hermit_lock_sha256: String,
    pub nested_lock_sha256: String,
    pub reverie_head: String,
    pub reverie_tree: String,
    pub oracle_sha256: BTreeMap<String, String>,
    pub guest: PathBuf,
    pub guest_receipt: PathBuf,
    pub standalone: Leaf,
    pub detcore: Leaf,
    pub config_fingerprint: String,
    pub producer_source: PathBuf,
    pub producer_sha256: String,
    pub executed_tests: u64,
    pub full_m1_pass_claimed: bool,
}
