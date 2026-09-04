/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use clap::ValueEnum;
pub(crate) use hermit::logdiff_report::RecordEnvelopePolicy;

/// Versioned identity of the record envelope applied before log selection.
///
/// The predicate and its name travel together in [`RecordEnvelope`], so a
/// comparison cannot silently filter records while reporting an unfiltered
/// policy. Only the two fully specified policies are eligible for parity;
/// caller-defined predicates are deliberately non-qualifying.
/// A record predicate bound to the versioned policy name that describes it.
#[derive(Clone, Copy)]
pub(crate) struct RecordEnvelope {
    policy: RecordEnvelopePolicy,
    keep_record: fn(&str) -> bool,
}

impl std::fmt::Debug for RecordEnvelope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RecordEnvelope")
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl RecordEnvelope {
    pub(crate) const fn all_records_v1() -> Self {
        Self {
            policy: RecordEnvelopePolicy::AllRecordsV1,
            keep_record: keep_all_records,
        }
    }

    pub(crate) const fn dbt_evidence_transport_v1() -> Self {
        Self {
            policy: RecordEnvelopePolicy::DbtEvidenceTransportV1,
            keep_record: keep_dbt_evidence_record,
        }
    }

    pub(crate) const fn cross_backend_detcore_v1() -> Self {
        Self {
            policy: RecordEnvelopePolicy::CrossBackendDetcoreV1,
            keep_record: keep_cross_backend_detcore_record,
        }
    }

    #[cfg(test)]
    pub(crate) const fn caller_defined(keep_record: fn(&str) -> bool) -> Self {
        Self {
            policy: RecordEnvelopePolicy::CallerDefined,
            keep_record,
        }
    }

    pub(crate) fn policy(self) -> RecordEnvelopePolicy {
        self.policy
    }

    pub(crate) fn predicate(self) -> fn(&str) -> bool {
        self.keep_record
    }

    #[cfg(test)]
    pub(crate) fn keeps(self, record: &str) -> bool {
        (self.keep_record)(record)
    }
}

/// User-selectable named envelopes for standalone `hermit log-diff`.
///
/// The caller-defined variant is intentionally absent: the standalone command
/// cannot claim a policy whose predicate it cannot describe in its JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub(crate) enum RecordEnvelopeArg {
    AllRecordsV1,
    DbtEvidenceTransportV1,
    CrossBackendDetcoreV1,
}

impl RecordEnvelopeArg {
    pub(crate) const fn envelope(self) -> RecordEnvelope {
        match self {
            Self::AllRecordsV1 => RecordEnvelope::all_records_v1(),
            Self::DbtEvidenceTransportV1 => RecordEnvelope::dbt_evidence_transport_v1(),
            Self::CrossBackendDetcoreV1 => RecordEnvelope::cross_backend_detcore_v1(),
        }
    }
}

fn keep_all_records(_record: &str) -> bool {
    true
}

/// Return whether a parsed DBT evidence record describes the guest rather than
/// the evidence transport itself.
///
/// Archived DBT logs must remain classifiable in builds without the optional
/// DBT runtime, so this policy lives in the CLI layer and has no `dbt` feature
/// dependency.
fn keep_dbt_evidence_record(record: &str) -> bool {
    let Some((level, body)) = record.split_once(' ') else {
        return true;
    };
    !matches!(level, "ERROR" | "WARN" | "INFO" | "DEBUG" | "TRACE")
        || !body
            .trim_start_matches(' ')
            .starts_with("reverie_dbt::evidence:")
}

/// Keep only the shared Detcore observation stream when comparing different
/// execution backends.
///
/// Backend launchers necessarily emit different lifecycle and transport
/// records.  Those are excluded by this named envelope rather than by ad-hoc
/// substring ignores.  Once a record belongs to `detcore` or one of its
/// modules, its complete canonical INFO payload is retained: in particular,
/// virtual time and RCB values are never rounded, removed, or reset to make two
/// backends agree.
fn keep_cross_backend_detcore_record(record: &str) -> bool {
    let Some((level, body)) = record.split_once(' ') else {
        return false;
    };
    if !matches!(level, "ERROR" | "WARN" | "INFO" | "DEBUG" | "TRACE") {
        return false;
    }
    let target = body
        .trim_start_matches(' ')
        .split_once(':')
        .map(|(target, _)| target)
        .unwrap_or_default();
    target == "detcore" || target.starts_with("detcore::")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dbt_envelope_is_narrowly_keyed_on_the_transport_target() {
        let envelope = RecordEnvelope::dbt_evidence_transport_v1();
        assert!(!envelope.keeps("INFO reverie_dbt::evidence: protected evidence initialized"));
        assert!(!envelope.keeps("WARN  reverie_dbt::evidence: direct entry must be refused"));
        assert!(
            envelope.keeps(
                "INFO detcore: DETLOG reverie_dbt::evidence: protected evidence initialized"
            )
        );
        assert!(envelope.keeps("INFO reverie_dbt::launcher: starting"));
    }

    #[test]
    fn policy_json_name_is_exact_and_round_trips() {
        let policy = RecordEnvelopePolicy::DbtEvidenceTransportV1;
        let encoded = serde_json::to_string(&policy).unwrap();
        assert_eq!(encoded, "\"dbt_evidence_transport_v1\"");
        assert_eq!(
            serde_json::from_str::<RecordEnvelopePolicy>(&encoded).unwrap(),
            policy
        );
    }

    #[test]
    fn cross_backend_envelope_keeps_complete_detcore_records_only() {
        let envelope = RecordEnvelope::cross_backend_detcore_v1();
        assert!(envelope.keeps("INFO detcore::scheduler: COMMIT turn 7 at time 1.234_567_890s"));
        assert!(envelope.keeps("INFO detcore: DETLOG [syscall] finish syscall #3: read = Ok(4)"));
        assert!(!envelope.keeps("INFO reverie_kvm: lifecycle phase timings"));
        assert!(!envelope.keeps("INFO hermit::sabre::fallback: completed"));
        assert!(!envelope.keeps("INFO reverie_dbt::evidence: protected evidence initialized"));
        assert_eq!(envelope.policy().as_str(), "cross_backend_detcore_v1");
    }
}
