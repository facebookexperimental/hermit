//! Independent constructor-placement receipt; the M1 and M2 envelopes retain
//! their original meanings and required observations.

use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bundle {
    pub schema: u32,
    pub stacks: super::m2_bundle::Bundle,
    pub guest: PathBuf,
    pub guest_receipt: PathBuf,
    pub guest_source_sha256: String,
    pub consumer_sha256: String,
    pub executed_tests: u64,
    pub full_constructor_byte_isolation_claimed: bool,
}
