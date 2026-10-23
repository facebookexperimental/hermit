//! Separate fixed-stack observation envelope; M1's typed bundle is unchanged.

use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use super::m1_bundle;

pub const FROZEN_C: &str = "5df36fa1c66dd9c5d9a7e52960685f88b8fd167b9c94054cea019de57363c84a";
pub const FROZEN_ORACLE: &str = "cbb3cd550d524d981352f520b4a4f756bd89703c6ab5197e399fd36d17e456c2";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bundle {
    pub schema: u32,
    pub allocator: m1_bundle::Bundle,
    pub guest: PathBuf,
    pub guest_receipt: PathBuf,
    pub guest_source_sha256: String,
    pub stack_oracle_sha256: String,
    pub executed_tests: u64,
    pub full_stack_isolation_claimed: bool,
}
