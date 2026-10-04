/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Machine-readable evidence that the selected backend performed its own work.

use serde::Deserialize;
use serde::Serialize;

/// The backend-specific value used by compatibility-envelope scoring.
///
/// The variants keep each number attached to what it counts. A bare numeric
/// field would allow a scheduler-turn count to be consumed as a mapped-site or
/// branch count while remaining perfectly well formed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "backend", rename_all = "lowercase", deny_unknown_fields)]
pub enum BackendEngagement {
    Ptrace {
        scheduler_turns: u64,
    },
    E9patch {
        candidate_sites: u64,
        mapped_sites: u64,
        b0_sites: u64,
    },
    Dbt {
        counted_branches: u64,
    },
}

/// One complete `--backend-engagement-json` record.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackendEngagementReport {
    pub schema: u8,
    pub engagement: BackendEngagement,
}

impl BackendEngagementReport {
    pub const SCHEMA: u8 = 2;

    pub const fn new(engagement: BackendEngagement) -> Self {
        Self {
            schema: Self::SCHEMA,
            engagement,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema != Self::SCHEMA {
            return Err(format!(
                "backend-engagement schema must be {}, got {}",
                Self::SCHEMA,
                self.schema
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_and_counter_cannot_be_recombined() {
        let report = BackendEngagementReport::new(BackendEngagement::E9patch {
            candidate_sites: 7,
            mapped_sites: 7,
            b0_sites: 0,
        });
        let json = serde_json::to_string(&report).unwrap();
        assert_eq!(
            json,
            r#"{"schema":2,"engagement":{"backend":"e9patch","candidate_sites":7,"mapped_sites":7,"b0_sites":0}}"#
        );
        let changed =
            serde_json::to_string(&BackendEngagementReport::new(BackendEngagement::E9patch {
                candidate_sites: 8,
                mapped_sites: 8,
                b0_sites: 0,
            }))
            .unwrap();
        assert_ne!(json, changed, "the producer's value must reach the record");

        let mismatched = r#"{"schema":2,"engagement":{"backend":"e9patch","counted_branches":7}}"#;
        let error = serde_json::from_str::<BackendEngagementReport>(mismatched).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unknown field `counted_branches`")
        );
    }

    #[test]
    fn unsupported_schema_refuses_by_name() {
        let report = BackendEngagementReport {
            schema: 3,
            engagement: BackendEngagement::Dbt {
                counted_branches: 11,
            },
        };
        assert_eq!(
            report.validate().unwrap_err(),
            "backend-engagement schema must be 2, got 3"
        );
    }
    #[test]
    fn e9patch_result_requires_all_three_counts() {
        for incomplete in [
            r#"{"schema":2,"engagement":{"backend":"e9patch","mapped_sites":7,"b0_sites":0}}"#,
            r#"{"schema":2,"engagement":{"backend":"e9patch","candidate_sites":7,"b0_sites":0}}"#,
            r#"{"schema":2,"engagement":{"backend":"e9patch","candidate_sites":7,"mapped_sites":7}}"#,
        ] {
            assert!(serde_json::from_str::<BackendEngagementReport>(incomplete).is_err());
        }
    }
}
