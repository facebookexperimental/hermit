/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Marker for the built-in application's existing core execution driver.
//!
//! The application never selects this marker as a physical adapter. Its empty
//! session prevents an accidental fake physical run; the Common completion
//! variants exist only for the shared verifier's existing injected-run tests.

use hermit::Error;

use super::backend_hook::BackendAdapter;
use super::backend_hook::EvidenceRequirement;
use super::backend_hook::Execute;
use super::backend_hook::Execution;
use super::backend_hook::Observation;
use super::backend_hook::PairEvidence;
use super::backend_hook::Prepare;
use super::backend_hook::VerificationLogTransport;

pub(super) struct CoreApplicationAdapter;
pub(super) enum CoreSession {}
pub(super) enum CorePending {
    #[cfg(test)]
    Common,
}
pub(super) enum CoreFacts {
    #[cfg(test)]
    Common,
}

impl BackendAdapter for CoreApplicationAdapter {
    const NAME: &'static str = "core";
    const EVIDENCE: EvidenceRequirement = EvidenceRequirement::Common;
    const CAPABILITIES: reverie::BackendCapabilities = reverie::BackendCapabilities::PTRACE;
    const LOG_TRANSPORT: VerificationLogTransport = VerificationLogTransport::CoordinatorTracing;
    type Options = ();
    type Session = CoreSession;
    type Pending = CorePending;
    type Facts = CoreFacts;

    fn parse(&self, _: &clap::ArgMatches) -> Result<(), clap::Error> {
        Ok(())
    }
    fn prepare(&self, (): (), _request: Prepare<'_>) -> Result<CoreSession, Error> {
        Err(Error::msg(
            "built-in physical execution is owned by the core driver",
        ))
    }
    fn execute(
        &self,
        session: &mut CoreSession,
        _request: Execute<'_>,
    ) -> Result<Execution<CorePending>, Error> {
        match *session {}
    }
    fn finish(
        &self,
        pending: CorePending,
        _observation: Observation<'_>,
    ) -> Result<CoreFacts, Error> {
        match pending {
            #[cfg(test)]
            CorePending::Common => Ok(CoreFacts::Common),
        }
    }
    fn pair_evidence(&self, first: &CoreFacts, second: &CoreFacts) -> Result<PairEvidence, Error> {
        let _ = second;
        match *first {
            #[cfg(test)]
            CoreFacts::Common => Ok(PairEvidence::Common),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unsupported_strace_refuses_before_resolving_artifacts() {
        let error = CoreApplicationAdapter
            .strace(
                std::path::Path::new("/hermit-missing-disabled-sabre-program"),
                &[],
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not support syscall tracing")
        );
    }
}
